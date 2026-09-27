package dev.birb.wgpu

import dev.birb.wgpu.chunk.BlockFaceFlags
import dev.birb.wgpu.render.Wgpu
import dev.birb.wgpu.rust.WgpuNative
import net.minecraft.client.Minecraft
import net.neoforged.bus.api.SubscribeEvent
import net.neoforged.fml.common.EventBusSubscriber
import net.neoforged.neoforge.client.event.ClientTickEvent
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Builds the native block registry, once, as soon as the game is ready for it.
 *
 * The Rust side needs this for anything that turns a block state into geometry: `AIR`, and the model
 * behind every state, are both built from it. Two things have to be true first, and neither is
 * obvious from the outside:
 *
 *  - **the native renderer has to exist** ([Wgpu.isRendererLive]), because the registry is built
 *    inside it; and
 *  - **a resource reload has to have happened**, because the bake reads the block atlas that the
 *    reload stitches. Asking for the registry any earlier panics inside the native library - on a
 *    `#[jni_fn]` frame, which means the JVM aborts - so the trigger waits a few seconds after the
 *    first reload rather than racing it.
 *
 * It used to be triggered from the title screen's first frame, which is a place a launch does not
 * always visit: `--quickPlaySingleplayer` goes from the loading screen into the world without ever
 * rendering one, so the registry was never built and the first section offered for baking aborted
 * the JVM. Client ticks are reached on every launch, so the trigger lives here now, and the title
 * screen calls in rather than starting its own thread - which is what keeps this once-only.
 */
@EventBusSubscriber(modid = WgpuMcMod.MOD_ID)
object BlockCache {
	/**
	 * How long to wait after a reload before asking for the registry.
	 *
	 * The atlas is stitched by a reload listener of the game's, and this mod's listener may run
	 * before it; five seconds is far longer than the stitch takes and costs nothing, since the work
	 * happens on its own thread either way.
	 */
	private const val TICKS_AFTER_RELOAD = 100

	private val started = AtomicBoolean(false)

	/** Ticks since the last reload, or -1 while none has happened. */
	@Volatile
	private var sinceReload = -1

	/** Called by the resource-reload listener. */
	@JvmStatic
	fun resourceReloaded() {
		sinceReload = 0
	}

	/**
	 * Starts the cache once everything it needs is there.
	 *
	 * Safe to call from anywhere, as often as anything likes: the work happens on its own thread, and
	 * only the first caller that finds the game ready gets to start it.
	 */
	@JvmStatic
	fun start() {
		if (!Wgpu.isRendererLive()) return
		if (sinceReload < TICKS_AFTER_RELOAD) return
		if (!started.compareAndSet(false, true)) return

		val thread = Thread(Runnable(::cacheAndRebuild), "wgpu-mc block cache")
		thread.isDaemon = true
		thread.contextClassLoader = BlockCache::class.java.classLoader
		thread.start()

		WgpuMcMod.LOGGER.info("wgpu: caching block states for the native side")
	}

	/**
	 * Builds the registry, and then has Minecraft build its meshes again.
	 *
	 * The second half is not an optimisation, it is what makes the section feed work at all on a
	 * launch that goes straight into a world: the cache lands five seconds after the resource reload,
	 * which is *after* the chunks around the player have been meshed - and the feed drops an offer it
	 * cannot bake (`RustChunkBake.bake` returns early when the registry is not there yet, because a
	 * bake with no registry and no "air" would be a copy of nothing). Minecraft only offers a section
	 * again when something makes it stale, so those first sections were never offered a second time:
	 * the arena stayed at a handful of sections and the terrain pass drew an empty world, for a whole
	 * session, with nothing in the log about it.
	 *
	 * `allChanged` is what the game itself calls when the whole world has to be meshed again, and this
	 * is a launch-time cost of a second or two for the chunks that are loaded. It has to run on the
	 * render thread, which is where the meshes are built.
	 */
	private fun cacheAndRebuild() {
		WgpuNative.cacheBlockStates()

		// What every state says about the faces around it went over during that call - the native side
		// asks for it as it hands out each state's key, because that is the first moment the shapes can
		// be read at all (see `BlockFaceFlags`). This is the line that says the masks arrived: without
		// them the baker draws every face of every block.
		BlockFaceFlags.report()

		val client = Minecraft.getInstance()
		WgpuMcMod.LOGGER.info("wgpu: block states cached; asking Minecraft to mesh its sections again so the feed sees them")

		client.execute {
			client.levelRenderer.allChanged()
		}
	}

	@SubscribeEvent
	@JvmStatic
	fun onClientTick(event: ClientTickEvent.Post) {
		val ticks = sinceReload
		if (ticks >= 0) {
			sinceReload = ticks + 1
		}
		start()
	}
}