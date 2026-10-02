package dev.birb.wgpu.mixin.level;

import it.unimi.dsi.fastutil.objects.ObjectArrayList;
import net.minecraft.client.renderer.LevelRenderer;
import net.minecraft.client.renderer.chunk.SectionRenderDispatcher;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.gen.Accessor;

/**
 * The lists of sections Minecraft's own occlusion culling decided are visible this frame.
 *
 * <p>{@code LevelRenderer} keeps two of them and both are private: {@code visibleSections}, which is
 * what the level is actually drawn from, and {@code nearbyVisibleSections}, which is the subset the
 * game treats as close. The Rust terrain pass draws from the first - it stands in for the same draws -
 * so that is the one read here.
 *
 * <p>The list is <b>not</b> rebuilt every frame. It is filled by {@code LevelRenderer#applyFrustum},
 * which {@code cullTerrain} runs only when the camera has turned two degrees or
 * {@code SectionOcclusionGraph#consumeFrustumUpdate} says the graph's answer moved - so between rebuilds
 * it is a snapshot that the frame reads as it stands, and {@code VisibleSectionsMixin} counts the
 * rebuilds so the reader can tell a new list from the one it already has. What it holds when it is
 * rebuilt is a few hundred sections rather than the couple of thousand a frustum would name, because
 * the walk that fills it goes outward through sections it can actually see. Reading it once per frame
 * and handing it to the native side is the whole of the occlusion culling this renderer does for
 * terrain - see {@code RenderGraph::terrain_layers}' caller.
 *
 * <p>An accessor rather than a {@code @Redirect} or an {@code @Inject}, because nothing needs to
 * happen at any particular moment: the list is read when the frame is drawn, and it is the frame's own
 * list by then.
 */
@Mixin(LevelRenderer.class)
public interface VisibleSectionsAccessor {

    /**
     * The sections the level renderer is drawing this frame, in its own order.
     *
     * <p>{@code ObjectArrayList} and not {@code List}: an accessor is matched by name and **erased
     * descriptor**, so declaring the interface the field happens to satisfy is an
     * {@code InvalidAccessorException} at load - the field's own declared type is what has to be named
     * here. The same shape of mistake as the {@code this$0:Ljava/lang/Object} one this project already
     * paid for once.
     */
    @Accessor("visibleSections")
    ObjectArrayList<SectionRenderDispatcher.RenderSection> wgpu_mc$visibleSections();
}
