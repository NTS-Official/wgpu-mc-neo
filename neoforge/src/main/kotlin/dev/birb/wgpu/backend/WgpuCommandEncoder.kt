package dev.birb.wgpu.backend

import com.mojang.blaze3d.buffers.GpuBuffer
import com.mojang.blaze3d.buffers.GpuBufferSlice
import com.mojang.blaze3d.buffers.GpuFence
import com.mojang.blaze3d.platform.NativeImage
import com.mojang.blaze3d.systems.CommandEncoderBackend
import com.mojang.blaze3d.systems.GpuQuery
import com.mojang.blaze3d.systems.RenderPassBackend
import com.mojang.blaze3d.textures.GpuTexture
import com.mojang.blaze3d.textures.GpuTextureView
import dev.birb.wgpu.rust.WgpuNative
import dev.birb.wgpu.rust.WmNative
import net.minecraft.client.Minecraft
import net.minecraft.core.SectionPos
import org.lwjgl.system.MemoryUtil
import java.lang.foreign.Arena
import java.lang.foreign.MemorySegment
import java.nio.ByteBuffer
import java.util.OptionalDouble
import java.util.OptionalInt
import java.util.OptionalLong
import java.util.concurrent.atomic.AtomicBoolean
import java.util.function.Supplier
import dev.birb.wgpu.render.TerrainPass

/** wgpu requires mapped and staged ranges to be 16-byte aligned. */
private const val MAPPING_ALIGNMENT = 16

/**
 * Rounds [length] up to [MAPPING_ALIGNMENT].
 *
 * A buffer's size is what Minecraft asked for, which is not necessarily a multiple of the alignment
 * a staging copy needs - see `mapBuffer`.
 */
private fun roundUpToAlignment(length: Long): Long =
    (length + MAPPING_ALIGNMENT - 1) / MAPPING_ALIGNMENT * MAPPING_ALIGNMENT

/** `TextureAtlas#uploadAnimationFrames` labels its pass `"Animate " + the atlas location`. */
private const val ANIMATE_PASS_PREFIX = "Animate "

/**
 * Sprite animation passes since the renderer started, and the count the last report was made at.
 *
 * Shared by every encoder, because Minecraft makes a fresh one per pass and a per-instance counter
 * would report each pass as the first one.
 */
private val ANIMATION_PASSES = java.util.concurrent.atomic.AtomicLong()
private val ANIMATION_REPORTED_COUNT = java.util.concurrent.atomic.AtomicLong()

@Volatile
private var ANIMATION_REPORTED_AT = 0L

/**
 * Blaze3D's command encoder, forwarding to a `wgpu::CommandEncoder`.
 *
 * The three Blaze3D overloads of `createRenderPass` collapse onto one private helper, and the
 * "is a pass open" bookkeeping lives here because `wgpu::CommandEncoder` only guarantees that
 * passes are not interleaved.
 *
 * [presentTexture] is the important one for 26.1: `RenderTarget#blitToScreen()` calls it every
 * frame with the main target's colour view, which is exactly the texture the swapchain needs, so
 * the surface blit happens here rather than in `presentFrame`.
 */
class WgpuCommandEncoder(@get:JvmName("device") val device: WgpuDevice) : CommandEncoderBackend {

    /** Raw `wgpu::CommandEncoder` pointer, also read by the Java render pass. */
    @get:JvmName("nativeEncoder")
    val nativeEncoder: MemorySegment =
        WmNative.createCommandEncoder.invokeExact(device.renderer) as MemorySegment

    init {
        // Diagnostics: Minecraft's `CommandEncoder` is not `AutoCloseable` and has no `close`, so
        // the object is dropped to the GC - which costs nothing in the GL backend, whose encoder
        // owns nothing, and leaks a native encoder here. This says who makes them.
        if (Diagnostics.loggingEnabled()) {
            val site = Throwable().stackTrace
                .drop(1)
                .takeWhile { !it.className.startsWith("dev.birb.wgpu") || it.methodName == "createCommandEncoder" }
                .takeLast(4)
                .joinToString(" <- ") { "${it.className.substringAfterLast('.')}.${it.methodName}:${it.lineNumber}" }

            if (CREATION_SITES.add(site)) {
                dev.birb.wgpu.WgpuMcMod.LOGGER.info("wgpu: a command encoder was created by {}", site)
            }
        }
    }

    private val closed = AtomicBoolean(false)
    private var inPass = false

    /**
     * Nothing is released when this object is collected.
     *
     * Blaze3D's `CommandEncoder` is not `AutoCloseable` and has no `close`, which is true for the
     * OpenGL backend, whose encoder owns no resource - and used to be false here, where every
     * object owned a `wgpu::CommandEncoder`. Minecraft makes three of those a frame, and at the few
     * hundred frames a second this backend reaches without vsync that was a thousand command
     * buffers a second left to a garbage collector that cannot see native memory: the process went
     * from one gigabyte to twenty in a minute. The pointer below is a token now, and every handle
     * records into the one encoder the native side owns.
     */

    /**
     * Every `(label, format)` an upload through the `ByteBuffer` overload has been seen with.
     *
     * Diagnostics: that overload is handed the source pixels in a `NativeImage.Format` the native
     * side does not get told about, so which format Minecraft actually uploads with decides whether
     * the texture arrives with the channels it is sampled with.
     */
    private val uploadsFromByteBuffers = java.util.concurrent.ConcurrentHashMap.newKeySet<String>()

    override fun createRenderPass(
        label: Supplier<String>,
        colorTexture: GpuTextureView,
        clearColor: OptionalInt,
    ): RenderPassBackend = openPass(label, colorTexture, clearColor, null, OptionalDouble.empty())

    override fun createRenderPass(
        label: Supplier<String>,
        colorTexture: GpuTextureView,
        clearColor: OptionalInt,
        depthTexture: GpuTextureView?,
        clearDepth: OptionalDouble,
    ): RenderPassBackend = openPass(label, colorTexture, clearColor, depthTexture, clearDepth)

    private fun openPass(
        label: Supplier<String>,
        colorTexture: GpuTextureView,
        clearColor: OptionalInt,
        depthTexture: GpuTextureView?,
        clearDepth: OptionalDouble,
    ): RenderPassBackend {
        check(!closed.get()) { "Command encoder is closed" }
        inPass = true

        val name = label.get()
        if (name.startsWith(ANIMATE_PASS_PREFIX)) {
            reportAnimationPass()
        }

        return WgpuRenderPass(device, this, label, colorTexture, clearColor, depthTexture, clearDepth)
    }

    /**
     * Diagnostics: that the atlas animation is running, once a second.
     *
     * `TextureAtlas#cycleAnimationFrames` re-renders every animated sprite into the atlas through an
     * `Animate <atlas>` pass whenever a frame of it is due, so these passes going by at a steady rate
     * *is* the animation: the passes are the only thing that makes an animated texture change. This
     * side used to cancel that call outright (a mixin, from before the atlas render path worked), and
     * with the cancel in place the count drops to zero once the resources are loaded - which looks
     * exactly like an atlas whose sprites are simply not animated.
     */
    private fun reportAnimationPass() {
        // The logging switch, and checked before the counter rather than after it: this is the one
        // report that had no switch at all, so it printed a line a second in every run.
        if (!Diagnostics.loggingEnabled()) {
            return
        }

        val now = System.nanoTime()
        val count = ANIMATION_PASSES.incrementAndGet()
        if (now - ANIMATION_REPORTED_AT < 1_000_000_000L) {
            return
        }

        val since = count - ANIMATION_REPORTED_COUNT.getAndSet(count)
        ANIMATION_REPORTED_AT = now
        dev.birb.wgpu.WgpuMcMod.LOGGER.info("wgpu: {} sprite animation passes in the last second", since)
    }

    /** Invoked by [WgpuRenderPass.close] once the pass has been recorded natively. */
    fun onRenderPassClosed() {
        inPass = false
    }

    override fun isInRenderPass(): Boolean = inPass

    /**
     * Submits everything recorded so far.
     *
     * This is called from exactly two kinds of place: before a **readback** - a copy whose result
     * somebody is about to look at - and, through the native blit, before a **present**. Everything
     * else is recorded and left alone, because a submission is not free and there is no reason to
     * have more than one a frame:
     *
     *  - wgpu keeps recording order inside one encoder, so a clear, a pass, an upload and the next
     *    pass are executed in the order they were recorded whichever submission carries them, and
     *    the barriers between them are inserted by wgpu either way;
     *  - an upload through `Queue::write_buffer` or `write_texture` is applied at the *next*
     *    submission, ahead of the commands in it. That is what makes batching safe for Minecraft's
     *    uploads: it writes every uniform, vertex block and face mesh into a *fresh* slice of a ring
     *    buffer and never re-writes a region an already recorded draw reads, so "all of this frame's
     *    uploads, then all of this frame's commands" is the order it is written for;
     *  - a *readback* is the exception in both directions: `Queue::write_buffer` data is not
     *    visible to a command that was recorded after it unless the two are submitted together, and
     *    a copy whose result is read on the CPU has to have been submitted at all.
     *
     * A render pass borrows the encoder for as long as it is open, so this must not be called from
     * inside one - the native side refuses (and says so) rather than finishing an encoder wgpu is
     * still holding.
     */
    fun submitForReadback() {
        WmNative.flushEncoder.invokeExact(device.renderer, nativeEncoder) as Unit
    }

    // wgpu has no standalone clear, so each of these records a render pass whose only job is its
    // load op. They are not optional: `GameRenderer` clears the main colour and depth textures this
    // way before drawing anything, every frame, and with the clear dropped the frame kept whatever
    // the textures already held - a black window once the depth test became a real
    // `LESS_THAN_OR_EQUAL`, because an uncleared depth buffer rejects every fragment.
    //
    // They are *recorded*, not submitted: the pass is one more command in the frame's encoder, and
    // the submission at present carries it - see `submitForReadback`.
    //
    // `clear_texture` would not do: it clears to zero, and Minecraft clears depth to 1.0.

    override fun clearColorTexture(colorTexture: GpuTexture, clearColor: Int) {
        WmNative.clearColorTexture.invokeExact(
            nativeEncoder,
            (colorTexture as WgpuTexture).nativeTexture,
            clearColor,
        ) as Unit
    }

    override fun clearColorAndDepthTextures(
        colorTexture: GpuTexture,
        clearColor: Int,
        depthTexture: GpuTexture,
        clearDepth: Double,
    ) {
        WmNative.clearColorAndDepthTextures.invokeExact(
            nativeEncoder,
            (colorTexture as WgpuTexture).nativeTexture,
            clearColor,
            (depthTexture as WgpuTexture).nativeTexture,
            clearDepth,
        ) as Unit
    }

    override fun clearColorAndDepthTextures(
        colorTexture: GpuTexture,
        clearColor: Int,
        depthTexture: GpuTexture,
        clearDepth: Double,
        regionX: Int,
        regionY: Int,
        regionWidth: Int,
        regionHeight: Int,
    ) {
        WmNative.clearColorAndDepthTexturesRegion.invokeExact(
            nativeEncoder,
            (colorTexture as WgpuTexture).nativeTexture,
            clearColor,
            (depthTexture as WgpuTexture).nativeTexture,
            clearDepth,
            regionX,
            regionY,
            regionWidth,
            regionHeight,
        ) as Unit
    }

    override fun clearDepthTexture(depthTexture: GpuTexture, clearDepth: Double) {
        WmNative.clearDepthTexture.invokeExact(
            nativeEncoder,
            (depthTexture as WgpuTexture).nativeTexture,
            clearDepth,
        ) as Unit
    }

    /**
     * Stencil is not modelled: the depth attachment is a plain `Depth32Float` with no stencil
     * aspect, and 26.1 only clears stencil for pipelines this renderer never selects.
     */
    override fun clearStencilTexture(texture: GpuTexture, value: Int) = Unit

    override fun writeToBuffer(destination: GpuBufferSlice, data: ByteBuffer) {
        WmNative.writeToBuffer.invokeExact(
            device.renderer,
            (destination.buffer() as WgpuBuffer).nativeBuffer,
            destination.offset(),
            data.remaining().toLong(),
            MemorySegment.ofBuffer(data),
        ) as Unit
    }

    /**
     * Hands Blaze3D an aligned CPU staging buffer for [buffer].
     *
     * wgpu exposes no CPU-visible mapping, so the two directions are pushed across the ABI: a write
     * goes through `write_to_buffer` when the view is closed - which is exactly when Blaze3D
     * considers the data final - and a read is filled by `read_buffer` before the view is handed
     * back, because Blaze3D reads it immediately. Leaving the read half out is what made every
     * screenshot black: the staging buffer was allocated and never filled.
     */
    override fun mapBuffer(buffer: GpuBufferSlice, read: Boolean, write: Boolean): GpuBuffer.MappedView {
        val length = buffer.length()

        // The staging buffer is rounded up, and the upload is the rounded size, because a buffer's
        // size is whatever Minecraft asked for - which is not necessarily a multiple of wgpu's
        // `COPY_BUFFER_ALIGNMENT`, and `Queue::write_buffer` refuses a size that is not. The cloud
        // face buffer is 181,818 bytes, i.e. 181,816 in the middle and two bytes over the end of an
        // alignment, so writing exactly what Blaze3D filled is a validation error - and a validation
        // error ends the process here. The tail is this side's own memory and is zeroed, so the
        // padding is zeroes rather than whatever the allocator handed back.
        val upload = roundUpToAlignment(length)
        val staging = MemoryUtil.memAlignedAlloc(MAPPING_ALIGNMENT, upload.toInt())
        MemoryUtil.memSet(staging, 0)
        val wgpuBuffer = buffer.buffer() as WgpuBuffer
        val nativeBuffer = wgpuBuffer.nativeBuffer
        val label = wgpuBuffer.label
        val renderer = device.renderer

        // Diagnostics: a staging buffer that is allocated and never freed is native memory the
        // garbage collector cannot see, which is exactly the shape of "the game asks for memory and
        // never gives it back". The totals say whether every allocation comes back, and they are
        // counted only while the upload reports are on - the counters exist to be reported.
        if (Diagnostics.uploadsEnabled()) {
            STAGING_ALLOCATED.addAndGet(upload)
            STAGING_OPEN.incrementAndGet()
            if (STAGING_OPEN.get() > STAGING_HIGH_WATER.get()) {
                STAGING_HIGH_WATER.set(STAGING_OPEN.get())
            }
            reportStaging("map")
        }

        if (read) {
            val filled = WmNative.readBuffer.invokeExact(
                renderer,
                nativeBuffer,
                buffer.offset(),
                length,
                MemorySegment.ofAddress(MemoryUtil.memAddress0(staging)),
            ) as Boolean
            if (!filled) {
                dev.birb.wgpu.WgpuMcMod.LOGGER.error(
                    "wgpu: could not read {} bytes at {} back from a buffer; the mapping stays empty",
                    length, buffer.offset(),
                )
            }
        }

        return object : GpuBuffer.MappedView {
            override fun data(): ByteBuffer = staging

            override fun close() {
                try {
                    if (write) {
                        if (Diagnostics.uploadsEnabled()) {
                            reportMappedWrite(wgpuBuffer, staging, length)
                        }
                        WmNative.writeToBuffer.invokeExact(
                            renderer,
                            nativeBuffer,
                            buffer.offset(),
                            upload,
                            MemorySegment.ofAddress(MemoryUtil.memAddress0(staging)),
                        ) as Unit

                        // How much of the staging buffer Blaze3D actually filled. It stays at 0 when
                        // the writer used absolute puts, which is why it is only ever used to
                        // *shrink* what the checks below look at - never to decide that nothing was
                        // written.
                        val written = staging.position().toLong()
                        wgpuBuffer.lastMappedWrite = written

                        if (Diagnostics.uploadsEnabled()) {
                            verifyMappedWrite(wgpuBuffer, buffer.offset(), staging, length, written)
                        }
                    }
                } finally {
                    MemoryUtil.memAlignedFree(staging)
                    if (Diagnostics.uploadsEnabled()) {
                        STAGING_FREED.addAndGet(upload)
                        STAGING_OPEN.decrementAndGet()
                        reportStaging("free")
                    }
                }
            }
        }
    }

    /**
     * Diagnostics: what a `mapBuffer` write is about to put into a buffer whose label looks like a
     * constant buffer, once per second.
     *
     * A mapped write is the one upload path this backend cannot read back afterwards - the buffer
     * is `MAP_WRITE`, and wgpu has no mapping to hand out - so the only place the bytes can be
     * looked at is here, between Blaze3D's last write and the upload. The cloud faces arrive this
     * way, and "the clouds are drawn with the sun's worth of faces but all of them in the player's
     * own cell" is a question about these bytes.
     */
    private fun reportMappedWrite(buffer: WgpuBuffer, staging: ByteBuffer, length: Long) {
        // The upload-report switch rather than the logging one: this is a line per upload, and it is
        // the pair of this and `verifyMappedWrite` that answers whether an upload landed.
        if (!Diagnostics.uploadsEnabled()) {
            return
        }
        if (!buffer.label.startsWith("Cloud")) {
            return
        }

        if (!throttle(buffer.label)) {
            return
        }

        val segment = MemorySegment.ofAddress(MemoryUtil.memAddress0(staging)).reinterpret(length)
        val ints = StringBuilder()
        var offset = 0L
        while (offset + 4 <= length && offset < 96) {
            ints.append(segment.get(java.lang.foreign.ValueLayout.JAVA_INT, offset)).append(' ')
            offset += 4
        }

        dev.birb.wgpu.WgpuMcMod.LOGGER.info(
            "wgpu: mapped write to {} ({} bytes, {} of them written) via {}, blaze usage {}, wgpu usage {}; first {} ints: {}",
            buffer.label, length, staging.position(), buffer.origin,
            describeBlazeUsage(buffer.usage()), describeWgpuUsage(buffer),
            offset / 4, ints.toString().trim(),
        )
    }

    /**
     * Diagnostics: whether the bytes a mapped write handed to the GPU are actually in the buffer.
     *
     * This is a self-check rather than a curiosity. A mapped write is `write_to_buffer`, which wgpu
     * only allows on a buffer created with `COPY_DST` - and Minecraft creates the buffers it maps
     * itself *without* it (`GpuBuffer.USAGE_MAP_WRITE` alone), so the upload was rejected and the
     * face buffer stayed as it was created. Nothing said so: the vertices were drawn, from zeroed
     * data, and the whole cloud layer was one square above the player. Reading the bytes back and
     * comparing them with what was sent turns that into a log line.
     *
     * The whole written range is compared, not a prefix of it: a mesh that arrives with its second
     * half missing is still "the first bytes arrived", and the second half is what draws the rest of
     * the layer. The non-zero counts on both sides are what tell a short mesh apart from a mesh of
     * zeroes, and the usage flags are what tell a buffer that cannot receive the write at all from
     * one that received it wrong.
     */
    private fun verifyMappedWrite(
        buffer: WgpuBuffer,
        offset: Long,
        staging: ByteBuffer,
        length: Long,
        written: Long,
    ) {
        // The upload-report switch, as above: this is the verification half of a mapped write, and it
        // is the half that costs something - the bytes are read back from the GPU to be compared.
        if (!Diagnostics.uploadsEnabled()) {
            return
        }
        if (!buffer.label.startsWith("Cloud") || !throttle("${buffer.label} (read back)")) {
            return
        }

        // The range Blaze3D filled, when it says how much it filled; the whole slice otherwise.
        // Bounded so that a buffer that is mapped whole and filled with a mesh stays a readback and
        // not a hitch.
        val probe = (if (written in 1..length) written else length).coerceAtMost(MAX_VERIFY_BYTES).toInt()
        if (probe <= 0) {
            return
        }

        val readBack = ByteBuffer.allocateDirect(probe)
        val read = WmNative.readBuffer.invokeExact(
            device.renderer,
            buffer.nativeBuffer,
            offset,
            probe.toLong(),
            MemorySegment.ofBuffer(readBack),
        ) as Boolean

        if (!read) {
            dev.birb.wgpu.WgpuMcMod.LOGGER.warn("wgpu: could not read {} back after a mapped write", buffer.label)
            return
        }

        val sent = MemorySegment.ofAddress(MemoryUtil.memAddress0(staging)).reinterpret(probe.toLong())
        val got = MemorySegment.ofBuffer(readBack)
        val mismatch = sent.asSlice(0, probe.toLong()).mismatch(got.asSlice(0, probe.toLong()))

        if (mismatch < 0) {
            dev.birb.wgpu.WgpuMcMod.LOGGER.info(
                "wgpu: all {} written bytes arrived in {} ({} of them non-zero); created via {}, blaze usage {}, wgpu usage {}",
                probe, buffer.label, countNonZero(sent, probe), buffer.origin,
                describeBlazeUsage(buffer.usage()), describeWgpuUsage(buffer),
            )
        } else {
            dev.birb.wgpu.WgpuMcMod.LOGGER.error(
                "wgpu: the bytes written to {} did not arrive: first difference at byte {} of {}; sent [{}] ({} non-zero), read back [{}] ({} non-zero); created via {}, blaze usage {}, wgpu usage {}",
                buffer.label, mismatch, probe,
                describe(sent, probe), countNonZero(sent, probe),
                describe(got, probe), countNonZero(got, probe),
                buffer.origin, describeBlazeUsage(buffer.usage()), describeWgpuUsage(buffer),
            )
        }

        // A texel buffer holds faces rather than floats, so the bytes can be read as what the shader
        // will make of them. This is the line that says whether the mesh the draws are about to read
        // is a cloud layer or ten thousand copies of the same cell - the two look identical in a
        // screenshot, and "one square of cloud above the player's head" is what the second one
        // looks like.
        if (buffer.usage() and GpuBuffer.USAGE_UNIFORM_TEXEL_BUFFER != 0) {
            reportCloudMesh(buffer, got, if (written in 1..length) written.toInt() else probe)
        }
    }

    /**
     * Diagnostics: the faces a texel buffer holds, decoded the way `rendertype_clouds.vsh` decodes
     * them.
     *
     * `CloudRenderer#encodeFace` writes three bytes per face - the cell's x and z halved, then the
     * direction with the halved bit of each coordinate and two flags packed above it - and the
     * shader rebuilds a cell coordinate by shifting the byte up and or-ing the packed bit back in.
     * Reading the buffer back through that same decode is what tells a mesh of real cells apart from
     * one whose faces all decode to cell (0, 0), which is a layer drawn as a single square above the
     * player.
     */
    private fun reportCloudMesh(buffer: WgpuBuffer, bytes: MemorySegment, length: Int) {
        // The logging switch, as above: this decodes the cloud face buffer for the log.
        if (!Diagnostics.loggingEnabled()) {
            return
        }
        val faces = length / 3
        if (faces <= 0) {
            return
        }

        var minX = Int.MAX_VALUE
        var maxX = Int.MIN_VALUE
        var minZ = Int.MAX_VALUE
        var maxZ = Int.MIN_VALUE
        var outOfRange = 0
        var inside = 0
        val directions = IntArray(6)
        val first = StringBuilder()

        for (face in 0 until faces) {
            val cellXByte = bytes.get(java.lang.foreign.ValueLayout.JAVA_BYTE, (face * 3).toLong()).toInt()
            val cellZByte = bytes.get(java.lang.foreign.ValueLayout.JAVA_BYTE, (face * 3 + 1).toLong()).toInt()
            val flags = bytes.get(java.lang.foreign.ValueLayout.JAVA_BYTE, (face * 3 + 2).toLong()).toInt()

            val cellX = (cellXByte shl 1) or ((flags and 0x80) shr 7)
            val cellZ = (cellZByte shl 1) or ((flags and 0x40) shr 6)
            val direction = flags and 7

            minX = minOf(minX, cellX)
            maxX = maxOf(maxX, cellX)
            minZ = minOf(minZ, cellZ)
            maxZ = maxOf(maxZ, cellZ)
            if (direction in 0..5) {
                directions[direction]++
            }
            if (kotlin.math.abs(cellX) > MAX_CLOUD_CELL || kotlin.math.abs(cellZ) > MAX_CLOUD_CELL) {
                outOfRange++
            }
            if (flags and 16 != 0) {
                inside++
            }

            if (face < 4) {
                first.append('[').append(cellX).append(',').append(cellZ).append(' ')
                    .append(CLOUD_DIRECTIONS.getOrElse(direction) { "dir$direction" })
                    .append(if (flags and 16 != 0) " inside" else "")
                    .append(if (flags and 32 != 0) " top" else "")
                    .append("] ")
            }
        }

        dev.birb.wgpu.WgpuMcMod.LOGGER.info(
            "wgpu: {} holds {} faces: x {}..{}, z {}..{}, {} beyond {} cells, {} marked inside; directions {}; first {}",
            buffer.label, faces, minX, maxX, minZ, maxZ, outOfRange, MAX_CLOUD_CELL, inside,
            CLOUD_DIRECTIONS.mapIndexed { index, name -> "$name=${directions[index]}" }.joinToString(" "),
            first.toString().trim(),
        )
    }

    /** The first few bytes of a segment, for a log line. */
    private fun describe(segment: MemorySegment, length: Int): String {
        val text = StringBuilder()
        var offset = 0L
        while (offset + 4 <= length) {
            text.append(segment.get(java.lang.foreign.ValueLayout.JAVA_INT, offset)).append(' ')
            offset += 4
        }
        return text.toString().trim()
    }

    /** How many bytes of [segment] are not zero, which is what a mesh of zeroes fails. */
    private fun countNonZero(segment: MemorySegment, length: Int): Int {
        var count = 0
        for (i in 0 until length) {
            if (segment.get(java.lang.foreign.ValueLayout.JAVA_BYTE, i.toLong()) != 0.toByte()) {
                count++
            }
        }
        return count
    }

    /**
     * Diagnostics: the wgpu usage flags a buffer carries, named.
     *
     * The JVM side asks the native side because the mask is not the one it passed: a mapped buffer
     * gains `COPY_DST` and a texel buffer becomes `STORAGE`. A bind group that wants `STORAGE` on a
     * buffer without it is a wgpu validation error, and a validation error ends the process - so
     * this is the line that says which of the three creation paths built what.
     */
    private fun describeWgpuUsage(buffer: WgpuBuffer): String {
        val bits = try {
            buffer.wgpuUsages()
        } catch (error: Throwable) {
            return "unavailable ($error)"
        }

        val names = ArrayList<String>()
        for ((bit, name) in WGPU_USAGE_NAMES) {
            if (bits and bit != 0L) {
                names.add(name)
            }
        }

        return if (names.isEmpty()) "0x${bits.toString(16)}" else names.joinToString("|")
    }

    /** Diagnostics: Blaze3D's own usage mask, named. */
    private fun describeBlazeUsage(usage: Int): String {
        val names = ArrayList<String>()
        for ((bit, name) in BLAZE_USAGE_NAMES) {
            if (usage and bit != 0) {
                names.add(name)
            }
        }
        return if (names.isEmpty()) usage.toString() else names.joinToString("|")
    }

    /** Whether a diagnostic line about [label] is due, so the log stays readable. */
    private fun throttle(label: String): Boolean {
        val now = System.nanoTime()
        val last = MAPPED_WRITES[label] ?: 0L
        if (now - last < 1_000_000_000L) {
            return false
        }
        MAPPED_WRITES[label] = now
        return true
    }

    private companion object {
        /** The most bytes a diagnostic readback will compare, so a big buffer stays a log line. */
        const val MAX_VERIFY_BYTES = 1L shl 20

        /**
         * What the renderer has already been told about the camera.
         *
         * Process-wide rather than per encoder, because an encoder is made per frame: state on the
         * instance would mean the same two integers crossing the ABI sixty times a second, and the
         * native side's "the camera moved" checks would never fire.
         */
        @Volatile
        private var sentCameraSectionX = Int.MIN_VALUE

        @Volatile
        private var sentCameraSectionY = Int.MIN_VALUE

        @Volatile
        private var sentCameraSectionZ = Int.MIN_VALUE

        /** The render distance last sent, in chunks. */
        @Volatile
        private var sentRenderDistance = -1

        /**
         * The furthest cell a cloud face can name.
         *
         * `CloudRenderer` builds its mesh out to the cloud range, 1024 blocks or 86 cells by default,
         * so a face naming a cell beyond that is a face the shader will place outside the layer - the
         * shape a decode that drops the sign takes.
         */
        const val MAX_CLOUD_CELL = 120

        /** `Direction#get3DDataValue` order, which is the order the shader's face arrays use. */
        val CLOUD_DIRECTIONS = listOf("down", "up", "north", "south", "west", "east")

        /** When each mapped write was last reported, so the log stays readable. */
        val MAPPED_WRITES = java.util.concurrent.ConcurrentHashMap<String, Long>()

        /** wgpu's `BufferUsages` bits, see `wgpu_buffer_usages` on the native side. */
        val WGPU_USAGE_NAMES = listOf(
            1L to "MAP_READ",
            2L to "MAP_WRITE",
            4L to "COPY_SRC",
            8L to "COPY_DST",
            16L to "INDEX",
            32L to "VERTEX",
            64L to "UNIFORM",
            128L to "STORAGE",
            256L to "INDIRECT",
            512L to "QUERY_RESOLVE",
        )

        /** Blaze3D's `GpuBuffer` usage bits. */
        val BLAZE_USAGE_NAMES = listOf(
            GpuBuffer.USAGE_MAP_READ to "MAP_READ",
            GpuBuffer.USAGE_MAP_WRITE to "MAP_WRITE",
            GpuBuffer.USAGE_HINT_CLIENT_STORAGE to "HINT_CLIENT_STORAGE",
            GpuBuffer.USAGE_COPY_DST to "COPY_DST",
            GpuBuffer.USAGE_COPY_SRC to "COPY_SRC",
            GpuBuffer.USAGE_VERTEX to "VERTEX",
            GpuBuffer.USAGE_INDEX to "INDEX",
            GpuBuffer.USAGE_UNIFORM to "UNIFORM",
            GpuBuffer.USAGE_UNIFORM_TEXEL_BUFFER to "UNIFORM_TEXEL_BUFFER",
        )

        /** Mapped-write staging: how much was allocated, freed, and is open right now. */
        private val STAGING_ALLOCATED = java.util.concurrent.atomic.AtomicLong()
        private val STAGING_FREED = java.util.concurrent.atomic.AtomicLong()
        private val STAGING_OPEN = java.util.concurrent.atomic.AtomicInteger()
        private val STAGING_HIGH_WATER = java.util.concurrent.atomic.AtomicInteger()
        private var STAGING_REPORTED_AT = 0L

        /**
         * Reports the staging totals at most once a second, when they changed.
         *
         * Gated on the upload reports rather than on the logging switch: this is a line a second for
         * as long as uploads happen, and the number it prints is about the upload path rather than
         * about the frame.
         */
        fun reportStaging(what: String) {
            if (!Diagnostics.uploadsEnabled()) {
                return
            }

            val now = System.nanoTime()
            if (now - STAGING_REPORTED_AT < 1_000_000_000L) {
                return
            }
            STAGING_REPORTED_AT = now
            dev.birb.wgpu.WgpuMcMod.LOGGER.info(
                "wgpu: mapped-write staging after {}: {} MB allocated, {} MB freed, {} open (high water {})",
                what,
                STAGING_ALLOCATED.get() / (1024 * 1024),
                STAGING_FREED.get() / (1024 * 1024),
                STAGING_OPEN.get(),
                STAGING_HIGH_WATER.get(),
            )
        }

        /** Where command encoders are created, so each site is reported once. */
        val CREATION_SITES = java.util.concurrent.ConcurrentHashMap.newKeySet<String>()

        /**
         * Sizes of the textures [presentTexture] has been handed, so each one is described once.
         *
         * Shared by every encoder, because Minecraft makes a fresh one each frame and a set per
         * instance would describe the same size once per frame - which is what the log used to do.
         */
        val presented = java.util.concurrent.ConcurrentHashMap.newKeySet<String>()
    }

    override fun copyToBuffer(source: GpuBufferSlice, target: GpuBufferSlice) {
        WmNative.copyBufferToBuffer.invokeExact(
            device.renderer,
            nativeEncoder,
            (source.buffer() as WgpuBuffer).nativeBuffer,
            (target.buffer() as WgpuBuffer).nativeBuffer,
            source.offset(),
            target.offset(),
            source.length(),
        ) as Unit
    }

    /**
     * Dumps a texture right after it was uploaded, when its label matches
     * [Diagnostics.DUMP_TEXTURE_LABEL] and the dumps are on.
     *
     * Diagnostics. A texture that arrives in the GPU with the wrong channels looks exactly like a
     * shader that samples the right texture the wrong way, and the uploaded bytes are the only
     * thing that tells the two apart.
     */
    private fun dumpUploadIfRequested(texture: WgpuTexture, depthOrLayer: Int) {
        if (!Diagnostics.dumpsEnabled()) return
        if (!texture.label.contains(Diagnostics.DUMP_TEXTURE_LABEL, ignoreCase = true)) return

        val name = "tex-" + texture.label.replace(Regex("[^A-Za-z0-9]+"), "-").trim('-') +
            "-layer$depthOrLayer.raw"
        Diagnostics.dumpTexture(device.renderer, texture.nativeTexture, name)
    }

    override fun writeToTexture(
        destination: GpuTexture,
        source: NativeImage,
        mipLevel: Int,
        depthOrLayer: Int,
        destX: Int,
        destY: Int,
        width: Int,
        height: Int,
        sourceX: Int,
        sourceY: Int,
    ) {
        val upload = NativeImageUpload.region(source, sourceX, sourceY, width, height)
        upload.use {
            WmNative.writeToTexture.invokeExact(
                device.renderer,
                (destination as WgpuTexture).nativeTexture,
                it.segment,
                it.byteSize,
                mipLevel,
                depthOrLayer,
                destX,
                destY,
                width,
                height,
            ) as Unit
        }
        dumpUploadIfRequested(destination as WgpuTexture, depthOrLayer)
    }

    override fun writeToTexture(
        destination: GpuTexture,
        source: ByteBuffer,
        format: NativeImage.Format,
        mipLevel: Int,
        depthOrLayer: Int,
        destX: Int,
        destY: Int,
        width: Int,
        height: Int,
    ) {
        if (Diagnostics.loggingEnabled() && uploadsFromByteBuffers.add("${destination.label} as $format")) {
            dev.birb.wgpu.WgpuMcMod.LOGGER.info(
                "wgpu: byte-buffer upload to {} as {}", destination.label, format
            )
        }
        // The native side takes the bytes as tightly packed RGBA. Any other layout would be
        // uploaded with the channels shifted by a byte per texel, which is not something to let
        // through quietly.
        if (format != NativeImage.Format.RGBA) {
            dev.birb.wgpu.WgpuMcMod.LOGGER.error(
                "wgpu: {} was uploaded as {}, which this backend only stores as RGBA; " +
                    "the texture will be wrong",
                destination.label,
                format,
            )
        }
        WmNative.writeToTexture.invokeExact(            device.renderer,
            (destination as WgpuTexture).nativeTexture,
            MemorySegment.ofBuffer(source),
            source.remaining().toLong(),
            mipLevel,
            depthOrLayer,
            destX,
            destY,
            width,
            height,
        ) as Unit
        dumpUploadIfRequested(destination as WgpuTexture, depthOrLayer)
    }

    override fun copyTextureToBuffer(
        source: GpuTexture,
        destination: GpuBuffer,
        offset: Long,
        callback: Runnable,
        mipLevel: Int,
    ) = copyTextureToBuffer(
        source,
        destination,
        offset,
        callback,
        mipLevel,
        0,
        0,
        source.getWidth(mipLevel),
        source.getHeight(mipLevel),
    )

    override fun copyTextureToBuffer(
        source: GpuTexture,
        destination: GpuBuffer,
        offset: Long,
        callback: Runnable,
        mipLevel: Int,
        x: Int,
        y: Int,
        width: Int,
        height: Int,
    ) {
        WmNative.copyTextureToBuffer.invokeExact(
            device.renderer,
            nativeEncoder,
            (source as WgpuTexture).nativeTexture,
            (destination as WgpuBuffer).nativeBuffer,
            offset,
            mipLevel,
            x,
            y,
            width,
            height,
        ) as Unit

        // A readback: the callback is handed a buffer the copy has to have filled, so this is one of
        // the two places a submission is not optional. It is also what `Screenshot` goes through -
        // `UberGpuBuffer` copies the frame into a mappable buffer and reads it in the callback.
        submitForReadback()
        callback.run()
    }

    override fun copyTextureToTexture(
        source: GpuTexture,
        destination: GpuTexture,
        mipLevel: Int,
        destX: Int,
        destY: Int,
        sourceX: Int,
        sourceY: Int,
        width: Int,
        height: Int,
    ) {
        WmNative.copyTextureToTexture.invokeExact(
            nativeEncoder,
            (source as WgpuTexture).nativeTexture,
            (destination as WgpuTexture).nativeTexture,
            mipLevel,
            destX,
            destY,
            sourceX,
            sourceY,
            width,
            height,
        ) as Unit
    }

    override fun presentTexture(texture: GpuTextureView) {
        // No submission here: the native blit records itself into the same encoder the frame is in
        // and submits once, at the end, before the swapchain image is presented. That is the frame's
        // one submission - the clears, the passes, the writes and this blit all travel in it.
        val view = texture as WgpuTextureView
        val described = "${view.texture.getWidth(0)}x${view.texture.getHeight(0)}"
        if (Diagnostics.loggingEnabled() && presented.add(described)) {
            dev.birb.wgpu.WgpuMcMod.LOGGER.info("wgpu: presents a {} texture", described)
        }

        // Where the camera is, in sections, sent once a frame and before the frame is presented:
        // this is the point in the frame that is known to happen exactly once, which is also where
        // the native side drains the arena and trims it. Sent before the blit so the position
        // belongs to the frame it was read in.
        sendCameraSection()

        device.surface.blitAndPresent(view, view.texture.getWidth(0), view.texture.getHeight(0))
    }

    /**
     * Hands the camera's section position to the renderer.
     *
     * Read from the camera rather than from the player, because the two differ whenever the game
     * renders from somewhere else - a spectator's free camera, a demo camera, the camera a mod moved -
     * and it is the camera's position that decides which sections are worth keeping. The entity is
     * not always there (the title screen has no camera), in which case the player's own position is
     * the closest thing, and neither being there means the frame is not a world frame at all, so
     * nothing is sent: the arena keeps the last position rather than being trimmed against a guess.
     */
    private fun sendCameraSection() {
        val client = Minecraft.getInstance() ?: return
        val level = client.level ?: return

        // How far the arena reaches follows Minecraft's render distance, which is also what decides
        // how far the game is drawing. Sent on change, so once at world join and whenever the slider
        // moves; the native side keeps it and trims the arena against it.
        val renderDistance = client.options.renderDistance().get()
        if (renderDistance != sentRenderDistance) {
            sentRenderDistance = renderDistance
            WgpuNative.setRenderDistance(renderDistance)
        }

        // While the Rust terrain is drawn, the section belongs to the terrain pass and is sent from
        // there, immediately before the pass is recorded: sent from here - which is not ordered against
        // the pass - the pass would offset its draws by the previous frame's section whenever the two
        // straddle a boundary, and the terrain would jump by sixteen blocks. With that path off, this
        // frame's bookkeeping is the only thing that keeps the arena trimmed against the camera.
        if (TerrainPass.isOn()) {
            return
        }

        // The live camera, deliberately: this call is the arena's trim, not a transform, and the
        // transform's own copy of the section is the one `TerrainPass` sends.
        val camera = client.gameRenderer.mainCamera
        val position = camera?.position() ?: client.player?.position() ?: return

        val sectionX = SectionPos.blockToSectionCoord(position.x)
        val sectionY = SectionPos.blockToSectionCoord(position.y)
        val sectionZ = SectionPos.blockToSectionCoord(position.z)

        if (sectionX == sentCameraSectionX && sectionY == sentCameraSectionY && sectionZ == sentCameraSectionZ) {
            return
        }

        sentCameraSectionX = sectionX
        sentCameraSectionY = sectionY
        sentCameraSectionZ = sectionZ
        WgpuNative.setCameraSection(sectionX, sectionY, sectionZ)
    }

    override fun createFence(): GpuFence = ImmediateFence

    override fun timerQueryBegin(): GpuQuery = NoopQuery

    override fun timerQueryEnd(query: GpuQuery) = Unit

    private object ImmediateFence : GpuFence {
        override fun awaitCompletion(timeoutMs: Long): Boolean = true
        override fun close() = Unit
    }

    private object NoopQuery : GpuQuery {
        override fun getValue(): OptionalLong = OptionalLong.empty()
        override fun close() = Unit
    }
}


/**
 * Scratch CPU buffer used to stage a [NativeImage] region for `write_to_texture`.
 *
 * Kotlin's `use` on this class makes the arena lifetime explicit at the call site, so the native
 * pointer cannot outlive its allocation.
 */
internal class NativeImageUpload private constructor(
    private val arena: Arena,
    @JvmField val segment: MemorySegment,
    @JvmField val byteSize: Long,
) : AutoCloseable {

    override fun close() = arena.close()

    companion object {
        private const val BYTES_PER_PIXEL = 4

        fun region(image: NativeImage, x: Int, y: Int, width: Int, height: Int): NativeImageUpload {
            val arena = Arena.ofConfined()
            val rowBytes = width * BYTES_PER_PIXEL
            val bytes = rowBytes.toLong() * height
            val segment = arena.allocate(bytes)

            // `NativeImage#pixels` hands back **ARGB** - the image stores ABGR and `getPixels`
            // converts on the way out - while an `Rgba8Unorm` texel is the four bytes R, G, B, A in
            // that order. The two were one `ByteBuffer` write apart from each other, and both
            // obvious spellings are wrong: an ARGB int written little-endian stores (B, G, R, A),
            // and written big-endian it stores (A, R, G, B). The channels are therefore taken apart
            // here rather than left to an endianness, which is also what the OpenGL backend does -
            // it passes the image's own ABGR buffer straight to `glTexSubImage2D`, whose
            // little-endian bytes are R, G, B, A.
            //
            // Getting this wrong is not subtle and not local: a blue sky is drawn from an uploaded
            // panorama, so it came out orange, the blue globe icon came out red, and every block,
            // item, GUI and font texture in the game arrived the same way.
            val destination = segment.asByteBuffer()
            val pixels = image.pixels
            for (row in 0 until height) {
                val rowStart = (y + row) * image.width + x
                var offset = row * rowBytes
                for (column in 0 until width) {
                    val argb = pixels[rowStart + column]
                    destination.put(offset, (argb ushr 16).toByte())
                    destination.put(offset + 1, (argb ushr 8).toByte())
                    destination.put(offset + 2, argb.toByte())
                    destination.put(offset + 3, (argb ushr 24).toByte())
                    offset += BYTES_PER_PIXEL
                }
            }

            return NativeImageUpload(arena, segment, bytes)
        }
    }
}
