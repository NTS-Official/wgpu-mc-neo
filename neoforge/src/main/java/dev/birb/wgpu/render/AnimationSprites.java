package dev.birb.wgpu.render;

import net.minecraft.client.renderer.texture.SpriteContents;
import net.minecraft.client.resources.model.ModelBakery;
import net.minecraft.resources.Identifier;

import java.util.Collections;
import java.util.Map;
import java.util.Set;
import java.util.WeakHashMap;

/**
 * Which sprite each atlas animation state was made for, and which two sprites are the fire.
 *
 * <p><b>Why it lives here and not beside the mixins that use it.</b> This is in the mod's own package
 * because Mixin refuses to load a plain class out of a package it owns: every class under a configured
 * mixin package is treated as a mixin, and one that is not a mixin fails its class load with
 * {@code IllegalClassLoadError: ... is in a defined mixin package dev.birb.wgpu.mixin.* owned by
 * wgpu_mc}. That is not a warning - the failure landed inside a resource reload, took the whole reload
 * down with it, and left the title screen black.
 *
 * <p>It is shared by two mixins that cannot see each other's fields - {@code SpriteContentsMixin}
 * records the association, {@code TextureAtlasMixin} reads it while ticking - so it is a class of its own
 * rather than statics on one of them.
 *
 * <p><b>Why an association is recorded at all, rather than followed.</b> The tick loop is handed an
 * {@code AnimationState} and nothing else, and a state does not say which sprite it belongs to. The
 * obvious route is to follow the objects back: {@code AnimationState} holds its {@code AnimatedTexture},
 * and that holds the outer {@code SpriteContents}, which carries {@code name()}. Two field accessors
 * walked that, and it cannot be done from a mod's package:
 *
 * <ul>
 *   <li>an accessor is matched against the field by <b>name and type</b>, where the type is the accessor
 *       method's own return type - so an accessor answering {@code Object} is not a wider version of one
 *       answering {@code AnimatedTexture}, it is a member that does not exist. A session died on the
 *       title screen with
 *       {@code InvalidAccessorException: No candidates were found matching this$0:Ljava/lang/Object;};</li>
 *   <li>and the type cannot be spelled out instead: {@code SpriteContents$AnimatedTexture} is
 *       package-private, and a type parameter erases to {@code Object}, which is the same member that
 *       does not exist.</li>
 * </ul>
 *
 * <p>So it is recorded where the sprite is already a <b>parameter</b>:
 * {@code SpriteContents.createAnimationState} is handed the sprite it is making the state for, and the
 * sprite is a public class with a public {@code name()}. Nothing private is named, so nothing is matched
 * by a type that cannot be written down.
 */
public final class AnimationSprites {

    /**
     * The sprite each state was made for.
     *
     * <p>Weak keys, because this is a cache of an association and not a list of states to keep alive: an
     * atlas reload throws every sprite away, and a strong entry would leak exactly what is being
     * replaced. Only read while the animated-texture switch is off.
     */
    private static final Map<SpriteContents.AnimationState, SpriteContents> BY_STATE =
        Collections.synchronizedMap(new WeakHashMap<>());

    /**
     * The two sprites of the fire animation, by the name their contents carry.
     *
     * <p><b>Built on first use and not in a field of its own, and that is a crash rather than
     * tidiness.</b> A static field here would be initialised by whoever first touches this class, and
     * reading {@code ModelBakery.FIRE_0} from an initialiser that runs early enough starts
     * {@code ModelBakery}'s own initialiser, which reads {@code Sheets.BLOCKS_MAPPER} - a field that is
     * not published until {@code Sheets} finishes initialising. When {@code Sheets} is what led here,
     * that field is {@code null} and the game dies on the title screen with
     *
     * <pre>
     * java.lang.NullPointerException: Cannot invoke
     *   "net.minecraft.client.renderer.SpriteMapper.defaultNamespaceApply(String)"
     *   because "net.minecraft.client.resources.Sheets.BLOCKS_MAPPER" is null
     *     at ModelBakery.&lt;clinit&gt;
     *     at TextureAtlas.&lt;clinit&gt;          &lt;- our static field was being built here
     *     at RenderTypes.createMovingBlockSetup
     *     at Sheets.&lt;clinit&gt;                &lt;- still initialising
     * </pre>
     *
     * <p>Holding the set in a nested class moves the read to the first tick of the atlas, long after
     * every one of those initialisers has finished. The reference to {@code ModelBakery}'s constants is
     * kept rather than the ids being spelled out, so there is still one place that says which sprites
     * the fire is. The build has a check for this - see {@code checkMixinClassInitialisers}.
     */
    private static final class Fire {
        private static final Set<Identifier> SPRITES = Set.of(
            ModelBakery.FIRE_0.texture(),
            ModelBakery.FIRE_1.texture()
        );
    }

    private AnimationSprites() {
    }

    /** Records that {@code state} was made for {@code sprite}. Called on the way out of the maker. */
    public static void record(SpriteContents.AnimationState state, SpriteContents sprite) {
        BY_STATE.put(state, sprite);
    }

    /** The sprite a state was made for, or {@code null} for one this side never saw created. */
    public static SpriteContents spriteOf(SpriteContents.AnimationState state) {
        return BY_STATE.get(state);
    }

    /** Whether a sprite is one of the two the fire animation is made of. */
    public static boolean isFire(SpriteContents sprite) {
        return sprite != null && Fire.SPRITES.contains(sprite.name());
    }
}
