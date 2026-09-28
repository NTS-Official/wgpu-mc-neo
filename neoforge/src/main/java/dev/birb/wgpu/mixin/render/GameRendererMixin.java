package dev.birb.wgpu.mixin.render;

import dev.birb.wgpu.chunk.RustChunkBake;
import net.minecraft.client.DeltaTracker;
import net.minecraft.client.renderer.GameRenderer;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

/**
 * A per-frame place to stand, for work that is throttled per frame rather than per tick.
 *
 * <p>{@code GameRenderer#render} is the frame: it is reached once per rendered frame whatever the tick
 * rate is doing, and it is the method every other part of a frame is reached from. What is hung off it
 * is the refusal drain's rebuild asks - {@link RustChunkBake#redirtyDue()} - which is deliberately not
 * a tick: a tick is 50 ms, and fifty milliseconds of rebuild requests arriving in one lump is a spike
 * the player pays for in frame time. The two things that throttle it are per-frame too: the budget it
 * spends, and the bake pool it backs off from, which is drained by frames and not by ticks.
 *
 * <p>{@code HEAD} rather than {@code TAIL}, because the rebuild it asks for starts a task on Minecraft's
 * chunk-build threads: asking at the top of the frame gives those threads the frame to work in, and the
 * mesh that comes back is drawn by the next frame either way. Asking at the end would spend the same
 * work and start it a frame later.
 *
 * <p>A section marked dirty here is one the arena refused, so the section it belongs to is already
 * drawn by Minecraft's own mesh or by nothing - there is no in-flight frame that could observe the two
 * meshes disagree, because {@code setSectionDirty} records a flag that the next rebuild reads.
 */
@Mixin(GameRenderer.class)
public abstract class GameRendererMixin {

    @Inject(method = "render", at = @At("HEAD"))
    private void wgpuMc$askForRefusedRebuilds(DeltaTracker deltaTracker, boolean advanceGameTime, CallbackInfo ci) {
        RustChunkBake.redirtyDue();
    }
}
