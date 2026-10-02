package dev.birb.wgpu.mixin.chunk;

import net.minecraft.client.renderer.chunk.VisGraph;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.gen.Accessor;

import java.util.BitSet;

/**
 * The opaque-block set a section's visibility answer is computed from, and the count of blocks that are
 * not in it.
 *
 * <p><b>Why this is reachable at all.</b> {@code VisGraph} answers exactly one question - which of a
 * section's six faces can be seen into from outside it - and its whole state is a {@link BitSet} with one
 * bit per position ({@code setOpaque} sets a bit and decrements {@code empty}) plus {@code resolve()},
 * which is the game's own flood fill over the boundary. This renderer already knows which positions are
 * solid, because it is the same per-block-state question the Rust baker answers for every face it emits;
 * {@code SectionCompilerMixin} computes that set in one pass over the section's own palette and storage
 * and puts it in here, so the 4096-block walk that used to fill it one {@code BlockPos} at a time is not
 * needed. {@code resolve()} is left exactly as it is, so the visibility answer itself is the game's.
 *
 * <p>The bit index is the game's: {@code VisGraph.getIndex} is {@code x | y << 8 | z << 4}, which is the
 * same order a {@code PalettedContainer}'s storage indexes its 4096 values in - so the position index the
 * mask is built from is already the bit to set.
 */
@Mixin(VisGraph.class)
public interface VisGraphMixin {

    /** The opaque set: bit {@code x | y << 8 | z << 4} is set for a position that is a full block. */
    @Accessor("bitSet")
    BitSet wgpu_mc$bits();

    /**
     * How many of the 4096 positions are *not* in it, which {@code resolve} uses twice: fewer than 256
     * opaque blocks is "you can see into this section from anywhere", and none at all is the opposite.
     * Written rather than counted down through {@code setOpaque}, because this side sets the bits in bulk.
     */
    @Accessor("empty")
    void wgpu_mc$setEmpty(int empty);
}
