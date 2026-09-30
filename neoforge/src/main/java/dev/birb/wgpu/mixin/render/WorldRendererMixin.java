package dev.birb.wgpu.mixin.render;

import dev.birb.wgpu.chunk.RustChunkBake;
import dev.birb.wgpu.rust.WgpuNative;
import net.minecraft.client.multiplayer.ClientLevel;
import net.minecraft.client.renderer.LevelRenderer;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

/**
 * Forgets the section bake on both sides when the world changes.
 *
 * <p>{@code LevelRenderer#setLevel} is where a new world, a dimension change and the way back to the
 * title screen all arrive - the last one as {@code setLevel(null)} - so this is the one place that
 * covers every way the ground under the renderer can be replaced.
 *
 * <p>Without it the two sides keep describing the world that was left behind: the native side bakes
 * against a cache of sections that no longer exist, and this side believes sections it has already
 * sent are there - and nothing rebuilds a section whose blocks have not changed, so the new world
 * would draw the old world's ground wherever the two share coordinates. The native call is what
 * numbers the new world; the number it answers with is adopted here, so the two sides cannot drift
 * into a state where every payload is refused.
 */
@Mixin(LevelRenderer.class)
public abstract class WorldRendererMixin {

    @Inject(method = "setLevel", at = @At("HEAD"))
    private void wgpuMc$forgetTheSectionBake(ClientLevel level, CallbackInfo ci) {
        RustChunkBake.forgetAll(WgpuNative.clearSections());
    }

    /**
     * Waits for the block registry before a world is meshed, so Rust bakes every section on its first
     * build instead of Minecraft meshing the world and Rust replacing it a few seconds later.
     *
     * <p><b>The problem this closes.</b> {@code RustChunkBake#bake} refuses to take a section while the
     * native registry is empty - {@code BLOCKS_CACHED} - and answers that way so that Minecraft meshes the
     * section rather than nothing drawing it. The registry is built by {@code BlockCache} as soon as the
     * block atlas is stitched, but the atlas lands only a few seconds before a world is entered and the
     * registry itself takes about four: so the first seconds of every world are sections Minecraft meshed
     * because Rust could not yet take them. Those sections are then meshed a second time - the cache's own
     * {@code allChanged} at the end of the bake - and the player watches the ground swap over.
     *
     * <p><b>What is waited for is the registry, not a fixed time.</b> {@code BlockCache.awaitCached}
     * returns as soon as the bake that builds it has finished, and gives up after a bounded wait so a bake
     * that fails cannot hang the client. A world drawn on Minecraft's own meshes is the picture this
     * renderer drew before any of this existed, and it is the direction to fail in.
     *
     * <p><b>The wait is on the render thread, which is the thread that meshes.</b> {@code setLevel} runs
     * there and the sections are built from it, so blocking here is what makes the registry ready in time;
     * anywhere later would be a world already meshed. Nothing the cache needs is owned by this thread - it
     * reads the atlas and the block registry and does its work on its own - so this cannot deadlock
     * against it.
     *
     * <p>{@code setLevel(null)} is the way back to the title screen and has no world to mesh, so it is
     * skipped and pays nothing.
     */
    @Inject(method = "setLevel", at = @At("HEAD"))
    private void wgpuMc$waitForTheBlockRegistry(ClientLevel level, CallbackInfo ci) {
        if (level == null) {
            return;
        }

        dev.birb.wgpu.BlockCache.awaitCached();
    }
}
