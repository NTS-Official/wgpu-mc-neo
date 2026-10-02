package dev.birb.wgpu.backend

import com.mojang.blaze3d.buffers.GpuBuffer
import com.mojang.blaze3d.pipeline.CompiledRenderPipeline
import com.mojang.blaze3d.pipeline.RenderPipeline
import com.mojang.blaze3d.shaders.ShaderSource
import com.mojang.blaze3d.systems.CommandEncoderBackend
import com.mojang.blaze3d.systems.GpuDeviceBackend
import com.mojang.blaze3d.textures.AddressMode
import com.mojang.blaze3d.textures.FilterMode
import com.mojang.blaze3d.textures.GpuSampler
import com.mojang.blaze3d.textures.GpuTexture
import com.mojang.blaze3d.textures.GpuTextureView
import com.mojang.blaze3d.textures.TextureFormat
import dev.birb.wgpu.rust.WgpuNative
import dev.birb.wgpu.rust.WmNative
import java.lang.foreign.MemorySegment
import java.nio.ByteBuffer
import java.util.OptionalDouble
import java.util.function.Supplier

/**
 * Blaze3D's GPU device, implemented on top of the wgpu renderer in `rust/wgpu-mc-jni`.
 *
 * This is the Kotlin counterpart of the Fabric module's `WgpuDevice`, adapted to the 26.1
 * `GpuDeviceBackend` contract: 26.1 has no `GpuSurfaceBackend`, so presenting is driven by
 * [WgpuCommandEncoder.presentTexture] and [presentFrame] instead of a surface interface.
 *
 * The renderer pointer comes from the JNI entry point `WgpuNative.createWmRendererOnWindow()`,
 * which returns the `*mut WmRenderer` owned by Rust. Every other call goes through the C ABI in
 * [WmNative] using that same pointer, so both bridges address one renderer instance.
 */
class WgpuDevice(
    @get:JvmName("defaultShaderSource") val defaultShaderSource: ShaderSource,
    /** Pointer to the `WmRenderer` owned by Rust; every native call below is addressed with it. */
    @get:JvmName("renderer") val renderer: MemorySegment,
    /**
     * True when Rust already registered the window while creating the renderer, which is the case
     * whenever the renderer was obtained through `createWmRendererOnWindow`.
     */
    surfaceAttached: Boolean = false,
) : GpuDeviceBackend {

    /** Surface state, attached as soon as the backend creates the device. */
    val surface = WgpuSurface(this, surfaceAttached)

    /**
     * What the adapter says about itself, read once.
     *
     * The four accessors below are what vanilla fills the F3 overlay's system block with, and on
     * the OpenGL backend they are `GL_VENDOR`, `GL_RENDERER`, "OpenGL" and `GL_VERSION`: the
     * graphics driver, naming itself. Reporting "wgpu" for all four made that block useless, so the
     * adapter's own answers are used and the wgpu wording moved to a line of its own below them.
     */
    private val adapter: AdapterInfo = AdapterInfo.parse(WgpuNative.getAdapterInfoSafe())

    init {
        // The same four answers the F3 overlay's system block prints, so the log says what a
        // screenshot of it would say. The rust side already logs the adapter it picked; this adds
        // the driver, which is what a graphics problem usually has to be reported against.
        dev.birb.wgpu.WgpuMcMod.LOGGER.info(
            "wgpu-mc device: {} {} ({}), {}",
            adapter.api,
            adapter.driver,
            adapter.vendor,
            adapter.name,
        )
    }

    // Read once: these are device constants and each access is a native downcall.
    private val uniformOffsetAlignment: Int =
        WmNative.minUniformOffsetAlignment.invokeExact(renderer) as Int

    private val maximumTextureSize: Int =
        WmNative.maxTextureSize.invokeExact(renderer) as Int

    override fun createCommandEncoder(): CommandEncoderBackend = WgpuCommandEncoder(this)

    override fun createSampler(
        addressModeU: AddressMode,
        addressModeV: AddressMode,
        minFilter: FilterMode,
        magFilter: FilterMode,
        maxAnisotropy: Int,
        maxLod: OptionalDouble,
    ): GpuSampler = WgpuSampler(this, addressModeU, addressModeV, minFilter, magFilter, maxAnisotropy, maxLod)

    override fun createTexture(
        label: Supplier<String>?,
        usage: Int,
        format: TextureFormat,
        width: Int,
        height: Int,
        depthOrLayers: Int,
        mipLevels: Int,
    ): GpuTexture = register(WgpuTexture(this, usage, label?.get() ?: UNNAMED_TEXTURE, format, width, height, depthOrLayers, mipLevels))

    override fun createTexture(
        label: String?,
        usage: Int,
        format: TextureFormat,
        width: Int,
        height: Int,
        depthOrLayers: Int,
        mipLevels: Int,
    ): GpuTexture = register(WgpuTexture(this, usage, label ?: UNNAMED_TEXTURE, format, width, height, depthOrLayers, mipLevels))

    /**
     * Textures by label, so a diagnostics dump can name one long after it was created.
     *
     * A texture that is *composed* rather than uploaded - every sprite atlas, and the GUI item atlas -
     * has no single upload to hang a dump on: it is filled by hundreds of passes, one sprite at a
     * time, and the interesting question ("does the atlas hold the sprites in the slots the game
     * thinks they are in?") can only be asked once the composition is done. The label is the only
     * handle the caller has, and the newest texture under a label is the live one.
     */
    private val texturesByLabel = java.util.concurrent.ConcurrentHashMap<String, WgpuTexture>()

    private fun register(texture: WgpuTexture): WgpuTexture {
        texturesByLabel[texture.label] = texture
        return texture
    }

    /** Every live texture whose label contains [wanted]. */
    fun texturesMatching(wanted: String): List<WgpuTexture> =
        texturesByLabel.values.filter { it.label.contains(wanted, ignoreCase = true) && !it.isClosed }

    override fun createTextureView(texture: GpuTexture): GpuTextureView =
        createTextureView(texture, 0, texture.getMipLevels())

    override fun createTextureView(texture: GpuTexture, baseMipLevel: Int, mipLevels: Int): GpuTextureView {
        // A view of a closed texture is a dangling pointer: `WgpuTexture.close` drops the native
        // texture, and wgpu answers a later `create_view` with "Texture with '<label>' label is
        // invalid" - a validation error, which runs the panic hook and ends the process. It happens
        // when Minecraft resizes or rebuilds a render target: the old depth texture is closed while
        // something still asks for a view of it. Rendering one frame from a placeholder is a much
        // better outcome than losing the session, and the log says whose texture it was.
        if (texture.isClosed) {
            if (noteDeadTexture(texture.label)) {
                dev.birb.wgpu.WgpuMcMod.LOGGER.error(
                    "wgpu: a view of {} was requested after the texture was closed; " +
                        "rendering from a {}-format placeholder instead",
                    texture.label,
                    texture.getFormat(),
                )
            }

            return WgpuTextureView(
                this,
                placeholder(texture.getFormat(), texture.getWidth(0), texture.getHeight(0)),
                baseMipLevel,
                mipLevels,
            )
        }

        return WgpuTextureView(this, texture as WgpuTexture, baseMipLevel, mipLevels)
    }

    /**
     * Textures whose views were requested after they were closed, so each one is reported once.
     *
     * Cleared once it has seen more labels than a session can plausibly have live targets: this is a
     * log filter, and a filter that grows for the whole session is a leak with extra steps.
     */
    private val deadTextures = java.util.concurrent.ConcurrentHashMap.newKeySet<String>()

    private fun noteDeadTexture(label: String): Boolean {
        if (deadTextures.size > 512) {
            deadTextures.clear()
        }

        return deadTextures.add(label)
    }

    /**
     * A stand-in texture of [format], at least as large as the closed texture was.
     *
     * The size has to match: a pass draws into this one with the scissor rectangle the real target
     * would have had, and wgpu refuses a scissor that does not fit the render target - a 1x1
     * stand-in turned that into "Scissor Rect { x: 0, y: 0, w: 878, h: 504 } is not contained in
     * the render target (1, 1, 1)", which is fatal. The format has to match because the view is
     * bound to a pipeline, and a pipeline's colour target has to agree with the attachment's.
     *
     * **One per format, grown on demand**, rather than one per format *and* size. A window that is
     * resized leaves a closed render target behind at every size it has ever had, and a placeholder
     * each of those is a full-size texture - thirteen megabytes at 2560x1334 - that nothing ever
     * frees. Being larger than the closed texture is harmless: the scissor that arrives is the real
     * target's and fits inside it. The old one is closed when a bigger one is needed, which is rare
     * and happens when it is not in use.
     */
    private fun placeholder(format: TextureFormat, width: Int, height: Int): WgpuTexture {
        val standIn = placeholders.compute(format) { _, existing ->
            val wanted = width.coerceAtLeast(1)
            val wantedHeight = height.coerceAtLeast(1)

            if (existing != null && existing.getWidth(0) >= wanted && existing.getHeight(0) >= wantedHeight) {
                return@compute existing
            }

            existing?.close()

            WgpuTexture(
                this,
                USAGE_RENDER_ATTACHMENT or USAGE_TEXTURE_BINDING,
                "<wgpu-mc/closed texture>",
                format,
                maxOf(wanted, existing?.getWidth(0) ?: 0),
                maxOf(wantedHeight, existing?.getHeight(0) ?: 0),
                1,
                1,
            )
        }

        return checkNotNull(standIn) { "the $format placeholder vanished while it was being made" }
    }

    private val placeholders = java.util.concurrent.ConcurrentHashMap<TextureFormat, WgpuTexture>()

    override fun createBuffer(label: Supplier<String>?, usage: Int, size: Long): GpuBuffer =
        WgpuBuffer.allocate(this, label?.get() ?: UNNAMED_BUFFER, usage, size, mapped = false)

    override fun createBuffer(label: Supplier<String>?, usage: Int, data: ByteBuffer): GpuBuffer =
        WgpuBuffer.of(this, label?.get() ?: UNNAMED_BUFFER, usage, data)

    override fun getImplementationInformation(): String = IMPLEMENTATION
    override fun getLastDebugMessages(): List<String> = emptyList()
    override fun isDebuggingEnabled(): Boolean = false

    /** The card's maker, as `GL_VENDOR` would report it: `NVIDIA`, `AMD`, `Intel`. */
    override fun getVendor(): String = adapter.vendor

    /** The card itself, as `GL_RENDERER` would report it. */
    override fun getRenderer(): String = adapter.name

    /** The API the card is driven through, as `getBackendName` says "OpenGL" on the GL backend. */
    override fun getBackendName(): String = adapter.api

    /** The driver and its version, as `GL_VERSION` carries them. */
    override fun getVersion(): String = adapter.driver

    override fun getMaxTextureSize(): Int = maximumTextureSize
    override fun getUniformOffsetAlignment(): Int = uniformOffsetAlignment
    override fun getEnabledExtensions(): List<String> = emptyList()

    /** Rust exposes one default sampler configuration, so anisotropy is not selectable yet. */
    override fun getMaxSupportedAnisotropy(): Int = 1

    /**
     * Minecraft's own precompilation, which it does for every pipeline it ships with.
     *
     * It is asked for without knowing which kind of pass will draw the pipeline, so this compiles
     * the variant a pass *without* a depth attachment needs: it is the one whose depth state is
     * absent rather than a second struct, and the depth variant is written on demand the first time
     * a pass with a depth attachment binds it. The point of `precompilePipeline` here is that a
     * shader that does not compile is found at load time, and that happens either way - the shader
     * work is the same for both variants.
     */
    override fun precompilePipeline(
        pipeline: RenderPipeline,
        shaderSource: ShaderSource?,
    ): CompiledRenderPipeline = WgpuCompiledRenderPipeline.of(
        this,
        pipeline,
        shaderSource ?: defaultShaderSource,
        wantsDepth = false,
    )

    override fun clearPipelineCache() = WgpuCompiledRenderPipeline.clearCaches()

    override fun setVsync(enabled: Boolean) = WgpuSurface.setVsync(enabled)

    override fun presentFrame() {
        framesPresented++
        surface.present()
    }

    /** wgpu follows the D3D/Metal convention where depth maps to [0, 1]. */
    override fun isZZeroToOne(): Boolean = true

    override fun close() {
        surface.close()
    }

    companion object {
        const val UNNAMED_TEXTURE = "<wgpu-mc/unnamed texture>"
        const val UNNAMED_BUFFER = "<wgpu-mc/unnamed buffer>"
        const val IMPLEMENTATION = "wgpu 29"

        /**
         * Frames presented since startup.
         *
         * Read by the diagnostics that report a number "per second": dividing by the frames in the
         * same window gives the per-frame number without assuming a framerate.
         */
        @JvmStatic
        var framesPresented: Long = 0L
            private set

        /** `GpuTexture.USAGE_RENDER_ATTACHMENT` and `USAGE_TEXTURE_BINDING`, for the placeholders. */
        private const val USAGE_RENDER_ATTACHMENT = 8
        private const val USAGE_TEXTURE_BINDING = 4
    }
}

/**
 * The adapter as [WgpuNative.getAdapterInfo] reports it.
 *
 * Four lines in a fixed order, and the four names are the four things `GpuDeviceBackend` asks a
 * device for. A description that is not four lines means there was no renderer to ask - the mod's
 * device is created before its renderer during startup - and the implementation string stands in,
 * which is at least true.
 */
private data class AdapterInfo(val vendor: String, val name: String, val api: String, val driver: String) {
    companion object {
        private val UNKNOWN = AdapterInfo(
            WgpuDevice.IMPLEMENTATION,
            WgpuDevice.IMPLEMENTATION,
            WgpuDevice.IMPLEMENTATION,
            WgpuDevice.IMPLEMENTATION,
        )

        fun parse(description: String): AdapterInfo {
            val lines = description.split('\n')
            return if (lines.size < 4) UNKNOWN else AdapterInfo(lines[0], lines[1], lines[2], lines[3])
        }
    }
}