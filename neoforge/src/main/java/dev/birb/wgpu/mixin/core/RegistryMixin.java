package dev.birb.wgpu.mixin.core;

import dev.birb.wgpu.rust.WgpuNative;
import net.minecraft.core.Registry;
import net.minecraft.core.registries.BuiltInRegistries;
import net.minecraft.resources.ResourceKey;
import net.minecraft.world.level.block.Block;
import net.minecraft.world.level.block.state.BlockState;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfoReturnable;

import java.util.stream.Collectors;

/**
 * Tells the native side about every block and block state as they are registered.
 *
 * <p>This is what fills the block registry the Rust terrain baker reads - `AIR`, and the model behind
 * every state, come from it - so when it stopped firing the whole native block path went quiet: the
 * registry stayed empty, `cacheBlockStates` found nothing to bake, and a section offered for baking
 * was refused. Nothing said so, because an injection that lands on a method nobody calls is not an
 * error.
 *
 * <p>Which is exactly what had happened. 26.1 registers blocks through
 * {@code Registry.register(Registry, ResourceKey, T)}: `Blocks` builds a {@code ResourceKey} with
 * `vanillaBlockId(name)` and hands it to that overload, and NeoForge's `DeferredRegister` goes
 * through the same one. The mixin was written against `register(Registry, Identifier, T)`, the
 * overload the 1.21.1 port used - and the descriptor still existed, so it applied, and never ran.
 *
 * <p>What it hands over is the state and its name, and that is all it can hand over: the masks the
 * native baker's face test reads (`BlockFaceFlags`) need the state's occlusion shapes, and those are
 * built by {@code BlockStateBase#initCache} - which the game calls at the *end* of `Blocks`' class
 * initializer, after every block is registered. Asking for one here is a null dereference during
 * bootstrap, which is how this crashed the game once. The masks are read on the block cache thread
 * instead, as each state's key is set - see {@code Wgpu#helperSetBlockStateIndex}.
 */
@Mixin(Registry.class)
public interface RegistryMixin {

    @Inject(
            method = "register(Lnet/minecraft/core/Registry;Lnet/minecraft/resources/ResourceKey;Ljava/lang/Object;)Ljava/lang/Object;",
            at = @At("RETURN")
    )
    private static void wgpuMc$registerBlock(
            Registry<?> registry, ResourceKey<?> key, Object entry, CallbackInfoReturnable<Object> cir
    ) {
        // This injection sees every registration into every registry, so both halves are checked. The
        // id comes from the key rather than from a lookup: for the block registry the key *is* the
        // block's id, and asking the registry would answer with the default key for anything that is
        // not in it yet.
        if (registry != BuiltInRegistries.BLOCK || !(entry instanceof Block block)) {
            return;
        }

        String blockId = key.identifier().toString();

        WgpuNative.registerBlock(blockId);

        for (BlockState state : block.getStateDefinition().getPossibleStates()) {
            // 26.1 changed StateHolder#getValues() from a Map to a Stream<Property.Value<?>>,
            // and Property.Value is a record whose toString() is exactly "<name>=<valueName>".
            String stateKey = state.getValues()
                    .map(Object::toString)
                    .collect(Collectors.joining(","));
            WgpuNative.registerBlockState(state, blockId, stateKey);
        }
    }
}
