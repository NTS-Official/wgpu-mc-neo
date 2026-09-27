package dev.birb.wgpu.mixin;

import dev.birb.wgpu.WgpuMcMod;
import dev.birb.wgpu.backend.Diagnostics;
import dev.birb.wgpu.render.Wgpu;
import net.minecraft.client.gui.components.DebugScreenOverlay;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.Unique;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.ModifyArg;

import java.util.List;

/**
 * Appends wgpu-mc diagnostics to the F3 debug overlay.
 *
 * <p>1.21.1 exposed {@code DebugScreenOverlay#getSystemInformation()}, which this mixin wrapped on
 * {@code RETURN}. 26.1 removed it: {@code extractRenderState} now allocates one
 * {@code List<String>} per column and hands each of them to the private {@code extractLines},
 * which renders it immediately and returns nothing. There is no return value left to extend, but
 * the list itself is a parameter, and it is a fresh {@code ArrayList} on every call - so adding to
 * it is both safe and the only seam that does not need a shadowed field.
 *
 * <p>The two calls are the right column ({@code ordinal = 0}, the one passed {@code true}) and the
 * left column ({@code ordinal = 1}). The diagnostics go on the left, next to the rest of the
 * system information.
 *
 * <p>The line naming the renderer backend is not here: it belongs with the rest of the graphics
 * information, and {@link DebugEntrySystemSpecsMixin} puts it there.
 *
 * <p>An earlier revision collected the lines into a static field behind a public accessor instead.
 * Mixin rejects that outright - "contains non-private static method" - because a mixin may not
 * contribute a non-private method to its target. The field and the accessor are gone; the lines are
 * built here, once per frame, and only while the overlay is actually open.
 */
@Mixin(DebugScreenOverlay.class)
public class DebugHUDMixin {

    @ModifyArg(
            method = "extractRenderState",
            at = @At(
                    value = "INVOKE",
                    target = "Lnet/minecraft/client/gui/components/DebugScreenOverlay;extractLines(Lnet/minecraft/client/gui/GuiGraphicsExtractor;Ljava/util/List;Z)V",
                    ordinal = 0
            ),
            index = 1
    )
    private static List<String> wgpu_mc$reportRightColumn(List<String> lines) {
        wgpu_mc$report("right", lines);
        return lines;
    }

    @ModifyArg(
            method = "extractRenderState",
            at = @At(
                    value = "INVOKE",
                    target = "Lnet/minecraft/client/gui/components/DebugScreenOverlay;extractLines(Lnet/minecraft/client/gui/GuiGraphicsExtractor;Ljava/util/List;Z)V",
                    ordinal = 1
            ),
            index = 1
    )
    private static List<String> wgpu_mc$addDiagnostics(List<String> lines) {
        if (WgpuMcMod.ENTRIES > 0) {
            lines.add("[Neolectrum] texSubImage2D call count: " + Wgpu.getTimesTexSubImageCalled());
            lines.add("[Neolectrum] avg uploading entities: " + (WgpuMcMod.TIME_SPENT_ENTITIES / WgpuMcMod.ENTRIES / 1E6) + "ms");
        }

        // The section feed, phase by phase, averaged over the rebuilds it has served: what Minecraft's
        // chunk-build threads spend in this renderer's half of a rebuild. One line, because the three
        // numbers are only meaningful next to each other - the question is which part dominates.
        long offers = WgpuMcMod.SECTION_OFFERS.sum();
        if (offers > 0) {
            lines.add("[Neolectrum] section feed per offer: light "
                    + (WgpuMcMod.TIME_SPENT_SECTION_LIGHT.sum() / offers / 1E6) + "ms, blocks "
                    + (WgpuMcMod.TIME_SPENT_SECTION_BLOCKS.sum() / offers / 1E6) + "ms, call "
                    + (WgpuMcMod.TIME_SPENT_SECTION_CALL.sum() / offers / 1E6) + "ms, "
                    + (WgpuMcMod.SECTION_PAYLOAD_BYTES.sum() / offers / 1E3) + " KB");
        }

        wgpu_mc$report("left", lines);
        return lines;
    }

    /**
     * Diagnostics: which column this is and what it holds, in order.
     *
     * <p>Both columns are reported because the interesting lines are split between them - the
     * version and the system block on one side, the biome and the player's position on the other -
     * and a screenshot only ever shows the top of the column that happens to be on screen.
     */
    @Unique
    private static void wgpu_mc$report(String column, List<String> lines) {
        if (!Diagnostics.loggingEnabled() || !wgpu_mc$REPORTED.add(column)) {
            return;
        }

        WgpuMcMod.LOGGER.info("wgpu: F3 {} column:\n  {}", column, String.join("\n  ", lines));
    }

    /** Diagnostics are reported once per session and per column, not once per frame. */
    @Unique
    private static final java.util.Set<String> wgpu_mc$REPORTED = java.util.concurrent.ConcurrentHashMap.newKeySet();
}
