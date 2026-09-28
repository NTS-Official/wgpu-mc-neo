package dev.birb.wgpu.mixin.render;

import net.minecraft.client.renderer.texture.SpriteContents;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.gen.Accessor;

/**
 * The sprite an animation belongs to.
 *
 * <p>{@code AnimatedTexture} is an inner class of {@code SpriteContents}, so the sprite is its
 * synthetic {@code this$0} - and reading it is what lets {@link TextureAtlasMixin} ask which sprite
 * an animation is playing, which is the whole of how the fire animation is picked out of the block
 * atlas.
 *
 * <p><b>Both this and its sibling return {@code Object} on purpose.</b> {@code AnimatedTexture} is
 * package-private, so an accessor that returned it could not be named from a mod's own package - and
 * the field's real type is not needed to walk the chain: the JVM widens a reference to {@code Object}
 * for free, and the only link that has to be named is the last one, {@code SpriteContents}, which is
 * public. What a wrong field name would cost is why {@link TextureAtlasMixin} catches the failure and
 * says so instead of throwing: an accessor is looked up by name the first time it is called, which is
 * in the middle of a running client.
 */
@Mixin(targets = "net.minecraft.client.renderer.texture.SpriteContents$AnimatedTexture")
public interface SpriteContentsAnimatedTextureAccessor {

    /** The sprite this animation was made from, as the synthetic outer reference. */
    @Accessor("this$0")
    Object wgpu_mc$sprite();
}
