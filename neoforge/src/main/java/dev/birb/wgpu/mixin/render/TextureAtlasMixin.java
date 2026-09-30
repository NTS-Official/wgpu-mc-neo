package dev.birb.wgpu.mixin.render;

import dev.birb.wgpu.render.AnimationSprites;
import dev.birb.wgpu.render.FireAnimation;
import net.minecraft.client.renderer.texture.SpriteContents;
import net.minecraft.client.renderer.texture.TextureAtlas;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.Unique;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Redirect;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

/**
 * Holds the fire animation still when the renderer's animated-texture switch is off.
 *
 * <p><b>Why the atlas and not the fire.</b> There are three places fire is drawn, and only one of them
 * is this mod's: the terrain, where a face whose sprite the game animates is either baked against the
 * game's atlas or against this side's frozen copy of one frame, decided in Rust. The other two are
 * Minecraft's own passes - {@code ScreenEffectRenderer} for the first-person overlay and
 * {@code FlameFeatureRenderer} for a burning entity - and they both sample {@code fire_0} and
 * {@code fire_1} straight out of the block atlas. The one thing all three share is the atlas contents,
 * and the one place that is decided for the last two is here: an animation that does not advance does
 * not move, whichever pass is reading it.
 *
 * <p>So this stops the ticks rather than changing what is drawn, which is also why the frame it stops on
 * is a real one: {@code fire_0} and {@code fire_1} are 32-frame strips, and holding the frame that
 * happens to be in the atlas is exactly what the terrain does with this switch off.
 *
 * <p><b>Which states are fire.</b> The loop this hooks is handed an {@code AnimationState} and nothing
 * else, and a state does not say which sprite it belongs to - so the association is recorded where the
 * atlas already knows it, by {@link SpriteContentsMixin}, and read back here through
 * {@link AnimationSprites}. Recovering it from the state's own private fields was tried first and cannot
 * be done from a mod's package; {@link AnimationSprites} has the whole of why.
 *
 * <p>If that ever fails to answer - a state this side never saw created, a sprite whose name is not what
 * the id says - the fire animates and one warning is logged. It deliberately does not fall back to
 * freezing the whole atlas: an atlas that quietly stops moving would be a much stranger bug than the one
 * this is fixing.
 */
@Mixin(TextureAtlas.class)
public abstract class TextureAtlasMixin {

    @Unique
    private static final Logger WGPU_MC$LOGGER = LoggerFactory.getLogger("wgpu-mc/fire-animation");

    /** Whether the trace below already failed, so that it warns once and not per tick. */
    @Unique
    private static boolean wgpu_mc$reportedTraceFailure;

    /**
     * Ticks every animated sprite's animation except the fire's, while the switch is off.
     *
     * <p>{@code cycleAnimationFrames} is the block atlas's own tick, and the loop it runs is what
     * advances every sprite the atlas animates. Skipping the call leaves that sprite's frame in the
     * atlas where it was.
     */
    @Redirect(
        method = "cycleAnimationFrames",
        at = @At(
            value = "INVOKE",
            target = "Lnet/minecraft/client/renderer/texture/SpriteContents$AnimationState;tick()V"
        )
    )
    private void wgpu_mc$tickUnlessFrozen(SpriteContents.AnimationState state) {
        if (!wgpu_mc$spriteIsFrozen(state)) {
            state.tick();
        }
    }

    /**
     * Whether this animation belongs to a fire sprite or a lava sprite, and is to be held still.
     *
     * <p>Two questions with two switches, and they are not the same kind of switch. The fire one is a
     * player's option - the fire animates in three places and only one of them is this renderer's, so the
     * atlas is what has to stop. The lava one is a diagnostic: it answers whether the shimmer on flowing
     * lava is the animation advancing. See {@link AnimationSprites#lavaIsHeld}.
     *
     * <p>A state this side never saw created answers `null` rather than a guess, in both cases: freezing the
     * wrong sprite would be a still picture somewhere in the world that nothing explains.
     */
    @Unique
    private static boolean wgpu_mc$spriteIsFrozen(SpriteContents.AnimationState state) {
        try {
            SpriteContents sprite = AnimationSprites.spriteOf(state);

            if (FireAnimation.firesAreFrozen() && AnimationSprites.isFire(sprite)) {
                return true;
            }

            return AnimationSprites.lavaIsHeld() && AnimationSprites.isLava(sprite);
        } catch (Throwable failure) {
            if (!wgpu_mc$reportedTraceFailure) {
                wgpu_mc$reportedTraceFailure = true;
                WGPU_MC$LOGGER.warn(
                    "wgpu: could not tell which sprite an atlas animation belongs to, so the fire "
                        + "animation keeps running while the animated-texture switch is off",
                    failure
                );
            }

            return false;
        }
    }
}
