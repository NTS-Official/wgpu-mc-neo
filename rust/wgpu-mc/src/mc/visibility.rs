//! What the game's occlusion graph resolved for each section: which of a section's six faces can see
//! out to which other face, as `from * 6 + to` over DOWN, UP, NORTH, SOUTH, WEST, EAST.
//!
//! This is the whole input of the direction-aware visibility walk (`crate::render::section_graph`), and
//! it arrives from the JVM inside the payload of a section bake: `wgpu-mc-jni`'s `Payload::apply` is the
//! only writer, and it reads the trailer that the JVM appends. It is kept on this side of the bridge
//! rather than the other because the walk that reads it lives here, beside the frustum and the game's
//! own visible list - and because that makes the crossing an ordinary call between two Rust crates
//! rather than a second JNI entry point.
//!
//! **A section with no answer here is not a section with no visibility.** "Nothing is visible out of
//! this section" and "nobody has said" are opposite instructions, and the second has to *expand* a walk
//! rather than prune it: a section treated as the first when it is the second is a hole in the world,
//! not a missed cull. Which of the two a payload carried is in its own encoding - the presence bit in
//! `wgpu-mc-jni`'s trailer - so what is missing here is simply "no entry".
//!
//! Zero is a legitimate answer, and the reason the store is `Option`-shaped by absence rather than by a
//! sentinel: a section you cannot see out of at all is exactly zero, and it is the most culled section
//! there is.
//!
//! A `static` for the reason the section cache is one: the writer is a chunk-build thread reached from
//! a JNI frame and the reader is the render thread, with no handle to pass between them.

use std::collections::HashMap;
use std::sync::LazyLock;

use glam::IVec3;
use parking_lot::RwLock;

/// Every answer that has been sent, by section.
static ANSWERS: LazyLock<RwLock<HashMap<IVec3, u64>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// How many answers are held before a trim is worth the scan it costs.
///
/// An answer is eight bytes, so this is a bound on the *bookkeeping* rather than on anything large - but
/// a session that crosses a continent names every section it passes, and an unbounded map is an
/// unbounded map.
const SOFT_LIMIT: usize = 8192;

/// How far a section may be, in chunks, before its answer is dropped.
///
/// The same radius the block cache uses, and for the same reason: what matters is the ring the walk can
/// reach, and a section outside it is one whose answer is re-sent by whichever payload mentions it next
/// - which is what happens the moment the player comes back, since coming back is what re-bakes it.
const KEEP_RADIUS: i32 = 48;

/// Records what one section's occlusion graph resolved to.
///
/// Not counted as missing by `wgpu-mc-jni`'s `WorldSections::missing`, deliberately: a section whose
/// blocks are here and whose answer is not is a section that can still be meshed, and this is only ever
/// read to decide what to *draw* - so asking for the whole neighbourhood again over one would trade a
/// missed cull for a payload of 27 sections.
pub fn set(pos: IVec3, pair: u64) {
    ANSWERS.write().insert(pos, pair);
}

/// What one section's graph resolved to, or `None` where nothing has been said about it.
pub fn get(pos: IVec3) -> Option<u64> {
    ANSWERS.read().get(&pos).copied()
}

/// How many sections have an answer. For the report, and for the cross-check's census.
pub fn len() -> usize {
    ANSWERS.read().len()
}

/// A copy of every answer, for a walk that must not hold the lock while it runs.
///
/// One read lock and a clone of a few thousand small entries, once per walk. The alternative - a lock
/// per polled section - is a lock taken thousands of times for a walk that runs once a second.
pub fn snapshot() -> HashMap<IVec3, u64> {
    ANSWERS.read().clone()
}

/// Drops the answers of sections far from `center`, once there are enough of them to be worth the scan.
pub fn trim(center: IVec3) {
    let mut answers = ANSWERS.write();

    if answers.len() < SOFT_LIMIT {
        return;
    }

    answers.retain(|pos, _| (pos.x - center.x).abs().max((pos.z - center.z).abs()) <= KEEP_RADIUS);
}

/// Drops every answer, for a world this side is no longer describing.
pub fn clear() {
    ANSWERS.write().clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test rather than several, because the store is a `static`: tests run in parallel and share
    /// it, so what a second test could observe is this one's writes.
    #[test]
    fn an_answer_is_kept_by_position_and_cleared_with_the_world() {
        let here = IVec3::new(4, 5, 6);
        let there = IVec3::new(-7, 8, 9);

        clear();
        assert_eq!(
            get(here),
            None,
            "nothing has been said about a section nobody wrote about"
        );

        // Zero is an answer - a section you cannot see out of - and not the same as silence.
        set(here, 0);
        set(there, 0b11);

        assert_eq!(get(here), Some(0), "an answer of zero is an answer");
        assert_eq!(get(there), Some(0b11));
        assert_eq!(len(), 2);
        assert_eq!(snapshot().get(&here), Some(&0));

        clear();
        assert_eq!(get(here), None);
        assert_eq!(len(), 0);
    }
}
