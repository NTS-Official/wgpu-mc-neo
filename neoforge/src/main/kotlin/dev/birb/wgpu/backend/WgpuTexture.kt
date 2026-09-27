package dev.birb.wgpu.backend

import com.mojang.blaze3d.textures.GpuTexture
import com.mojang.blaze3d.textures.GpuTextureView
import com.mojang.blaze3d.textures.TextureFormat
import dev.birb.wgpu.rust.NativeNames
import dev.birb.wgpu.rust.WmNative
import java.lang.foreign.MemorySegment
import java.util.concurrent.atomic.AtomicBoolean

/**
 * [GpuTexture] backed by a `wgpu::Texture`.
 *
 * The native handle is exposed to the backend package under [getNativeTexture] (the texture view,
 * the encoder and the render pass all hand it back into the C ABI) but never outside it.
 */
class WgpuTexture(
    @get:JvmName("device") val device: WgpuDevice,
    usage: Int,
    label: String,
    format: TextureFormat,
    width: Int,
    height: Int,
    depthOrLayers: Int,
    mipLevels: Int,
) : GpuTexture(usage, label, format, width, height, depthOrLayers, mipLevels) {

    /** Raw `wgpu::Texture` pointer. */
    @get:JvmName("nativeTexture")
    val nativeTexture: MemorySegment

    private val closed = AtomicBoolean(false)

    init {
        nativeTexture = WmNative.createTexture.invokeExact(
            device.renderer,
            WgpuFormat.nativeId(format),
            width,
            height,
            depthOrLayers,
            usage,
            mipLevels,
            NativeNames.utf8(label),
        ) as MemorySegment
    }

    override fun close() {
        if (closed.compareAndSet(false, true)) {
            WmNative.dropTexture.invokeExact(nativeTexture) as Unit
        }
    }

    override fun isClosed(): Boolean = closed.get()
}

/**
 * Hands a texture the **game** owns to the pass that draws the terrain.
 *
 * The two uses are the game's own block atlas and its lightmap, and each is a thing only the game can
 * answer:
 *
 *  - the **atlas**, because Minecraft animates it by rendering each due frame into it
 *    (`TextureAtlas#cycleAnimationFrames`), so a face baked against the copy this renderer packed at
 *    startup shows one frame forever - the fire that burns still. A face whose sprite the game animates
 *    is therefore baked with the game's own coordinates and marked for it, and the pass samples *this*
 *    texture for those faces; it animates for free, because the game was already doing it. See
 *    `UV_GAME_ATLAS` on the native side;
 *  - the **lightmap**, because that is where the game's lighting lives: the gamma and brightness
 *    options, the time of day, night vision, the darkness effect, all baked into a 16x16 texture that
 *    the game's own terrain shader does nothing but sample. See `GAME_LIGHTMAP`.
 *
 * The raw handle stays in this package: what crosses is the action, and the native side keeps a view
 * of the texture rather than the texture itself, so what it samples stays alive - including past the
 * game closing its own handle on the next resource reload.
 *
 * A texture that is not this backend's, or that the game has already closed, is left alone: the
 * native side would refuse it, and the pass keeps what it already had.
 */
fun GpuTexture.bindAtlasToTerrainPass() = withLiveHandle { renderer, texture ->
    WmNative.bindGameBlockAtlas.invokeExact(renderer, texture) as Unit
}

/** Hands the game's [lightmap][WmNative.bindGameLightmap] over. See [bindAtlasToTerrainPass]. */
fun GpuTexture.bindLightmapToTerrainPass() = withLiveHandle { renderer, texture ->
    WmNative.bindGameLightmap.invokeExact(renderer, texture) as Unit
}

/**
 * The same handover for a lightmap the game handed out as a **view**, which is what its own accessor
 * returns (`GameRenderer#levelLightmap`).
 *
 * A `GpuTextureView` cannot name the texture behind it from outside Blaze3D - the field is private -
 * so this has to live in the package that owns [WgpuTextureView]. The view is stable for the life of
 * the lightmap, which is what lets the caller ask every frame and hand over once.
 */
fun GpuTextureView.bindLightmapToTerrainPass() {
    (this as? WgpuTextureView)?.texture?.bindLightmapToTerrainPass()
}

/**
 * Runs [hand] with the renderer and the raw `wgpu::Texture` handle behind this texture.
 *
 * Nothing happens for a texture this backend did not create, or one the game has already closed - the
 * native side refuses a closed one rather than viewing it, because `create_view` on a dropped texture
 * is a wgpu validation error and a validation error on that path ends the process.
 */
private inline fun GpuTexture.withLiveHandle(hand: (MemorySegment, MemorySegment) -> Unit) {
    val texture = this as? WgpuTexture ?: return

    if (texture.isClosed) return

    hand(texture.device.renderer, texture.nativeTexture)
}

/**
 * [GpuTextureView] backed by a `wgpu::TextureView`.
 *
 * The view is what a render pass binds and what gets presented, so [getNativeView] is the handle
 * the encoder and the surface pass back to Rust.
 *
 * The mip range has to reach Rust: a view is a render target only when it covers exactly one mip
 * level, and the level it covers decides both which mip is written and how large the pass renders.
 * 26.1 renders each mip level of a sprite atlas through its own view.
 */
class WgpuTextureView(
    @get:JvmName("device") val device: WgpuDevice,
    @get:JvmName("texture") val texture: WgpuTexture,
    private val baseMipLevel: Int,
    private val viewMipLevels: Int,
) : GpuTextureView(texture, baseMipLevel, viewMipLevels) {

    /** Raw `wgpu::TextureView` pointer. */
    @get:JvmName("nativeView")
    val nativeView: MemorySegment =
        WmNative.createTextureView.invokeExact(
            device.renderer,
            texture.nativeTexture,
            texture.usage(),
            baseMipLevel,
            viewMipLevels,
        ) as MemorySegment

    private val closed = AtomicBoolean(false)

    override fun close() {
        if (closed.compareAndSet(false, true)) {
            WmNative.dropTextureView.invokeExact(nativeView) as Unit
        }
    }

    override fun isClosed(): Boolean = closed.get()
}