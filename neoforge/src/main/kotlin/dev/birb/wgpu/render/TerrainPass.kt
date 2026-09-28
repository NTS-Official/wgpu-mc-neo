package dev.birb.wgpu.render

import dev.birb.wgpu.WgpuMcMod
import dev.birb.wgpu.backend.bindAtlasToTerrainPass
import dev.birb.wgpu.backend.bindLightmapToTerrainPass
import dev.birb.wgpu.chunk.RustChunkBake
import dev.birb.wgpu.rust.WmNative
import dev.birb.wgpu.rust.WgpuNative
import net.minecraft.client.Minecraft
import com.mojang.blaze3d.vertex.PoseStack
import com.mojang.math.Axis
import net.minecraft.client.renderer.state.level.CameraRenderState
import net.minecraft.core.SectionPos
import org.joml.Matrix4f
import java.lang.foreign.MemorySegment
import java.lang.foreign.ValueLayout

/**
 * The Rust terrain pass, from this side: which of Minecraft's passes it stands in for, and the camera
 * it is drawn with.
 *
 * The graph pass draws the sections the Rust baker meshed (see [RustChunkBake]), and it draws them
 * *instead of* Minecraft's own terrain rather than beside it: two sets of the same terrain are two
 * sets of the same triangles, and the pass that draws it has to be the one the rest of the frame shares
 * its depth buffer with - which is why the takeover happens where Minecraft's own pass would have
 * been opened. So the two things this side has to get right are which pass (see [replaces], and what
 * that pass turns out to contain) and with what camera (see [sendCameraMatrices]).
 */
object TerrainPass {
	/**
	 * The pipeline whose pass the graph draws instead.
	 *
	 * This is where the takeover fires, and it takes the *whole* pass with it - which in 26.1 is the
	 * game's `OPAQUE` section-layer group: `ChunkSectionsToRender#renderGroup` opens one render pass
	 * and walks the group's layers inside it, calling `setPipeline` first for the solid layer and then
	 * for the cutout one. So the graph pass draws both of those layers out of the arena.
	 */
	private const val SOLID_TERRAIN = "minecraft:pipeline/solid_terrain"

	/**
	 * The other pass the graph draws: the game's `TRANSLUCENT` group, which is one layer - water, ice,
	 * stained glass and every other block model whose sprite blends.
	 *
	 * It is a second takeover rather than a third layer of the first one because Minecraft draws it as a
	 * group of its own: `LevelRenderer` submits the transparent features, copies the main target's depth
	 * into whichever target the group draws into, and only then calls `renderGroup` for it. The two are
	 * also not interchangeable at the pipeline level - this one blends, does not write depth, and is
	 * drawn far-to-near - which the graph has as two pipelines (`terrain` and `translucent_terrain`).
	 *
	 * What makes it takeable here is that the group's own target usually **does not exist**:
	 * `ChunkSectionLayerGroup#outputTarget` falls back to the main target, and the translucent target is
	 * only created when a transparency post chain is loaded - the Fabulous preset, or a resource pack
	 * that ships one. See [usesOwnTarget], which is what refuses the takeover in that case rather than
	 * drawing water into the wrong texture.
	 */
	private const val TRANSLUCENT_TERRAIN = "minecraft:pipeline/translucent_terrain"

	/** Whether the path is switched on at all. See [RustChunkBake.MARKER]. */
	fun isOn(): Boolean = RustChunkBake.isOn()

	/** Whether the graph draws the pass a pipeline belongs to. */
	fun replaces(location: String?): Boolean =
		location == SOLID_TERRAIN || location == TRANSLUCENT_TERRAIN

	/** Whether a pipeline is the translucent group's, which is the second of the two takeovers. */
	fun isTranslucent(location: String?): Boolean = location == TRANSLUCENT_TERRAIN

	/** Whether the message about the translucent layer being taken over has been written. */
	private val reportedTranslucent = java.util.concurrent.atomic.AtomicBoolean()

	fun noteTranslucentTakenOver(): Boolean = reportedTranslucent.compareAndSet(false, true)

	/**
	 * Whether the arena has anything to draw.
	 *
	 * A pass stays Minecraft's until it does: the graph draws the arena's contents, so a pass taken
	 * over while the arena is empty is a frame with no ground in it - the same trade [ready] makes
	 * about the pipeline, for the same reason. Asked per frame, because the answer changes as the world
	 * is meshed; the call is one lock and a length.
	 */
	fun hasGeometry(renderer: MemorySegment): Boolean =
		(WmNative.terrainArenaSections.invokeExact(renderer) as Int) > 0

	/**
	 * Whether the pass a pipeline belongs to draws into a target of its own, which this pass cannot be
	 * handed.
	 *
	 * Only the translucent group ever has one, and only under a transparency post chain: `Fabulous` or a
	 * resource pack's own chain, which `Minecraft.useShaderTransparency` is the switch for. The
	 * takeover is handed the colour and depth views Minecraft opened *this* pass with, so drawing the
	 * translucent layer into an opaque pass's target would put it in the wrong texture - and, because
	 * the copy of the depth belongs to that other target, at the wrong depth.
	 *
	 * A resource pack can turn this on at any time, so it is asked per frame rather than cached.
	 */
	fun usesOwnTarget(location: String?): Boolean {
		if (location != TRANSLUCENT_TERRAIN) {
			return false
		}

		val renderer = Minecraft.getInstance().levelRenderer
		return renderer.getTranslucentTarget() != null
	}

	/** Whether the message about a refused translucent takeover has been written. See [usesOwnTarget]. */
	private val reportedOwnTarget = java.util.concurrent.atomic.AtomicBoolean()

	/** Says once that the translucent layer is still Minecraft's, and why. */
	fun reportOwnTarget() {
		if (reportedOwnTarget.compareAndSet(false, true)) {
			WgpuMcMod.LOGGER.info(
				"wgpu: the translucent terrain is still Minecraft's - improved transparency (the " +
					"Fabulous preset, or a resource pack's transparency chain) draws it into a target " +
					"of its own, which this pass cannot be handed. Water and glass are the game's again."
			)
		}
	}

	/**
	 * Whether the graph can draw it yet.
	 *
	 * The graph's terrain pipeline is built from the block atlas, which a resource reload stitches on a
	 * background thread, so for the first seconds of a session there is nothing to draw with. A pass
	 * taken away from Minecraft then is a frame with no ground in it rather than a frame drawn another
	 * way, so the question is asked first - and the answer is kept, because a pipeline does not unbuild
	 * itself and this is a native call per frame otherwise.
	 */
	fun ready(renderer: MemorySegment): Boolean {
		if (canDraw) {
			return true
		}

		canDraw = WmNative.terrainPassReady.invokeExact(renderer) as Boolean
		if (canDraw) {
			WgpuMcMod.LOGGER.info(
				"wgpu: the render graph is drawing the solid, cutout and translucent terrain; " +
					"Minecraft's own meshes for those three layers are skipped"
			)
		}
		return canDraw
	}

	@Volatile
	private var canDraw = false

	private val projection = FloatArray(16)

	/**
	 * The modelview matrix the pass is drawn with: the camera's *view rotation*, without the
	 * projection in it and without the bob, which is how Minecraft hands it to the level.
	 *
	 * Column-major, as every matrix here is - `Matrix4f.get` writes that order and WGSL reads it.
	 */
	private val view = FloatArray(16)
	private val model = FloatArray(16)

	/**
	 * Sends the three matrices the graph's terrain pipeline binds.
	 *
	 * The projection is Minecraft's own level projection: the camera's projection with the damage tilt
	 * and the view bobbing multiplied into it. `GameRenderer.renderLevel` builds exactly that - it
	 * takes `cameraState.projectionMatrix` and multiplies the `bobHurt` + `bobView` pose stack into it -
	 * and it hands `cameraState.viewRotationMatrix` down as the level's modelview matrix, which is why
	 * the view here is that matrix and no longer the identity.
	 *
	 * Drawing with `camera.getViewRotationProjectionMatrix` instead - the same two, without the bob -
	 * left every triangle this pass draws standing still while the rest of the world bobbed with the
	 * step. Two copies of the same terrain, one on each side of the bob, is what the flickering glass
	 * was: the triangles of the block behind it and the block itself swapping over every frame.
	 *
	 * The view is the camera's rotation with the camera's offset **within its own section**, and the
	 * model matrix is the identity: the section the pass draws is placed relative to the camera's
	 * section by the graph itself, so nothing here has to carry an absolute position. See the comment on
	 * the translate below for what that buys, and `setCameraSection` on the native side for the section
	 * it has to agree with.
	 */
	fun sendCameraMatrices() {
		val client = Minecraft.getInstance() ?: return
		val camera = client.gameRenderer.mainCamera ?: return

		// The camera state of the frame being drawn, which `GameRenderer.renderLevel` filled in before
		// it called into the level renderer this pass belongs to.
		val cameraState = client.gameRenderer.gameRenderState.levelRenderState.cameraRenderState

		val bob = levelBob(cameraState, client)

		lastBobView = client.options.bobView().get()
		lastIsPlayer = cameraState.entityRenderState.isPlayer
		lastWalk = cameraState.entityRenderState.backwardsInterpolatedWalkDistance
		lastBobX = bob.last().pose().m30()
		lastBobY = bob.last().pose().m31()

		Matrix4f(cameraState.projectionMatrix).mul(bob.last().pose()).get(projection)

		val position = cameraState.pos

		// The section the camera is in, which is the origin everything this pass draws is placed
		// relative to. Sent with the matrices rather than read back out of them: the view no longer
		// carries the camera's absolute position (below), so this is the only copy of it - and it has to
		// be the same frame's camera that built the matrix.
		val sectionX = SectionPos.blockToSectionCoord(position.x)
		val sectionY = SectionPos.blockToSectionCoord(position.y)
		val sectionZ = SectionPos.blockToSectionCoord(position.z)

		val originX = SectionPos.sectionToBlockCoord(sectionX).toDouble()
		val originY = SectionPos.sectionToBlockCoord(sectionY).toDouble()
		val originZ = SectionPos.sectionToBlockCoord(sectionZ).toDouble()

		// The view is the camera's rotation with the camera's offset **within its own section** in it -
		// not the camera's position, which is what it used to carry, and the difference is the whole
		// reason for this arrangement.
		//
		// The graph places a section at `(section - cameraSection) * 16`, a number of at most a few
		// thousand, and this matrix then takes the fractional camera position off it. Both numbers stay
		// small, so the vertex that reaches the depth buffer is accurate to about a millionth of a block
		// - while folding in the camera's *absolute* position, as this matrix used to, meant adding a
		// number like 30000 to a number like 12 in `f32`, where the step is 0.004 blocks. That error is
		// a function of the world position and not of the camera, and it is what made the ground fight
		// with the shadow lying on top of it: an entity's shadow is a quad on the top face of a block
		// (`EntityRenderer#extractShadowPiece`), drawn afterwards with `LESS_THAN_OR_EQUAL`, and which of
		// the two the depth test saw was decided by their last bits - stripes that stayed where they were
		// while the camera moved.
		//
		// Vanilla reaches the same place from the other side: `terrain.vsh` computes
		// `Position + (ChunkPosition - CameraBlockPos) + CameraOffset` and its `ModelViewMat` is the
		// camera's rotation alone, so the big numbers never meet.
		Matrix4f(cameraState.viewRotationMatrix)
			.translate(
				-(position.x - originX).toFloat(),
				-(position.y - originY).toFloat(),
				-(position.z - originZ).toFloat(),
			)
			.get(view)

		Matrix4f().get(model)

		// The arena's trim hint, and the transform's own origin: the native side reads it for both
		// (the trim horizontally, the transform in all three axes). A fraction of a section of lag here
		// is not a nicety - it is sixteen blocks of terrain in the wrong place - which is why it is sent
		// on this line rather than on a tick.
		lastModelX = position.x.toFloat()
		lastModelY = position.y.toFloat()
		lastModelZ = position.z.toFloat()
		lastSectionX = sectionX
		lastSectionZ = sectionZ

		WgpuNative.setCameraSection(sectionX, sectionY, sectionZ)

		WgpuNative.setMatrix(MATRIX_PROJECTION, projection)
		WgpuNative.setMatrix(MATRIX_VIEW, view)
		WgpuNative.setMatrix(MATRIX_MODEL, model)

		sendFog(cameraState, position.x - originX, position.y - originY, position.z - originZ)

		bindLightmap()
	}

	/** The fog block the shader reads. See [sendFog] for what goes in it. */
	private val fog = FloatArray(12)

	/**
	 * Sends the frame's fog: the game's own numbers, and the camera's offset inside its section.
	 *
	 * The eleven are read from `CameraRenderState#fogData`, which is not a copy of the fog - it *is* the
	 * fog: `GameRenderer.renderLevel` writes that object into the game's fog buffer
	 * (`FogRenderer#updateBuffer(cameraState.fogData)`) and hands the slice to the level renderer, which
	 * binds it for the pass this one stands in for. So what arrives here is what the game's own terrain
	 * would have been drawn with, whatever produced it - water, lava, blindness, the darkness effect, the
	 * biome's fog, the render distance.
	 *
	 * The last three are this side's, and they are not fog at all: the shader computes a position relative
	 * to the camera's *section* (that is the whole of its precision, see [sendCameraMatrices]) and the fog
	 * distances are measured from the camera itself, so the offset between the two has to travel with the
	 * rest. `vec3` plus a padding float, because that is what the shader's struct has.
	 */
	private fun sendFog(cameraState: CameraRenderState, offsetX: Double, offsetY: Double, offsetZ: Double) {
		val fogData = cameraState.fogData

		fog[0] = fogData.color.x
		fog[1] = fogData.color.y
		fog[2] = fogData.color.z
		fog[3] = fogData.color.w
		fog[4] = fogData.environmentalStart
		fog[5] = fogData.environmentalEnd
		fog[6] = fogData.renderDistanceStart
		fog[7] = fogData.renderDistanceEnd
		fog[8] = offsetX.toFloat()
		fog[9] = offsetY.toFloat()
		fog[10] = offsetZ.toFloat()
		fog[11] = 0.0f

		WgpuNative.setFogEnvironment(fog)
	}

	/** The lightmap texture the graph was last handed, so the handover is one call rather than sixty a second. */
	@Volatile
	private var boundLightmap: Any? = null

	/**
	 * Hands the renderer the lightmap of the frame being drawn, the first time it sees it.
	 *
	 * The game does not build a new lightmap when the light changes - `Lightmap#render` writes into the
	 * texture it already has - so the handover is per *texture*, and what follows the light is the
	 * texture's contents rather than a new binding. Asking every frame is what catches the one case that
	 * would leave the terrain lit by a stale copy: the game replacing the lightmap. It is a pointer
	 * comparison and, once, a native call.
	 *
	 * Until it lands, the pass draws with the 16x16 fallback the native side builds for itself - the
	 * light curve this renderer used before it sampled the game's - so the first frames of a world are
	 * the picture this renderer has always drawn rather than a world lit by a placeholder.
	 */
	private fun bindLightmap() {
		val lightmap = Minecraft.getInstance().gameRenderer.levelLightmap()

		// The game hands out the same view for the life of the lightmap, so this is an identity check
		// rather than anything about the texture - and the one case it catches is the game replacing the
		// lightmap, which would leave the terrain lit by a stale one.
		if (lightmap === boundLightmap) {
			return
		}

		boundLightmap = lightmap

		lightmap.bindLightmapToTerrainPass()
	}

	/** Where the bob pose put the camera on the last frame the pass was sent matrices for. */
	@Volatile
	private var lastBobX = 0.0f

	/**
	 * The camera the model matrix was built from, and the section that went with it.
	 *
	 * Kept for [describeFrame]: the model translation and the per-section offsets the graph adds to it
	 * have to come from the *same* camera, and the one the graph's section comes from is sent by
	 * `WgpuCommandEncoder` from the live camera object. If those two ever disagree, the sum is out by a
	 * whole section - so the numbers are worth having in the log rather than in a theory.
	 */
	@Volatile
	private var lastModelX = 0.0f

	@Volatile
	private var lastModelY = 0.0f

	@Volatile
	private var lastModelZ = 0.0f

	@Volatile
	private var lastSectionX = 0

	@Volatile
	private var lastSectionZ = 0

	/**
	 * `GameRenderer.bobHurt` / `bobView`, found once. `null` when either cannot be reached, in which
	 * case [levelBob] falls back to the copy of their arithmetic.
	 */
	private val gameBob: Map<String, java.lang.invoke.MethodHandle> by lazy {
		listOf("bobHurt", "bobView").mapNotNull { name ->
			try {
				val method = net.minecraft.client.renderer.GameRenderer::class.java.getDeclaredMethod(
					name,
					CameraRenderState::class.java,
					PoseStack::class.java,
				)

				method.isAccessible = true

				name to java.lang.invoke.MethodHandles.lookup().unreflect(method)
			} catch (error: Throwable) {
				null
			}
		}.toMap()
	}

	/** Whether the bob source has been reported this run. See [reportBobSource]. */
	private val bobSourceReported = java.util.concurrent.atomic.AtomicBoolean()

	private fun callGameBob(
		name: String,
		renderer: net.minecraft.client.renderer.GameRenderer,
		state: CameraRenderState,
		pose: PoseStack,
	): Boolean {
		val handle = gameBob[name] ?: return false

		return try {
			handle.invokeWithArguments(renderer, state, pose)
			true
		} catch (error: Throwable) {
			false
		}
	}

	/**
	 * Diagnostics: which bob the pass is drawn with, said once.
	 *
	 * It is worth a line because the two are supposed to agree and only one of them can be the game's:
	 * a shake that survives one and not the other says which is wrong.
	 */
	/**
	 * Diagnostics: the cameras one frame can have, side by side.
	 *
	 * The model matrix's translation and the section the graph offsets its draws by have to come from
	 * the same camera, and they do not: the translation is built here from the frame's extracted camera
	 * state - the one the bob and the projection only exist in - while the section is sent by
	 * `WgpuCommandEncoder` from the live camera object. A whole section of disagreement between them is
	 * sixteen blocks of terrain in the wrong place, so the numbers and the difference between the two
	 * positions belong on the line rather than in a theory. `sec(live)` is what the graph is offsetting
	 * by, `sec(model)` what the translation was built with.
	 */
	private fun describeCamera(): String {
		val client = Minecraft.getInstance()
		val live = client.gameRenderer.mainCamera?.position()
		val state = client.gameRenderer.gameRenderState.levelRenderState.cameraRenderState.pos

		val liveSection = live?.let {
			"${SectionPos.blockToSectionCoord(it.x)}, ${SectionPos.blockToSectionCoord(it.z)}"
		} ?: "none"

		return ("cam live=(%.2f, %.2f, %.2f) state=(%.2f, %.2f, %.2f) model=(%.2f, %.2f, %.2f) " +
			"sec(live)=[$liveSection] sec(model)=[$lastSectionX, $lastSectionZ]").format(
			live?.x ?: 0.0,
			live?.y ?: 0.0,
			live?.z ?: 0.0,
			state.x,
			state.y,
			state.z,
			lastModelX,
			lastModelY,
			lastModelZ,
		)
	}

	private fun reportBobSource(source: String) {
		if (bobSourceReported.compareAndSet(false, true)) {
			dev.birb.wgpu.WgpuMcMod.LOGGER.info("wgpu: the terrain pass bobs with {}", source)
		}
	}

	@Volatile
	private var lastBobY = 0.0f

	/** Whether the game has view bobbing on, and whether the camera state is the player's. */
	@Volatile
	private var lastBobView = false

	@Volatile
	private var lastIsPlayer = false

	/** The interpolated walk distance of the last frame: zero here means the bob has nothing to use. */
	@Volatile
	private var lastWalk = 0.0f

	/**
	 * Diagnostics: what the pass drew with, for the line the takeover logs - or `null` when the frame
	 * is not worth a line.
	 *
	 * The pass is taken over on **every frame of a world**, so a line per frame is eight hundred lines
	 * and half a megabyte in thirty seconds: enough to bury everything else the game says, which is
	 * what it did. So there are three answers here, and only one of them is per frame:
	 *
	 *  - **the first frame of a session, and any frame where something changed** - a frame the pass
	 *    could not draw, the arena starting to refuse sections (geometry that is baked and not on
	 *    screen), or the arena growing, which is the answer to that - says everything, once;
	 *  - **any other frame** says the arena and the two counters and nothing else, and only once a
	 *    second at that.
	 *
	 * The numbers themselves are the ones this path is checked with. The section count says the pass
	 * has had a world to draw; the fluid counts say whether the fluid mesher has anything at all -
	 * blocks counted with no faces drawn is a fluid whose sprite is not in the atlas, and no blocks at
	 * all is a fluid that never reached the baker; the arena numbers say whether the geometry fits, and
	 * the largest section says what has to fit; and the bob is what says the camera this pass draws
	 * with is the one the rest of the world bobs with. Read here rather than from the Rust log because
	 * the Rust log does not reach the game's log file.
	 *
	 * `drawn` is whether the pass recorded anything this frame - the one thing in here that is about
	 * this frame rather than about the session - and a change in it is a change worth a full line.
	 */
	fun describeFrame(drawn: Boolean): String? = try {
		val sections = WmNative.terrainSectionsDrawn.invokeExact() as Int
		val fluidBlocks = WmNative.terrainFluidBlocks.invokeExact() as Int
		val fluidQuads = WmNative.terrainFluidQuads.invokeExact() as Int
		val refused = WmNative.terrainSectionsRefused.invokeExact() as Int
		val slots = WmNative.terrainArenaSlots.invokeExact() as Int
		val used = WmNative.terrainArenaUsed.invokeExact() as Int
		val largest = WmNative.terrainArenaLargestSection.invokeExact() as Int
		val watched = WgpuNative.watchedBlockFaces()
		val layers = layerCounts()

		// A *moving* watched count is worth the full line - under a second apart, so the loop while the
		// world is meshed says what the watched blocks did and then stops: once they hold still, the
		// compact line is the only one left, and the last full line carries the final numbers.
		val due = reportDue(drawn, refused, slots, watched != reportedWatched)
		reportedWatched = watched

		val share = if (slots <= 0) 0L else used.toLong() * 100 / slots
		val megabytes = used.toLong() * 4 / (1024 * 1024)

		val arena = "arena %,d of %,d slot(s) handed out (%d%%, %d MB), the largest section %,d slot(s)"
			.format(used, slots, share, megabytes, largest)

		// The six numbers, named for the three layers in the order the arena holds them. A layer whose
		// `drawn` is zero while the arena has geometry is a pass that is not drawing it; a layer whose
		// `empty` is every section is a bake that never landed. See `terrain_layer_counts`.
		val perLayer = "solid %d+%d, cutout %d+%d, transparent %d+%d (drawn+empty)"
			.format(layers[0], layers[1], layers[2], layers[3], layers[4], layers[5])

		when (due) {
			null -> null
			false ->
				"; %,d section draw(s), %s, %,d refused; %s".format(sections, arena, refused, perLayer)
			true ->
				"; %,d section draw(s) - the three layers of two passes -, ".format(sections) +
					"%,d fluid block(s) in the bakes, ".format(fluidBlocks) +
					"%,d fluid face(s), %,d section(s) refused by the arena, ".format(fluidQuads, refused) +
					arena +
					", bob (%.2f, %.2f)".format(lastBobX, lastBobY) +
					", bobView=$lastBobView player=$lastIsPlayer walk=%.2f".format(lastWalk) +
					"; $perLayer" +
					"; ${RustChunkBake.fluidDiagnostics}" +
					"; ${RustChunkBake.refusedDiagnostics}" +
					(watched.takeIf { it.isNotEmpty() }?.let { "; watched $it" } ?: "") +
					"; ${describeCamera()}"
		}
	} catch (error: Throwable) {
		if (reportDue(drawn, -1, -1, false) == null) null else "; the native counters could not be read: $error"
	}

	/**
	 * The per-layer counts, read from the two packed words, or six zeroes with one warning.
	 *
	 * The native side *drains* the counters, so this is what the pass drew since the last report - and it
	 * is read once here rather than in each branch of [describeFrame], because a second read would return
	 * an empty second.
	 *
	 * A failure is said out loud once rather than swallowed into six zeroes: zeroes are a *plausible
	 * answer* to the question this asks, so a silent failure here reads as "the pass drew nothing" and
	 * sends whoever is looking in the wrong direction. That is not hypothetical - it is what this did
	 * first, and the six zeroes were believed.
	 */
	private fun layerCounts(): LongArray = try {
		val drawn = WmNative.terrainLayerCounts.invokeExact(0) as Long
		val empty = WmNative.terrainLayerCounts.invokeExact(1) as Long

		LongArray(6) { index ->
			val word = if (index % 2 == 0) drawn else empty
			(word ushr (21 * (index / 2))) and 0x1F_FFFF
		}
	} catch (error: Throwable) {
		if (layerCountsReported.compareAndSet(false, true)) {
			WgpuMcMod.LOGGER.warn("wgpu: the per-layer terrain counts could not be read", error)
		}

		LongArray(6)
	}

	private val layerCountsReported = java.util.concurrent.atomic.AtomicBoolean()

	/** When the pass last said what it drew, and the numbers that line carried. See [describeFrame]. */
	private var reportedAt = 0L
	private var reportedRefused = -1
	private var reportedPool = -1
	private var reportedDrawn: Boolean? = null
	private var reportedWatched = ""

	/**
	 * Whether this frame is one to report, and whether it is a full report: `true` for the first frame
	 * and for a change in something that has no business changing, `false` for the once-a-second
	 * sample, `null` for a frame with nothing to say.
	 *
	 * The watched counts are in here rather than in the compact line because they stop moving: while
	 * the world is being meshed they change every frame and the full line says so once a second, and
	 * when they hold still the last full line is the answer and nothing more is printed about them.
	 */
	private fun reportDue(drawn: Boolean, refused: Int, pool: Int, watchMoved: Boolean): Boolean? {
		val changed = refused != reportedRefused ||
			pool != reportedPool ||
			reportedDrawn != drawn ||
			reportedAt == 0L

		val now = System.nanoTime()
		val overdue = now - reportedAt >= REPORT_INTERVAL_NANOS

		if (!changed && !overdue) {
			return null
		}

		reportedAt = now
		reportedRefused = refused
		reportedPool = pool
		reportedDrawn = drawn

		return changed || watchMoved
	}

	/** How often the pass says what it drew when nothing has changed. See [describeFrame]. */
	private const val REPORT_INTERVAL_NANOS = 1_000_000_000L

	/**
	 * The damage tilt and the view bob, which `GameRenderer.renderLevel` multiplies into the level
	 * projection - `bobHurt` first, then `bobView` when the option is on.
	 *
	 * Copied from those two methods as they read in 26.1 rather than called: both are private, and the
	 * access transformer is applied by NeoForge when the game loads, not to the jar the Kotlin
	 * compiler sees, so a call to either does not compile. It is a handful of lines of arithmetic over
	 * the frame's camera state; the day it drifts from the game is the day the world and this pass bob
	 * differently again, which is the bug this exists to fix.
	 */
	private fun levelBob(state: CameraRenderState, client: Minecraft): PoseStack {
		val bob = PoseStack()

		// Minecraft's own two methods first, reflectively: the access transformer widens them for the
		// game at load time but not for this compiler, and a *replica* of them is a second place the bob
		// can be got wrong - the shake this is here to remove was still there with the replica. If the
		// lookup fails the replica below is used, and which one ran is logged once.
		if (callGameBob("bobHurt", client.gameRenderer, state, bob) &&
			(!client.options.bobView().get() || callGameBob("bobView", client.gameRenderer, state, bob))
		) {
			reportBobSource("the game's own bobHurt/bobView")
			return bob
		}

		reportBobSource("a copy of the game's arithmetic")

		val entity = state.entityRenderState

		if (entity.isLiving) {
			var hurt = entity.hurtTime

			if (entity.isDeadOrDying) {
				val death = minOf(entity.deathTime, 20.0f)
				bob.mulPose(Axis.ZP.rotationDegrees(40.0f - 8000.0f / (death + 200.0f)))
			}

			// A damage type marked "no_flinch" does not shake the screen (Neo), and a hit that has not
			// been rendered yet has a hurt time below zero.
			if (hurt >= 0.0f && !entity.preventFlinch) {
				hurt /= entity.hurtDuration
				hurt = Math.sin(hurt * hurt * hurt * hurt * Math.PI).toFloat()
				val direction = entity.hurtDir
				bob.mulPose(Axis.YP.rotationDegrees(-direction))
				bob.mulPose(
					Axis.ZP.rotationDegrees(
						// The tilt strength is a double in the options state, which is why the game
						// narrows this product itself.
						(
							-hurt * 14.0 *
								client.gameRenderer.gameRenderState.optionsRenderState.damageTiltStrength
							).toFloat()
					)
				)
				bob.mulPose(Axis.YP.rotationDegrees(direction))
			}
		}

		if (entity.isPlayer && client.options.bobView().get()) {
			// `Mth.sin`/`Mth.cos` take and return a double in 26.1 - the game's own calls widen a float
			// into them silently, Kotlin will not - so the angle stays a double and the results are
			// narrowed back to the floats the pose stack wants.
			val walk = entity.backwardsInterpolatedWalkDistance.toDouble() * Math.PI
			val amount = entity.bob
			val turn = Math.sin(walk).toFloat()

			bob.translate(
				turn * amount * 0.5f,
				-Math.abs(Math.cos(walk) * amount.toDouble()).toFloat(),
				0.0f,
			)
			bob.mulPose(Axis.ZP.rotationDegrees(turn * amount * 3.0f))
			bob.mulPose(
				Axis.XP.rotationDegrees(
					Math.abs(Math.cos(walk - 0.2) * amount.toDouble()).toFloat() * 5.0f
				)
			)
		}

		return bob
	}

	/** The ids `set_matrix` in `renderer.rs` reads: 0 = projection, 2 = view, 3 = model. */
	private const val MATRIX_PROJECTION = 0
	private const val MATRIX_VIEW = 2
	private const val MATRIX_MODEL = 3
}
