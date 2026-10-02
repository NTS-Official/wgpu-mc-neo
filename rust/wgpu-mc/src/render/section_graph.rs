//! A direction-aware flood over the sections a renderer holds, ported from VulkanMod's `SectionGraph`.
//!
//! The question it answers is **occlusion**, not the frustum: given the game's own compiled visibility
//! for every section - a 6x6 matrix of "from this face you can see out of that face" - which sections
//! can the camera actually reach, walking face to face through sections that do not stop the walk?
//! Vanilla answers it too, in its `SectionOcclusionGraph`, and this renderer has been *consuming* that
//! answer (the `visibleSections` list the JVM pushes). The flood is the same answer computed here, for
//! the same reason VulkanMod computes it: so the traversal can be run against this side's own view of
//! the world, with a budget that is a number rather than a property of the game's graph.
//!
//! **What is ported, exactly.** `SectionGraph.updateRenderChunks` and `visitAdjacentNodes`/`addNode`
//! (`references/VulkanMod/.../graph/SectionGraph.java:164-299`), with the propagation rules kept as
//! they are rather than reinterpreted:
//!
//! - a node's **`directions`** are the faces it may still propagate out of: the parent's set with the
//!   face pointing back at the parent removed (so the flood never steps straight back), then
//!   intersected with what the visibility says is possible and with the neighbours that exist;
//! - a node's **`source_steps`** are the step directions it was reached by, **in the parent's
//!   convention** - the face the parent moved along - which is why the seed's is a spare bit rather
//!   than a face: the first step off the camera has no direction it could have been reached by;
//! - **`direction_changes`** counts *turns*: a step costs one when its direction is not one this node
//!   was reached by **and** the node it is leaving holds something. An empty section is free to turn
//!   in, which is what makes a cave mouth a way through rather than a wall;
//! - **within [`STEP_GUARD`] sections of the camera a turn is free**, which is the original's
//!   `steps < 10 ? 0 : 127` - a real rule rather than the guard it looks like, and the reason `steps`
//!   is kept at all;
//! - a node's **visibility is settled on its first visit**, and a later visit only remembers another way
//!   in and refines `direction_changes` (taking the minimum). That makes the result depend on the order
//!   the queue is walked in, which the original has too, and it is preserved here rather than corrected;
//! - **but the way out of a node is asked once per way in, which is the game's rule and not the
//!   original's.** VulkanMod reads the visibility of the single face `mainDir` names; Minecraft remembers
//!   every source direction and accepts a step if *any* of them can see that way. See
//!   [`Node::exits_through`] for the rule and for why this one reference is the game;
//! - a node over the budget is **polled but neither expanded nor drawn**.
//!
//! **Where this deliberately differs, and why.**
//!
//! - **The frustum test is the caller's, asked as a third question.** The original tests each polled
//!   node against the frustum, with a per-area three-state table in front of it (`notInFrustum`,
//!   `:210-219`), and the test is asked **first** - so a node outside the frustum is neither drawn nor
//!   walked out of, and the budget is not spent on it. This module keeps the occlusion rules; which
//!   volume the camera can see is a different question and lives where the frustum does, so the caller
//!   answers it through [`SectionSource::in_frustum`]. A caller that always answers `true` draws what
//!   is behind the camera, which is what the default and every test here do.
//! - **A section with no visibility is open, and this one was measured rather than reasoned.** The
//!   original walks its own grid, where every loaded chunk is present and an air section is
//!   present-and-empty. This side has no such grid - it has the arena, which holds what was *meshed* -
//!   and the first version of this rule read "no answer" as "a wall", on the argument that a section the
//!   walk cannot judge is one it should not walk through.
//!
//!   **That was wrong, and a live run said so.** Minecraft never compiles a section that is all air, so no
//!   payload carries an answer for one - and sight lines go through air, not through rock. The walk
//!   therefore propagated through the terrain and was stopped by the sky: it drew 710 of the 1487 sections
//!   the game named, and the picture was missing ground wherever the camera looked across open air. The
//!   rule is now the other way round, and what bounds the walk is the world's own box and then the
//!   frustum, both asked before anything else.
//! - **The world's own edge is the caller's too, and it is asked before the frustum.** The original has
//!   no such question because its grid *is* the world: every section in its render distance box exists
//!   and a neighbour outside it is `null`. Here the arena holds what was *meshed* and the frustum covers
//!   empty space, so "is there a world here at all" has to be asked separately - through
//!   [`SectionSource::in_world`], answered from the game's own `ViewArea` (see `crate::mc::world_extent`).
//!   Without it, "no answer means air" makes the sky above the build limit and the void below the level
//!   part of the world, and the walk fills the whole frustum cone with them: 770,568 sections polled per
//!   walk, against 482 for the same walk bounded to the sections the game actually has.
//! - **The camera's own section takes the union of every row** (`NO_STEP`), where the original leaves
//!   `mainDir` at whatever the frame before left in it. The union is what "was not entered through
//!   anything" means, and it does not depend on the frame before.
//! - **The packed visibility is `d1 * 6 + d2`**, not the original's `d1 * 8 + d2`: six faces need six
//!   bits and the two spare ones buy nothing. And the matrix is read from the game's own
//!   `VisibilitySet` rather than from an `@Overwrite` of it, so `set(.., false)` still clears.
//!
//! **The walk decides only when the setting asks it to.** `RenderGraph::rebuild_terrain_frame` draws
//! from the flood while `adv_culling` is above zero and from the game's own `visibleSections` list
//! otherwise, and `cross_check_the_flood` measures the two against each other once a second while the
//! setting is off. See `Settings::adv_culling` and `ADV_CULLING` in `render/graph.rs`.

use glam::IVec3;
use std::collections::{HashMap, VecDeque};

/// The six faces, in `Direction.ordinal()` order: down, up, north, south, west, east.
///
/// That order is not arbitrary here: opposite faces are adjacent in it, so [`opposite`] is `face ^ 1`,
/// and it is the order the packed visibility is indexed by.
pub const FACES: usize = 6;

/// Every face, as a mask.
const ALL_FACES: u8 = (1 << FACES) - 1;

/// The step taken by a section the flood did not step into: the camera's own.
const NO_STEP: usize = FACES;

/// How deep the turn budget starts counting. See the module docs, and its use in [`flood`].
const STEP_GUARD: u8 = 10;

/// The opposite face, which `Direction.ordinal()` order makes a single exclusive-or.
pub const fn opposite(face: usize) -> usize {
    face ^ 1
}

/// Which way a step along `face` moves, in sections.
pub const fn face_offset(face: usize) -> IVec3 {
    match face {
        0 => IVec3::new(0, -1, 0),
        1 => IVec3::new(0, 1, 0),
        2 => IVec3::new(0, 0, -1),
        3 => IVec3::new(0, 0, 1),
        4 => IVec3::new(-1, 0, 0),
        _ => IVec3::new(1, 0, 0),
    }
}

/// A section's compiled visibility: bit `from * FACES + to` is set when the game found a line of sight
/// from face `from` out of face `to`.
///
/// This is `net.minecraft.client.renderer.chunk.VisibilitySet` flattened, and the flattening is the
/// whole reason it is a `u64`: 36 bits of a symmetric 6x6 matrix, carried as one value instead of a
/// `BitSet` of 64.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Visibility(u64);

impl Visibility {
    /// No sight line between any pair of faces: a wall, for the flood's purposes.
    pub const NONE: Visibility = Visibility(0);

    /// The game's `setAll(true)`: every pair sees every other.
    pub const EVERY_PAIR: Visibility = Visibility((1u64 << (FACES * FACES)) - 1);

    /// From the bits as they are packed. See the type.
    pub const fn from_bits(bits: u64) -> Self {
        Visibility(bits)
    }

    /// The bits, as packed. See the type.
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// A sight line both ways between two faces, which is what a corridor or open sky gives.
    pub const fn pair(a: usize, b: usize) -> Self {
        Visibility((1u64 << (a * FACES + b)) | (1u64 << (b * FACES + a)))
    }

    /// Whether there is a sight line from `from` out of `to`.
    pub const fn has(self, from: usize, to: usize) -> bool {
        self.0 & (1u64 << (from * FACES + to)) != 0
    }

    /// **The faces anything can be seen out of, given that the sight line started at `from`.**
    pub const fn row(self, from: usize) -> u8 {
        let mut faces = 0u8;
        let mut to = 0;

        while to < FACES {
            if self.has(from, to) {
                faces |= 1 << to;
            }

            to += 1;
        }

        faces
    }

    /// **The faces this section can be left through**, having been entered by a step along `step`.
    ///
    /// The row is the *opposite* face, because the game compiles a section's visibility in terms of the
    /// face a sight line leaves through, and the face back toward where the flood came from is the one
    /// it entered by - so "seen from the face I came in at" is exactly "the row of the opposite face".
    ///
    /// `NO_STEP` is the camera's own section, which was not entered through anything: every row is
    /// unioned, so anything visible at all is a way out. That is the one place this port makes a choice
    /// where the original leans on a stale field - see the module docs.
    pub const fn exits_from(self, step: usize) -> u8 {
        if step >= FACES {
            return self.exits_from_anywhere();
        }

        self.row(opposite(step))
    }

    /// Every face anything is visible through, over all six entry faces. See [`Visibility::exits_from`].
    pub const fn exits_from_anywhere(self) -> u8 {
        let mut faces = 0u8;
        let mut from = 0;

        while from < FACES {
            faces |= self.row(from);
            from += 1;
        }

        faces
    }
}

/// What the flood can learn about the world: the game's visibility for a section, and whether that
/// section holds anything worth drawing.
pub trait SectionSource {
    /// The compiled visibility for one section, or `None` when this side has none - which stops the
    /// flood, because a section it cannot judge is one it cannot walk through. See the module docs.
    fn visibility(&self, pos: IVec3) -> Option<Visibility>;

    /// Whether the section holds nothing to draw. An empty section is walked *through* and never drawn,
    /// and it can be turned in for free - see the module docs on `direction_changes`.
    fn is_empty(&self, pos: IVec3) -> bool;

    /// Whether the section is inside the camera's frustum.
    ///
    /// Asked before anything else about a polled node, so `false` means neither drawn nor walked out of:
    /// it is what bounds the walk's cost in the direction the camera can see, and what keeps a section
    /// behind the camera from being reported as something to draw.
    ///
    /// The default is "everything is", which is what a test wants and what a caller with no camera has
    /// to say - it is also the honest answer for a caller that has already restricted the start to
    /// something inside the frustum, since the question is only ever asked about what the walk reached.
    fn in_frustum(&self, _pos: IVec3) -> bool {
        true
    }

    /// Whether the section is inside the world at all, rather than a position in the empty space the
    /// frustum happens to cover.
    ///
    /// Asked before the frustum test and on the same terms: `false` means the node is neither drawn nor
    /// walked out of. The two are the walk's whole boundary and they answer different questions - the
    /// world's box is "is there a section here at all", the frustum is "is it in view" - and the box is
    /// asked first because it is the cheaper test and because a position outside the world is not a cull
    /// but the end of the world. See `crate::mc::world_extent` for where the box comes from.
    ///
    /// The default is "everything is", which is what a test wants: a test's world is the map it hands
    /// over, and a position that map has no answer for is already a wall by [`Self::visibility`].
    fn in_world(&self, _pos: IVec3) -> bool {
        true
    }
}

/// What one flood produced.
#[derive(Debug, Default, Clone)]
pub struct Flood {
    /// The sections the flood reached that hold something to draw, in the order the queue settled
    /// them. That is not a useful order for anything but a test; a caller that draws sorts its own way.
    pub visible: Vec<IVec3>,
    /// How many sections were taken off the queue. A section reached by several routes is polled once.
    pub polled: usize,
    /// How many of those were over the budget, and were therefore neither expanded nor drawn.
    pub over_budget: usize,
    /// How many were outside the caller's frustum, and were therefore neither drawn nor walked out of.
    ///
    /// The count is the frustum's share of the work the walk did *not* do, which is the number that says
    /// whether the caller's frustum is doing anything: at zero, every node this side polled was inside it.
    pub out_of_frustum: usize,
    /// How many were outside the world the caller describes, and were therefore neither drawn nor walked
    /// out of. See [`SectionSource::in_world`].
    ///
    /// Counted separately from [`Self::out_of_frustum`] because the two are answers to different
    /// questions, and the pair of them is what says which bound the walk is actually paying for: a walk
    /// whose refused nodes are all behind the camera is one the frustum is bounding, and a walk whose
    /// refused nodes are above the build limit is one the world's own box is.
    pub outside_the_world: usize,
}

/// One section's state for the frame this flood is running in. See the module docs for each field.
#[derive(Debug, Clone, Copy)]
struct Node {
    visibility: Visibility,
    directions: u8,
    /// Every face the flood moved along to reach this section, or `1 << NO_STEP` for the camera's own.
    ///
    /// A set rather than the one face the first route came in through, because the step *out* of this
    /// node is asked once per way in. See [`Node::exits_through`].
    source_steps: u8,
    direction_changes: u8,
    steps: u8,
}

impl Node {
    /// Whether this node can be left along `face`, over **every** way it was reached.
    ///
    /// **Minecraft's rule, and the one place this port deliberately leaves the original.** VulkanMod
    /// settles a node on its first visit: `mainDir` is the single face it was reached by then, and a later
    /// way in only re-prices the node. Minecraft's own graph remembers *every* source direction
    /// (`SectionOcclusionGraph.Node#addSourceDirection`) and accepts a step out of the node if **any** of
    /// them can see that way (`facesCanSeeEachother(source.getOpposite(), direction)`) - the same question,
    /// asked once per way in rather than once per node.
    ///
    /// The difference is a whole class of section: one entered through a face that cannot see the way the
    /// walk needs to go, while another way in can see it. What this walk is measured against is not the
    /// original's cull performance but the game's own visible list - `cross_check_the_flood` compares the
    /// two - so where the two references disagree, the game's rule is the one that is kept.
    fn exits_through(&self, face: usize) -> bool {
        let mut steps = self.source_steps;

        while steps != 0 {
            let step = steps.trailing_zeros() as usize;
            steps &= steps - 1;

            let open = if step >= FACES {
                // The camera's own section was not entered through anything, so anything visible at all
                // is a way out. See [`Visibility::exits_from`].
                self.visibility.exits_from_anywhere()
            } else if self.visibility.has(opposite(step), face) {
                1 << face
            } else {
                0
            };

            if open & (1 << face) != 0 {
                return true;
            }
        }

        false
    }
}

/// Walks out from `start`, drawing what it reaches, with at most `max_direction_changes` turns.
///
/// That budget is VulkanMod's `CONFIG.advCulling - 1`: the knob the whole approximation hangs on, where
/// every extra turn reaches further around corners at the cost of visiting more of the world. A flood
/// with no budget at all visits what the frustum holds; the budget is what makes it cheap, and what
/// makes a too-small one leave holes behind a hill's shoulder.
pub fn flood<S: SectionSource>(source: &S, start: IVec3, max_direction_changes: u8) -> Flood {
    let mut result = Flood::default();

    let mut nodes: HashMap<IVec3, Node> = HashMap::new();
    let mut queue: VecDeque<IVec3> = VecDeque::new();

    let Some(seed) = source.visibility(start) else {
        return result;
    };

    nodes.insert(
        start,
        Node {
            visibility: seed,
            directions: ALL_FACES,
            // The spare bit again, for the same reason as `NO_STEP`: a step whose direction is not one
            // this node was reached by is a turn, and the camera's own section was reached by nothing.
            source_steps: 1 << NO_STEP,
            direction_changes: 0,
            steps: 0,
        },
    );
    queue.push_back(start);

    while let Some(pos) = queue.pop_front() {
        result.polled += 1;

        let node = nodes[&pos];

        // **The world's own edge first.** A position the caller's box does not hold is not a section at
        // all - it is the empty space the frustum also covers, above the build limit and below the level
        // included - so the walk stops there rather than counting it as something behind the camera. See
        // [`SectionSource::in_world`].
        if !source.in_world(pos) {
            result.outside_the_world += 1;
            continue;
        }

        // **And then the frustum, which is the caller's answer.** The original tests every polled node
        // against the frustum before anything else, and a node outside it is neither drawn nor walked
        // out of - so the budget below is never spent on what the camera cannot see, which is most of
        // what makes the walk cost anything.
        if !source.in_frustum(pos) {
            result.out_of_frustum += 1;
            continue;
        }

        // **Over budget is polled and dropped.** The original checks this after the frustum test and
        // before everything else, so such a section is neither counted as visible nor walked out of -
        // its neighbours are simply not reached, which is the whole of what the budget does.
        if node.direction_changes > max_direction_changes {
            result.over_budget += 1;
            continue;
        }

        let empty_here = source.is_empty(pos);

        if !empty_here {
            result.visible.push(pos);
        }

        for face in 0..FACES {
            if node.directions & (1 << face) == 0 {
                continue;
            }

            // **Every way in is a way to look out.** See [`Node::exits_through`] for the rule, and for why
            // this is the game's rule rather than the original's.
            if !node.exits_through(face) {
                continue;
            }

            let next = pos + face_offset(face);

            // No visibility for the neighbour is no way through it. See the module docs.
            let Some(visibility) = source.visibility(next) else {
                continue;
            };

            let steps = node.steps.saturating_add(1);

            // **A turn costs one unless this step is one the node was reached by, or the node it is
            // leaving is empty.** `source_steps` is in the parent's convention - the step direction -
            // which is why it is `face` here and `opposite(face)` in `directions` above.
            let turns = if node.source_steps & (1 << face) == 0 && !empty_here {
                node.direction_changes.saturating_add(1)
            } else {
                node.direction_changes
            };

            match nodes.get_mut(&next) {
                // **First visit settles it**: the route that got here first is the one the section's own
                // state comes from. See the module docs for why a later, cheaper route does not reopen
                // it, and `a_route_found_later_does_not_reopen_a_section` for the assertion that says so.
                None => {
                    nodes.insert(
                        next,
                        Node {
                            visibility,
                            // The face that points back at the parent, so the flood cannot turn round.
                            directions: node.directions & !(1 << opposite(face)),
                            source_steps: 1 << face,
                            direction_changes: if steps < STEP_GUARD { 0 } else { turns },
                            steps,
                        },
                    );
                    queue.push_back(next);
                }
                // **And a later visit only refines the price.** Another way in is remembered, and the
                // turns taken are the cheapest of every route found - which is what decides whether the
                // section survives the budget, since one polled before a cheaper route was found keeps
                // the dearer one. The original behaves the same way.
                Some(seen) => {
                    seen.source_steps |= 1 << face;
                    seen.direction_changes = seen.direction_changes.min(turns);
                }
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// A world of open sections, minus the ones the camera cannot see.
    ///
    /// Its own source rather than a flag on `TestSections`, because the frustum is a question the other
    /// tests deliberately do not answer: leaving their default alone is what keeps their expectations
    /// about the occlusion rules readable.
    struct HalfOpenWorld {
        visibility: HashMap<IVec3, Visibility>,
        outside: HashSet<IVec3>,
    }

    impl SectionSource for HalfOpenWorld {
        fn visibility(&self, pos: IVec3) -> Option<Visibility> {
            self.visibility.get(&pos).copied()
        }

        fn is_empty(&self, _pos: IVec3) -> bool {
            false
        }

        fn in_frustum(&self, pos: IVec3) -> bool {
            !self.outside.contains(&pos)
        }
    }

    /// The frustum is asked **first**, so what it rejects is not drawn and not walked through either.
    ///
    /// This is the difference between "the camera cannot see it" and "the camera cannot get to it", and
    /// the walk has to treat them the same way: a section outside the frustum that *were* walked through
    /// would hand the flood the whole world behind the camera, one step at a time.
    #[test]
    fn a_section_outside_the_frustum_is_neither_drawn_nor_walked_through() {
        let open = Visibility::EVERY_PAIR;
        let mut visibility = HashMap::new();

        for x in 0..3 {
            visibility.insert(IVec3::new(x, 0, 0), open);
        }

        // One section past the camera and one past that: the far one is only reachable through the near
        // one, and the near one is behind the camera.
        let source = HalfOpenWorld {
            visibility,
            outside: HashSet::from([IVec3::new(1, 0, 0)]),
        };

        let seen = flood(&source, IVec3::new(0, 0, 0), 0);

        assert_eq!(
            seen.visible,
            vec![IVec3::new(0, 0, 0)],
            "the section outside the frustum is not drawn, and the one past it is never reached"
        );
        assert_eq!(
            seen.out_of_frustum, 1,
            "and the walk counted what it rejected"
        );
    }

    /// A world a test can state exactly: which sections have visibility, and which of those are empty.
    struct TestSections {
        visibility: HashMap<IVec3, Visibility>,
        empty: HashSet<IVec3>,
    }

    impl TestSections {
        /// A run of `count` sections along +X, every one of them a corridor open west to east.
        fn corridor_along_x(count: i32, visibility: Visibility) -> Self {
            let mut sections = TestSections {
                visibility: HashMap::new(),
                empty: HashSet::new(),
            };

            for x in 0..count {
                sections.visibility.insert(IVec3::new(x, 0, 0), visibility);
            }

            sections
        }

        fn with(mut self, pos: IVec3, visibility: Visibility) -> Self {
            self.visibility.insert(pos, visibility);
            self
        }

        fn empty(mut self, pos: IVec3) -> Self {
            self.empty.insert(pos);
            self
        }
    }

    impl SectionSource for TestSections {
        fn visibility(&self, pos: IVec3) -> Option<Visibility> {
            self.visibility.get(&pos).copied()
        }

        fn is_empty(&self, pos: IVec3) -> bool {
            self.empty.contains(&pos)
        }
    }

    /// The same world with an edge: sections inside `inside` are as [`TestSections`] says, and every
    /// other position is not the world at all.
    ///
    /// This is what a caller with the game's own `ViewArea` answers - see `crate::mc::world_extent` - and
    /// what makes "no answer means air" survivable: the air is only open where the game has a world.
    struct BoundedWorld {
        world: TestSections,
        inside: HashSet<IVec3>,
    }

    impl SectionSource for BoundedWorld {
        fn visibility(&self, pos: IVec3) -> Option<Visibility> {
            self.world.visibility(pos)
        }

        fn is_empty(&self, pos: IVec3) -> bool {
            self.world.is_empty(pos)
        }

        fn in_world(&self, pos: IVec3) -> bool {
            self.inside.contains(&pos)
        }
    }

    /// **A way in the first route did not have is a way out.** The rule here is the game's and not the
    /// original's, and it is asked of a node directly because the queue makes the difference hard to stage:
    /// with one seed and a first-in-first-out queue, a second route to a node usually arrives after that
    /// node has been polled, so what this pins is the rule rather than a whole world that needs it.
    #[test]
    fn a_node_is_left_through_every_way_it_was_reached() {
        // From the south face this section can see east; from the west face it can see nothing at all.
        let visibility = Visibility::pair(3, 5);

        let by_the_first_route = Node {
            visibility,
            directions: ALL_FACES,
            // Reached by a step east, so entered through its own west face - which cannot see east.
            source_steps: 1 << 5,
            direction_changes: 0,
            steps: 1,
        };

        assert!(
            !by_the_first_route.exits_through(5),
            "one way in, and it is a dead end: this is what the original's `mainDir` would leave the \
             node at"
        );

        let by_another_too = Node {
            // And reached by a step north as well, which enters it through its south face.
            source_steps: (1 << 5) | (1 << 2),
            ..by_the_first_route
        };

        assert!(
            by_another_too.exits_through(5),
            "one way in that can see the way out is enough, which is the game's rule"
        );
        assert!(
            !by_another_too.exits_through(4),
            "and a face neither way in can see is still refused"
        );

        let the_camera = Node {
            source_steps: 1 << NO_STEP,
            ..by_the_first_route
        };

        assert!(
            the_camera.exits_through(5) && the_camera.exits_through(3),
            "the camera's own section was not entered through anything, so every face it can see out of \
             is a way out - unioned over the rows"
        );
        assert!(
            !the_camera.exits_through(0),
            "and one it cannot see out of is not"
        );
    }

    /// A corridor open from west to east: face 4 is west and face 5 is east. See [`FACES`].
    fn west_to_east() -> Visibility {
        Visibility::pair(4, 5)
    }

    fn reached(flooded: &Flood) -> Vec<IVec3> {
        let mut visible = flooded.visible.clone();
        visible.sort_by_key(|pos| (pos.x, pos.y, pos.z));
        visible
    }

    /// A straight line of open sections is walked to its end, and the walk never steps back: the section
    /// it came from is not reached a second time, because the parent's directions carry the face back
    /// toward it removed.
    #[test]
    fn a_straight_corridor_is_walked_once_and_never_back() {
        let world = TestSections::corridor_along_x(5, west_to_east());

        let flooded = flood(&world, IVec3::ZERO, 0);

        assert_eq!(
            reached(&flooded),
            (0..5).map(|x| IVec3::new(x, 0, 0)).collect::<Vec<_>>(),
            "every section of the corridor is reached, and none of them twice"
        );
        assert_eq!(
            flooded.polled, 5,
            "polled once each: the face back toward the section the flood came from is removed from \
             `directions`, so the corridor is not walked backwards"
        );
        assert_eq!(flooded.over_budget, 0);
    }

    /// The walk stops at the world's own edge, and what it refuses there is counted as the edge rather
    /// than as something behind the camera.
    ///
    /// The corridor is five sections long and the world holds three of them. The two past the edge are
    /// still *reachable* - they are in the map, so [`SectionSource::visibility`] answers for them - which
    /// is the case this pins: the box is what refuses them, one poll each, and the expansion stops there.
    #[test]
    fn the_walk_stops_at_the_worlds_own_edge() {
        let world = TestSections::corridor_along_x(5, west_to_east());
        let inside = (0..3).map(|x| IVec3::new(x, 0, 0)).collect();

        let flooded = flood(&BoundedWorld { world, inside }, IVec3::ZERO, 0);

        assert_eq!(
            reached(&flooded),
            (0..3).map(|x| IVec3::new(x, 0, 0)).collect::<Vec<_>>(),
            "the three sections the world holds are drawn, and the two past its edge are not"
        );
        assert_eq!(
            flooded.polled, 4,
            "and the first section past the edge was polled before it was refused: the walk does not \
             know where the world ends until it looks"
        );
        assert_eq!(
            flooded.outside_the_world, 1,
            "which is the world's edge, and not something behind the camera"
        );
        assert_eq!(flooded.out_of_frustum, 0);
    }

    /// A section that sees nothing out of any face is reached, drawn if it holds anything, and **not
    /// walked through**: it is a wall, and what is behind it is not reached at all.
    #[test]
    fn a_section_that_sees_nothing_out_of_any_face_is_a_wall() {
        let world = TestSections::corridor_along_x(5, west_to_east())
            .with(IVec3::new(2, 0, 0), Visibility::NONE);

        let flooded = flood(&world, IVec3::ZERO, 4);

        assert_eq!(
            reached(&flooded),
            vec![
                IVec3::new(0, 0, 0),
                IVec3::new(1, 0, 0),
                IVec3::new(2, 0, 0)
            ],
            "the wall itself is reached and drawn - it holds geometry - and nothing past it is"
        );
        assert_eq!(flooded.polled, 3);
    }

    /// **The rule the whole approximation turns on**: a turn costs one direction change, unless the
    /// section being left is empty - which is what makes a cave mouth a way through and a corner of
    /// solid ground a corner the flood has to pay for.
    #[test]
    fn a_turn_is_free_through_an_empty_section_and_costs_one_through_a_solid_one() {
        // **Eleven sections long on purpose**: within `STEP_GUARD` sections of the camera a turn is free,
        // so a budget of zero can only be seen to bite past that depth. West to east through (0,0,0) to
        // (10,0,0), and the turn north out of the last - which is the second step
        // named above. North is face 2 and south is face 3. See [`FACES`].
        let corner = Visibility::from_bits(
            west_to_east().bits()
                | Visibility::pair(4, 2).bits()
                | Visibility::pair(5, 2).bits()
                | Visibility::pair(5, 3).bits(),
        );

        // The section the turn reaches, walkable in its own right so that only the budget can stop it.
        let turned_to = IVec3::new(10, 0, -1);

        let solid = TestSections::corridor_along_x(11, corner).with(turned_to, west_to_east());
        let air = TestSections::corridor_along_x(11, corner)
            .with(turned_to, west_to_east())
            .empty(IVec3::new(10, 0, 0));

        // With no budget at all, only the free route survives it.
        let through_solid = flood(&solid, IVec3::ZERO, 0);

        assert!(
            !through_solid.visible.contains(&turned_to),
            "the turn was in a section that holds geometry, so it cost a direction change and the \
             budget refused it"
        );
        assert_eq!(through_solid.over_budget, 1);

        let through_air = flood(&air, IVec3::ZERO, 0);

        assert!(
            through_air.visible.contains(&turned_to),
            "the same turn, in an empty section, is free - which is the whole reason a cave mouth is a \
             way through"
        );
        assert_eq!(through_air.over_budget, 0);

        // And with one change to spend, the solid turn is affordable.
        let with_budget = flood(&solid, IVec3::ZERO, 1);

        assert!(with_budget.visible.contains(&turned_to));
        assert_eq!(with_budget.over_budget, 0);
    }

    /// A section reached by a cheaper route after it was already polled keeps the price it settled at,
    /// which is the order dependence the original has. This pins it, so that any later change to it is a
    /// decision rather than an accident.
    #[test]
    fn a_route_found_later_does_not_reopen_a_section() {
        let world = TestSections::corridor_along_x(3, west_to_east());

        let flooded = flood(&world, IVec3::ZERO, 1);

        assert_eq!(
            flooded.polled, 3,
            "each section is polled once, whoever found it"
        );
        assert_eq!(flooded.over_budget, 0);
        assert_eq!(flooded.visible.len(), 3);
    }

    /// **A turn within [`STEP_GUARD`] sections of the camera is free**, whatever it is made of. The
    /// original's `steps < 10 ? 0 : 127` reads like a guard against a runaway walk, and it is not one: it
    /// is what lets the first few corners - a cave mouth, a doorway, the shoulder of the hill the camera
    /// is standing on - be walked through without spending the budget. This asserts the boundary from the
    /// inside; `a_turn_is_free_through_an_empty_section_and_costs_one_through_a_solid_one` is the outside,
    /// and its corridor is eleven sections long for exactly this reason.
    #[test]
    fn a_turn_within_the_step_guard_is_free_even_through_solid_ground() {
        // The same corner as that test, but three sections from the camera rather than eleven.
        let corner = Visibility::from_bits(
            west_to_east().bits()
                | Visibility::pair(4, 2).bits()
                | Visibility::pair(5, 2).bits()
                | Visibility::pair(5, 3).bits(),
        );

        let turned_to = IVec3::new(2, 0, -1);

        // Every section of the corridor holds geometry, so the turn is as dear as it ever gets.
        let world = TestSections::corridor_along_x(3, corner).with(turned_to, west_to_east());

        let flooded = flood(&world, IVec3::ZERO, 0);

        assert!(
            flooded.visible.contains(&turned_to),
            "inside the guard the turn costs nothing even through solid ground, so a budget of zero \
             still reaches it"
        );
        assert_eq!(flooded.over_budget, 0);
    }

    /// The flood walks down as readily as it walks along. The face order is `Direction.ordinal()`'s, so
    /// down and up are faces 0 and 1 - which is what makes a shaft, a cave and a doorway the same case as
    /// a corridor, and why nothing in the walk is written for the horizontal faces alone.
    #[test]
    fn the_flood_walks_down_as_well_as_along() {
        // Down to up, both ways: a vertical shaft, with the camera's own section at its top.
        let shaft = Visibility::pair(0, 1);
        let world = TestSections::corridor_along_x(1, shaft).with(IVec3::new(0, -1, 0), shaft);

        let flooded = flood(&world, IVec3::ZERO, 0);

        assert_eq!(
            reached(&flooded),
            vec![IVec3::new(0, -1, 0), IVec3::new(0, 0, 0)],
            "the section below the camera is reached by the same walk that reaches the ones beside it"
        );
        assert_eq!(flooded.polled, 2);
    }

    /// **The bit layout the payload carries, pinned here because another language writes it.**
    ///
    /// The packing is `from * FACES + to` in `Direction.ordinal()` order, and the Java side produces this
    /// exact `long` by asking the game's `VisibilitySet.visibilityBetween` for all 36 pairs. A change made
    /// on one side and not the other is a section packed one way and read another - and that is not a
    /// compile error anywhere, which is the whole reason this test exists rather than a comment.
    #[test]
    fn the_packed_layout_is_from_times_six_plus_to() {
        // Down and up are faces 0 and 1, so the pair is bits 0 * 6 + 1 and 1 * 6 + 0.
        assert_eq!(Visibility::pair(0, 1).bits(), (1u64 << 1) | (1u64 << 6));

        // West and east, the corridor pair, are faces 4 and 5: bits 4 * 6 + 5 and 5 * 6 + 4.
        assert_eq!(Visibility::pair(4, 5).bits(), (1u64 << 29) | (1u64 << 34));

        // The highest bit any pair can reach is 5 * 6 + 5, so the whole matrix fits in 36 bits - which is
        // why this is a `u64` and the game's own `BitSet` is a `BitSet` of 64.
        assert_eq!(Visibility::EVERY_PAIR.bits(), (1u64 << 36) - 1);
        assert_eq!(Visibility::EVERY_PAIR.bits().count_ones(), 36);

        // And the exits read the same convention: a step *up* was entered from below, so the row asked
        // for is the opposite face - down - and a shaft answers "out of the top".
        assert_eq!(Visibility::pair(0, 1).exits_from(1), 1u8 << 1);
        assert_eq!(Visibility::pair(0, 1).exits_from(0), 1u8 << 0);
        assert_eq!(
            Visibility::pair(0, 1).exits_from_anywhere(),
            (1u8 << 0) | (1u8 << 1)
        );
    }
}
