package dev.birb.wgpu

import dev.birb.wgpu.chunk.BlockFaceFlags
import dev.birb.wgpu.chunk.RustChunkBake
import dev.birb.wgpu.backend.bindAtlasToTerrainPass
import dev.birb.wgpu.render.Wgpu
import dev.birb.wgpu.rust.WgpuNative
import net.minecraft.client.Minecraft
import net.minecraft.data.AtlasIds
import net.neoforged.bus.api.SubscribeEvent
import net.neoforged.fml.common.EventBusSubscriber
import net.neoforged.neoforge.client.event.ClientTickEvent
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger

/**
 * Builds the native block registry, and rebuilds it whenever the game reads its resources again.
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
 * screen calls in rather than starting its own thread - which is what keeps this once-per-reload.
 *
 * ## Every reload, not just the first
 *
 * The registry is a function of the resource pack: which sprite a face samples, what that sprite's own
 * pixels say about the pass it belongs to, and which rectangles the game's animated sprites sit in are
 * all read out of the pack while a model is baked. So a reload - F3+T, a pack switched in the options,
 * `DebugReload`'s marker - has to bake the models again, against the pack that has just been read, and
 * has to have every section meshed again, because the vertices of the last bake hold the last pack's
 * coordinates.
 *
 * What that costs is the same few seconds it costs at launch, on this object's own thread, once per
 * reload; what it replaces was a renderer that went on drawing the launch pack for the rest of the
 * session while the game's own textures, models and atlases all reloaded around it. See
 * [cacheAndRebuild] for the sequence and `WgpuNative.beginBlockReload` for what is forgotten first.
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

	/** How many reloads the game has announced, from the resource-reload listener. */
	private val reloads = AtomicInteger(0)

	/** The reload the cache has caught up with. Zero until the first one is done. */
	private val handled = AtomicInteger(0)

	/**
	 * Whether a bake is running.
	 *
	 * What keeps two of them off the same atlas: a reload that lands while the previous one is still
	 * baking is left for the next tick rather than started beside it, and the tick sees it because
	 * [handled] has not caught up with [reloads] yet.
	 */
	private val baking = AtomicBoolean(false)

	/** Ticks since the last reload, or -1 while none has happened. */
	@Volatile
	private var sinceReload = -1

	/**
	 * Called by the resource-reload listener, on every reload.
	 *
	 * A counter rather than a flag, because a reload is not one event: the game announces one, this
	 * side spends a few seconds reading the new pack, and a second reload can arrive in the middle of
	 * that. What has to survive that is which reload the work in flight belongs to.
	 */
	@JvmStatic
	fun resourceReloaded() {
		sinceReload = 0
		reloads.incrementAndGet()
	}

	/**
	 * Why the block models are being baked.
	 *
	 * The three differ in what has to be forgotten first, and in nothing else: a bake always rebuilds
	 * every model against the current atlas, and always has the world meshed again afterwards.
	 */
	private enum class Bake {
		/** The first one, at launch: the game's own registrations are already waiting on the native side. */
		First,

		/** A resource reload: the pack behind every model and every sprite changed. */
		Reload,

		/**
		 * A setting that is written into the models changed - today the animated-texture switch. The
		 * pack did not, so the atlas and the game's sprite rectangles stay exactly as they are.
		 */
		Setting,
	}

	/** Whether a setting that is baked into the block models has been applied. See [blockTexturesChanged]. */
	private val pendingRebake = AtomicBoolean(false)

	/**
	 * Catches the cache up with the newest reload, once everything it needs is there.
	 *
	 * Safe to call from anywhere, as often as anything likes: the work happens on its own thread, one
	 * reload at a time, and only the caller that claims [baking] starts it.
	 *
	 * The wait is for the *game's* atlas, not this side's: a reload stitches it on a background thread
	 * and this mod's listener may run before that listener does, so reading it immediately is reading
	 * the previous pack's - or a closed texture. Five seconds is far longer than the stitch takes and
	 * costs nothing, since the work happens on its own thread either way.
	 */
	@JvmStatic
	fun start() {
		if (!Wgpu.isRendererLive()) return
		if (sinceReload < TICKS_AFTER_RELOAD) return

		val reload = reloads.get()
		if (reload == 0) return
		if (handled.get() == reload) return

		// The claim comes first, and `handled` is moved only by the caller that gets it: a reload that
		// arrives while the previous one is still baking has to be *left* for the next tick, and a
		// caller that marked it handled on the way out would leave it unhandled for good.
		if (!baking.compareAndSet(false, true)) return

		handled.set(reload)

		// The first reload is the one that builds the block registry out of the registrations the game
		// made while it was starting. Every reload after it *rebuilds* that registry against the pack
		// that has just been read, and it needs two things the first one did not: the native side has to
		// forget the registry it has (it drops it at the end of every bake), and this side has to offer
		// it again from the copy `BlockRegistryFeed` kept. See `cacheAndRebuild`.
		val mode = if (reload == 1) Bake.First else Bake.Reload

		try {
			// Registered before the thread starts, on the thread that owns the atlas: the native side
			// drains this queue while it bakes, and a bake that ran first would bake every animated face
			// with the wrong coordinates and then never be told. See `registerAtlasSprites`.
			registerAtlasSprites()

			// The other half of the same thing, and from the same stitch: *where* those sprites are was
			// just sent, and this is *which texture* the pass has to sample to see the game's own copy of
			// them. See `bindBlockAtlas`.
			bindBlockAtlas()
		} catch (failure: Throwable) {
			// The two above already wrap themselves; this is for anything outside them, and the point is
			// that a failed reload leaves the renderer drawing the pack it had rather than a thread that
			// never starts and a `baking` flag that never clears.
			WgpuMcMod.LOGGER.warn("wgpu: the reload was not handed over: {}", failure.toString())
			baking.set(false)
			return
		}

		startBake(mode)
	}

	/**
	 * Starts the bake, on its own thread.
	 *
	 * [baking] has already been claimed by the caller - the reload path claims it before it hands the
	 * atlas over, and [blockTexturesChanged] is reached from the tick that claims it - so this is only
	 * the thread, and the release of the claim when it is done.
	 */
	private fun startBake(mode: Bake) {
		val thread = Thread(
			{
				try {
					cacheAndRebuild(mode)
				} finally {
					baking.set(false)
				}
			},
			when (mode) {
				Bake.First -> "wgpu-mc block cache"
				Bake.Reload -> "wgpu-mc block reload"
				Bake.Setting -> "wgpu-mc block rebake"
			},
		)
		thread.isDaemon = true
		thread.contextClassLoader = BlockCache::class.java.classLoader
		thread.start()

		WgpuMcMod.LOGGER.info(
			"wgpu: caching block states for the native side{}",
			when (mode) {
				Bake.First -> ""
				Bake.Reload -> " (resource reload)"
				Bake.Setting -> " (a baked setting changed)"
			},
		)
	}

	/**
	 * Which atlas to ask the game for, and the one thing about it that is not obvious.
	 *
	 * **26.1's `AtlasManager` has two id spaces, and `getAtlasOrThrow` takes the definition one.**
	 * `AtlasConfig` carries both: a *texture* id (`TextureAtlas.LOCATION_BLOCKS`,
	 * `minecraft:textures/atlas/blocks.png` - the atlas as a texture, which is what it is registered
	 * under in the `TextureManager`) and a *definition* id (`AtlasIds.BLOCKS`, `minecraft:blocks` - the
	 * directory of sprite sources the atlas is stitched from). The manager keeps two maps,
	 * `atlasByTexture` and `atlasById`, and `getAtlasOrThrow` reads `atlasById`:
	 *
	 * ```java
	 * public TextureAtlas getAtlasOrThrow(Identifier atlasId) {
	 *     AtlasEntry atlasEntry = this.atlasById.get(atlasId);
	 *     if (atlasEntry == null) throw new IllegalArgumentException("Invalid atlas id: " + atlasId);
	 * ```
	 *
	 * so the texture id throws - and it threw for both callers below, which is why nothing animated and
	 * no sprite rectangle had ever reached the native side. It is a `warn` and not a launch failure
	 * because both callers wrap themselves, which is exactly why it went unnoticed for a whole round of
	 * work: the atlas simply kept the frozen copy of every sprite.
	 */
	private val BLOCKS_ATLAS = AtlasIds.BLOCKS

	/**
	 * Hands the native side every sprite of the block atlas: **where it is, and the layer it is in**.
	 *
	 * Both are things only the game knows. The rectangle is the game's own atlas layout - the native
	 * side packs its sprites into an atlas of its own, at coordinates of its own - and it is what makes
	 * an animated texture animate: the game animates its atlas by rendering the due frame into it, so a
	 * face baked with *these* coordinates samples the game's atlas and moves with it, instead of being
	 * frozen at whatever frame was copied. The layer is the game's reading of the sprite's transparency
	 * (`Transparency#hasTransparent/hasTranslucent`), where the native side otherwise guesses from the
	 * pixels and gets a sprite that is part opaque and part cutout wrong.
	 *
	 * Everything is wrapped: this runs on the render thread during startup, and a failure here is a
	 * sprite registered without its rectangle - which is exactly how the renderer behaved before this
	 * existed - rather than a launch that does not finish.
	 */
	private fun registerAtlasSprites() {
		try {
			val atlas = Minecraft.getInstance().atlasManager.getAtlasOrThrow(BLOCKS_ATLAS)

			var registered = 0

			for ((name, sprite) in atlas.textures) {
				val transparency = sprite.transparency()

				// Partly transparent pixels blend, fully transparent ones are cut out, neither is
				// opaque. These are the native side's own three layers, named on that side.
				val layer = when {
					transparency.hasTranslucent -> WgpuNative.LAYER_TRANSPARENT
					transparency.hasTransparent -> WgpuNative.LAYER_CUTOUT
					else -> WgpuNative.LAYER_SOLID
				}

				WgpuNative.registerSprite(
					name.toString(),
					sprite.u0,
					sprite.v0,
					sprite.u1,
					sprite.v1,
					layer,
				)
				registered++
			}

			WgpuMcMod.LOGGER.info("wgpu: registered {} sprite(s) of the block atlas", registered)
		} catch (failure: Throwable) {
			WgpuMcMod.LOGGER.warn("wgpu: the block atlas's sprites were not registered: {}", failure.toString())
		}
	}

	/**
	 * Hands the native side the game's own block atlas texture: the half of an animated texture that
	 * [`registerAtlasSprites`] is no use without.
	 *
	 * The rectangles above say *where* each sprite is in the game's atlas; this says *which texture*
	 * the terrain pass has to sample to see it. Both have to come from the same stitch - the game
	 * re-stitches its atlas on every resource reload, and a rectangle from one atlas read against
	 * another is a face with the wrong texture on it, where a face that was never sent to the game's
	 * atlas is only the frozen one this renderer has always drawn.
	 *
	 * Wrapped like the registration above, and for the same reason: this runs on the render thread
	 * during startup, and a failure here has to leave the renderer drawing what it drew before rather
	 * than take the launch down.
	 */
	private fun bindBlockAtlas() {
		try {
			val atlas = Minecraft.getInstance().atlasManager.getAtlasOrThrow(BLOCKS_ATLAS)
			val texture = atlas.texture

			texture.bindAtlasToTerrainPass()

			WgpuMcMod.LOGGER.info(
				"wgpu: bound the block atlas texture to the terrain pass ({}x{}, {} mip level(s))",
				texture.getWidth(0),
				texture.getHeight(0),
				texture.mipLevels,
			)
		} catch (failure: Throwable) {
			WgpuMcMod.LOGGER.warn("wgpu: the block atlas texture was not bound to the terrain pass: {}", failure.toString())
		}
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
	private fun cacheAndRebuild(mode: Bake) {
		// A reload starts here rather than on the tick that noticed it, and that is deliberate: this
		// thread is about to do nothing but read the new pack, and every moment between the clear below
		// and the bake that follows is a moment a section baked by the feed has no atlas to look its
		// sprites up in. Two calls on this thread rather than one on each is that window at its
		// shortest.
		//
		// The first bake is the exception in the other direction: the game's own registrations are
		// already queued on the native side, and forgetting them would leave nothing to bake.
		if (mode != Bake.First) {
			// `reload` says whether the pack behind the models changed as well: that is what the block
			// atlas is emptied for. A *setting* that only changes how the models are baked - the
			// animated-texture switch - leaves the atlas alone, and with it the game's rectangles for
			// the animated sprites, which a second registration would have to be asked for again.
			WgpuNative.beginBlockBake(mode == Bake.Reload)
			BlockFaceFlags.reset()
			BlockRegistryFeed.replay()
		}

		WgpuNative.cacheBlockStates()

		// What every state says about the faces around it went over during that call - the native side
		// asks for it as it hands out each state's key, because that is the first moment the shapes can
		// be read at all (see `BlockFaceFlags`). This is the line that says the masks arrived: without
		// them the baker draws every face of every block.
		BlockFaceFlags.report()

		// The two ways a block was baked into nothing, which the native side counts as it bakes: a face
		// dropped for a sprite the atlas does not have, and a state whose mesh came out with no faces in
		// it at all. Both leave a block that is registered, keyed, culling its neighbours - and
		// invisible, which is a hole in the world with no visible cause. The count lives on the native
		// side because that is where the baking happens, and it is logged here because the native log
		// does not reach this file.
		val missed = WgpuNative.blockBakeDiagnostics()

		if (missed.isNotEmpty()) {
			WgpuMcMod.LOGGER.warn("wgpu: the block models did not all bake: {}", missed)
		}

		val client = Minecraft.getInstance()

		// Before the re-mesh, not after: what makes a section be offered again is this side forgetting
		// that Rust already has it, and `allChanged` below is what offers them. Without it the offers
		// carry nothing - the blocks themselves have not changed, only the models they bake to - and the
		// arena goes on drawing the geometry of the last bake. See `RustChunkBake.forgetSent`.
		if (mode != Bake.First) {
			RustChunkBake.forgetSent()
		}

		WgpuMcMod.LOGGER.info(
			"wgpu: block states cached{}; asking Minecraft to mesh its sections again so the feed sees them",
			when (mode) {
				Bake.First -> ""
				Bake.Reload -> " (resource reload)"
				Bake.Setting -> " (a baked setting changed)"
			},
		)

		client.execute {
			client.levelRenderer.allChanged()
		}
	}

	/**
	 * Asks for the block models to be baked again, because a setting that is written into them moved.
	 *
	 * Called by the **native** side, from the options screen's Apply: the animated-texture switch
	 * decides, face by face, which atlas that face samples, so its answer is not read on the draw path
	 * but *baked* - into the coordinates and the flag in the vertex. Moving it therefore invalidates
	 * every baked model, and there is no way to apply it that does not bake them again.
	 *
	 * Nothing is done here. This arrives on the render thread, in the middle of the options screen's
	 * own work, and a bake is seconds of reading and meshing: the note is left for the next client tick,
	 * which starts it on the block cache thread exactly as a reload does - and, like a reload, leaves it
	 * for a later tick if one is already running.
	 */
	@JvmStatic
	fun blockTexturesChanged() {
		pendingRebake.set(true)
	}

	@SubscribeEvent
	@JvmStatic
	fun onClientTick(event: ClientTickEvent.Post) {
		val ticks = sinceReload
		if (ticks >= 0) {
			sinceReload = ticks + 1
		}
		start()

		// A setting that is baked into the models, applied from the options screen. Claimed like a
		// reload, and only when nothing else is baking: the flag is left set otherwise, so the next tick
		// sees it again.
		if (pendingRebake.get() && baking.compareAndSet(false, true)) {
			pendingRebake.set(false)
			startBake(Bake.Setting)
		}

		// The arena's refusals, drained here because a tick is the cheapest place that is reached on
		// every launch: a section it had no room for is a hole this side would otherwise go on
		// believing was filled. See `RustChunkBake.forgetRefused`.
		RustChunkBake.forgetRefused()
	}
}