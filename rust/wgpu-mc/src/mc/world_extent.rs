//! The box the game says its world occupies: how far the view reaches, and which section layers exist.
//!
//! This is the half of the direction-aware walk (`crate::render::section_graph`) that
//! `crate::mc::visibility` cannot supply. That store says what the game resolved for a section - and it
//! is *silent* about every section Minecraft never compiled, which is every section that is all air. A
//! walk that read that silence as a wall propagates through rock and is stopped by sky; a walk that
//! reads it as open air, which is the rule the walk uses, has no boundary at all, because "open" is then
//! the answer for every position in the frustum - above the build limit, below the level, and out to the
//! far plane.
//!
//! What is missing is not per-section data, it is the world's own extent, and the game has it in one
//! place: `ViewArea`. Minecraft's chunk view is a `(2 * viewDistance + 1)` square of chunk columns around
//! the camera, spanning every section layer of the level - and its own occlusion graph refuses to leave
//! that box: `SectionOcclusionGraph#getRelativeFrom` asks `ChunkTrackingView#isInViewDistance` and the
//! level's height before it will look at a neighbour at all, and `ViewArea#containsSection` is the same
//! square in the same coordinates. The walk here is bounded by the same three numbers.
//!
//! Three atomics rather than a lock: the writer is the frame (once per frame at most) and the reader is
//! the walk, which asks this once per polled node. A lock per poll is the cost this exists to avoid, and
//! the three values are set together by one thread and read by another, so a poll that catches two of
//! them from one frame and the third from the next is a section at the very edge of the view - which is
//! what the next frame's walk settles anyway.
//!
//! **Absent is not empty.** Until the JVM has said anything - a test, a demo, the first frame of a run -
//! `get` is `None`, and a caller must keep whatever behaviour it had. A zero-sized box would be a
//! plausible-looking answer that makes the walk draw nothing at all, which is exactly the kind of
//! silent emptiness this project has already paid for once.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use glam::IVec3;

/// How far, in sections, the game's view reaches on each side of the camera's own section.
static HORIZON: AtomicI32 = AtomicI32::new(0);

/// The level's lowest section layer, inclusive.
static MIN_SECTION_Y: AtomicI32 = AtomicI32::new(0);

/// The level's highest section layer, inclusive.
static MAX_SECTION_Y: AtomicI32 = AtomicI32::new(0);

/// Whether anything has been said about the world at all. See the module docs.
static KNOWN: AtomicBool = AtomicBool::new(false);

/// Whether the arrival has been written to the log - one line per process, because a run's log is the
/// only place that can show this bridge working: nothing draws differently until a walk is asked for.
static REPORTED: AtomicBool = AtomicBool::new(false);

/// The world's box, as three inclusive numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    /// How far the view reaches on each side of the camera's section, in sections - Minecraft's own
    /// render distance in chunks, which is the same number because a chunk is a section in x and z.
    pub horizon: i32,
    /// The level's lowest section layer, inclusive.
    pub min_section_y: i32,
    /// The level's highest section layer, inclusive.
    pub max_section_y: i32,
}

impl Extent {
    /// Whether `pos` is inside the loaded world, as seen from the camera's own section.
    ///
    /// The horizontal test is the game's own square - `ViewArea#containsSection` asks
    /// `|dx| <= viewDistance && |dz| <= viewDistance` - and not the diamond a server's chunk tracking
    /// would use. The vertical one is the level's layers: the whole column, not a distance from the
    /// camera, because that is the whole column `ViewArea` allocates sections for.
    pub fn holds(&self, pos: IVec3, camera: IVec3) -> bool {
        self.holds_the_layer(pos.y)
            && (pos.x - camera.x).abs() <= self.horizon
            && (pos.z - camera.z).abs() <= self.horizon
    }

    /// Whether one section layer is one the level has, whatever the camera is doing.
    ///
    /// The question a caller with a camera it cannot place has to ask first: a walk whose seed is above
    /// the build limit or below the level is a walk that starts outside its own box, and the honest
    /// answer there is not to walk at all. See `RenderGraph::terrain_walk_budget`.
    pub fn holds_the_layer(&self, y: i32) -> bool {
        y >= self.min_section_y && y <= self.max_section_y
    }
}

/// Records the world's box. Called by `wgpu-mc-jni`'s `terrain_world_bounds`, from the JVM's own
/// `ViewArea`.
pub fn set(horizon: i32, min_section_y: i32, max_section_y: i32) {
    HORIZON.store(horizon, Ordering::Relaxed);
    MIN_SECTION_Y.store(min_section_y, Ordering::Relaxed);
    MAX_SECTION_Y.store(max_section_y, Ordering::Relaxed);
    KNOWN.store(true, Ordering::Relaxed);

    if !REPORTED.swap(true, Ordering::Relaxed) {
        log::info!(
            "wgpu-mc: the world's own box arrived: the occlusion walk is bounded to +/- {horizon} \
             chunk(s) around the camera and to section layers {min_section_y}..={max_section_y}"
        );
    }
}

/// The world's box, or `None` while nothing has been said about it. See the module docs.
pub fn get() -> Option<Extent> {
    KNOWN.load(Ordering::Relaxed).then(|| Extent {
        horizon: HORIZON.load(Ordering::Relaxed),
        min_section_y: MIN_SECTION_Y.load(Ordering::Relaxed),
        max_section_y: MAX_SECTION_Y.load(Ordering::Relaxed),
    })
}

/// Forgets the box, for a world this side is no longer describing.
///
/// The next frame's push puts the new world's own box back, which is why this does not try to guess one:
/// a level change is followed by a chunk load, and a stale horizon is a walk that draws nothing.
pub fn clear() {
    KNOWN.store(false, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test rather than several, because the store is a `static`: tests run in parallel and share it,
    /// so what a second test could observe is this one's writes.
    #[test]
    fn the_worlds_box_is_a_square_of_layers_around_the_camera_and_absent_until_it_is_set() {
        let camera = IVec3::new(10, 6, -20);

        clear();
        assert_eq!(
            get(),
            None,
            "a world nobody has described has no box, and that is not a box of zero"
        );

        set(8, -4, 19);

        let extent = get().expect("just set");
        assert_eq!(extent.horizon, 8);
        assert_eq!(extent.min_section_y, -4);
        assert_eq!(extent.max_section_y, 19);

        // The square: the corner at the horizon is inside it and one past it is not.
        assert!(
            extent.holds(IVec3::new(18, 0, -12), camera),
            "the far corner"
        );
        assert!(!extent.holds(IVec3::new(19, 0, -12), camera), "one past it");
        assert!(
            !extent.holds(IVec3::new(18, 0, -11), camera),
            "one past it in z"
        );

        // The layers: the whole column is in the world, and nothing outside it is.
        assert!(
            extent.holds(IVec3::new(10, 19, -20), camera),
            "the top layer"
        );
        assert!(
            !extent.holds(IVec3::new(10, 20, -20), camera),
            "above the level"
        );
        assert!(!extent.holds(IVec3::new(10, -5, -20), camera), "below it");

        // And the layer question on its own, which is what a camera that cannot be placed asks.
        assert!(extent.holds_the_layer(-4));
        assert!(extent.holds_the_layer(19));
        assert!(!extent.holds_the_layer(-5));
        assert!(!extent.holds_the_layer(20));

        clear();
        assert_eq!(get(), None);
    }
}
