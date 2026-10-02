package dev.birb.wgpu.mixin.chunk;

import net.minecraft.client.renderer.chunk.SectionCompiler;
import net.minecraft.world.level.block.entity.BlockEntity;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.gen.Invoker;

/**
 * The one thing in a section compile that is private and still needed.
 *
 * <p>{@code handleBlockEntity} is what turns a position that has a block entity into an entry in
 * {@code Results.blockEntities}, after asking the game's {@code BlockEntityRenderer} registry whether it
 * is a renderer that draws in the world at all. That is the game's answer to give and not this side's, so
 * it is called rather than copied: {@code SectionCompilerMixin} skips the loop that used to find those
 * positions and calls this for the ones it found its own way.
 */
@Mixin(SectionCompiler.class)
public interface SectionCompilerInvoker {

    @Invoker("handleBlockEntity")
    void wgpu_mc$handleBlockEntity(SectionCompiler.Results results, BlockEntity blockEntity);
}
