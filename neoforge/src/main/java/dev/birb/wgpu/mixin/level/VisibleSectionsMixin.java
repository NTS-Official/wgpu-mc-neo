package dev.birb.wgpu.mixin.level;

import dev.birb.wgpu.render.VisibleSectionsRevision;
import net.minecraft.client.renderer.LevelRenderer;
import net.minecraft.client.renderer.culling.Frustum;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

/**
 * Counts the rebuilds of the list Minecraft's own occlusion culling fills - the game's own "the list is
 * new" signal, lifted out for the one thing that consumes the list.
 *
 * <p>{@code LevelRenderer#applyFrustum} is the only place {@code visibleSections} is filled, and it runs
 * on the game's own terms rather than per frame: a two-degree turn of the camera, or the occlusion graph
 * reporting that its answer moved ({@code SectionOcclusionGraph#consumeFrustumUpdate}). Both are dropped
 * on the floor by the game - the angle comparison is a local and the flag is *consumed* by the branch
 * that reads it - so a reader outside the game has no way to ask "is this list the one I already have".
 *
 * <p>The counter it bumps is what {@code TerrainPass#sendVisibleSections} compares against, and that is
 * the whole of the mechanism: a frame the game did not rebuild the list on does not walk the list, does
 * not allocate the keys and does not call across the bridge. It is also what keeps the one *cleared* list
 * anybody else can see - the one {@code LevelRenderer#allChanged} leaves behind, on a render-distance
 * change, a resource reload or the way into a world, since that method clears without refilling - from being
 * sent, where it would say "the game looked and saw nothing" about a game that had not looked yet. The clear
 * inside {@code applyFrustum} is followed by the refill in the same call on the render thread, so nothing
 * outside it can observe that one.
 *
 * <p>An inject at {@code TAIL} rather than at the call to {@code addSectionsInFrustum}, because the two
 * are the same moment and only one of them leaves the list filled. Nothing is captured and nothing is
 * cancelled: the method runs exactly as it did, and this is a count beside it.
 */
@Mixin(LevelRenderer.class)
public abstract class VisibleSectionsMixin {

    @Inject(method = "applyFrustum", at = @At("TAIL"))
    private void wgpuMc$theVisibleListIsNew(Frustum frustum, CallbackInfo ci) {
        VisibleSectionsRevision.rebuilt();
    }
}