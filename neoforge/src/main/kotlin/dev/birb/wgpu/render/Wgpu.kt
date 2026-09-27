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

	@JvmStatic
	fun linkRenderDoc() {
		try {
			System.loadLibrary("renderdoc")
		} catch (e: UnsatisfiedLinkError) {
			WgpuMcMod.LOGGER.warn("Error while loading RenderDoc", e)
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
}
