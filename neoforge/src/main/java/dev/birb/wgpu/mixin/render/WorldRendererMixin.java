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
}
