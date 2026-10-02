package dev.birb.wgpu.mixin.chunk;

import net.minecraft.client.renderer.block.dispatch.BlockStateModel;
import net.minecraft.client.renderer.block.dispatch.WeightedVariants;
import net.minecraft.util.random.WeightedList;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.gen.Accessor;

/**
 * The list of models a weighted blockstate variant chooses between.
 *
 * <p>A blockstate whose variant is a <em>list</em> bakes to one {@code WeightedVariants}, and the entry
 * drawn at a given position is picked by {@code WeightedList#getRandomOrThrow} on a {@code RandomSource}
 * seeded with {@code blockState.getSeed(pos)} - see {@code ModelBlockRenderer#tesselateBlock}, which is
 * the call the game's own mesher makes. The list is private and has no accessor, so the choice cannot be
 * made from outside without reading it, and making the choice <em>here</em> rather than in Rust is the
 * point: the game's own list, its own weights and its own random number generator answer the question
 * exactly, where a second implementation of any of the three would be a second thing to keep in step.
 *
 * <p>{@code RustChunkBake} reads this to send one variant index per position of the section being
 * rebuilt; see {@code section::SectionBlocks::variants} on the Rust side for what it is used for.
 */
@Mixin(WeightedVariants.class)
public interface WeightedVariantsAccessor {

    /** The entries this variant chooses between, in the order the blockstate listed them. */
    @Accessor("list")
    WeightedList<BlockStateModel> wgpu_mc$list();
}