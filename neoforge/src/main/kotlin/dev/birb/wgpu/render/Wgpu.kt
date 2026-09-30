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

	@JvmStatic
	fun helperGetBlockColor(x: Int, y: Int, z: Int, tintIndex: Int): Int {
		val client = Minecraft.getInstance()
		if (client == null || client.level == null) {
			return 0xFFFFFFFF.toInt()
		}
		val level = client.level ?: return 0xFFFFFFFF.toInt()

		val pos = BlockPos(x, y, z)
		// 26.1 replaced BlockColors#getColor(state, level, pos, tintIndex) with the
		// BlockTintSource pipeline: look up the tint source for the index, then ask it
		// for the world-space colour.
		val state = level.getBlockState(pos)
		val tintSource = client.blockColors.getTintSource(state, tintIndex)
			?: return 0xFFFFFFFF.toInt()
		val color = tintSource.colorInWorld(state, level, pos)
		val r = color shr 16 and 0xFF
		val g = color shr 8 and 0xFF
		val b = color and 0xFF

		return r or (g shl 8) or (b shl 16)
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
			val r = color shr 16 and 0xFF
			val g = color shr 8 and 0xFF
			val b = color and 0xFF
			return r or (g shl 8) or (b shl 16)
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
