package dev.birb.wgpu.mixin.render;

import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.gen.Accessor;

/**
 * The animation an {@code AnimationState} is playing, so that a state can be traced back to the
 * sprite it belongs to.
 *
 * <p>The chain is three links long and every one of them is private: an {@code AnimationState} holds
 * its {@code AnimatedTexture}, that holds the outer {@code SpriteContents}, and that is what carries
 * the sprite's name. See {@link SpriteContentsAnimatedTextureAccessor} for why this link answers
 * {@code Object}, and {@link TextureAtlasMixin} for what walks it.
 */
@Mixin(targets = "net.minecraft.client.renderer.texture.SpriteContents$AnimationState")
public interface SpriteContentsAnimationStateAccessor {

    /** The animation this state is playing. */
    @Accessor("animationInfo")
    Object wgpu_mc$animationInfo();
}
