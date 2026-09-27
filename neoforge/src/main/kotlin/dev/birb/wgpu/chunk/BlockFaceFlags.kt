package dev.birb.wgpu.chunk

import dev.birb.wgpu.WgpuMcMod
import net.minecraft.core.BlockPos
import net.minecraft.core.Direction
import net.minecraft.world.level.EmptyBlockGetter
import net.minecraft.world.level.block.state.BlockState
import net.minecraft.world.phys.shapes.Shapes

/**
 * What a block *state* says about the faces around it, as two masks of six bits.
 *
 * These are what the native baker's face test reads, and they come from the state rather than from
 * the model on purpose: the earlier test asked whether the neighbour's model has a full-size quad on
 * the side facing us, and glass, ice, leaves, stained glass and every plant are full-cube *models*
 * that occlude **nothing** (`noOcclusion()`, or `noCollision()` for a plant - both set
 * `canOcclude = false`). So the stone placed against a glass block lost the face it shared with it,
 * and the world showed its insides wherever one of them touched anything.
 *
 * | Bits | Asked as | Read by the baker as |
 * | --- | --- | --- |
 * | occlusion | `getFaceOcclusionShape(dir) == Shapes.block()` | the neighbour covers that whole face |
 * | self-hide | `skipRendering(state, dir)` | the neighbour is the *same state*, which hides the face between them |
 *
 * Both are the game's own answers: the first is what `Block#shouldRenderFace` asks of the neighbour,
 * and the second is how glass, ice, a pane's bars and a fluid leave out the faces between two blocks
 * of their own kind.
 *
 * **When this may be asked matters.** `getFaceOcclusionShape` reads a field that
 * `BlockStateBase#initCache` fills in, and the game calls that at the *end* of `Blocks`' class
 * initializer - so for the whole of block registration a state's shapes are null, and asking earlier
 * is a null dereference during bootstrap. That is why the masks are read from the block cache
 * thread, once per state, at the moment the native side sets that state's key: [describe], called
 * from `Wgpu#helperSetBlockStateIndex`.
 */
object BlockFaceFlags {
	/** How many states have been described, for the line that says whether the masks arrived. */
	@Volatile
	var described: Int = 0
		private set

	/** How many states could not be read, reported once rather than a line per state. */
	@Volatile
	var unreadable: Int = 0
		private set

	/**
	 * The six bits of `getFaceOcclusionShape(dir) == Shapes.block()`, in `Direction.ordinal()` order.
	 *
	 * Identity is the game's own comparison: a state that occludes nothing gets `Shapes.empty()`, and
	 * every full-block occlusion shape is the one `Shapes.block()` instance - the two arrays
	 * `BlockStateBase` fills in when its cache is built.
	 */
	@JvmStatic
	fun occlusion(state: BlockState): Int {
		var mask = 0

		for (direction in Direction.values()) {
			if (state.getFaceOcclusionShape(direction) === Shapes.block()) {
				mask = mask or (1 shl direction.ordinal)
			}
		}

		return mask
	}

	/**
	 * The six bits of `skipRendering(state, dir)`, in `Direction.ordinal()` order.
	 *
	 * The state is asked with itself as the neighbour, which is the only case a per-state mask can
	 * carry: the game's implementations answer `neighborState.is(this)` for a block like glass, so
	 * what this stores is "the face between two blocks of this state", and the native side only
	 * consults it when the neighbour is the same state.
	 */
	@JvmStatic
	fun selfHide(state: BlockState): Int {
		var mask = 0

		for (direction in Direction.values()) {
			if (state.skipRendering(state, direction)) {
				mask = mask or (1 shl direction.ordinal)
			}
		}

		return mask
	}

	/**
	 * Reads one state's masks and hands them to the native side under [key], the packed state key the
	 * native registry gave it.
	 *
	 * A state that cannot be read - one whose cache the game never initialized, the failure this whole
	 * arrangement exists to avoid - is sent as zeroes, which the baker reads as "occludes nothing, hides
	 * nothing": every face of that block is drawn, which is more geometry and no hole in the world. The
	 * same failure for [shadeBrightness] reads as "does not darken", which draws *less* shadow than
	 * vanilla: the two directions are chosen so that a broken read is a picture with more of it, never a
	 * hole in it.
	 */
	@JvmStatic
	fun describe(key: Int, state: BlockState) {
		var occlusion = 0
		var selfHide = 0
		var shades = 0
		var motion = 0

		try {
			occlusion = occlusion(state)
			selfHide = selfHide(state)
			shades = shadeBrightness(state)
			motion = blocksMotion(state)
		} catch (error: Throwable) {
			if (unreadable == 0) {
				WgpuMcMod.LOGGER.warn(
					"wgpu: could not read what {} says about the faces around it; every face of it is drawn",
					state,
					error,
				)
			}

			unreadable++
		}

		dev.birb.wgpu.rust.WgpuNative.registerBlockStateFaceFlags(
			key,
			occlusion,
			selfHide,
			shades,
			motion,
		)
		described++
	}

	/**
	 * Whether this block is one a fluid flows *past*: the game's own `blocksMotion`.
	 *
	 * It has exactly one caller on the native side, and that caller is `FlowingFluid#getFlow`'s "is
	 * there fluid below the neighbour" question - the one that makes a stream's surface point at the
	 * drop it is about to fall down. The three things this side already sends all look like candidates
	 * and all answer something else: a plant does not occlude, a slab is not a full cube, and glass does
	 * not occlude either - while the question is whether the block has collision at all.
	 *
	 * The method is deprecated in 26.1 and still the one the game's own flow code calls, which is the
	 * reason to read it rather than to approximate it: an approximation that disagrees is a fluid
	 * flowing the wrong way on screen.
	 */
	@JvmStatic
	@Suppress("DEPRECATION")
	fun blocksMotion(state: BlockState): Int = if (state.blocksMotion()) 1 else 0

	/**
	 * Whether this block darkens the corners it touches: `getShadeBrightness` below `1.0`.
	 *
	 * The game's own answer, and it has to be read rather than derived. The default is
	 * `isCollisionShapeFullBlock ? 0.2 : 1.0`, so this is "does the block fill its whole block" - but the
	 * blocks that override it are the ones a player notices: glass and ice are the *same shape*, both
	 * full cubes with no occlusion, and glass answers `1.0` while ice answers `0.2`. Neither the
	 * occlusion masks above nor the model can tell those two apart, and only one of them casts a shadow
	 * in vanilla.
	 *
	 * Read with the empty level and the origin, which is what the game's own state cache does for the
	 * shape question this derives from: a state whose answer depends on where it is standing is a state
	 * either side of this bridge is guessing about.
	 */
	@JvmStatic
	fun shadeBrightness(state: BlockState): Int {
		val brightness = state.getShadeBrightness(EmptyBlockGetter.INSTANCE, BlockPos.ZERO)

		return if (brightness >= 1.0f) 0 else 1
	}

	/**
	 * Forgets the counts, for a resource reload.
	 *
	 * Every state is described again on every reload, because the native side hands every key out
	 * again - so a count that carries the previous run into this one is a number that describes
	 * neither. The masks themselves need no clearing: they are recomputed and re-sent under the same
	 * keys, and the native side drains the ones it is given at the end of the same call.
	 */
	@JvmStatic
	fun reset() {
		described = 0
		unreadable = 0
	}

	/** The line that says whether the masks reached the native side at all. */
	@JvmStatic
	fun report() {
		WgpuMcMod.LOGGER.info(
			"wgpu: {} block state(s) described for the native face test{}",
			described,
			if (unreadable == 0) "" else " ($unreadable could not be read)",
		)
	}
}