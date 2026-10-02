package dev.birb.wgpu.render

import java.util.concurrent.atomic.AtomicInteger

/**
 * How many times the game has rebuilt the list of sections its own occlusion culling says are visible.
 *
 * `LevelRenderer#visibleSections` is the list this renderer draws terrain from, and exactly one place
 * fills it: `LevelRenderer#applyFrustum`, which clears it and walks the occlusion octree back into it
 * (`SectionOcclusionGraph#addSectionsInFrustum`, `LevelRenderer:443-450`). The game does *not* run that
 * every frame, and the two things it runs it on are the whole meaning of this counter
 * (`LevelRenderer#cullTerrain`, `LevelRenderer:427-435`):
 *
 *  - **the camera turned by two degrees** - `floor(xRot / 2)` or `floor(yRot / 2)` differs from the value
 *    the last rebuild left behind, so a camera turning steadily rebuilds the list about 180 times per
 *    full turn and not once per frame;
 *  - **or the graph said so** - `SectionOcclusionGraph#consumeFrustumUpdate`, which is set when the
 *    graph's full update lands, or when a section compiled since the last rebuild is inside the offset
 *    frustum. That is the streaming case: a chunk that has just finished meshing.
 *
 * The rest of it is a caveat about what an empty list means. There is a third state - "the list has been
 * cleared and not yet refilled" - and an empty list cannot be made to say it, because to the native side an
 * empty list is `Some(empty)`: *the game looked and saw nothing*. That state is reachable from
 * `LevelRenderer#allChanged`, which clears without refilling (a render-distance change, a resource reload,
 * the way into a world) and leaves the list empty until the graph's next update lands; the clear inside
 * `applyFrustum` is followed by its refill in the same call on the render thread, so nothing outside can
 * observe that one. Nothing here can express "not yet", so the reader must not *send* that state; see
 * [TerrainPass.sendVisibleSections], the only reader.
 *
 * **[VisibleSectionsMixin] is the writer**: one `@Inject` at the tail of `applyFrustum`, which is the
 * moment the list is new. A counter rather than a boolean, because a reader that is a frame behind has to
 * be able to tell - it keeps the revision its keys were taken at and rebuilds only when the two differ.
 */
object VisibleSectionsRevision {

	/**
	 * The count, and an atomic rather than a plain `var`.
	 *
	 * `applyFrustum` throws if it is not the render thread and the reader is on that same thread, so
	 * nothing here needs to be atomic today - but the cost is one lock prefix on a path that runs a
	 * handful of times a second, and what a lost increment would buy is a list the native side is never
	 * told about: terrain that stops being drawn until the camera happens to cross another two degrees.
	 */
	private val revisions = AtomicInteger()

	/** The list is new. Called at the tail of `LevelRenderer#applyFrustum`. */
	@JvmStatic
	fun rebuilt() {
		revisions.incrementAndGet()
	}

	/** The revision the list is at, which is what the reader compares against the one it last sent. */
	@JvmStatic
	fun value(): Int = revisions.get()
}