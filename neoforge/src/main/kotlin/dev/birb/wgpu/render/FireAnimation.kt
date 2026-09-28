package dev.birb.wgpu.render

import dev.birb.wgpu.rust.RendererSettings

/**
 * Whether the fire animation is frozen, which is the renderer's `animated_textures` switch as the
 * atlas sees it.
 *
 * The switch is one row that answers one question for the whole game: do the block textures
 * Minecraft animates move? On the terrain this renderer draws, that is decided when a face is baked
 * - a sprite the game animates is baked against the game's own atlas, which the game is animating,
 * or against this side's frozen copy of one frame of it. Those are the two halves of the setting,
 * and both are in the Rust side.
 *
 * Fire is the one animated block texture that is *not* on the terrain this renderer draws: the first
 * person overlay and a burning entity are Minecraft's own passes, and they sample the same two
 * sprites (`fire_0` and `fire_1`) out of the same block atlas. So the same switch has to reach them
 * somewhere else - and the only place that reaches both is the atlas itself, which is why turning
 * this off stops the animations advancing rather than drawing anything differently.
 * See `TextureAtlasMixin`, which is the whole of it.
 */
object FireAnimation {

    /** The setting, named the way the renderer's schema names it. */
    private const val SETTING = "animated_textures"

    /**
     * The index of `AnimatedTextures::Fancy` in the renderer's schema, which is the variant that
     * animates. The order is the schema's own (`Fast`, then `Fancy`), and the default is `Fancy` -
     * see `rust/wgpu-mc-jni/src/settings.rs`.
     */
    private const val FANCY = 1

    /**
     * Whether the fire animation should be held still.
     *
     * A renderer that has no such setting - an older build, or a document that failed to parse -
     * answers "no", because animating is what the game does.
     */
    @JvmStatic
    fun firesAreFrozen(): Boolean = RendererSettings.enumIndex(SETTING)?.let { it != FANCY } ?: false
}
