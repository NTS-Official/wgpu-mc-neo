package dev.birb.wgpu.mixin.chunk;

import dev.birb.wgpu.chunk.RustChunkBake;
import net.minecraft.client.renderer.block.BlockAndTintGetter;
import net.minecraft.client.renderer.block.BlockQuadOutput;
import net.minecraft.client.renderer.block.FluidRenderer;
import net.minecraft.client.renderer.block.ModelBlockRenderer;
import net.minecraft.client.renderer.block.dispatch.BlockStateModel;
import net.minecraft.client.renderer.chunk.SectionCompiler;
import net.minecraft.core.BlockPos;
import net.minecraft.world.level.block.state.BlockState;
import net.minecraft.world.level.material.FluidState;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Redirect;

/**
 * Skips building Minecraft's geometry for a section the Rust baker has taken.
 *
 * <p><b>These are the two calls that cost the work.</b> A section compile is one pass over its 4096
 * blocks, and per block it does three separable things: it fills in the visibility set, it collects the
 * renderable block entities, and it turns the block into geometry. Only the third is skipped here, and
 * it is skipped at the call that does it rather than after the fact - so the per-block model lookup
 * ({@code blockModelSet.get}), the part collection, the lighting, the vertex building, the packing into
 * the scratch buffers and the sort-key pass are all never paid for. What is left per block is a
 * {@code isAir} test, a {@code isSolidRender} test, an {@code hasBlockEntity} test and two virtual
 * calls that return immediately.
 *
 * <p><b>What it replaced, and why that was the wrong place.</b> This used to redirect
 * {@code new CompiledSectionMesh(...)} inside {@code RebuildTask.doTask}, which is *after*
 * {@code SectionCompiler.compile} has built every vertex - so the only thing it saved was the upload and
 * the derived data. The per-block work, which is nearly all of it, was still paid in full and then
 * thrown away.
 *
 * <p><b>The two things that must not be skipped, and are not.</b>
 * {@code visGraph.setOpaque} feeds {@code results.visibilitySet}, which is what
 * {@code SectionOcclusionGraph} walks to decide which sections are even worth visiting - skipping it
 * would make every section look transparent and the occlusion graph meaningless.
 * {@code handleBlockEntity} fills {@code results.renderableBlockEntities}, which is how a chest, a sign
 * or a banner is found at all by {@code LevelRenderer.extractVisibleBlockEntities}. Both are outside the
 * calls redirected here, and both still run.
 *
 * <p><b>Why the empty result is safe.</b> With no geometry produced, {@code startedLayers} stays empty,
 * so {@code results.renderedLayers} is empty and {@code results.transparencyState} is never set. That is
 * exactly the state an all-air section compiles to, and it is the one state in this dispatcher known to
 * be safe: {@code doTask} takes its {@code renderedLayers.isEmpty()} path, the section's mesh is an
 * empty {@code CompiledSectionMesh}, and the occlusion graph is told the section is ready. Nothing has to
 * be closed or released, because nothing was built - which is the other half of the saving, since
 * {@code MeshData.close} is what returns the scratch buffers to a fixed pool.
 *
 * <p><b>NeoForge's own hook is deliberately left alone.</b> {@code ClientHooks.addAdditionalGeometry}
 * runs after the loop and is a different mechanism: a mod's additional renderer is handed the
 * {@code ModelBlockRenderer} and builds its own geometry. That geometry is not something this renderer
 * knows how to build in Rust, so it is left to Minecraft to produce, exactly as before.
 *
 * <p>The answer comes from {@code RustChunkBake} rather than from the setting, and that is the point:
 * the setting says the path is on, while the bake says whether Rust actually took *this* section. A bake
 * that failed - no block cache yet, an exception, the registry not ready - leaves Minecraft's geometry
 * as the fallback for that one section instead of a hole in the world. That answer is per **thread**,
 * since rebuilds run on a pool of chunk-build workers, and {@code compile} runs on the same thread as
 * the bake that set it.
 */
@Mixin(SectionCompiler.class)
public class SectionCompilerMixin {

    /**
     * The block geometry: {@code blockRenderer.tesselateBlock(...)}.
     *
     * <p>Redirected rather than cancelled at a line number, so the target is the call and not a position:
     * a descriptor match cannot silently start patching nothing because a patch moved a line. The
     * arguments are the receiver and the nine the call takes, in order, and none of them is used - the
     * whole point is that the values that would have been passed, including the model lookup inlined
     * into the call site, are never computed.
     */
    @Redirect(
        method = "compile(Lnet/minecraft/core/SectionPos;Lnet/minecraft/client/renderer/chunk/RenderSectionRegion;Lcom/mojang/blaze3d/vertex/VertexSorting;Lnet/minecraft/client/renderer/SectionBufferBuilderPack;Ljava/util/List;)Lnet/minecraft/client/renderer/chunk/SectionCompiler$Results;",
        at = @At(
            value = "INVOKE",
            target = "Lnet/minecraft/client/renderer/block/ModelBlockRenderer;tesselateBlock(" +
                "Lnet/minecraft/client/renderer/block/BlockQuadOutput;" +
                "FFFLnet/minecraft/client/renderer/block/BlockAndTintGetter;" +
                "Lnet/minecraft/core/BlockPos;" +
                "Lnet/minecraft/world/level/block/state/BlockState;" +
                "Lnet/minecraft/client/renderer/block/dispatch/BlockStateModel;J)V"
        )
    )
    private void wgpuMc$skipBlockGeometry(
            ModelBlockRenderer renderer,
            BlockQuadOutput output,
            float x, float y, float z,
            BlockAndTintGetter level,
            BlockPos pos,
            BlockState blockState,
            BlockStateModel model,
            long seed) {
        if (RustChunkBake.meshesInRust()) {
            return;
        }

        renderer.tesselateBlock(output, x, y, z, level, pos, blockState, model, seed);
    }

    /**
     * The fluid geometry: {@code fluidRenderer.tesselate(...)}.
     *
     * <p>Only reached for a fluid with no custom renderer, which is what the surrounding
     * {@code customRenderer == null || !customRenderer.renderFluid(..)} guard decides - and that guard is
     * left where it is, because a mod's fluid renderer is the same case as
     * {@code addAdditionalGeometry}: geometry this renderer cannot build in Rust, so it stays
     * Minecraft's.
     */
    @Redirect(
        method = "compile(Lnet/minecraft/core/SectionPos;Lnet/minecraft/client/renderer/chunk/RenderSectionRegion;Lcom/mojang/blaze3d/vertex/VertexSorting;Lnet/minecraft/client/renderer/SectionBufferBuilderPack;Ljava/util/List;)Lnet/minecraft/client/renderer/chunk/SectionCompiler$Results;",
        at = @At(
            value = "INVOKE",
            target = "Lnet/minecraft/client/renderer/block/FluidRenderer;tesselate(" +
                "Lnet/minecraft/client/renderer/block/BlockAndTintGetter;" +
                "Lnet/minecraft/core/BlockPos;" +
                "Lnet/minecraft/client/renderer/block/FluidRenderer$Output;" +
                "Lnet/minecraft/world/level/block/state/BlockState;" +
                "Lnet/minecraft/world/level/material/FluidState;)V"
        )
    )
    private void wgpuMc$skipFluidGeometry(
            FluidRenderer renderer,
            BlockAndTintGetter level,
            BlockPos pos,
            FluidRenderer.Output output,
            BlockState blockState,
            FluidState fluidState) {
        if (RustChunkBake.meshesInRust()) {
            return;
        }

        renderer.tesselate(level, pos, output, blockState, fluidState);
    }
}
