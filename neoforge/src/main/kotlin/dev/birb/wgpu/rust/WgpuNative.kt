package dev.birb.wgpu.rust

import java.io.File
import java.io.FileNotFoundException
import java.io.IOException
import java.nio.file.AccessDeniedException
import java.nio.file.Files
import java.nio.file.StandardCopyOption
import java.util.HashMap

/**
 * Where the native library may sit inside the mod jar, in probe order.
 *
 * Deliberately a top-level declaration rather than a member of [WgpuNative]: the object loads the
 * native library from its own `init` block, and Kotlin runs an object's initializers in
 * declaration order, so a property declared below that `init` block is still `null` when the
 * loader reads it. Keeping it here makes the ordering impossible to get wrong.
 */
private val NATIVE_RESOURCE_ROOTS = listOf("META-INF/natives/", "assets/wgpu_mc/natives/")

/**
 * The properties a launcher uses to name the directory it unpacks the game's natives into, in probe
 * order. NeoForge's own version JSON sets every one of them to `${natives_directory}`.
 */
private val NATIVE_DIRECTORY_PROPERTIES = listOf(
	"org.lwjgl.system.SharedLibraryExtractPath",
	"jna.tmpdir",
	"io.netty.native.workdir",
	"org.lwjgl.librarypath",
)

/**
 * The directory the native library and its symbols are unpacked into.
 *
 * The launcher's natives directory rather than a `lib` folder of this mod's own: that is where the
 * game's other natives (LWJGL, GLFW, jemalloc, JNA) already sit, it is on `java.library.path`, and it
 * is the directory a debugger searches when it resolves a module's symbols - which is what PIX needs
 * the PDB for. The launcher names it in [NATIVE_DIRECTORY_PROPERTIES]; failing that it is found by
 * looking through `java.library.path` for the directory holding LWJGL's own libraries, identified by
 * their names rather than by the directory's, so it works on every platform and cannot be confused
 * with the JDK's own `bin`.
 *
 * A development run has neither, so the fallback is `natives/` under the working directory, which is
 * the same layout one level down.
 */
private fun nativeDirectory(): File {
	for (property in NATIVE_DIRECTORY_PROPERTIES) {
		val configured = System.getProperty(property)?.takeIf { it.isNotBlank() } ?: continue
		val directory = File(configured)
		if (directory.isDirectory) return directory
	}

	val searchPath = System.getProperty("java.library.path").orEmpty()
	for (entry in searchPath.split(File.pathSeparator)) {
		val directory = File(entry)
		if (!directory.isDirectory) continue
		val holdsLwjgl = directory.listFiles()?.any { file ->
			val name = file.name.lowercase()
			name.contains("lwjgl") || name.contains("glfw")
		} == true
		if (holdsLwjgl) return directory
	}

	return File("natives")
}

/**
 * JNI side of the native bridge: shader settings, entity model registration, block-state baking,
 * palettes, panic handling and the renderer handle itself.
 *
 * GPU resource handles (textures, buffers, command encoders, render passes, pipelines) deliberately
 * do **not** live here. They are owned by [WmNative], which binds the C ABI that
 * `rust/wgpu-mc-jni` exports, and every one of them is addressed with the `WmRenderer` pointer
 * returned by [createWmRendererOnWindow].
 *
 * Every declaration in this file must exist as a `#[jni_fn]` in `rust/wgpu-mc-jni`. A declaration
 * without one throws `UnsatisfiedLinkError` - an `Error`, so nothing catches it - the first time it
 * is called, which is why the old GPU-object declarations (`createTexture`, `createBuffer`,
 * `createCommandEncoder`, `presentTexture`, ...) were deleted once the C ABI took over: the Rust
 * side no longer exports those JNI entry points.
 */
object WgpuNative {
	init {
		loadWm()
	}

	@JvmStatic
	fun getClassLoader(): ClassLoader {
		return WgpuNative::class.java.classLoader
	}

	/**
	 * Extracts and loads the native library, then hands the JVM side what Rust needs from it.
	 *
	 * Only two things happen here, and both matter:
	 *
	 *  - `load` puts the cdylib in place and `System.load`s it;
	 *  - [setClassLoader] gives Rust the loader it must use to reach mod classes. `FindClass` from
	 *    an attached native thread resolves against the *system* loader, which cannot see NeoForge's
	 *    transformed classes, so without this every resource lookup panics on an empty cell - and a
	 *    panic on a JNI frame aborts the process rather than unwinding.
	 *
	 * `CoreLib.init()` used to be called here too, asking Rust to route its allocations through
	 * LWJGL's allocator. That shim is commented out on the Rust side
	 * (`rust/wgpu-mc-jni/src/alloc.rs`), so there is no `setAllocator` to call, and the resulting
	 * `UnsatisfiedLinkError` is an `Error` - the `catch (e: Exception)` below does not see it, so it
	 * took the game down during `Minecraft`'s constructor. `CoreLib` has been removed with it.
	 *
	 * Anything added here has to exist as a `#[jni_fn]` in `rust/wgpu-mc-jni`, or the first touch
	 * of this object kills the game.
	 */
	@JvmStatic
	fun loadWm() {
		try {
			load("wgpu_mc_jni", true)
			// This object's own loader, not the thread context loader: it is the one that can
			// resolve the mod's classes, and the only one Rust ever needs to look a class up.
			setClassLoader(WgpuNative::class.java.classLoader)
		} catch (e: Exception) {
			throw IllegalStateException(e)
		}
	}

	private external fun setClassLoader(contextClassLoader: ClassLoader)

	@Suppress("unused")
	private val idLists: HashMap<Any, Long> = HashMap()

	/**
	 * Loads a native library from the resources of this Jar.
	 *
	 * @param name Library to load
	 * @param forceOverwrite Force overwrite the library file
	 * @throws FileNotFoundException Library not found in resources
	 * @throws IOException Cannot move library out of Jar
	 */
	@JvmStatic
	@Throws(IOException::class)
	fun load(name: String, forceOverwrite: Boolean) {
		System.load(resolveNativeLibrary(name, forceOverwrite))
	}

	/**
	 * Extracts the native library [name] out of the mod jar and returns the absolute path of
	 * the extracted file, without loading it.
	 *
	 * The path is what [java.lang.foreign.SymbolLookup.libraryLookup] needs in order to bind
	 * the C ABI surface exposed by the same cdylib that backs the JNI entry points.
	 *
	 * Both roots in [NATIVE_RESOURCE_ROOTS] are probed. This build populates the
	 * `assets/wgpu_mc/natives/` one - `copyNatives` in build.gradle.kts puts the Rust build's
	 * library and its PDB there - so the other is only for a jar assembled the other way around.
	 */
	@JvmStatic
	@Throws(IOException::class)
	fun resolveNativeLibrary(name: String, forceOverwrite: Boolean = false): String {
		val mappedName = System.mapLibraryName(name)
		val libraryFile = File(nativeDirectory(), mappedName)
		// Fast path: the prebuilt library sits in the Rust workspace output.
		if (!libraryFile.exists() || forceOverwrite) {
			libraryFile.parentFile?.mkdirs()
			val resourceName = NATIVE_RESOURCE_ROOTS
				.firstOrNull { WgpuNative::class.java.classLoader.getResource(it + mappedName) != null }
				?: throw FileNotFoundException(
					"Could not find lib $mappedName in jar (looked in ${NATIVE_RESOURCE_ROOTS.joinToString()})"
				)

			WgpuNative::class.java.classLoader.getResourceAsStream(resourceName + mappedName).use { input ->
				try {
					Files.copy(input!!, libraryFile.toPath(), StandardCopyOption.REPLACE_EXISTING)
				} catch (denied: AccessDeniedException) {
					// Windows keeps a loaded DLL locked until the process that mapped it exits,
					// and two clients sharing a run directory is what that looks like: the copy
					// cannot replace the file the other instance is running. The file that is
					// there is the same library this build produced, so use it rather than
					// refusing to start - the alternative is a crash on launch.
					if (!libraryFile.exists()) throw denied
					dev.birb.wgpu.WgpuMcMod.LOGGER.warn(
						"wgpu: {} is locked by another process; using the copy already on disk",
						libraryFile,
					)
				}
			}

			// The symbols for that same build, next to the library - which is where a debugger looks
			// for them. A PIX timing capture resolves the functions in its CPU samples and
			// callstacks through the PDB that matches the module, so without this every frame from
			// the native renderer is an address in the capture's function information. Written at
			// the same time as the library and for the same reason: the two are one build, and a
			// PDB from the previous one no longer matches.
			copySymbolsBeside(mappedName)
		}
		return libraryFile.absolutePath
	}

	/**
	 * Puts the PDB for [mappedName] beside the extracted library, if this build has one.
	 *
	 * Best effort on purpose: a build without symbols - or a packaged jar that carries only the
	 * library - works exactly as before, it just profiles by address. The file is not locked by the
	 * process that loaded the library (only debuggers read it), so it can be replaced while another
	 * client is running.
	 */
	@JvmStatic
	@Throws(IOException::class)
	fun copySymbolsBeside(mappedName: String) {
		val symbolsName = mappedName.substringBeforeLast('.') + ".pdb"
		val resourceName = NATIVE_RESOURCE_ROOTS
			.firstOrNull { WgpuNative::class.java.classLoader.getResource(it + symbolsName) != null }
			?: return

		val symbolsFile = File(nativeDirectory(), symbolsName)

		try {
			WgpuNative::class.java.classLoader.getResourceAsStream(resourceName + symbolsName).use { input ->
				Files.copy(input!!, symbolsFile.toPath(), StandardCopyOption.REPLACE_EXISTING)
			}
		} catch (error: IOException) {
			dev.birb.wgpu.WgpuMcMod.LOGGER.warn("wgpu: could not write {}: {}", symbolsFile, error.message)
		}
	}

	@JvmStatic
	external fun getSettingsStructure(): String

	@JvmStatic
	external fun getSettings(): String

	@JvmStatic
	external fun sendSettings(settings: String): Boolean

	@JvmStatic
	external fun sendRunDirectory(dir: String)

	@JvmStatic
	external fun setPanicHook()

	@JvmStatic
	external fun registerBlockState(state: Any, blockId: String, stateKey: String)

	/**
	 * Offers what one block state says about the faces around it, under the key the registry gave it.
	 *
	 * [occlusion] and [selfHide] are six bits each, one per `Direction.ordinal()`:
	 * `state.getFaceOcclusionShape(dir) == Shapes.block()` and `state.skipRendering(state, dir)`.
	 * They are what the baker's face test reads - a neighbour's *state*, not its model - so a
	 * full-cube model that occludes nothing (glass, ice, leaves, every plant) no longer culls the
	 * faces of the block next to it. See `wgpu_mc::mc::block::FaceFlags`.
	 *
	 * Called once per state from `Wgpu#helperSetBlockStateIndex`, *not* from the registration mixin:
	 * the shapes these are read from are null until `BlockStateBase#initCache`, which the game runs at
	 * the end of `Blocks`' class initializer. See `BlockFaceFlags`.
	 */
	@JvmStatic
	/**
	 * One sprite of the game's block atlas, in one call: **where it is, and the layer the game puts it
	 * in**.
	 *
	 * Called before [cacheBlockStates], while the game's atlas is still in hand. The rectangle is the
	 * game's own layout - this side packs its sprites into an atlas of its own, at coordinates of its
	 * own - and [layer] is one of the `LAYER_*` numbers below: the game's reading of the sprite's
	 * transparency, where the Rust side otherwise guesses from the pixels and gets a sprite that is part
	 * opaque and part cutout wrong.
	 *
	 * The rectangle is what makes an animated texture animate: the game animates its atlas by rendering
	 * the due frame into it, so a face whose sprite the game animates is baked with *these* coordinates
	 * and samples the game's atlas, and it moves with the game instead of being frozen at whatever was
	 * copied. See `Atlas::register_sprite`.
	 */
	external fun registerSprite(name: String, u0: Float, v0: Float, u1: Float, v1: Float, layer: Int, levelCap: Int)

	/**
	 * The layer numbers [registerSprite] carries: the Rust side's three, by name.
	 *
	 * Named here rather than taken from an enum's ordinal, because these cross a bridge: a variant added
	 * on the Rust side must not silently change what a number means on this one. One name differs
	 * between the two sides and it is the third: the game calls that chunk layer `TRANSLUCENT`, and the
	 * Rust side calls it `Transparent` - so a sprite the game files under `TRANSLUCENT` is registered
	 * with [LAYER_TRANSPARENT], deliberately and not by accident of numbering.
	 */
	const val LAYER_SOLID = 0
	const val LAYER_CUTOUT = 1
	const val LAYER_TRANSPARENT = 2

	external fun setVisibleSections(keys: LongArray)

	/** How many section bakes are queued or running, and the ceiling they are measured against. */
	external fun queuedBakes(): Int

	external fun maxQueuedBakes(): Int

	/** How many sections the arena is actually drawing. See the note on the native side. */
	external fun arenaSections(): Int

	/** Whether the arena is at the device's buffer limit, so a refusal is permanent. */
	external fun terrainArenaAtCapacity(): Boolean

	external fun registerBlockStateFaceFlags(
		key: Int,
		occlusion: Int,
		selfHide: Int,
		shades: Int,
		blocksMotion: Int,
		offsetMaxY: Float,
		offsetXz: Int,
		/** Whether the state's block is a `LeavesBlock` - the game's own `instanceof` check. */
		leaves: Int,
	)

	@JvmStatic
	external fun getBackend(): String

	/**
	 * [getBackend] that degrades to a placeholder instead of throwing when the renderer has not
	 * been created yet. Blaze3D queries the backend name while building the device info, which can
	 * happen before the first frame is rendered.
	 */
	@JvmStatic
	fun getBackendSafe(): String =
		try {
			getBackend()
		} catch (_: Throwable) {
			"wgpu"
		}

	/**
	 * The adapter behind the renderer: its vendor, its name, the API it is driven through and its
	 * driver, one per line.
	 *
	 * These are the four things `GpuDeviceBackend` is asked for, and on the OpenGL backend they are
	 * `GL_VENDOR`, `GL_RENDERER`, "OpenGL" and `GL_VERSION` - so this is what lets the F3 overlay
	 * name the graphics driver instead of saying "wgpu".
	 */
	@JvmStatic
	external fun getAdapterInfo(): String

	/**
	 * [getAdapterInfo] that degrades to an empty string instead of throwing.
	 *
	 * The same reasoning as [getBackendSafe], plus one more: this is read from the device's
	 * constructor, which runs before the mod's own renderer exists on the very first frame.
	 */
	@JvmStatic
	fun getAdapterInfoSafe(): String =
		try {
			getAdapterInfo()
		} catch (_: Throwable) {
			""
		}

	@JvmStatic
	external fun setWorldRenderState(render: Boolean)

	@JvmStatic
	external fun createPalette(): Long

	@JvmStatic
	external fun destroyPalette(rustPalettePointer: Long)

	@JvmStatic
	external fun paletteIndex(ptr: Long, `object`: Any, index: Int): Int

	@JvmStatic
	external fun paletteSize(rustPalettePointer: Long): Int

	@JvmStatic
	external fun createPaletteStorage(
		copy: LongArray,
		elementsPerLong: Int,
		elementBits: Int,
		maxValue: Long,
		indexScale: Int,
		indexOffset: Int,
		indexShift: Int,
		size: Int
	): Long

	@JvmStatic
	external fun paletteReadPacket(slabIndex: Long, array: ByteArray, currentPosition: Int, blockstateOffsets: LongArray): Int

	@JvmStatic
	external fun registerBlock(name: String)

	/**
	 * Whether the native side has finished caching block states.
	 *
	 * Everything that bakes geometry needs the registry that cache builds - `AIR`, and the model
	 * behind every block state - so a bake is only allowed once this answers true. It is false while
	 * the client is still on the title screen, which is also when a quickplay launch is already
	 * loading chunks.
	 */
	@JvmStatic
	external fun blocksCached(): Boolean

	@JvmStatic
	external fun clearPalette(l: Long)

	@JvmStatic
	external fun cacheBlockStates()

	/**
	 * The first half of a bake: forget what the last one was given, so it can be offered again.
	 *
	 * `reload` says whether the resource **pack** changed as well. When it did, everything this side
	 * holds that came out of a pack goes - the block list, the block atlas with its packed rectangles
	 * and its animation table, and the diagnostics that count what the *last* pack failed to read - and
	 * the JVM re-registers the sprites and hands the atlas texture over before the bake. When it did
	 * not - a setting that only changes how the models are *baked* - the atlas is kept, and with it the
	 * game's rectangles for its animated sprites, which nothing here would ask for again.
	 *
	 * What it never touches is the block manager's meshes and the state keys, because they are still
	 * being drawn from while this runs; the bake that follows replaces them block by block, in the same
	 * order, which is what keeps the keys valid.
	 *
	 * Paired with [BlockRegistryFeed.replay] and then [cacheBlockStates], in that order, on the block
	 * cache thread. See `beginBlockBake` on the native side for what is cleared and why.
	 */
	external fun beginBlockBake(reload: Boolean)

	/**
	 * What the model baker could not draw, as a sentence - empty when it drew everything.
	 *
	 * The two ways a block is baked into nothing while looking perfectly registered: a face dropped for
	 * a sprite the atlas does not have, and a state whose mesh came out with no faces at all. Both
	 * leave a block that has a key, that occludes its neighbours - so the block behind it loses the
	 * face between them - and that is never drawn. Read once, right after [cacheBlockStates], which is
	 * where both are decided, and logged from there because the native log does not reach this file.
	 */
	@JvmStatic
	external fun blockBakeDiagnostics(): String

	/**
	 * Hands the game's `cutoutLeaves` option over, before a bake, because the answer is written into the
	 * geometry rather than read as it draws.
	 *
	 * That is `Options#cutoutLeaves` - the Fancy/Fast leaves switch the graphics presets move - and it
	 * reaches baking as `ModelBlockRenderer#forceOpaque`: with it off a leaf block's faces go to the
	 * solid layer whatever their sprite says, so a leaf texture's transparent gaps are filled by its own
	 * colour instead of being cut away. See `wgpu_mc::mc::block::CUTOUT_LEAVES`.
	 */
	@JvmStatic
	external fun setCutoutLeaves(cutout: Boolean)

	/**
	 * The window mode the renderer's setting names, as the index of its variant - `0` exclusive
	 * fullscreen, `1` borderless, `2` off.
	 *
	 * Read by [dev.birb.wgpu.backend.DisplayMode.Mode.current] rather than through
	 * [dev.birb.wgpu.rust.RendererSettings], because it is read on the path that *puts the window into a
	 * mode* - and `RendererSettings` caches for a second, so a mode applied immediately after the setting
	 * moved could read the value from before it and put the window back where it was.
	 */
	@JvmStatic
	external fun windowMode(): Int

	/**
	 * Whether the window mode moved in the apply that just happened, so the settings screen can say so:
	 * `-1` when it did, and the current variant index when it did not.
	 *
	 * The schema cannot answer this. It says whether a setting *may* need a restart; this is whether it
	 * *did* - and for a player who opened the page and moved nothing, it did not.
	 */
	@JvmStatic
	external fun windowModeReloadResult(): Int
	/**
	 * Stores the window mode and puts the window into it, for the cycle button on **Minecraft's own video
	 * settings screen** - see [dev.birb.wgpu.backend.DisplayMode.windowModeOption].
	 *
	 * The value goes to this renderer's config rather than to `options.txt`, because it is three states and
	 * `options.txt`'s `fullscreen` is a boolean; the row is the game's because that is where a player looks.
	 * The apply is done by the native side rather than here so that there is one writer and one path.
	 */
	@JvmStatic
	external fun setFullscreenMode(mode: Int)

	/**
	 * Which atlas the faces baked since the last call went to, as a short string, and **reset** so the
	 * next call reports the next interval.
	 *
	 * The one thing about the atlas routing that cannot be seen: both atlases are 2048x2048 and answer to
	 * the same filters, so a face on the wrong one is not drawn differently - it samples a mip chain built
	 * from the whole packed sheet instead of per sprite, which is a blurred, half-transparent block. The
	 * game-atlas number should be nearly all of them and the own-atlas number nearly none.
	 *
	 * Read on the once-a-second report rather than beside [blockBakeDiagnostics], which runs right after
	 * the block cache is built and therefore before a single section has been baked.
	 */
	@JvmStatic
	external fun atlasFaceCounts(): String

	/**
	 * What the watched blocks have been seen, drawn and culled for - empty until one of them is baked.
	 *
	 * The one line that says why a block is invisible: `seen 0` is a state that never reached a bake,
	 * `drawn 0 culled N` is a model whose every face a neighbour test removed, and `drawn N` is faces
	 * that are in the section mesh - which puts the fault after the bake rather than in it. The blocks
	 * watched are named in `chunk::WATCHED_BLOCKS` on the native side.
	 */
	@JvmStatic
	external fun watchedBlockFaces(): String

	/**
	 * Offers one section rebuild, in one call.
	 *
	 * `address` is the base of a payload `RustChunkBake` wrote into reusable off-heap memory and
	 * `length` is how much of it is in use: the 27 sections around `(x, y, z)` - Minecraft's own
	 * storage longs and a palette translation table for the ones that changed, and the light layers
	 * that changed - followed by masks saying which sections the native side is expected to have. The
	 * layout is written down in `rust/wgpu-mc-jni/src/section.rs`.
	 *
	 * Every record in it is stamped with the world it describes, and this is the answer:
	 *
	 *  - bits 0..26 (the rejected mask): those records were written for another world - a level change
	 *    while this chunk build was already running - so they were not applied. The caller must not
	 *    record them as sent, or the section stays a hole;
	 *  - bit 27 (the resync bit): the native side is missing part of the neighbourhood this call
	 *    described, so the caller forgets what it has sent and calls once more with everything.
	 */
	@JvmStatic
	external fun bakeSections(x: Int, y: Int, z: Int, address: Long, length: Int): Int

	/**
	 * Forgets every section of the world the bake was built against, and answers the new generation.
	 *
	 * Called from `LevelRenderer#setLevel`, which is a new world, a dimension change and the way back
	 * to the title screen alike. The number returned is what the caller stamps its own payloads with;
	 * the native side refuses any record stamped with anything else, which is what keeps a chunk build
	 * that was already running from writing the old world's blocks under the new one's coordinates.
	 */
	@JvmStatic
	external fun clearSections(): Int

	/**
	 * The sections the section arena had no room for since the last call, as `SectionPos.asLong` keys.
	 *
	 * The other half of the refusal counter on the terrain line: the counter says how much of the view
	 * was dropped, and this says which. A section that was refused is one the caller has been told was
	 * baked and which is not drawn at all - and since its rebuild has already happened, and a rebuild
	 * only carries what changed, nothing would offer it again. Taking these keys out of the caller's
	 * "what have I sent" table is what gives each of them another chance.
	 *
	 * Drains: what one call does not take is handed over by the next. Called once a client tick, and
	 * an empty array in the normal case.
	 */
	@JvmStatic
	external fun refusedSections(): LongArray

	@JvmStatic
	external fun setMatrix(type: Int, mat: FloatArray)

	/**
	 * Sends the frame's **fog**, as twelve floats in the order the terrain shader's `FogEnvironment`
	 * declares them: the colour (`rgba`), `environmentalStart`, `environmentalEnd`,
	 * `renderDistanceStart`, `renderDistanceEnd`, then the camera's offset inside its own section and a
	 * padding float.
	 *
	 * Eleven of the twelve are the game's own numbers, read from the camera render state's `FogData` -
	 * the same object the game writes into its terrain fog buffer - and the last three are this side's,
	 * because the shader measures a fog distance from a *camera-relative* position and the position it
	 * computes is relative to the camera's section. See `renderer::FOG` and the shader's `apply_fog`.
	 */
	external fun setFogEnvironment(values: FloatArray)

	@JvmStatic
	external fun registerEntities(toString: String)

	@JvmStatic
	external fun scheduleStop()

	@JvmStatic
	external fun reloadShaders()

	/**
	 * Creates the renderer (wgpu instance, adapter, device, queue) and returns the pointer to the
	 * `WmRenderer` that Rust keeps alive.
	 *
	 * This pointer is what the whole C ABI in [WmNative] is addressed with, so it is obtained once
	 * per backend and threaded through the device and everything below it.
	 *
	 * The historical `createDevice(long, long, int, int)` declaration is gone: Rust's
	 * `create_device` is a no-argument JNI function, so the four-argument JVM declaration could
	 * never have resolved. [createWmRendererOnWindow] is what the backend calls; this one exists
	 * for a backend that is never presented.
	 *
	 * Returns 0 when no graphics backend could be created, which happens when the backend named
	 * in the renderer config is not usable on this machine *and* the fallback is unavailable too.
	 */
	@JvmStatic
	external fun createWmRenderer(): Long

	/**
	 * [createWmRenderer], but told which window the renderer will present to.
	 *
	 * Rust creates the surface first and then requires the adapter to support it, because an
	 * adapter that cannot present to this window would leave a device that renders perfectly and
	 * never shows anything. Without the handles the renderer can only guess, which is fine for a
	 * backend that is never presented but not for one that is.
	 *
	 * @param display the raw GLFW display handle, or 0 where the platform has none
	 * @param window the raw GLFW window handle
	 * @param width the window's framebuffer width in pixels, or 0 when it has no size yet
	 * @param height the window's framebuffer height in pixels, or 0 when it has no size yet
	 * @return the `WmRenderer` pointer, or 0 if no backend could be created
	 */
	@JvmStatic
	external fun createWmRendererOnWindow(display: Long, window: Long, width: Int, height: Int): Long

	/**
	 * Says where the camera is, in sections.
	 *
	 * Sections are what the terrain path is keyed by - the baker's positions, the arena's vertex and
	 * index ranges, the graph pass's grid - so this is the one coordinate the renderer needs from the
	 * camera, and it is sent rather than derived: the renderer has no camera of its own, only the
	 * matrices Minecraft hands it.
	 *
	 * All three axes. The arena is trimmed against `x` and `z` (a vertical slice of the world is loaded
	 * all at once), and the terrain pass *draws* against all three: a section is placed at
	 * `(section - cameraSection) * 16`, with the camera's offset inside its own section carried by the
	 * view matrix, so the numbers that reach the depth buffer stay small and the ground stops fighting
	 * with the shadow lying on it. That is why the height is here and why it is sent by the same code
	 * that builds the matrices, a frame's disagreement being sixteen blocks of terrain in the wrong
	 * place.
	 */
	@JvmStatic
	external fun setCameraSection(x: Int, y: Int, z: Int)

	/**
	 * Says how far the section arena should reach, in chunks.
	 *
	 * Minecraft's own render distance, which is what the arena is trimmed to: it holds the sections
	 * the game is drawing and frees the ones behind the player. Sent when the value changes rather
	 * than once a frame - it moves when the slider does.
	 */
	@JvmStatic
	external fun setRenderDistance(chunks: Int)
}
