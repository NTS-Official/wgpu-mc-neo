package dev.birb.wgpu.mixin.render;

import dev.birb.wgpu.render.AnimationSprites;
import net.minecraft.client.renderer.texture.SpriteContents;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfoReturnable;

/**
 * Records which sprite each animation state belongs to, so that the atlas tick can tell whose animation
 * it is holding still.
 *
 * <p>This is the one place the atlas does not offer a nameable field and a public parameter does it
 * instead: {@code createAnimationState} is a public method of a public class, it is handed the
 * {@code SpriteContents} it is making the state for, and it returns the state. Both halves of the
 * association are therefore in the signature. See {@link AnimationSprites} for why the association is
 * recorded rather than followed through {@code AnimationState}'s private fields.
 *
 * <p>The {@code null} return is an unanimated sprite - it has no animation state and is on nobody's tick
 * list - so there is nothing to record.
 */
@Mixin(SpriteContents.class)
public abstract class SpriteContentsMixin {

    @Inject(method = "createAnimationState", at = @At("RETURN"))
    private void wgpu_mc$rememberAnimationSprite(
            CallbackInfoReturnable<SpriteContents.AnimationState> cir) {
        SpriteContents.AnimationState state = cir.getReturnValue();
        if (state != null) {
            AnimationSprites.record(state, (SpriteContents) (Object) this);
        }
    }
}
