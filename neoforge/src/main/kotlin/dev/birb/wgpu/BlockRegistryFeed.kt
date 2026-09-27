package dev.birb.wgpu

import dev.birb.wgpu.rust.WgpuNative

/**
 * The block registry as the game offered it, kept so that a **resource reload** can offer it again.
 *
 * The native side drops what it was told at the end of every `cacheBlockStates`: the states cross as
 * JNI global references and are released as soon as their keys have been handed out, which is the
 * deliberate price of not holding thirty thousand of them for the life of the session. That is the
 * right trade for a registry, which is built once and never changes - and the wrong one for a reload,
 * where the pack behind every model and every texture can be a different pack, so the models have to be
 * baked again, and baking a `multipart` model needs the states: which pieces a fence with *these*
 * connections is made of is a question about the state.
 *
 * The order is the registration order, recorded rather than re-derived from `BuiltInRegistries.BLOCK`
 * on the way back. The native side names a block by the index it was baked at, those indices are in
 * every state's stored key and in every baked vertex, and a replay in a different order would shuffle
 * all of them - so the order is kept where it is known for certain rather than assumed of a registry
 * iteration.
 *
 * What is stored is one reference per state object, which the block registry itself holds forever
 * anyway: these are the same `BlockState` singletons the game is drawing with, so the list costs the
 * three arrays and not the states.
 */
object BlockRegistryFeed {
	/** Every block id, in the order the game registered them. */
	private val blockIds = ArrayList<String>()

	/** Every state's block id, in the same order as [states]. */
	private val stateBlockIds = ArrayList<String>()

	/** Every registered state, in registration order. */
	private val states = ArrayList<Any>()

	/** Every state's properties as `"<name>=<value>[,...]"`, which is the key the native side matches. */
	private val stateKeys = ArrayList<String>()

	/** Called by `RegistryMixin` once per block, as the game registers it. */
	@JvmStatic
	fun registerBlock(blockId: String) {
		blockIds.add(blockId)

		WgpuNative.registerBlock(blockId)
	}

	/** Called by `RegistryMixin` once per state of that block. */
	@JvmStatic
	fun registerState(state: Any, blockId: String, stateKey: String) {
		stateBlockIds.add(blockId)
		states.add(state)
		stateKeys.add(stateKey)

		WgpuNative.registerBlockState(state, blockId, stateKey)
	}

	/**
	 * Offers the whole registry again, for a reload.
	 *
	 * Paired with `WgpuNative.beginBlockReload`, which forgets the native side's copy first: without
	 * that the blocks would be offered twice and every model baked twice, and the state list the native
	 * side clears is the one this is putting back.
	 *
	 * Both halves are replayed in their original order and in one pass, because the native side pairs
	 * them positionally: block ids are what it bakes `variants` models from, states are what it bakes
	 * `multipart` ones from, and a state's `blockId` is which of those blocks it belongs to.
	 */
	@JvmStatic
	fun replay() {
		for (blockId in blockIds) {
			WgpuNative.registerBlock(blockId)
		}

		for (index in states.indices) {
			WgpuNative.registerBlockState(states[index], stateBlockIds[index], stateKeys[index])
		}

		WgpuMcMod.LOGGER.info(
			"wgpu: offered the block registry again for the reload: {} block(s), {} state(s)",
			blockIds.size,
			states.size,
		)
	}
}
