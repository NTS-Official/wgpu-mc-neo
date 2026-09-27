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
 * *instead of* Minecraft's own solid layer rather than beside it: two sets of the same terrain are two
 * sets of the same triangles, and the pass that draws it has to be the one the rest of the frame shares
 * its depth buffer with - which is why the takeover happens where Minecraft's own pass would have
 * been opened. So the two things this side has to get right are which pass (see [replaces]) and with
 * what camera (see [sendCameraMatrices]).
 */
object TerrainPass {
	/**
	 * The pipeline whose pass the graph draws instead.
	 *
	 * The solid layer only: the baker meshes that layer, and the cutout and translucent layers stay
	 * Minecraft's - drawn into the depth buffer this pass fills, which is the whole point of drawing it
	 * in the same pass Minecraft would have.
	 */
	private const val SOLID_TERRAIN = "minecraft:pipeline/solid_terrain"

	/** Whether the path is switched on at all. See [RustChunkBake.MARKER]. */
	fun isOn(): Boolean = RustChunkBake.isOn()

	/** Whether the graph draws the pass a pipeline belongs to. */
	/**
	 * Whether the arena has anything to draw.
	 *
	 * The solid layer's pass stays Minecraft's until it does: the graph draws the arena's contents, so a
	 * pass taken over while the arena is empty is a frame with no ground in it - the same trade [ready]
	 * makes about the pipeline, for the same reason. Asked per frame, because the answer changes as the
	 * world is meshed; the call is one lock and a length.
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
				"wgpu: the render graph is drawing the solid terrain; Minecraft's own meshes for that layer are skipped"
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
	 * Diagnostics: what the pass drew with, for the line the takeover logs.
	 *
	 * The section count says the pass had a world to draw; the fluid counts say whether the fluid
	 * mesher has anything at all - blocks counted with no faces drawn is a fluid whose sprite is not in
	 * the atlas, and no blocks at all is a fluid that never reached the baker; and the bob is what says
	 * the camera this pass draws with is the one the rest of the world bobs with, which is the whole of
	 * the last fix. Read here rather than from the Rust log because the Rust log does not reach the
	 * game's log file.
	 */
	fun describeFrame(): String = try {
		val sections = WmNative.terrainSectionsDrawn.invokeExact() as Int
		val fluidBlocks = WmNative.terrainFluidBlocks.invokeExact() as Int
		val fluidQuads = WmNative.terrainFluidQuads.invokeExact() as Int
		val refused = WmNative.terrainSectionsRefused.invokeExact() as Int

		"; $sections section(s) drawn, $fluidBlocks fluid block(s) in the bakes, " +
			"$fluidQuads fluid face(s), $refused section(s) refused by the arena, " +
			"bob (%.2f, %.2f)".format(lastBobX, lastBobY) +
			", bobView=$lastBobView player=$lastIsPlayer walk=%.2f".format(lastWalk) +
			"; ${RustChunkBake.fluidDiagnostics}" +
			"; ${describeCamera()}"
	} catch (error: Throwable) {
		"; the native counters could not be read: $error"
	}

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