package dev.birb.wgpu.render

import dev.birb.wgpu.WgpuMcMod
import dev.birb.wgpu.chunk.BlockFaceFlags
import dev.birb.wgpu.entity.EntityState
import dev.birb.wgpu.palette.RustBlockStateAccessor
import dev.birb.wgpu.rust.WgpuNative
import net.minecraft.client.Minecraft
import net.minecraft.core.BlockPos
import net.minecraft.world.level.block.state.BlockState
import org.lwjgl.glfw.GLFW
import org.lwjgl.glfw.GLFWNativeCocoa
import org.lwjgl.glfw.GLFWNativeWayland
import org.lwjgl.glfw.GLFWNativeWin32
import org.lwjgl.glfw.GLFWNativeX11

object Wgpu {
	@JvmField
	val keyStates: HashMap<Int, Int> = HashMap()

	/**
	 * Whether the game has reached the main screen, which is when a resource reload can rebuild
	 * pipelines and entity models can be uploaded.
	 *
	 * Set by `TitleScreenMixin#init` - see there for why that is the right moment and why a render
	 * hook would not be.
	 */
	@Volatile
	private var initialized: Boolean = false

	/**
	 * Whether the native renderer has been created, which happens while the game's window is built.
	 *
	 * Kept apart from [isInitialized] on purpose: that one is about the *game* being ready, and this
	 * one about the renderer existing, which is minutes earlier. The block cache needs the renderer
	 * and a resource reload, but not the main screen - a `--quickPlaySingleplayer` launch is already
	 * loading chunks by then.
	 */
	@Volatile
	private var rendererLive: Boolean = false

	@JvmStatic
	fun isRendererLive(): Boolean {
		return rendererLive
	}

	@JvmStatic
	fun setRendererLive(live: Boolean) {
		rendererLive = live
	}

	/**
	 * The device the game is rendering with, for the work that happens outside a pass.
	 *
	 * The device is Minecraft's to create and is handed to every pass it opens, so nothing needed to
	 * keep a reference to it before; the pipeline precompile does, because it runs on its own thread
	 * while the game is still on the loading screen and has no pass to borrow one from.
	 */
	@Volatile
	private var device: Any? = null

	@JvmStatic
	fun device(): dev.birb.wgpu.backend.WgpuDevice? = device as? dev.birb.wgpu.backend.WgpuDevice

	@JvmStatic
	fun setDevice(created: dev.birb.wgpu.backend.WgpuDevice) {
		device = created
	}

	@Volatile
	private var mayInitialize: Boolean = false

	@Volatile
	private var nativeBackendProbed: Boolean = false

	private var timesTexSubImageCalled: Int = 0

	@JvmStatic
	fun isInitialized(): Boolean {
		return initialized
	}

	@JvmStatic
	fun setInitialized(initialized: Boolean) {
		this.initialized = initialized
	}

	@JvmStatic
	fun isMayInitialize(): Boolean {
		return mayInitialize
	}

	@JvmStatic
	fun setMayInitialize(mayInitialize: Boolean) {
		this.mayInitialize = mayInitialize
	}

	/**
	 * Reports whether the renderer is live.
	 *
	 * The 1.21.1 port used this hook to create the device from the title screen, because that was
	 * the first frame where a window handle existed. In 26.1 that is no longer how the renderer
	 * comes up: `WgpuBackendSelectionMixin` installs [dev.birb.wgpu.backend.WgpuBackend] as the
	 * game's GpuBackend, and `WgpuBackend#createDevice` creates the device and attaches the surface
	 * while the window is still being built. Creating a second renderer here would hand the C ABI
	 * two different `WmRenderer` pointers, so this now only reports state.
	 */
	@JvmStatic
	fun probeNativeBackendOnce() {
		if (nativeBackendProbed) {
			return
		}
		nativeBackendProbed = true

		WgpuMcMod.LOGGER.info("wgpu-mc renderer active: {}", runCatching { WgpuNative.getBackend() }.getOrElse { "not initialised" })
	}
	@JvmStatic
	fun getTimesTexSubImageCalled(): Int {
		return timesTexSubImageCalled
	}

	/**
	 * Offers the RenderDoc capture layer to the renderer, **quietly when it is not installed**.
	 *
	 * RenderDoc is a developer's tool: it is present on a machine where somebody is profiling, and absent
	 * on every other machine - including an ordinary player's and this project's own development
	 * environment. `System.loadLibrary` is the all-or-nothing version of "load it if it is there", so an
	 * absent library is an `UnsatisfiedLinkError` and nothing else, and logging that as a warning with a
	 * full stack trace made every single launch look broken.
	 *
	 * So the search is done first and the load is only attempted when it can succeed: the library is
	 * looked for on `java.library.path`, and its absence is a debug line rather than a warning. When it
	 * *is* found, finding it is worth a line of its own - that is the state a capture is possible in, and
	 * knowing which state a run was in is the whole reason this hook reports at all.
	 *
	 * A library that is present but refuses to load is still a warning with the exception attached: that
	 * is not an absence, it is a broken install, and it is the one case where the stack trace is the
	 * useful part.
	 */
	@JvmStatic
	fun linkRenderDoc() {
		val path = System.getProperty("java.library.path").orEmpty()
			.split(java.io.File.pathSeparatorChar)
			.filter { it.isNotBlank() }

		val found = path.firstOrNull { directory ->
			val name = if (java.io.File.separatorChar == '\\') "renderdoc.dll" else "librenderdoc.so"
			java.io.File(directory, name).isFile
		}

		if (found == null) {
			// The ordinary case, and not worth a warning - see above. `debug` rather than nothing at all,
			// so a log with debug on still says why there is no capture layer.
			WgpuMcMod.LOGGER.debug(
				"wgpu: RenderDoc is not installed; the capture layer is off (looked in {} director{} on " +
					"java.library.path)",
				path.size,
				if (path.size == 1) "y" else "ies",
			)
			return
		}

		try {
			System.loadLibrary("renderdoc")
			WgpuMcMod.LOGGER.info("wgpu: the RenderDoc capture layer is loaded from {}", found)
		} catch (e: UnsatisfiedLinkError) {
			// Present and refusing - a broken install, not an absence, and the trace is the useful part.
			WgpuMcMod.LOGGER.warn("wgpu: RenderDoc is installed at {} but could not be loaded", found, e)
		}
	}

	@JvmStatic
	fun rustPanic(message: String) {
		WgpuMcMod.LOGGER.error(message)
		throw IllegalStateException(message)
	}

	@JvmStatic
	fun rustDebug(message: String) {
		WgpuMcMod.LOGGER.info("[Engine] {}", message)
	}

	@JvmStatic
	fun helperSetBlockStateIndex(state: Any?, blockstateKey: Int) {
		if (state is RustBlockStateAccessor) {
			state.`wgpu_mc$setRustBlockStateIndex`(blockstateKey)
		}

		// What the state says about the faces around it goes over with its key, and this is the only
		// moment it can be read: the native side computes the key here, seconds after the game is up,
		// while the masks need the occlusion shapes that `BlockStateBase#initCache` fills in - and the
		// game calls that at the end of `Blocks`' class initializer, long after every block was
		// registered. Reading them at registration is a null dereference during bootstrap.
		if (state is BlockState) {
			BlockFaceFlags.describe(blockstateKey, state)
		}
	}

	@JvmStatic
	fun helperSetPartIndex(entity: String, part: String, index: Int) {
		EntityState.matrixIndices.computeIfAbsent(entity) { HashMap() }[part] = index
	}

	/**
	 * The position the block-tint callback is asked about, reused per bake thread.
	 *
	 * **A fresh `BlockPos` per call was one allocation per tint**, and there were five or six calls per
	 * tinted block: a grass block is one `up` face plus four side overlays, all `tintindex: 0`, and leaves
	 * are six faces of it. The Rust baker now asks once per colour a block needs (see `bake_layers`), so
	 * what is left here is one allocation per *block* per bake thread - which is still thousands a second,
	 * and is this.
	 *
	 * A mutable position rests on one assumption: **a tint source reads the position and does not keep
	 * it.** Every source the game ships does - `GrassColor` and its neighbours ask `level.getBiome(pos)`
	 * and return - and a source that kept the reference would see the position move under it, which is the
	 * failure to look for if a modded one ever misbehaves here. Thread-local because the bakes run on a
	 * pool: one position shared between two of them would be a wrong colour, not a race that throws.
	 */
	private val tintPos: ThreadLocal<BlockPos.MutableBlockPos> =
		ThreadLocal.withInitial { BlockPos.MutableBlockPos() }

	/**
	 * The answer to [helperGetSectionTints] for a section with no colour to give: no air, no tinted state,
	 * or no level yet. Shared rather than allocated, and **read-only**: the Rust side copies what it reads
	 * out of it before the next bake can ask again.
	 */
	private val EMPTY_TINTS = LongArray(0)

	/**
	 * How many tint indices are probed per state.
	 *
	 * Models use `tintindex: 0` and a few use 1 or 2; the game keeps a state's sources in an array indexed by
	 * the tint index, so a state with one source answers null from index 1 on and the probe stops paying
	 * after the first.
	 */
	private const val TINT_INDEX_PROBES = 4

	/**
	 * The packing every tint answer uses: the game's `ARGB` turned into the `BGR` order the Rust side
	 * multiplies into a vertex.
	 *
	 * **One function, because two of them drifting is exactly what went wrong once.** The three helpers that
	 * answer a colour - the two single-call ones and the two bulk tables - all have to agree with
	 * `scale_rgb` on the other side, and a bulk table that stored `colorInWorld`'s `ARGB` verbatim read as a
	 * red-blue swap on screen: `2b4323` of grass came out `23432b`, and the counters could not see it - they
	 * count *keys found*, and every key was found.
	 */
	private fun packTint(color: Int): Int {
		val r = color shr 16 and 0xFF
		val g = color shr 8 and 0xFF
		val b = color and 0xFF

		return r or (g shl 8) or (b shl 16)
	}

	/**
	 * The colour a block's face is tinted by at a position, asked of the game itself.
	 *
	 * The game's tint is a property of the **position** - grass follows the biome under the block - so
	 * nothing here is cached: the Rust baker asks once per colour a block needs and keeps the answer for
	 * that block, which is exactly as far as it stays true. What is left per call is the chunk lookup and
	 * palette decode in `level.getBlockState`, the tint source for the index, and the biome in
	 * `colorInWorld`.
	 */
	@JvmStatic
	fun helperGetBlockColor(x: Int, y: Int, z: Int, tintIndex: Int): Int {
		val client = Minecraft.getInstance()
		if (client == null || client.level == null) {
			return 0xFFFFFFFF.toInt()
		}
		val level = client.level ?: return 0xFFFFFFFF.toInt()

		val pos = tintPos.get().set(x, y, z)
		// 26.1 replaced BlockColors#getColor(state, level, pos, tintIndex) with the
		// BlockTintSource pipeline: look up the tint source for the index, then ask it
		// for the world-space colour.
		val state = level.getBlockState(pos)
		val tintSource = client.blockColors.getTintSource(state, tintIndex)
			?: return 0xFFFFFFFF.toInt()
		val color = tintSource.colorInWorld(state, level, pos)

		return packTint(color)
	}

	/**
	 * Every biome tint one section needs, in **one** call, packed one colour per `Long`.
	 *
	 * The layout is shared with `wgpu-mc-jni`'s `TintHelpers::section_tints`, which unpacks it:
	 *
	 * ```text
	 * bits 63..36  the position in the section, in Minecraft's own storage order
	 *              (`x | z << 4 | y << 8`, which is the order the Rust baker walks blocks in)
	 * bits 35..32  the model's tint index
	 * bits 31..0   the colour, in the same packing `helperGetBlockColor` returns
	 * ```
	 *
	 * **Why a table rather than a call per face.** A bake asks for a colour per tinted face; a grass block
	 * is five of those (an `up` face and four side overlays) and a leaf block six, and every one of them used
	 * to be a JNI call *and* the `level.getBlockState` + biome lookup behind it. The Rust side memoizes what
	 * it asks for, and this answers all of it at once: the walk below is per position either way, but it
	 * happens once per section instead of once per call, on the same thread, against the section's own
	 * palette rather than through a `BlockPos` each time.
	 *
	 * Sections that cannot have a tint cost nothing: `hasOnlyAir` and `maybeHas` are both answered from the
	 * section's palette before a single state is read, and an empty answer means the baker draws those faces
	 * white - which is what the game's own `getTintSource` returning null means.
	 */
	@JvmStatic
	fun helperGetSectionTints(x: Int, y: Int, z: Int): LongArray {
		val client = Minecraft.getInstance()
		if (client == null || client.level == null) {
			return EMPTY_TINTS
		}
		val level = client.level ?: return EMPTY_TINTS

		val section = level.getChunk(x, z).getSection(y - level.minSectionY)
		if (section == null || section.hasOnlyAir()) {
			return EMPTY_TINTS
		}

		// Asked of the palette before a block is read: a section of stone and a section of air are the two
		// commonest cases in the world and neither has a state a tint source answers for.
		if (!section.maybeHas { state -> tintedIndices(client, state).isNotEmpty() }) {
			return EMPTY_TINTS
		}

		// Per *state*, not per position: a section holds a handful of distinct states and thousands of
		// positions, and the tint sources of a state do not depend on where it is.
		val indices = HashMap<BlockState, IntArray>()
		var out = LongArray(0)
		var count = 0

		for (index in 0 until 4096) {
			val state = section.getBlockState(index and 15, index shr 8, index shr 4 and 15)
			val wanted = indices.getOrPut(state) { tintedIndices(client, state) }

			for (tint in wanted) {
				val tintSource = client.blockColors.getTintSource(state, tint) ?: continue
				val color = tintSource.colorInWorld(state, level, tintPos.get().set(
					(x shl 4) + (index and 15),
					(y shl 4) + (index shr 8),
					(z shl 4) + (index shr 4 and 15),
				))

				if (count == out.size) {
					out = out.copyOf(if (count == 0) 64 else count * 2)
				}

				out[count++] =
					(index.toLong() shl 36) or (tint.toLong() shl 32) or
						(packTint(color).toLong() and 0xFFFFFFFFL)
			}
		}

		return if (count == out.size) out else out.copyOf(count)
	}

	/**
	 * Which tint indices a state has a source for, in the order they are asked.
	 *
	 * `BlockColors#getTintSource` answers null for an index a state has no source at, which is the same
	 * answer the game's own model baking gets, so a state with one source stops after one probe - and the
	 * baker draws a face white exactly where the game would.
	 */
	private fun tintedIndices(client: Minecraft, state: BlockState): IntArray {
		var found = 0
		val indices = IntArray(TINT_INDEX_PROBES)

		for (tint in 0 until TINT_INDEX_PROBES) {
			if (client.blockColors.getTintSource(state, tint) != null) {
				indices[found++] = tint
			}
		}

		return indices.copyOf(found)
	}

	/**
	 * Every fluid tint one section needs, in **one** call, packed one colour per `Long`.
	 *
	 * The same shape as [helperGetSectionTints] and for the same reason, with one difference that decides how
	 * it is written: a fluid tint is asked **per block** rather than per face (`bake_fluid_faces_with` asks
	 * once and gives the colour to every face of that block), so the walk here is what the single-call path
	 * already did per block - a `getFluidState`, the fluid model's tint source, and the biome behind it.
	 *
	 * ```text
	 * bits 63..36  the position in the section, in Minecraft's own storage order
	 * bits 31..0   the colour, in the same packing `helperGetFluidColor` returns
	 * ```
	 *
	 * It is a second call rather than more entries in the first one because the two are answered for
	 * different reasons: this one is only ever made for a section the baker already knows holds water, and
	 * the block table is only ever made for a section with a tinted block face. A section of grass with no
	 * water pays for neither the fluid walk nor its colours.
	 */
	@JvmStatic
	fun helperGetSectionFluidTints(x: Int, y: Int, z: Int): LongArray {
		val client = Minecraft.getInstance()
		if (client == null || client.level == null) {
			return EMPTY_TINTS
		}
		val level = client.level ?: return EMPTY_TINTS

		val section = level.getChunk(x, z).getSection(y - level.minSectionY)
		if (section == null || section.hasOnlyAir()) {
			return EMPTY_TINTS
		}

		val models = client.modelManager.fluidStateModelSet
		var out = LongArray(0)
		var count = 0

		for (index in 0 until 4096) {
			// The fluid is read off the *block* state that is already in hand rather than through a second
			// `getFluidState` per position: one palette read answers both questions, which is the difference
			// between this walk being cheaper than the per-block calls it replaces and being another one.
			val state = section.getBlockState(index and 15, index shr 8, index shr 4 and 15)
			val fluidState = state.fluidState

			if (fluidState.isEmpty) {
				continue
			}

			val color = try {
				val tintSource = models.get(fluidState).fluidTintSource() ?: continue
				val at = tintPos.get().set(
					(x shl 4) + (index and 15),
					(y shl 4) + (index shr 8),
					(z shl 4) + (index shr 4 and 15),
				)

				tintSource.colorInWorld(fluidState, state, level, at)
			} catch (failure: Throwable) {
				// Wrapped like every other read this mod makes of the game, and warned once for the life of
				// the process: a fluid whose tint cannot be read keeps the default colour rather than
				// stopping a client.
				if (!warnedFluidTint) {
					warnedFluidTint = true
					WgpuMcMod.LOGGER.warn(
						"wgpu: could not read a fluid's tint; water keeps the default colour",
						failure,
					)
				}

				continue
			}

			if (count == out.size) {
				out = out.copyOf(if (count == 0) 64 else count * 2)
			}

			out[count++] = (index.toLong() shl 36) or (packTint(color).toLong() and 0xFFFFFFFFL)
		}

		return if (count == out.size) out else out.copyOf(count)
	}

	/**
	 * The colour a **fluid** is tinted by at a position, which is a different question from
	 * [helperGetBlockColor] and one that function cannot answer.
	 *
	 * `FluidRenderer#tesselate` asks the *fluid* model rather than the block model:
	 *
	 * ```java
	 * FluidModel model = this.fluidModels.get(fluidState);
	 * int tintColor = model.fluidTintSource() != null
	 *     ? model.fluidTintSource().colorInWorld(fluidState, blockState, level, pos)
	 *     : -1;
	 * ```
	 *
	 * and for water that source is NeoForge's `FluidTintSources.water()`, whose answer is the **biome's**
	 * water colour - so an ocean, a swamp and a cold river are three different colours in the game. The
	 * native side used one constant for all three, which is why water did not follow the biome.
	 *
	 * The `-1` above is the game's own "no tint" and is passed through unaltered: white in the packing
	 * the native side reads, so a fluid with no tint source draws untinted rather than black. Lava is
	 * exactly that case - `FluidStateModelSet` builds the lava model with a null tint source - and the
	 * native side does not ask for lava in the first place.
	 *
	 * Packed the way the native side reads a tint: **red in the low byte**, the same packing
	 * [helperGetBlockColor] returns.
	 */
	@JvmStatic
	fun helperGetFluidColor(x: Int, y: Int, z: Int): Int {
		val client = Minecraft.getInstance()
		if (client == null || client.level == null) {
			return 0xFFFFFFFF.toInt()
		}
		val level = client.level ?: return 0xFFFFFFFF.toInt()

		val pos = BlockPos(x, y, z)

		try {
			val fluidState = level.getFluidState(pos)
			val model = client.modelManager.fluidStateModelSet.get(fluidState)
			val tintSource = model.fluidTintSource() ?: return 0xFFFFFFFF.toInt()
			val color = tintSource.colorInWorld(fluidState, level.getBlockState(pos), level, pos)

			return packTint(color)
		} catch (failure: Throwable) {
			// Wrapped like every other read this mod makes of the game: this runs on the bake thread
			// while a world loads, and a failure here is water drawn with the default colour rather
			// than a client that stops.
			if (!warnedFluidTint) {
				warnedFluidTint = true
				WgpuMcMod.LOGGER.warn(
					"wgpu: could not read a fluid's tint; water keeps the default colour",
					failure,
				)
			}

			return 0xFFFFFFFF.toInt()
		}
	}

	/** One warning for the fluid tint rather than one per fluid block, for the life of the process. */
	@Volatile
	private var warnedFluidTint = false
}
