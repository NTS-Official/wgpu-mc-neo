package dev.birb.wgpu.mixin.render;

import dev.birb.wgpu.render.FireAnimation;
import net.minecraft.client.renderer.texture.SpriteContents;
import net.minecraft.client.renderer.texture.TextureAtlas;
import net.minecraft.client.resources.model.ModelBakery;
import net.minecraft.resources.Identifier;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.Unique;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Redirect;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

import java.util.Set;

/**
 * Holds the fire animation still when the renderer's animated-texture switch is off.
 *
 * <p><b>Why the atlas and not the fire.</b> There are three places fire is drawn, and only one of
 * them is this mod's: the terrain, where a face whose sprite the game animates is either baked
 * against the game's atlas or against this side's frozen copy of one frame, decided in Rust. The
 * other two are Minecraft's own passes - {@code ScreenEffectRenderer} for the first-person overlay
 * and {@code FlameFeatureRenderer} for a burning entity - and they both sample {@code fire_0} and
 * {@code fire_1} straight out of the block atlas. The one thing all three share is the atlas
 * contents, and the one place that is decided for the last two is here: an animation that does not
 * advance does not move, whichever pass is reading it.
 *
 * <p>So this stops the ticks rather than changing what is drawn, which is also why the frame it
 * stops on is a real one: {@code fire_0} and {@code fire_1} are 32-frame strips, and holding the
 * frame that happens to be in the atlas is exactly what the terrain does with this switch off.
 *
 * <p><b>Which states are fire.</b> {@code AnimationState} does not say which sprite it belongs to,
 * so it is traced back: state to its animation, animation to its outer {@code SpriteContents}, and
 * that to the sprite's name - the two accessors beside this class. The name is compared against the
 * identifiers {@code ModelBakery} builds its {@code FIRE_0}/{@code FIRE_1} sprite ids from, rather
 * than against the paths spelled out again here.
 *
 * <p>If that trace ever fails to answer - a refactor that breaks an accessor, a sprite whose name is
 * not what the id says - the fire animates and one warning is logged. It deliberately does not fall
 * back to freezing the whole atlas: an atlas that quietly stops moving would be a much stranger bug
 * than the one this is fixing.
 */
@Mixin(TextureAtlas.class)
public abstract class TextureAtlasMixin {

    @Unique
    private static final Logger WGPU_MC$LOGGER = LoggerFactory.getLogger("wgpu-mc/fire-animation");

    /**
     * The two sprites of the fire animation, by the name their contents carry.
     *
     * <p>Built from {@code ModelBakery}'s own constants rather than written out: a sprite id is an
     * atlas and a texture path, and it is the texture path that a {@code SpriteContents} is named
     * after.
     */
    @Unique
    private static final Set<Identifier> WGPU_MC$FIRE = Set.of(
        ModelBakery.FIRE_0.texture(),
        ModelBakery.FIRE_1.texture()
    );

    /** Whether the trace above already failed, so that a broken accessor warns once and not per tick. */
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
        if (!wgpu_mc$firesAreFrozen(state)) {
            state.tick();
        }
    }

    /** Whether this animation belongs to a fire sprite, and is to be held still. */
    @Unique
    private static boolean wgpu_mc$firesAreFrozen(SpriteContents.AnimationState state) {
        try {
            if (!FireAnimation.firesAreFrozen()) {
                return false;
            }

            // Both accessors answer `Object` - see `SpriteContentsAnimatedTextureAccessor` - so the
            // anonymous type is named once, by the only link that is public.
            Object animation = ((SpriteContentsAnimationStateAccessor) state).wgpu_mc$animationInfo();
            Object sprite = ((SpriteContentsAnimatedTextureAccessor) animation).wgpu_mc$sprite();

            return sprite instanceof SpriteContents contents && WGPU_MC$FIRE.contains(contents.name());
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
