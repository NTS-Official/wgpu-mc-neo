package dev.birb.wgpu.render

import dev.birb.wgpu.WgpuMcMod
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
	 * for the cutout one. So the graph pass draws both of those layers out of the arena, and the
	 * translucent layer is the one that stays Minecraft's - it is a group of its own, drawn in a pass
	 * of its own, into a target of its own.
	 */
	private const val SOLID_TERRAIN = "minecraft:pipeline/solid_terrain"

	/** Whether the path is switched on at all. See [RustChunkBake.MARKER]. */
	fun isOn(): Boolean = RustChunkBake.isOn()

	/** Whether the graph draws the pass a pipeline belongs to. */
	/**
	 * Whether the arena has anything to draw.
	 *
	 * That pass stays Minecraft's until it does: the graph draws the arena's contents, so a pass taken
	 * over while the arena is empty is a frame with no ground in it - the same trade [ready] makes
	 * about the pipeline, for the same reason. Asked per frame, because the answer changes as the world
	 * is meshed; the call is one lock and a length.
	 */
	fun hasGeometry(renderer: MemorySegment): Boolean =
		(WmNative.terrainArenaSections.invokeExact(renderer) as Int) > 0

	fun replaces(location: String?): Boolean = location == SOLID_TERRAIN

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
				"wgpu: the render graph is drawing the solid and cutout terrain; Minecraft's own " +
					"meshes for those two layers are skipped"
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
	 * The model matrix is what makes the shader's own arithmetic camera-relative. It places a section
	 * at `section * 16` relative to the camera's *section*, and the difference between that and where
	 * the camera actually is, is what this translates by. Y is not taken relative to a section - the
	 * graph works in absolute section heights - so the camera's own height is in the matrix as it is.
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

		// The view matrix carries the camera's own translation, and the graph offsets a section by its
		// *absolute* position: the frame's transform is then one matrix pair and nothing else, and the
		// terrain pass is not told a second camera it could disagree with. `view = R * translate(-p)`,
		// which is what lets the section be read back out of it - see `derive_camera_section` on the
		// native side, where the same value is used for the arena's trim.
		//
		// The model matrix is the identity for that reason, and the section position the graph sends
		// with each draw is absolute. The cost is precision far from the origin: `p` is a float in the
		// matrix, so a world tens of millions of blocks out quantises the terrain. The form this
		// replaced kept the camera-section offset instead and paid for it with a section that had to be
		// sent separately - and with the sixteen-block jump that a section sent out of step with these
		// matrices produced. If far-out precision ever matters more, the honest fix is to send `p` as two
		// floats (high and low part), not to reintroduce a second camera.
		val position = cameraState.pos

		Matrix4f(cameraState.viewRotationMatrix)
			.translate(-position.x.toFloat(), -position.y.toFloat(), -position.z.toFloat())
			.get(view)

		Matrix4f().get(model)

		// The arena's trim hint, and nothing else: the native side reads the section it draws with back
		// out of the view matrix (`derive_camera_section`), so a hint that is a frame late costs nothing -
		// while a section that disagreed with these matrices was sixteen blocks of terrain in the wrong
		// place. This one is still sent because the arena is trimmed whether or not this path is drawn.
		val sectionX = SectionPos.blockToSectionCoord(position.x)
		val sectionZ = SectionPos.blockToSectionCoord(position.z)

		lastModelX = position.x.toFloat()
		lastModelY = position.y.toFloat()
		lastModelZ = position.z.toFloat()
		lastSectionX = sectionX
		lastSectionZ = sectionZ

		WgpuNative.setCameraSection(sectionX, sectionZ)

		WgpuNative.setMatrix(MATRIX_PROJECTION, projection)
		WgpuNative.setMatrix(MATRIX_VIEW, view)
		WgpuNative.setMatrix(MATRIX_MODEL, model)
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

		// A *moving* watched count is worth the full line - under a second apart, so the loop while the
		// world is meshed says what the watched blocks did and then stops: once they hold still, the
		// compact line is the only one left, and the last full line carries the final numbers.
		val due = reportDue(drawn, refused, slots, watched != reportedWatched)
		reportedWatched = watched

		val share = if (slots <= 0) 0L else used.toLong() * 100 / slots
		val megabytes = used.toLong() * 4 / (1024 * 1024)

		val arena = "arena %,d of %,d slot(s) handed out (%d%%, %d MB), the largest section %,d slot(s)"
			.format(used, slots, share, megabytes, largest)

		when (due) {
			null -> null
			false -> "; %,d section draw(s), %s, %,d refused".format(sections, arena, refused)
			true ->
				"; %,d section draw(s) - the solid and cutout layers of one pass -, ".format(sections) +
					"%,d fluid block(s) in the bakes, ".format(fluidBlocks) +
					"%,d fluid face(s), %,d section(s) refused by the arena, ".format(fluidQuads, refused) +
					arena +
					", bob (%.2f, %.2f)".format(lastBobX, lastBobY) +
					", bobView=$lastBobView player=$lastIsPlayer walk=%.2f".format(lastWalk) +
					"; ${RustChunkBake.fluidDiagnostics}" +
					"; ${RustChunkBake.refusedDiagnostics}" +
					(watched.takeIf { it.isNotEmpty() }?.let { "; watched $it" } ?: "") +
					"; ${describeCamera()}"
		}
	} catch (error: Throwable) {
		if (reportDue(drawn, -1, -1, false) == null) null else "; the native counters could not be read: $error"
	}

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