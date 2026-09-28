package dev.birb.wgpu.mixin.level;

import net.minecraft.client.renderer.chunk.SectionRenderDispatcher;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.gen.Accessor;

/**
 * The packed position a render section is named by.
 *
 * <p>{@code RenderSection.sectionNode} is private, and it is a {@code SectionPos.asLong} - the same
 * packing the JVM already uses to key its own section records ({@code RustChunkBake}), and the same
 * one the native side unpacks. Reading it and passing it straight through is what keeps the three in
 * step: nothing re-derives a xyz triple that could disagree with the key the section was baked under.
 *
 * <p>There is a {@code renderOrigin} field beside it, but that is a block position maintained for the
 * render path rather than the section's own name, and the node is what the occlusion graph itself
 * keys on.
 */
@Mixin(targets = "net.minecraft.client.renderer.chunk.SectionRenderDispatcher$RenderSection")
public interface RenderSectionNodeAccessor {

    /** The section's position as {@code SectionPos.asLong}. */
    @Accessor("sectionNode")
    long wgpu_mc$sectionNode();
}
