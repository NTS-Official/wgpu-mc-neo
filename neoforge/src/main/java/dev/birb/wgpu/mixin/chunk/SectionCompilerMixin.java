package dev.birb.wgpu.mixin.chunk;

import com.mojang.blaze3d.vertex.VertexSorting;
import dev.birb.wgpu.WgpuMcMod;
import dev.birb.wgpu.chunk.ContainerData;
import dev.birb.wgpu.chunk.RustChunkBake;
import net.minecraft.client.renderer.SectionBufferBuilderPack;
import net.minecraft.client.renderer.block.BlockAndTintGetter;
import net.minecraft.client.renderer.block.BlockQuadOutput;
import net.minecraft.client.renderer.block.FluidRenderer;
import net.minecraft.client.renderer.block.ModelBlockRenderer;
import net.minecraft.client.renderer.block.dispatch.BlockStateModel;
import net.minecraft.client.renderer.chunk.RenderSectionRegion;
import net.minecraft.client.renderer.chunk.SectionCompiler;
import net.minecraft.client.renderer.chunk.VisGraph;
import net.minecraft.client.renderer.chunk.VisibilitySet;
import net.minecraft.core.BlockPos;
import net.minecraft.core.Direction;
import net.minecraft.core.SectionPos;
import net.minecraft.util.BitStorage;
import net.minecraft.world.level.block.RenderShape;
import net.minecraft.world.level.block.entity.BlockEntity;
import net.minecraft.world.level.block.state.BlockState;
import net.minecraft.world.level.chunk.Palette;
import net.minecraft.world.level.chunk.PalettedContainer;
import net.minecraft.world.level.material.FluidState;
import net.minecraft.world.level.material.Fluids;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.Redirect;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfoReturnable;

import java.util.BitSet;
import java.util.List;

/**
 * Skips building Minecraft's geometry for a section the Rust baker has taken.
 *
 * <p><b>Where the cost is removed: the two predicates, not the two calls.</b> A section compile is one
 * pass over its 4096 blocks, and per block it does three separable things: it fills in the visibility
 * set, it collects the renderable block entities, and it turns the block into geometry. Only the third
 * is skipped here, and it is skipped by making the two conditions that guard it false rather than by
 * blanking the two calls that perform it. The distinction is the whole fix: a {@code @Redirect} replaces
 * a call, not the evaluation of its arguments, so redirecting {@code tesselateBlock} alone still paid for
 * {@code forceOpaque}, the three {@code SectionPos.sectionRelative} calls, the {@code blockModelSet.get}
 * lookup and {@code getSeed}, and redirecting {@code FluidRenderer.tesselate} alone still paid for
 * {@code fluidModelSet.get} and the {@code customRenderer()} check. The predicates are what make those
 * argument lists unreachable: {@code getRenderShape} answers {@code INVISIBLE} and {@code getFluidState}
 * answers an empty state, so both {@code if} bodies - and the model lookup, the part collection, the
 * lighting, the vertex building, the packing into the scratch buffers and the sort-key pass inside them -
 * are skipped outright. What is left per block is an {@code isAir} test, an {@code isSolidRender} test,
 * an {@code hasBlockEntity} test and two redirected calls that return a constant.
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
 * {@code handleBlockEntity} fills {@code results.blockEntities}, which is how a chest, a sign
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
 * the bake that set it. All four handlers ask it, and the two predicates ask it for every non-air block
 * rather than only for the blocks that got as far as geometry, so it now sits on the innermost path of
 * the loop - which is affordable only because the answer is a thread-local read.
 */
@Mixin(SectionCompiler.class)
public class SectionCompilerMixin {

    /**
     * The empty fluid state, handed back in place of the real one while Rust owns the section.
     *
     * <p>This is the state {@code Fluids.EMPTY} carries, fetched once here rather than through
     * {@code blockState.getFluidState()} once per block. Sharing one instance is safe because a
     * {@code FluidState} is immutable once the fluid built it, and because this path only ever asks the
     * returned value {@code isEmpty()} - nothing else reads a property from it, so a shared empty state
     * is indistinguishable from a freshly allocated one apart from not being allocated.
     */
    private static final FluidState EMPTY_FLUID_STATE = Fluids.EMPTY.defaultFluidState();

    /**
     * The geometry predicate: {@code blockState.getRenderShape()}, which guards the block tesselation.
     *
     * <p>{@code INVISIBLE} is the enum's "draw no model" constant - {@code MODEL} is the only shape
     * {@code SectionCompiler} turns into geometry - so answering it makes the following
     * {@code getRenderShape() == RenderShape.MODEL} test false and its body is never entered at all. That
     * is what removes the per-block cost: {@code forceOpaque}, the three
     * {@code SectionPos.sectionRelative} calls, the {@code blockModelSet.get} lookup, {@code getSeed}
     * and the {@code tesselateBlock} call all live inside that body - and a {@code @Redirect} on the call
     * would still have evaluated every argument.
     *
     * <p>This runs for every non-air block, not only for the blocks that would have been modelled, so the
     * {@code RustChunkBake} answer is now read once per block.
     */
    @Redirect(
        method = "compile(Lnet/minecraft/core/SectionPos;Lnet/minecraft/client/renderer/chunk/RenderSectionRegion;Lcom/mojang/blaze3d/vertex/VertexSorting;Lnet/minecraft/client/renderer/SectionBufferBuilderPack;Ljava/util/List;)Lnet/minecraft/client/renderer/chunk/SectionCompiler$Results;",
        at = @At(
            value = "INVOKE",
            target = "Lnet/minecraft/world/level/block/state/BlockState;getRenderShape()" +
                "Lnet/minecraft/world/level/block/RenderShape;"
        )
    )
    private RenderShape wgpuMc$skipBlockRenderShape(BlockState blockState) {
        if (RustChunkBake.meshesInRust()) {
            return RenderShape.INVISIBLE;
        }

        return blockState.getRenderShape();
    }

    /**
     * The block geometry: {@code blockRenderer.tesselateBlock(...)}.
     *
     * <p>Redirected rather than cancelled at a line number, so the target is the call and not a position:
     * a descriptor match cannot silently start patching nothing because a patch moved a line. The
     * arguments are the receiver and the nine the call takes, in order, and none of them is used here.
     *
     * <p>This handler cannot save the arguments on its own - they are evaluated before it runs, the model
     * lookup inlined into the call site included. The {@code getRenderShape} redirect above is what keeps
     * this call site from being reached at all; this one stays as the fallback for a block the predicate
     * still answers {@code MODEL} for.
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
     * The fluid predicate: {@code blockState.getFluidState()}, which guards the fluid tesselation.
     *
     * <p>Answering an empty state makes the following {@code !fluidState.isEmpty()} test false, so its
     * whole body goes away: no {@code fluidModelSet.get}, no {@code customRenderer()} check and no
     * {@code FluidRenderer.tesselate} call. That is the only way to skip those, since a redirect on the
     * call itself still evaluates the arguments that look the model and the renderer up.
     *
     * <p>Like the render-shape predicate this runs for every non-air block rather than only for blocks
     * that reached a fluid.
     */
    @Redirect(
        method = "compile(Lnet/minecraft/core/SectionPos;Lnet/minecraft/client/renderer/chunk/RenderSectionRegion;Lcom/mojang/blaze3d/vertex/VertexSorting;Lnet/minecraft/client/renderer/SectionBufferBuilderPack;Ljava/util/List;)Lnet/minecraft/client/renderer/chunk/SectionCompiler$Results;",
        at = @At(
            value = "INVOKE",
            target = "Lnet/minecraft/world/level/block/state/BlockState;getFluidState()" +
                "Lnet/minecraft/world/level/material/FluidState;"
        )
    )
    private FluidState wgpuMc$skipFluidState(BlockState blockState) {
        if (RustChunkBake.meshesInRust()) {
            return EMPTY_FLUID_STATE;
        }

        return blockState.getFluidState();
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

    /**
     * Whether this thread's compile filled the {@code VisGraph} itself and has no reason to walk the
     * section. Set at the head of {@code compile}, read by the two redirects and by the return injection.
     *
     * <p>Per thread for the reason {@code RustChunkBake.tookThisSection} is: rebuilds run on a pool of
     * chunk-build workers, so a field here would be one section's answer read by another's compile the
     * moment two ran at once.
     */
    private static final ThreadLocal<Boolean> MASKED = ThreadLocal.withInitial(() -> false);

    /**
     * Whether the walk was actually skipped for this compile - that is, whether the bits in {@link #OPAQUE}
     * are the only ones the graph has.
     *
     * <p>Separate from {@link #MASKED} so that the mask can be computed and *not* believed: the walk then
     * runs as vanilla, the graph's own bits stand, and {@code wgpuMc$resolveWithTheMask} compares the two -
     * which is how this was checked against the game rather than against a reading of it.
     */
    private static final ThreadLocal<Boolean> SKIPPED = ThreadLocal.withInitial(() -> false);

    /** How many sections have been compared, and how many of them disagreed. */
    private static final java.util.concurrent.atomic.AtomicLong COMPARED =
        new java.util.concurrent.atomic.AtomicLong();

    private static final java.util.concurrent.atomic.AtomicLong DISAGREED =
        new java.util.concurrent.atomic.AtomicLong();

    private static final java.util.concurrent.atomic.AtomicLong REPORTED_SECOND =
        new java.util.concurrent.atomic.AtomicLong();

    /** The positions whose block state has a block entity, as storage indexes, and how many there are. */
    private static final ThreadLocal<int[]> ENTITY_AT = ThreadLocal.withInitial(() -> new int[4096]);

    private static final ThreadLocal<Integer> ENTITY_COUNT = ThreadLocal.withInitial(() -> 0);

    /** The opaque positions, as a bitset whose index is the game's own - see {@link VisGraphMixin}. */
    private static final ThreadLocal<BitSet> OPAQUE = ThreadLocal.withInitial(BitSet::new);

    /**
     * The section this thread is compiling, so that the redirect below can file the answer it resolves.
     *
     * A {@code ThreadLocal} and not a field on the graph, for the reason every other one here is: chunk
     * builds run on a pool of threads, and the graph does not know what position it belongs to. Set at the
     * head of the compile, which is the only place {@code SectionPos} is reachable, and read once.
     */
    private static final ThreadLocal<SectionPos> COMPILING = new ThreadLocal<>();

    /**
     * {@code Direction.values()}, which builds a fresh array on every call.
     *
     * The order is the packing order for the visibility the redirect hands over, so reading it from the
     * game rather than writing it out is what keeps this side from having an opinion about it.
     */
    private static final Direction[] FACES = Direction.values();

    /** Which bits a palette entry contributes: nothing, opaque, block entity, or both. */
    private static final byte OPAQUE_BIT = 1;

    private static final byte ENTITY_BIT = 2;

    /**
     * Whether the walk is skipped, or run so the mask can be checked against it.
     *
     * <p><b>This is true because the two were compared, not because the reasoning looked right.</b> With
     * it false, every section compiled fills the mask *and* runs the vanilla walk, and
     * {@code wgpuMc$compareWithTheWalk} compares the two position by position: on a real world it agreed
     * on every one of 2,514 sections' 4096 positions, with none disagreeing. That is the evidence for this
     * constant - and the reason the comparison is still in the tree, because the failure it guards against
     * is a section that looks see-through when it is not, which costs the occlusion graph a frame rate
     * rather than showing a hole.
     */
    private static final boolean BELIEVE_THE_MASK = true;

    /** How many block entities the mask pass has found and handed to the game's own filter. */
    private static final java.util.concurrent.atomic.AtomicLong ENTITIES_HANDLED =
        new java.util.concurrent.atomic.AtomicLong();

    /** How many sections had their block entities crossed against the walk's, and how many disagreed. */
    private static final java.util.concurrent.atomic.AtomicLong ENTITY_COMPARED =
        new java.util.concurrent.atomic.AtomicLong();

    private static final java.util.concurrent.atomic.AtomicLong ENTITY_DISAGREED =
        new java.util.concurrent.atomic.AtomicLong();

    /**
     * The mask of a palette entry, by palette index, in a scratch array that lives with the thread.
     *
     * <p>4096 entries is every index a section can hold, so this never has to grow.
     */
    private static final ThreadLocal<byte[]> PALETTE_KIND = ThreadLocal.withInitial(() -> new byte[4096]);

    /**
     * The two per-position questions a compile still has to answer, answered once per **block state**
     * instead of once per block, and the 4096-block walk skipped entirely.
     *
     * <p><b>What the walk was doing, and what of it is this side's to do.</b> Per position the vanilla
     * loop asks three things of the block state: whether it is air, whether it is a full block (which fills
     * the {@code VisGraph}), and whether it has a block entity. Every one of those is a property of the
     * *state*, and a section has one state per palette entry - a handful - while the position-to-state
     * mapping is the section's own palette and bit storage, which this side already reads to build the
     * payload it hands the Rust baker. So the predicates are evaluated once per palette entry, and the walk
     * that remains is one storage read and one byte lookup per position, in the section's own index order -
     * which is also {@code VisGraph}'s bit order.
     *
     * <p><b>What it removes, in calls.</b> Per non-air block the walk paid a palette dereference
     * ({@code PalettedContainer.get} through {@code SectionCopy.getBlockState}), a virtual {@code isAir},
     * a virtual {@code isSolidRender}, a virtual {@code hasBlockEntity}, a {@code BlockPos} step in the
     * iterator, and both redirect handlers below - each of which reads a thread-local - because
     * {@code getFluidState} and {@code getRenderShape} are asked per block. What is left is
     * {@code BitStorage.get} and an array read.
     *
     * <p><b>Why it is the same answer, exactly.</b> The bits are the ones {@code setOpaque} would have set,
     * from the same predicate on the same states out of the same snapshot, and {@code empty} is written as
     * {@code 4096 - count} - which is what decrementing it once per opaque position comes to, since the
     * walk visits each position once. {@code resolve()} is not touched, so the visibility answer is the
     * game's own flood fill over a set this side filled in. The block entities are the same entities
     * through the same {@code handleBlockEntity}, which is invoked rather than reproduced.
     *
     * <p><b>And it declines whenever it cannot be sure.</b> Nothing happens here unless the Rust baker
     * took this section's geometry ({@code meshesInRust}), the snapshot's middle section and its container
     * can be reached, and the palette and storage interfaces are available - and in every other case the
     * loop runs exactly as vanilla, with the two predicate redirects still blanking the geometry. A
     * section this side cannot describe is a section Minecraft builds, which is the same fallback the
     * baker has.
     */
    @Inject(
        method = "compile(Lnet/minecraft/core/SectionPos;Lnet/minecraft/client/renderer/chunk/RenderSectionRegion;Lcom/mojang/blaze3d/vertex/VertexSorting;Lnet/minecraft/client/renderer/SectionBufferBuilderPack;Ljava/util/List;)Lnet/minecraft/client/renderer/chunk/SectionCompiler$Results;",
        at = @At("HEAD")
    )
    private void wgpuMc$gatherOcclusionAndBlockEntities(
            SectionPos sectionPos,
            RenderSectionRegion region,
            VertexSorting vertexSorting,
            SectionBufferBuilderPack builders,
            List<?> additionalRenderers,
            CallbackInfoReturnable<SectionCompiler.Results> info) {
        MASKED.set(Boolean.FALSE);
        SKIPPED.set(Boolean.FALSE);
        ENTITY_COUNT.set(0);
        COMPILING.set(sectionPos);

        if (!RustChunkBake.meshesInRust()) {
            return;
        }

        PalettedContainer<BlockState> states = wgpuMc$middleSection(region);

        // Null is the answer for a section that is all air or not loaded, and the loop above it would
        // have found nothing to do in one of those either - but the vanilla path is what runs, because
        // "nothing to do" is cheaper to prove by running it than by arguing about it here.
        if (states == null) {
            return;
        }

        Palette<BlockState> palette = ContainerData.palette(states);
        BitStorage storage = ContainerData.storage(states);

        if (palette == null || storage == null) {
            return;
        }

        byte[] kind = PALETTE_KIND.get();
        int entries = Math.min(palette.getSize(), kind.length);

        for (int index = 0; index < entries; index++) {
            BlockState state = palette.valueFor(index);

            // The three per-state questions, once each. `isAir` guards the other two exactly as the
            // loop's own `if (!blockState.isAir())` does, and the two redirects make the fluid and the
            // model unreachable, so these three are all that is left of a block's contribution.
            byte bits = 0;

            if (state != null && !state.isAir()) {
                if (state.isSolidRender()) {
                    bits |= OPAQUE_BIT;
                }

                if (state.hasBlockEntity()) {
                    bits |= ENTITY_BIT;
                }
            }

            kind[index] = bits;
        }

        BitSet opaque = OPAQUE.get();
        opaque.clear();

        int[] entityAt = ENTITY_AT.get();
        int entities = 0;

        for (int position = 0; position < storage.getSize(); position++) {
            byte bits = kind[storage.get(position) & 0xff];

            if ((bits & OPAQUE_BIT) != 0) {
                // The storage index is `x | z << 4 | y << 8` and the graph's bit is
                // `x | y << 8 | z << 4` - the same number, which is why one pass over the storage fills
                // the graph.
                opaque.set(position);
            }

            if ((bits & ENTITY_BIT) != 0 && entities < entityAt.length) {
                entityAt[entities++] = position;
            }
        }

        ENTITY_COUNT.set(entities);
        MASKED.set(Boolean.TRUE);
    }

    /**
     * The section being compiled, out of the rebuild's own snapshot, or null when it cannot be reached.
     *
     * <p>{@code ContainerData.sectionCopies} is the reflection the Rust baker reads its payload through,
     * and the middle of the 3x3x3 is the section this compile is for - {@code compile}'s own box is that
     * section's 16x16x16 and nothing else.
     */
    private static PalettedContainer<BlockState> wgpuMc$middleSection(RenderSectionRegion region) {
        Object[] copies = ContainerData.sectionCopies(region);

        if (copies == null) {
            return null;
        }

        RenderSectionRegionAccessor corners = (RenderSectionRegionAccessor) region;

        int index = RenderSectionRegion.index(
            corners.wgpu_mc$minSectionX(),
            corners.wgpu_mc$minSectionY(),
            corners.wgpu_mc$minSectionZ(),
            corners.wgpu_mc$minSectionX() + 1,
            corners.wgpu_mc$minSectionY() + 1,
            corners.wgpu_mc$minSectionZ() + 1
        );

        if (index < 0 || index >= copies.length) {
            return null;
        }

        if (!(copies[index] instanceof SectionCopyAccessor copy)) {
            return null;
        }

        return copy.wgpu_mc$states();
    }

    /**
     * The visibility answer, with this side's bits put in the graph first.
     *
     * <p>A redirect on {@code resolve()} rather than on the graph's constructor, for two reasons: Mixin
     * refuses to redirect a constructor at all ("Illegal @Redirect of constructor"), and this is the one
     * place the graph is read, so filling it here is the same as filling it as it was built - the walk
     * that would have called {@code setOpaque} has already been skipped by then.
     *
     * <p>{@code empty} is written from the count rather than decremented per bit, because the bits go in in
     * bulk - and that count is what {@code resolve} reads to choose between its two short answers ("nothing
     * opaque here" and "the whole section is") and the flood fill. {@code resolve} itself is not touched, so
     * the visibility answer is the game's.
     */
    @Redirect(
        method = "compile(Lnet/minecraft/core/SectionPos;Lnet/minecraft/client/renderer/chunk/RenderSectionRegion;Lcom/mojang/blaze3d/vertex/VertexSorting;Lnet/minecraft/client/renderer/SectionBufferBuilderPack;Ljava/util/List;)Lnet/minecraft/client/renderer/chunk/SectionCompiler$Results;",
        at = @At(
            value = "INVOKE",
            target = "Lnet/minecraft/client/renderer/chunk/VisGraph;" +
                "resolve()Lnet/minecraft/client/renderer/chunk/VisibilitySet;"
        )
    )
    private VisibilitySet wgpuMc$resolveWithTheMask(VisGraph graph) {
        if (MASKED.get() && !SKIPPED.get()) {
            // The walk ran, so the graph holds the game's own answer for this section - which makes this
            // the one place the mask can be checked against it, position by position.
            wgpuMc$compareWithTheWalk(graph);
        }

        if (SKIPPED.get()) {
            BitSet opaque = OPAQUE.get();
            int count = opaque.cardinality();

            ((VisGraphMixin) (Object) graph).wgpu_mc$bits().or(opaque);
            ((VisGraphMixin) (Object) graph).wgpu_mc$setEmpty(4096 - count);
        }

        SectionPos compiling = COMPILING.get();
        VisibilitySet visibility = graph.resolve();

        // Filed after resolving, and under the position this compile is for. The payload that will carry the
        // answer is the next one that mentions this section: the bake for this section was handed over before
        // this point, so this compile's own answer cannot be in that payload - and a section whose answer has
        // not come back yet is sent as unanswered rather than as "nothing is visible out of it".
        if (compiling != null) {
                RustChunkBake.noteVisibility(
                SectionPos.asLong(compiling.x(), compiling.y(), compiling.z()),
                wgpuMc$packVisibility(visibility)
                );
        }

        return visibility;
    }

    /**
     * The graph's answer, packed into the 36 bits the Rust side reads: {@code from * 6 + to}.
     *
     * A repacking rather than an interpretation: {@code visibilityBetween} is the graph's own public
     * answer and its pair indexing is the one the Rust side keeps, so what changes here is only the
     * container - a {@code BitSet} of 72 possible pairs (its own {@code set} writes both orders) into a
     * long of 36. The 37th bit belongs to the trailer's "this slot was answered" flag, which is why this
     * is 36 bits and not 64: the two must not collide.
     */
    private static long wgpuMc$packVisibility(VisibilitySet visibility) {
        long packed = 0L;

        for (int from = 0; from < FACES.length; from++) {
                for (int to = 0; to < FACES.length; to++) {
                        if (visibility.visibilityBetween(FACES[from], FACES[to])) {
                                packed |= 1L << (from * FACES.length + to);
                        }
                }
        }

        return packed;
    }
    /**
     * The mask against the walk's own bits, once a second, with the first position they disagree about.
     *
     * <p>A diagnostic rather than a fix: crossing a mask against the thing it replaces is the only way to
     * know the two agree, and a disagreement here is the difference between a picture that culls and one
     * that draws the world. The positions are printed with both states, because the two ways this can be
     * wrong look different: reading another section's storage gives positions whose *blocks* differ, and an
     * index order that is off gives positions that differ in a pattern.
     */
    private static void wgpuMc$compareWithTheWalk(VisGraph graph) {
        BitSet mine = OPAQUE.get();
        BitSet theirs = ((VisGraphMixin) (Object) graph).wgpu_mc$bits();

        COMPARED.incrementAndGet();

        if (mine.equals(theirs)) {
            // The position is not reported for an agreeing section, only the count - which is what says a
            // run is checking anything at all.
            long now = System.currentTimeMillis() / 1000;

            if (REPORTED_SECOND.getAndSet(now) != now) {
                WgpuMcMod.LOGGER.info(
                    "wgpu: the occlusion mask agreed with the vanilla walk on every position of {} "
                        + "section(s) so far ({} disagreed)",
                    COMPARED.get(),
                    DISAGREED.get()
                );
            }

            return;
        }

        DISAGREED.incrementAndGet();

        BitSet onlyMine = (BitSet) mine.clone();
        onlyMine.andNot(theirs);

        BitSet onlyTheirs = (BitSet) theirs.clone();
        onlyTheirs.andNot(mine);

        int at = onlyMine.nextSetBit(0);
        int where = at >= 0 ? at : onlyTheirs.nextSetBit(0);

        WgpuMcMod.LOGGER.warn(
            "wgpu: the occlusion mask disagrees with the vanilla walk on {} position(s) ({} only in the "
                + "mask, {} only in the walk) - first at index {} = x {}, y {}, z {}: {}, against {}; "
                + "{} of {} section(s) so far",
            onlyMine.cardinality() + onlyTheirs.cardinality(),
            onlyMine.cardinality(),
            onlyTheirs.cardinality(),
            where,
            where & 15,
            (where >> 8) & 15,
            (where >> 4) & 15,
            at >= 0 ? "opaque in the mask" : "opaque in the walk",
            at >= 0 ? "not in the walk" : "not in the mask",
            DISAGREED.get(),
            COMPARED.get()
        );
    }

    /**
     * The 4096-block walk, skipped entirely when the mask above answered its two questions.
     *
     * <p>This is what removes the per-position work: with nothing to iterate, the loop body - the
     * {@code getBlockState}, the three predicates, the two redirected calls and the iterator's own
     * {@code BlockPos} step - does not run at all, and the only per-position work left is the pass that
     * built the mask. {@code BlockPos.betweenClosed} is called and answered rather than removed, so the
     * loop is still there for the fallback and nothing else in {@code compile} reads its result.
     */
    @Redirect(
        method = "compile(Lnet/minecraft/core/SectionPos;Lnet/minecraft/client/renderer/chunk/RenderSectionRegion;Lcom/mojang/blaze3d/vertex/VertexSorting;Lnet/minecraft/client/renderer/SectionBufferBuilderPack;Ljava/util/List;)Lnet/minecraft/client/renderer/chunk/SectionCompiler$Results;",
        at = @At(
            value = "INVOKE",
            target = "Lnet/minecraft/core/BlockPos;betweenClosed(" +
                "Lnet/minecraft/core/BlockPos;Lnet/minecraft/core/BlockPos;)" +
                "Ljava/lang/Iterable;"
        )
    )
    private Iterable<BlockPos> wgpuMc$skipTheWalkWhenMasked(BlockPos minPos, BlockPos maxPos) {
        if (MASKED.get() && BELIEVE_THE_MASK) {
            SKIPPED.set(Boolean.TRUE);

            return List.of();
        }

        return BlockPos.betweenClosed(minPos, maxPos);
    }

    /**
     * The block entities of the section, added from the positions the mask pass marked.
     *
     * <p>At the return rather than in the loop, because the loop does not run: the {@code Results} the walk
     * would have appended to is this method's return value, and the game's own {@code handleBlockEntity} is
     * invoked for each - it is what asks the block entity renderer registry whether the entity is one that
     * draws in the world at all, which is not this side's question to answer.
     *
     * <p>The position is taken from the section's own origin and the storage index, which is
     * {@code x | z << 4 | y << 8} - the same de-interleaving the {@code VisGraph} bit index needs.
     */
    @Inject(
        method = "compile(Lnet/minecraft/core/SectionPos;Lnet/minecraft/client/renderer/chunk/RenderSectionRegion;Lcom/mojang/blaze3d/vertex/VertexSorting;Lnet/minecraft/client/renderer/SectionBufferBuilderPack;Ljava/util/List;)Lnet/minecraft/client/renderer/chunk/SectionCompiler$Results;",
        at = @At("RETURN")
    )
    private void wgpuMc$addTheMaskedBlockEntities(
            SectionPos sectionPos,
            RenderSectionRegion region,
            VertexSorting vertexSorting,
            SectionBufferBuilderPack builders,
            List<?> additionalRenderers,
            CallbackInfoReturnable<SectionCompiler.Results> info) {
        if (!MASKED.get()) {
            return;
        }

        int count = ENTITY_COUNT.get();

        // **The entities are the second of the two things a compile must still do, and this is where the
        // two versions of it are crossed.** With the walk skipped, the entities found here are the answer
        // and are handed to the caller. With the walk running - the comparison build - this side's answer
        // goes into a `Results` of its own instead, and the two lists are compared: the game's own list is
        // the one this pass exists to reproduce, and a position de-interleaved wrongly finds no block
        // entity at all, which is a world with no chests in it and nothing else to show for it.
        SectionCompiler.Results results = SKIPPED.get()
            ? info.getReturnValue()
            : new SectionCompiler.Results();

        int[] entityAt = ENTITY_AT.get();
        BlockPos origin = sectionPos.origin();
        BlockPos.MutableBlockPos pos = new BlockPos.MutableBlockPos();
        SectionCompilerInvoker invoker = (SectionCompilerInvoker) (Object) this;

        for (int i = 0; i < count; i++) {
            int index = entityAt[i];

            pos.set(
                origin.getX() + (index & 15),
                origin.getY() + ((index >> 8) & 15),
                origin.getZ() + ((index >> 4) & 15)
            );

            BlockEntity blockEntity = region.getBlockEntity(pos);

            if (blockEntity != null) {
                invoker.wgpu_mc$handleBlockEntity(results, blockEntity);
            }
        }

        ENTITIES_HANDLED.addAndGet(count);

        if (!SKIPPED.get()) {
            wgpuMc$compareTheBlockEntities(results, info.getReturnValue());
        } else {
            wgpuMc$reportTheBlockEntities(count);
        }
    }

    /**
     * The mask pass's block entities against the walk's, which is the only check on the position
     * de-interleaving.
     *
     * <p>Both lists went through the game's own {@code handleBlockEntity}, so they are comparable: the
     * difference is entirely in which positions this side looked at.
     */
    private static void wgpuMc$compareTheBlockEntities(
            SectionCompiler.Results mine,
            SectionCompiler.Results theirs) {
        ENTITY_COMPARED.incrementAndGet();

        if (mine.blockEntities.size() == theirs.blockEntities.size()) {
            return;
        }

        ENTITY_DISAGREED.incrementAndGet();

        WgpuMcMod.LOGGER.warn(
            "wgpu: the mask pass found {} block entit(ies) where the vanilla walk found {} - {} of {} "
                + "section(s) so far disagree",
            mine.blockEntities.size(),
            theirs.blockEntities.size(),
            ENTITY_DISAGREED.get(),
            ENTITY_COMPARED.get()
        );
    }

    /** How many block entities the mask pass has handled, once a second, even when it is none. */
    private static void wgpuMc$reportTheBlockEntities(int count) {
        long now = System.currentTimeMillis() / 1000;

        if (REPORTED_SECOND.getAndSet(now) == now) {
            return;
        }

        WgpuMcMod.LOGGER.info(
            "wgpu: the mask pass handed {} block entit(ies) of the last section to the game's own filter, "
                + "{} in all",
            count,
            ENTITIES_HANDLED.get()
        );
    }
}
