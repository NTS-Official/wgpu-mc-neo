package dev.birb.wgpu.mixin.chunk;

import dev.birb.wgpu.chunk.RustChunkBake;
import com.mojang.blaze3d.vertex.MeshData;
import net.minecraft.client.renderer.chunk.CompiledSectionMesh;
import net.minecraft.client.renderer.chunk.SectionCompiler;
import net.minecraft.client.renderer.chunk.TranslucencyPointOfView;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Redirect;

/**
 * Drops the geometry Minecraft builds for a section the Rust baker has taken.
 *
 * <p>While the Rust terrain path is on, this side bakes the same section from the same snapshot and the
 * render graph draws it; Minecraft's mesh for those blocks is built, uploaded into the game's own uber
 * buffers and then never read, because the pass that would draw it has been taken over. On a populated
 * world that is the whole cost of the terrain pipeline - the per-block model lookup, the vertex
 * building, the packing into the scratch buffers, the sort key pass and the upload into a GPU buffer -
 * paid for nothing.
 *
 * <p><b>What is kept, and why it is not thrown away with the rest.</b> {@code compile} does three jobs
 * and only one of them is the mesh: it also collects the section's **renderable block entities** and
 * computes its **visibility set**. Both are read by things that are not the terrain -
 * {@code LevelRenderer.extractVisibleBlockEntities} walks
 * {@code section.getSectionMesh().getRenderableBlockEntities()}, which is how a chest is found at all,
 * and {@code SectionOcclusionGraph} uses the visibility set to decide which sections are even worth
 * walking to - so the compile still runs, and what is thrown away is the part that is paid for and not
 * read.
 *
 * <p><b>Where this hooks.</b> {@code new CompiledSectionMesh(pointOfView, results)} is the one call in
 * {@code RebuildTask.doTask} that is handed both, and it is the last moment before those layers would be
 * uploaded. Emptying {@code renderedLayers} there leaves the caller taking its
 * {@code results.renderedLayers.isEmpty()} path - the path an all-air section takes, which is the one
 * state in this dispatcher that is known to be safe: the section's mesh becomes an empty
 * {@code CompiledSectionMesh} and the occlusion graph is told the section is ready.
 *
 * <p>{@code MeshData} is {@code AutoCloseable} and its {@code close} is what returns the scratch buffers
 * to the pool, so an emptied map is a released one; {@code SortState} goes with it, because it is only
 * read to re-sort a translucent mesh that no longer exists.
 *
 * <p>The answer comes from {@code RustChunkBake} rather than from the setting, and that is the point:
 * the setting says the path is on, while the bake says whether Rust actually took *this* section. A bake
 * that failed - no block cache yet, an exception, the registry not ready - leaves Minecraft's mesh as
 * the fallback for that one section instead of a hole in the world. That answer is per **thread**, since
 * rebuilds run on a pool of chunk-build workers.
 */
@Mixin(targets = "net.minecraft.client.renderer.chunk.SectionRenderDispatcher$RenderSection$RebuildTask")
public class SectionCompilerMixin {

    @Redirect(
        method = "doTask",
        at = @At(
            value = "NEW",
            target = "net/minecraft/client/renderer/chunk/CompiledSectionMesh"
        )
    )
    private CompiledSectionMesh wgpuMc$dropTheMeshRustAlreadyBuilt(
            TranslucencyPointOfView pointOfView, SectionCompiler.Results results) {
        if (RustChunkBake.meshesInRust()) {
            // Closed one at a time and *then* emptied, rather than cleared: `renderedLayers` is an
            // `EnumMap`, and clearing a map does not close what was in it - `MeshData.close` is what
            // returns the scratch buffers to a **fixed pool**, so a bare `clear()` here would leak the
            // whole section's staging buffers. (`results.release()` is the same walk, and is what the
            // game itself calls when a compile has to be thrown away.)
            for (MeshData layer : results.renderedLayers.values()) {
                layer.close();
            }

            results.renderedLayers.clear();
            // Only ever read to re-sort a translucent mesh that no longer exists.
            results.transparencyState = null;
        }

        return new CompiledSectionMesh(pointOfView, results);
    }
}
