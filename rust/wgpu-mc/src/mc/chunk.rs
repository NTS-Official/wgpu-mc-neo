//! # Everything regarding minecraft chunks
//!
//! This handles storing the state of all blocks in a chunk, as well as baking the chunk mesh
//!
//! # Chunk sections?
//!
//! Minecraft splits chunks into 16-block tall pieces called chunk sections, for
//! rendering purposes.
use arrayvec::ArrayVec;
use glam::{IVec2, IVec3, Vec3Swizzles, ivec3, vec3};
use range_alloc::RangeAllocator;
use std::collections::HashMap;
use std::fmt::Debug;
use std::ops::{Not, Range};
use std::sync::Arc;

use crate::WmRenderer;
use crate::mc::BlockManager;
use crate::mc::block::{BlockModelFace, ChunkBlockState, FaceFlags, ModelMesh};
use crate::mc::direction::Direction;
use crate::mc::resource::ResourcePath;
use crate::render::atlas::Atlas;
use crate::render::pipeline::{BLOCK_ATLAS, Vertex};
use crate::texture::UV;

pub const CHUNK_WIDTH: usize = 16;
pub const CHUNK_AREA: usize = CHUNK_WIDTH * CHUNK_WIDTH;
pub const CHUNK_HEIGHT: usize = 384;
pub const CHUNK_SECTION_HEIGHT: usize = 16;
pub const SECTION_VOLUME: usize = CHUNK_AREA * CHUNK_SECTION_HEIGHT;

#[derive(Clone, Copy, Debug)]
pub struct LightLevel {
    pub byte: u8,
}

impl LightLevel {
    pub const fn from_sky_and_block(sky: u8, block: u8) -> Self {
        Self {
            byte: (sky << 4) | (block & 0b1111),
        }
    }

    pub fn get_sky_level(&self) -> u8 {
        self.byte >> 4
    }

    pub fn get_block_level(&self) -> u8 {
        self.byte & 0b1111
    }
}

/// Return a [ChunkBlockState] within the provided world coordinates.
pub trait BlockStateProvider {
    fn get_state(&self, pos: IVec3) -> ChunkBlockState;

    /// The fluid at a position, in the encoding `section::fluid_of` reads: kind, amount, falling.
    ///
    /// Zero - no fluid - is what a provider that knows nothing about fluids answers, and then the
    /// mesher draws the block and no fluid rather than a fluid it made up.
    fn get_fluid(&self, _pos: IVec3) -> u8 {
        0
    }

    fn get_light_level(&self, pos: IVec3) -> LightLevel;

    fn is_section_empty(&self, rel_pos: IVec3) -> bool;

    fn get_block_color(&self, pos: IVec3, tint_index: i32) -> u32;
}

/// How many u32 slots of arena a render distance is worth, in chunks.
///
/// The pool is sized from the render distance rather than fixed because the two are the same question:
/// a view at N chunks holds (2N+1)^2 columns, a column of an ordinary world is a few sections deep, and
/// a section costs a few tens of kilobytes between its vertices and its indices. The numbers are round
/// and deliberately generous: the cost of being too high is video memory, and the cost of being too low
/// is sections that cannot be baked at all - which, since a section that cannot be allocated keeps the
/// geometry it already had, is stale ground rather than a hole in it.
///
/// The margin is the ring [`SectionStorage::trim`] keeps beyond the view, so that a section the game
/// has just meshed is not refused before the camera even sees it.
///
/// ## What the constants are worth
///
/// `SLOTS_PER_SECTION` was **8 000** and that was too small, by a factor this run measured rather than
/// guessed. One quad is twenty-two slots - four vertices of four words, then six indices - so 8 000 is
/// 364 quads, and a section of ordinary surface geometry is an order of magnitude past that: the shell
/// of a full cube alone is 1 536 quads, 33 792 slots, before the cutout layer draws a single plant. A
/// session at 16 chunks (which asks for 32.9M slots, 131 MB, by the old numbers) filled the arena up
/// within seconds of joining, refused **231** sections in the next twenty, and - because a refused
/// section keeps the geometry it had rather than being replaced - stopped updating the terrain
/// altogether. 16 000 is that measurement's first correction and not the last word on it: the honest
/// number is what a session reports, which is why `WmRenderer::grow_arena` doubles the pool when a
/// section does not fit and the diagnostic line carries the used-slot count. See [`LARGEST_SECTION`].
///
/// The first session run against the corrected numbers, 16 chunks, same world:
///
/// ```text
/// arena 51,753,658 of 65,712,000 slot(s) handed out (79%, 197 MB), the largest section 168,014 slot(s)
/// ```
///
/// Three things that says. The pool is the size the world needs, with a fifth of it spare - and the
/// `used` count is the *sum* over every section the arena holds, so the two constants above are only
/// ever right together, as a product: 1 369 columns × 48 000 slots came out at 79% of the pool. The
/// largest section is 168 014 slots, 672 KB, **ten times** the per-section constant - a section is not
/// "about 16 000 slots", it is whatever its geometry is, and one dense enough (a jungle canopy, a
/// leaf-covered hillside) is an order of magnitude past the average. And the refusal count is zero,
/// which is the number this is all for.
pub const fn arena_slots(render_distance: u32) -> u32 {
    /// The ring beyond the view the trim keeps.
    const RING: u32 = 2;
    /// How many sections deep a column of an ordinary world is meshed.
    const SECTIONS_PER_COLUMN: u32 = 3;
    /// One section's vertices and indices, in u32 slots: about 64 KB, or 727 quads.
    const SLOTS_PER_SECTION: u32 = 16_000;

    let capped = if render_distance > 64 {
        64
    } else {
        render_distance
    };
    let width = capped * 2 + 1 + RING * 2;

    width * width * SECTIONS_PER_COLUMN * SLOTS_PER_SECTION
}

/// The arena's pool in u32 slots, as the pool [`Scene`](crate::mc::Scene) starts with.
///
/// A placeholder, and deliberately a small one: the pool is sized to the render distance the game
/// reports, and that report arrives a frame or two after the window opens - long before the world has
/// anything to bake. Sizing this generously instead is how a session came to allocate 457 MB of arena
/// at startup for a world that was going to ask for a fraction of it. A session that never gets a
/// report at all is one with no world in it, and one whose report arrives late is covered by
/// `WmRenderer::grow_arena`.
pub const ARENA_SLOTS: u32 = arena_slots(8);

#[derive(Debug, Copy, Clone, Hash, Eq, PartialEq)]
pub enum RenderLayer {
    Solid = 0,
    Cutout = 1,
    Transparent = 2,
}

impl RenderLayer {
    /// The layer of the two that has to be drawn later, if they differ.
    ///
    /// Which one that is is the variant order: a face that blends cannot be drawn in the pass that
    /// writes opaque depth, so the strongest thing a model asks for is what its faces get.
    pub fn stronger(self, other: RenderLayer) -> RenderLayer {
        if (self as u8) >= (other as u8) {
            self
        } else {
            other
        }
    }
}

#[derive(Clone)]
pub struct SectionRanges {
    pub vertex_range: Range<u32>,
    pub index_range: Range<u32>,
}

///The struct representing a Chunk section, with various render layers, split into sections
pub struct SectionStorage {
    storage: HashMap<IVec3, Section>,
    allocator: RangeAllocator<u32>,
    /// Ranges waiting to be given back, one bucket per frame that may still be in flight.
    ///
    /// A range stops being *stored* one frame and is reused the next, and the frame that still draws it
    /// may still be on the GPU: a `queue.write_buffer` is only ordered against submissions made after
    /// it. So a range is parked for as many frames as the renderer is allowed to have in flight - the
    /// same number the present paces itself by, because that is the first frame whose submission is
    /// known to have been waited for. See [`SectionStorage::free_deferred`].
    deferred: Vec<Vec<Range<u32>>>,
    /// The bucket this frame's ranges are parked in.
    deferred_head: usize,
    /// How many buckets there are. See [`SectionStorage::set_deferred_depth`].
    deferred_depth: usize,
    /// How many slots the pool holds, which is what an allocation is refused against.
    pool: u32,
    /// Whether the last [`SectionStorage::allocate_ranges`] ran out of pool.
    refused: bool,
    /// The sections an allocation was refused for since the JVM last asked, as their positions.
    ///
    /// The boolean above says "this allocation failed"; this says *which* ones, because the two sides
    /// have to agree about it: a section the JVM has been told was baked, and which the arena had no
    /// room for, is a hole nothing will ever fill - its rebuild has already happened, and a rebuild
    /// only carries what changed. So the positions are kept until [`SectionStorage::refused`] hands
    /// them over, and the JVM forgets what it had recorded for them.
    refused_positions: Vec<IVec3>,
    width: i32,
}

/// How many refused positions are kept before the oldest are dropped.
///
/// The JVM drains them every client tick, so this is only reached when the arena is refusing
/// everything - in which case the positions it cannot remember are sections it will be told about
/// again by the next refusal, and the count is in the log either way.
const REFUSED_LIMIT: usize = 4096;
impl SectionStorage {
    pub fn new(range: u32) -> Self {
        SectionStorage {
            storage: HashMap::new(),
            width: 0,
            allocator: RangeAllocator::new(0..range),
            deferred: vec![Vec::new()],
            deferred_head: 0,
            deferred_depth: 1,
            pool: range,
            refused: false,
            refused_positions: Vec::new(),
        }
    }
    /// Narrows or widens the pool to a render distance, which is only possible while it is empty.
    ///
    /// The pool comes from one range allocator, and a range allocator cannot be resized under live
    /// allocations: every stored section's ranges would have to be handed out again, and the data they
    /// point at is in the buffer. The render distance is therefore taken from the *first* report, which
    /// is a frame or two after the window opens and long before the world has anything to bake - and a
    /// later change is refused here rather than corrupting what is already drawn.
    pub fn set_pool(&mut self, slots: u32) -> bool {
        if !self.storage.is_empty() {
            return false;
        }

        self.allocator = RangeAllocator::new(0..slots);
        self.pool = slots;
        self.refused = false;
        self.deferred.clear();
        self.deferred_head = 0;

        true
    }

    /// How many slots the pool has.
    pub fn pool_slots(&self) -> u32 {
        self.pool
    }

    /// How many slots of the pool are handed out right now.
    ///
    /// The number that says how close the arena is to refusing: `pool_slots - used_slots` is what a
    /// section has to fit into, and a run that is refusing sections is one where that is small or
    /// fragmented. Reported on the terrain line beside the refusal count - see [`LARGEST_SECTION`] for
    /// the other half of the same question.
    pub fn used_slots(&self) -> u32 {
        self.pool.saturating_sub(self.free_slots())
    }

    /// How many slots of the pool are free, counted across the free list rather than as one run.
    pub fn free_slots(&self) -> u32 {
        self.allocator.total_available()
    }

    /// Widens the pool, keeping every range that is already handed out where it is.
    ///
    /// Growing is the one resize a range allocator *can* do under live allocations, and the only reason
    /// this is possible at all: the new pool is the old one with more space after it, so every offset
    /// already handed out stays valid - which is what makes a bigger arena a buffer *copy* rather than
    /// a re-bake of the world. [`SectionStorage::set_pool`] replaces the pool and can therefore only run
    /// while it is empty; this one is the opposite trade, and `WmRenderer::grow_arena` does the copy.
    ///
    /// `false` when the pool is already at least this large: a growth request that arrives twice for one
    /// refusal must not count as one.
    pub fn grow_pool(&mut self, slots: u32) -> bool {
        if slots <= self.pool {
            return false;
        }

        self.allocator.grow_to(slots);
        self.pool = slots;

        true
    }

    /// Drops every stored section, for a world the renderer is no longer drawing.
    ///
    /// The ranges go **straight back to the allocator**, which is the one free in this file that does
    /// not go through [`Self::defer_free`], and the difference is deliberate. A deferred range is
    /// parked for as many frames as the renderer may have in flight, because the frame that draws it
    /// has not necessarily been submitted yet and a write that reaches it would be a section drawn
    /// from another section's bytes. A level change is not that moment: it arrives on a client tick,
    /// between frames - the frame that drew these sections has been submitted, and a
    /// `queue.write_buffer` is ordered after every submission before it, while the next frame has not
    /// been recorded at all.
    ///
    /// What the difference buys is the whole pool, immediately. The new world's sections are baked
    /// over the next seconds and the pool has to hold them; ranges parked for `frames_in_flight`
    /// frames would have the first of them refused, and a refusal now costs a section the JVM has to
    /// be told about and re-bake (see `refused_positions`) - for a frame that is already gone.
    pub fn forget(&mut self) {
        for section in self.storage.values() {
            for ranges in section.layers.iter().flatten() {
                self.allocator.free_range(ranges.vertex_range.clone());
                self.allocator.free_range(ranges.index_range.clone());
            }
        }

        self.storage.clear();

        // The refusals described positions in the world that has just been left behind: the JVM is
        // clearing its own record of them at the same moment (`forgetAll`), and handing them over
        // afterwards would have it forget sections of the *new* world that share a coordinate. They
        // are counted as dropped rather than forgotten, so the sum the diagnostic checks still closes.
        REFUSED_DROPPED.fetch_add(
            self.refused_positions.len() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        self.refused_positions.clear();
    }
    /// How far the arena reaches from the camera, in chunks.
    pub fn width(&self) -> i32 {
        self.width
    }

    pub fn set_width(&mut self, w: i32) {
        self.width = w;
    }
    pub fn trim(&mut self, pos: IVec2) {
        let mut to_remove = vec![];
        let mut deferred = Vec::new();
        for (k, section) in &self.storage {
            let dist = (k.xz() - pos).abs();
            let radius = self.width + 2; //temp fix until proper sync
            if dist.x > radius || dist.y > radius {
                to_remove.push(*k);
                for layer in &section.layers {
                    if let Some(l) = layer.as_ref() {
                        // Deferred like every other free: walking away from a section removes it from
                        // the storage, but the frame already on the GPU may still be reading its
                        // ranges. See `allocate`.
                        deferred.push(l.vertex_range.clone());
                        deferred.push(l.index_range.clone());
                    }
                }
            }
        }
        to_remove.iter().for_each(|pos| {
            self.storage.remove(pos);
        });
        self.defer_free(deferred);
    }
    /// Allocates a section's ranges, and hands back the ones the section it replaces was using.
    ///
    /// Split from [`Self::insert`] because the two halves have to happen on either side of the GPU
    /// write: a range that is in the storage is a range the frame will draw, so publishing one whose
    /// bytes are not in the buffer yet is a section drawn from whatever was there before - which, while
    /// the player walks and Minecraft keeps rebuilding sections behind them, is a flicker per rebuild.
    ///
    /// The ranges handed back are *not* freed here either: the previous frame's commands may still be
    /// reading them on the GPU, and `queue.write_buffer` is only ordered against submissions made after
    /// it. [`Self::defer_free`] is where they go, and the frame after that is where they come back.
    ///
    /// `None` when the pool is full. The section is then left exactly as it was - its ranges are not
    /// given back, and it is not replaced - because the alternative is a section that vanishes from the
    /// world and stays gone until something else asks for it: stale geometry is a wrong picture, and no
    /// geometry is a hole.
    pub fn allocate(
        &mut self,
        pos: IVec3,
        baked_layers: &[BakedLayer],
    ) -> Option<(Section, Vec<Range<u32>>)> {
        let mut freed = Vec::new();

        if let Some(previous_section) = self.storage.get(&pos) {
            for layer in &previous_section.layers {
                if let Some(l) = layer.as_ref() {
                    freed.push(l.vertex_range.clone());
                    freed.push(l.index_range.clone());
                }
            }
        }

        let section = self.allocate_ranges(baked_layers);

        if self.refused {
            // Remembered, not just counted: the JVM has to be told which section it may not record
            // as sent, or nothing will offer it again. See `refused_positions`.
            if self.refused_positions.len() < REFUSED_LIMIT {
                self.refused_positions.push(pos);
            } else {
                // Past the cap the position is lost rather than kept: it is counted here so that
                // `refused == handed_over + dropped` still closes, which is what makes a mismatch
                // mean a broken channel rather than a full list.
                REFUSED_DROPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }

            return None;
        }

        Some((section, freed))
    }

    /// Hands over the sections an allocation was refused for, and forgets them.
    ///
    /// Called from the JVM once a tick: what it does with them is drop the record of having sent
    /// them, so the next rebuild of each carries its blocks again and the section gets another
    /// chance at the pool. The count handed over is kept in [`REFUSED_REPORTED`], because it is the
    /// number the JVM's own running total has to match.
    pub fn refused(&mut self) -> Vec<IVec3> {
        let refused = std::mem::take(&mut self.refused_positions);

        REFUSED_REPORTED.fetch_add(refused.len() as u64, std::sync::atomic::Ordering::Relaxed);

        refused
    }

    /// Queues ranges to be freed once the frames that may still draw them are done. See [`Self::allocate`].
    pub fn defer_free(&mut self, ranges: Vec<Range<u32>>) {
        if self.deferred.is_empty() {
            self.deferred.push(Vec::new());
            self.deferred_head = 0;
        }

        let head = self.deferred_head;

        self.deferred[head].extend(ranges);
    }

    /// Sets how many frames of submissions the renderer may have in flight, which is how long a range
    /// waits before it can be handed out again.
    ///
    /// Called from the present, which is where the same number is read to pace itself: a frame is only
    /// presented once the one `frames_in_flight` behind it has finished, so a range parked for that many
    /// frames is parked until its submission is known to be done. Resizing frees what the old buckets
    /// held rather than dropping it - a range that is never freed is a leak the fixed pool cannot afford.
    pub fn set_deferred_depth(&mut self, depth: usize) {
        let depth = depth.clamp(1, 8);

        if depth == self.deferred_depth && !self.deferred.is_empty() {
            return;
        }

        for bucket in std::mem::take(&mut self.deferred) {
            for range in bucket {
                self.allocator.free_range(range);
            }
        }

        self.deferred = (0..depth).map(|_| Vec::new()).collect();
        self.deferred_head = 0;
        self.deferred_depth = depth;
    }

    /// Frees the bucket whose submission has been waited for. Called once per frame, before the frame's
    /// updates, so a range parked this frame is freed once the present has run `depth` more times.
    pub fn free_deferred(&mut self) {
        if self.deferred.is_empty() {
            return;
        }

        self.deferred_head = (self.deferred_head + 1) % self.deferred.len();

        let bucket = std::mem::take(&mut self.deferred[self.deferred_head]);

        for range in bucket {
            self.allocator.free_range(range);
        }
    }

    /// Publishes an allocated section, which is what makes the next frame draw it.
    pub fn insert(&mut self, pos: IVec3, section: Section) {
        self.storage.insert(pos, section);
    }

    fn allocate_ranges(&mut self, baked_layers: &[BakedLayer]) -> Section {
        // A full arena is a state this has to survive rather than panic on: the ranges come out of one
        // fixed pool, and a render distance whose sections do not fit in it is a *policy* problem -
        // see `arena_slots` for how the pool is sized, and `allocate` for what happens when it is not
        // enough. Panicking here ended the game (the panic hook throws, and the JVM aborts on a panic
        // through a native frame), which is not a trade a renderer gets to make.
        self.refused = false;

        let mut layers: Vec<Option<SectionRanges>> = Vec::with_capacity(baked_layers.len());

        for layer in baked_layers {
            // An empty layer is one that draws nothing; `allocate_range(0)` is not a no-op either, it
            // is an assertion, so the two halves of a layer are checked together.
            if layer.vertices.is_empty() || layer.indices.is_empty() {
                layers.push(None);
                continue;
            }

            let vertices = match self
                .allocator
                .allocate_range(layer.vertices.len() as u32 / 4)
            {
                Ok(range) => range,
                Err(_) => {
                    self.refuse(&mut layers);
                    return Section { layers };
                }
            };

            let indices = match self
                .allocator
                .allocate_range(layer.indices.len() as u32 / 4)
            {
                Ok(range) => range,
                Err(_) => {
                    // Give the vertices back before the section is abandoned: they are part of what
                    // `refuse` hands over, and a range the allocator still believes is handed out is
                    // a piece of the pool that never comes back.
                    self.allocator.free_range(vertices);
                    self.refuse(&mut layers);
                    return Section { layers };
                }
            };

            layers.push(Some(SectionRanges {
                vertex_range: vertices,
                index_range: indices,
            }));
        }

        // What one section of this world costs at its worst, which is the number `arena_slots` is
        // guessing at: a pool has to hold *every* section it keeps at once, and the constant in there is
        // per section. One quad is twenty-two slots - four vertices of four words and six indices - so a
        // section whose shell is a thousand quads is twenty-two thousand, three times the constant this
        // path was written with.
        let slots = layers
            .iter()
            .flatten()
            .map(|ranges| {
                (ranges.vertex_range.end - ranges.vertex_range.start)
                    + (ranges.index_range.end - ranges.index_range.start)
            })
            .sum::<u32>();

        if slots > LARGEST_SECTION.load(std::sync::atomic::Ordering::Relaxed) {
            LARGEST_SECTION.store(slots, std::sync::atomic::Ordering::Relaxed);
        }

        Section { layers }
    }

    /// Gives back everything a refused section's allocation had already taken, and marks the refusal.
    ///
    /// This is the difference between a full arena and a shrinking one. The layers allocated before
    /// the one that failed used to be dropped along with the section, and their ranges were never
    /// freed - so every refusal cost the pool the geometry of the layers that *did* fit, permanently.
    /// The arena filled, refused, and got fuller; a run that had been refusing a section here and there
    /// ended up refusing everything, which is exactly what a world whose terrain has stopped updating
    /// looks like from the player's side.
    fn refuse(&mut self, layers: &mut [Option<SectionRanges>]) {
        for layer in layers.iter_mut() {
            if let Some(ranges) = layer.take() {
                self.allocator.free_range(ranges.vertex_range);
                self.allocator.free_range(ranges.index_range);
            }
        }

        self.refused = true;
        report_full_arena(self.pool, self.used_slots());
    }
    pub fn iter(&self) -> std::collections::hash_map::Iter<'_, IVec3, Section> {
        self.storage.iter()
    }

    /// How many sections the arena holds geometry for.
    pub fn len(&self) -> usize {
        self.storage.len()
    }

    pub fn is_empty(&self) -> bool {
        self.storage.is_empty()
    }
}

/// Says the arena is full, once every so often rather than once per section.
///
/// The number that matters is how much of the view does not fit: a handful of sections is a world edge
/// nobody notices, and thousands of them is a rendering-policy problem - the pool is fixed and the
/// render distance is not.
/// How many sections the arena has refused to hold, over the whole run.
///
/// Read by the JVM side and printed on the terrain line: a run where this is not zero is a run whose
/// sections were being dropped for want of arena space, which looks like ground that comes and goes.
static REFUSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many of those refusals the JVM has been *told* about - handed over by [`SectionStorage::refused`]
/// rather than only counted.
///
/// The pair is the whole diagnostic of the return channel: a section that was refused and not handed
/// over is one the JVM still believes was baked, and nothing will offer it again. Over a session the
/// three numbers have to add up exactly -
///
/// ```text
/// refused = handed_over + dropped
/// ```
///
/// - and a run where the JVM has drained fewer than [`sections_refused_reported`] says is a channel
///   that is not working: a call that threw, a list that was cleared in between, a bridge that stopped
///   being wired to anything.
static REFUSED_REPORTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many refusals were never handed over, so that the sum above closes.
///
/// Two ways to lose one, and both are by design: the list has a cap ([`REFUSED_LIMIT`], reached only
/// when the arena is refusing everything), and a level change clears the list along with the arena it
/// describes. What is *not* by design is a refusal that neither reaches the JVM nor appears here.
static REFUSED_DROPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// sections refused for the JVM side. See [REFUSED].
pub fn sections_refused() -> u64 {
    REFUSED.load(std::sync::atomic::Ordering::Relaxed)
}

/// The most slots any one section has ever taken, over the whole run.
///
/// The measurement `arena_slots`' `SLOTS_PER_SECTION` is a guess at, and the reason a pool can be full
/// while holding fewer sections than that constant times the number of columns would suggest: what a
/// section costs is its geometry, and geometry does not care about the estimate. Read on the same
/// diagnostic line as the used-slot count, so a run that refuses sections says both how full the arena
/// was and how big the thing that did not fit was.
static LARGEST_SECTION: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// See [LARGEST_SECTION].
pub fn largest_section_slots() -> u32 {
    LARGEST_SECTION.load(std::sync::atomic::Ordering::Relaxed)
}

/// How many refusals have been handed to the JVM. See [REFUSED_REPORTED].
pub fn sections_refused_reported() -> u64 {
    REFUSED_REPORTED.load(std::sync::atomic::Ordering::Relaxed)
}

/// How many refusals were never handed over. See [REFUSED_DROPPED].
pub fn sections_refused_dropped() -> u64 {
    REFUSED_DROPPED.load(std::sync::atomic::Ordering::Relaxed)
}

fn report_full_arena(pool: u32, used: u32) {
    let refused = REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if refused < 4 || refused.is_multiple_of(512) {
        // What is printed is the whole diagnosis, because the Rust log is the only place these numbers
        // exist: how much of the pool is handed out against how much there is, how big the largest
        // section ever meshed was, and the return channel's two counters - what the JVM has been told
        // and what was lost on the way. `handed + dropped == refused` is the invariant, and the JVM
        // checks its own drained total against the first of them.
        //
        // A refusal is not fatal to the section: `SectionStorage::allocate` leaves the section it could
        // not replace exactly as it was, so the world keeps the geometry it had - stale ground rather
        // than a hole - and `WmRenderer::grow_arena` doubles the pool on the next frame.
        log::warn!(
            "wgpu-mc: the section arena is full, so this section was left without new geometry \
             ({refused} section(s) refused so far, {used} of {pool} slot(s) handed out, the largest \
             section meshed so far {largest} slot(s), {} handed to the JVM, {} dropped before it \
             could be)",
            sections_refused_reported(),
            sections_refused_dropped(),
            largest = largest_section_slots(),
        );
    }
}

#[derive(Clone)]
pub struct Section {
    pub layers: Vec<Option<SectionRanges>>,
}

impl Default for Section {
    fn default() -> Self {
        Self::new()
    }
}

impl Section {
    pub fn new() -> Self {
        Self { layers: Vec::new() }
    }
}

#[inline]
fn get_block(block_manager: &BlockManager, state: ChunkBlockState) -> Option<Arc<ModelMesh>> {
    let key = match state {
        ChunkBlockState::Air => return None,
        ChunkBlockState::State(key) => key,
    };

    block_manager
        .blocks
        .get_index(key.block as usize)?
        .1
        .get_model(key.augment, 0)
}

/// The face flags of a state, or none when the state was never described.
///
/// "None" is the two masks at zero, which reads as "this state occludes nothing and hides nothing" -
/// the conservative reading, where the face is drawn. That is the one to fail towards: a missing bit
/// costs a face between two blocks that nothing can see, and a bit that should not have been set
/// costs a hole in the world.
#[inline]
fn face_flags(block_manager: &BlockManager, state: ChunkBlockState) -> FaceFlags {
    match state {
        ChunkBlockState::Air => FaceFlags::default(),
        ChunkBlockState::State(key) => block_manager
            .face_flags
            .get(&key.pack())
            .copied()
            .unwrap_or_default(),
    }
}

/// Whether Minecraft would leave this face out of the mesh: `Block.shouldRenderFace`, as far as two
/// per-state masks and the model can carry it.
///
/// Only called for a face that *declared* a `cullface`: a face without one is never culled, which is
/// the model's own answer and the one `add_face` checks before it gets here. `dir` is that declared
/// direction, turned with the model, rather than the plane the face's geometry ended up in.
///
/// The neighbour's *state* is what decides, and that is the difference this replaced. Reading the
/// neighbour's model instead - a full-size quad on the side facing us - culls the face of every block
/// next to a full-cube model that does not occlude anything: glass, ice, leaves, stained glass, every
/// plant. Those are exactly the blocks a player looks *through*, so the face that should have been
/// behind them was missing and the world showed its insides.
///
/// Vanilla's rule has three parts; these are the two a mask can hold:
///
/// - `neighbour.occlusion[dir.opposite()]`: the neighbour's shape covers that whole face
///   (`getFaceOcclusionShape(opposite) == Shapes.block()`), so nothing of ours can be seen behind it;
/// - `self.self_hide[dir]` with the neighbour the *same state*: `state.skipRendering(neighborState,
///   direction)`, which is how glass, ice, a pane's bars and a fluid leave out the faces between two
///   blocks of their own kind. The mask is a per-state answer, so it can only speak for the same-state
///   neighbour - two states of one block that wear different models (a pane's connections, say) are
///   not recognised, which costs an invisible face between two blocks rather than a hole.
///
/// The third part - vanilla intersects the two occlusion *shapes* when the neighbour's is partial -
/// is what the old geometry test approximates and is kept as an additional condition: a face is only
/// left out where the neighbour's model also has a full-size quad on that side. Vanilla hides a few
/// of those that this draws; every one of them is a face between two blocks, at a boundary no camera
/// can be on both sides of, so what is drawn there cannot be seen.
fn face_is_hidden(
    block_manager: &BlockManager,
    state: ChunkBlockState,
    neighbour: ChunkBlockState,
    dir: Direction,
) -> bool {
    if neighbour.is_air() {
        return false;
    }

    // No model is a neighbour drawn as something else - the fallback block - so the geometry test
    // has nothing to say about it and the face is kept.
    let Some(mesh) = get_block(block_manager, neighbour) else {
        return false;
    };

    if (mesh.cull >> dir.opposite() as u8) & 1 != 1 {
        return false;
    }

    let flags = face_flags(block_manager, neighbour);

    if flags.occludes(dir.opposite()) {
        return true;
    }

    state == neighbour && face_flags(block_manager, state).hides_same_state(dir)
}

/// Whether a face is left out of the mesh, by the model's own declaration first.
///
/// A face without a `cullface` is one the game adds as *unculled* (`UnbakedCuboidGeometry`): it is
/// drawn whatever is beside it, and that is the whole answer for it. Only a face that names a
/// direction is handed to the neighbour test - and that direction, not the plane the geometry ended
/// up in, is the one tested.
fn face_is_culled<Provider: BlockStateProvider>(
    face: &BlockModelFace,
    block_manager: &BlockManager,
    state: ChunkBlockState,
    provider: &Provider,
    pos: IVec3,
) -> bool {
    let Some(declared) = face.cull else {
        return false;
    };

    face_is_hidden(
        block_manager,
        state,
        provider.get_state(pos + declared.to_vec()),
        declared,
    )
}

pub fn bake_section<Provider: BlockStateProvider>(pos: IVec3, wm: &WmRenderer, bsp: &Provider) {
    let bm = wm.mc.block_manager.read();

    // The fluid mesher has to know where the fluids' sprites ended up in the atlas, so the atlas is
    // handed down with the block manager. Without one the fluids are simply not baked, which is
    // where this started: lava and water are not block models and nothing else draws them.
    let atlases = wm.mc.texture_manager.atlases.read();
    let atlas = atlases.get(BLOCK_ATLAS);

    let baked_section = bake_layers(pos, &bm, bsp, atlas);

    report_bake(pos, &baked_section);
    report_winding(pos, &baked_section);

    wm.chunk_update_queue.0.send((pos, baked_section)).unwrap();
}

/// How a baked layer's quads are wound, counted. See [`report_winding`].
pub struct WindingCounts {
    pub checked: u64,
    pub outward: u64,
    /// The quads that came out the other way, by the direction their own normal names.
    pub inward_by_direction: [u64; 6],
}

/// Walks one baked layer's quads and asks each whether it is wound the way its own normal points.
///
/// Kept apart from the logging so it can be tested on a quad whose answer is known: the check is the
/// whole point of the diagnostic, and a diagnostic nobody has seen produce a number is a diagnostic
/// that reports zero for a reason nobody knows - which is exactly what happened the first time it ran.
pub fn count_winding(layers: &[BakedLayer]) -> WindingCounts {
    let mut counts = WindingCounts {
        checked: 0,
        outward: 0,
        inward_by_direction: [0; 6],
    };

    let Some(layer) = layers.get(RenderLayer::Solid as usize) else {
        return counts;
    };

    // A quad is four vertices of sixteen bytes. The index stream is not needed to walk them, because
    // the baker emits them four at a time in that order.
    //
    // `chunks_exact` rather than the `as_chunks` clippy suggests: `as_chunks` is a nightly-only
    // inherent method on slices, so asking for it here would put the whole crate behind a feature it
    // does not otherwise need - for a length the two agree on and a remainder neither looks at.
    #[allow(clippy::chunks_exact_to_as_chunks)]
    for quad in layer.vertices.chunks_exact(Vertex::VERTEX_LENGTH * 4) {
        let Some((first, normal)) = read_vertex(&quad[0..16]) else {
            continue;
        };
        let Some((second, _)) = read_vertex(&quad[16..32]) else {
            continue;
        };
        let Some((third, _)) = read_vertex(&quad[32..48]) else {
            continue;
        };

        counts.checked += 1;

        let cross = (second - first).cross(third - first);

        if cross.dot(normal) > 0.0 {
            counts.outward += 1;
        } else if let Some(direction) = direction_of(normal) {
            counts.inward_by_direction[direction as usize] += 1;
        }
    }

    counts
}

/// Diagnostics: which way the quads of a baked section face.
///
/// A quad carries its own normal - three bits in the twelfth byte - and that normal is the direction
/// the face is *supposed* to be seen from. The mesher takes the winding from the model file instead,
/// which has no reason to agree with it: MC's own mesher re-orders every face, and a quad whose
/// triangle comes out wound the other way is a face the pass's back-face culling drops exactly where
/// it should be visible. "Some sections are inside out and others are not" is that, per section.
///
/// So each quad is asked here: does the cross product of its first triangle point the way its own
/// normal does? Counted per bake and logged, because this is the question a picture of the world
/// answers only if you already know what you are looking at.
fn report_winding(pos: IVec3, layers: &[BakedLayer]) {
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Quads checked across all bakes, and how they came out.
    static CHECKED: AtomicU64 = AtomicU64::new(0);
    static OUTWARD: AtomicU64 = AtomicU64::new(0);
    static REPORTED: AtomicU64 = AtomicU64::new(0);

    let counts = count_winding(layers);
    if counts.checked == 0 {
        return;
    }

    let total_checked = CHECKED.fetch_add(counts.checked, Ordering::Relaxed) + counts.checked;
    let total_outward = OUTWARD.fetch_add(counts.outward, Ordering::Relaxed) + counts.outward;

    // Once a second, and *not* behind the logging switch: the first version of this was, and it
    // produced no line at all for reasons that took a run to notice. A line a second is nothing next
    // to a bake's worth of work, and this is the one number that says whether the terrain is wound
    // the way it is drawn.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);

    if REPORTED.swap(now, Ordering::Relaxed) == now {
        return;
    }

    log::info!(
        "wgpu-mc: winding: {total_checked} quad(s) checked over all bakes, {total_outward} wound the \
         way their own normal points ({} inward, {}% outward); this section {pos:?}: {} of {} outward, \
         inward by direction [west, east, down, up, north, south] {:?}",
        total_checked - total_outward,
        total_outward * 100 / total_checked.max(1),
        counts.outward,
        counts.checked,
        counts.inward_by_direction,
    );
}

/// One baked vertex's position and normal, decoded the way `terrain.wgsl` decodes them.
///
/// The format is `Vertex::compressed`'s, and the two have to agree: the position is one byte per axis
/// in sixteenths, with a coordinate of exactly sixteen stored as a flag instead (it does not fit in a
/// byte), and the normal is three bits. `None` for anything that does not decode, which is a vertex
/// this diagnostic has nothing to say about rather than a reason to stop.
fn read_vertex(bytes: &[u8]) -> Option<(glam::Vec3, glam::Vec3)> {
    use glam::Vec3;

    if bytes.len() < 16 {
        return None;
    }

    let flags = bytes[11] >> 5;

    let axis = |byte: u8, flag: u8| -> f32 {
        if flags & flag != 0 {
            16.0
        } else {
            byte as f32 / 16.0
        }
    };

    let position = Vec3::new(axis(bytes[0], 1), axis(bytes[1], 2), axis(bytes[2], 4));

    let normal = match (bytes[11] >> 2) & 0b111 {
        0b000 => Vec3::X,
        0b100 => Vec3::NEG_X,
        0b001 => Vec3::Y,
        0b101 => Vec3::NEG_Y,
        0b010 => Vec3::Z,
        0b110 => Vec3::NEG_Z,
        _ => return None,
    };

    Some((position, normal))
}

/// The `Direction` a normal is, for the per-direction counts above.
fn direction_of(normal: glam::Vec3) -> Option<Direction> {
    [
        Direction::West,
        Direction::East,
        Direction::Down,
        Direction::Up,
        Direction::North,
        Direction::South,
    ]
    .into_iter()
    .find(|direction| direction.to_vec().as_vec3() == normal)
}

/// The blocks whose faces the baker counts by name: seen, drawn, culled.
///
/// A block that is invisible has four explanations, and they are one number apart:
///
/// ```text
/// seen 0                    the state never reached a bake - the section was not baked, or the
///                           palette decoded this position as something else
/// seen > 0, drawn+culled 0  the state was there and had no mesh - the model lookup failed
/// culled > 0, drawn 0       every face was culled - a neighbour test, not a model
/// drawn > 0                 the faces are in the section mesh, and something after the bake
///                           dropped them: the arena, the draw, or the vertex itself
/// ```
///
/// Nothing else in the logs can tell those apart. The per-section numbers are dominated by the
/// terrain around the block, the model builder's numbers say what was *baked* rather than what was
/// drawn, and the arena's say what was stored rather than what was asked for. Watching a few blocks
/// by name - the ones a report is about - is what turns four possibilities into one line.
///
/// By name because that is what a person has, and the indices are resolved once, when the registry is
/// built: a name the registry does not have is simply never counted. The list is short on purpose;
/// the lookup is per block per bake.
pub static WATCHED_BLOCKS: [(&str, BlockFaces); 4] = [
    ("minecraft:brown_mushroom_block", BlockFaces::new()),
    ("minecraft:red_mushroom_block", BlockFaces::new()),
    ("minecraft:mushroom_stem", BlockFaces::new()),
    ("minecraft:oak_leaves", BlockFaces::new()),
];

/// How many faces one watched block has been seen, drawn and culled for. See [`WATCHED_BLOCKS`].
#[derive(Debug)]
pub struct BlockFaces {
    pub seen: std::sync::atomic::AtomicU64,
    pub drawn: std::sync::atomic::AtomicU64,
    pub culled: std::sync::atomic::AtomicU64,
}

impl BlockFaces {
    const fn new() -> Self {
        Self {
            seen: std::sync::atomic::AtomicU64::new(0),
            drawn: std::sync::atomic::AtomicU64::new(0),
            culled: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn note(&self, drawn: u32, culled: u32) {
        use std::sync::atomic::Ordering::Relaxed;

        self.seen.fetch_add(1, Relaxed);
        self.drawn.fetch_add(drawn as u64, Relaxed);
        self.culled.fetch_add(culled as u64, Relaxed);
    }
}

/// What the watched blocks have been seen, drawn and culled for, as one line - or empty when there is
/// nothing to say. Read by the JVM side and put on the terrain line.
pub fn watched_faces() -> String {
    use std::sync::atomic::Ordering::Relaxed;

    let mut report = String::new();

    for (name, faces) in WATCHED_BLOCKS.iter() {
        let seen = faces.seen.load(Relaxed);

        if seen == 0 {
            continue;
        }

        if !report.is_empty() {
            report.push_str(", ");
        }

        report.push_str(&format!(
            "{} seen {seen} drawn {} culled {}",
            name.trim_start_matches("minecraft:"),
            faces.drawn.load(Relaxed),
            faces.culled.load(Relaxed),
        ));
    }

    report
}

/// The slot in [`WATCHED_BLOCKS`] a block index is watched under, or `None`.
#[inline]
pub fn watched_slot(watched: &[(u16, u8)], state: ChunkBlockState) -> Option<usize> {
    let ChunkBlockState::State(key) = state else {
        return None;
    };

    watched
        .iter()
        .find(|(index, _)| *index == key.block)
        .map(|(_, slot)| *slot as usize)
}

/// Says what one bake produced, without flooding the log with a world's worth of sections.
///
/// While the render graph does not draw these sections yet, these numbers are the only thing that
/// separates "the Java side handed us the right data" from "the bake found nothing"; so the first
/// few are always reported and the rest are sampled - unless the renderer's logging switch is on, in
/// which case every bake is a line, because a session that turned logging on is one where the
/// question is about specific sections rather than about the shape of a world's worth of them.
fn report_bake(pos: IVec3, layers: &[BakedLayer]) {
    use std::sync::atomic::{AtomicU64, Ordering};

    static BAKES: AtomicU64 = AtomicU64::new(0);

    let baked = BAKES.fetch_add(1, Ordering::Relaxed);
    if baked >= 8 && !baked.is_multiple_of(64) && !DIAGNOSTIC_LOGGING.load(Ordering::Relaxed) {
        return;
    }

    let vertices: usize = layers.iter().map(|layer| layer.vertices.len()).sum();
    let indices: usize = layers.iter().map(|layer| layer.indices.len()).sum();
    let solids = layers
        .get(RenderLayer::Solid as usize)
        .map(|layer| layer.indices.len() / 4)
        .unwrap_or(0);

    log::info!(
        "wgpu-mc: baked {pos:?} in Rust: {vertices} B of vertices, {indices} B of indices \
         ({solids} in the solid layer), {} bake(s) so far",
        baked + 1
    );
}

/// Whether the renderer's diagnostic log lines are on.
///
/// Set from the JVM side when the settings are applied - see `wgpu_mc_jni::debug` - because the
/// switch itself lives there. It is read for the *sampled* lines in this crate: a per-bake line is
/// worth having when someone turned logging on to look at one section, and is noise otherwise.
pub static DIAGNOSTIC_LOGGING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[derive(Clone, Default)]
pub struct BakedLayer {
    pub vertices: Vec<u8>,
    pub indices: Vec<u8>,
}

fn bake_layers<Provider: BlockStateProvider>(
    section_pos: IVec3,
    block_manager: &BlockManager,
    state_provider: &Provider,
    atlas: Option<&Atlas>,
) -> Vec<BakedLayer> {
    let mut layers = vec![BakedLayer::default(); 3];

    let section_offset = 16 * section_pos;

    if state_provider.is_section_empty(ivec3(0, 0, 0)) {
        return layers;
    }

    for block_index in 0..16 * 16 * 16 {
        let pos = ivec3(block_index & 15, block_index >> 8, (block_index & 255) >> 4);

        let fpos = vec3(pos.x as f32, pos.y as f32, pos.z as f32);

        let block_state: ChunkBlockState = state_provider.get_state(pos);

        // Which watched block this is, if any: counted per block rather than per face, so the cost is
        // one short scan and three atomics for the handful of blocks in the list. See
        // [`WATCHED_BLOCKS`] for what the three numbers are for.
        let watched = watched_slot(&block_manager.watched, block_state);
        let mut faces_drawn = 0u32;
        let mut faces_culled = 0u32;

        if let Some(model_mesh) = get_block(block_manager, block_state) {
            // The winding, and the one thing about a baked quad that nothing else can tell you is
            // wrong: the pass culls back faces with a front face of counter-clockwise, and this backend
            // gives Minecraft's shaders OpenGL's clip space (`preprocessing.rs`) - which *mirrors* the
            // clip space, and a mirror turns every triangle over. The model's own vertex order, drawn
            // through that flip, comes out inside out: the faces that point away from the camera are the
            // ones kept, so the terrain shows its insides and loses its outsides. Reversing the two
            // triangles puts the outside back; the same triangles in the other order are
            // `[1, 3, 0, 2, 3, 1]`.
            const INDICES: [u32; 6] = [0, 3, 1, 1, 3, 2];
            let mut add_quad =
                |face: &BlockModelFace, _light_level: LightLevel, dir: Direction, color: u32| {
                    let baked_layer = &mut layers[face.layer as usize];
                    let vec_index = baked_layer.vertices.len() / Vertex::VERTEX_LENGTH;

                    let dir_vec = dir.to_vec();

                    baked_layer.vertices.extend(
                        (0..4)
                            .map(|vert_index| {
                                let model_vertex = face.vertices[vert_index as usize];

                                let (b1, b2, b3, light_level) = if model_mesh.any.is_empty() {
                                    let vertex_biases = ivec3(
                                        if model_vertex.position.x as i32 == 0 {
                                            -1
                                        } else {
                                            1
                                        },
                                        if model_vertex.position.y as i32 == 0 {
                                            -1
                                        } else {
                                            1
                                        },
                                        if model_vertex.position.z as i32 == 0 {
                                            -1
                                        } else {
                                            1
                                        },
                                    );

                                    let axis = dir_vec - vertex_biases; //equivalent to -(vertex_biases - dir_vec)

                                    let mut axes: ArrayVec<IVec3, 2> = ArrayVec::new_const();

                                    if axis.x != 0 {
                                        axes.push(ivec3(axis.x, 0, 0));
                                    }

                                    if axis.y != 0 {
                                        axes.push(ivec3(0, axis.y, 0));
                                    }

                                    if axis.z != 0 {
                                        axes.push(ivec3(0, 0, axis.z));
                                    }

                                    let p1 = vertex_biases + pos;
                                    let p2 = p1 + axes[0];
                                    let p3 = p1 + axes[1];

                                    let b1 = state_provider.get_state(p1).is_air().not() as u8;
                                    let b2 = state_provider.get_state(p2).is_air().not() as u8;
                                    let b3 = state_provider.get_state(p3).is_air().not() as u8;

                                    let l1 = state_provider.get_light_level(p1);
                                    let l2 = state_provider.get_light_level(p2);
                                    let l3 = state_provider.get_light_level(p3);
                                    let l4 = state_provider.get_light_level(pos + dir_vec);

                                    let average_sky = ((l1.get_sky_level()
                                        + l2.get_sky_level()
                                        + l3.get_sky_level()
                                        + l4.get_sky_level())
                                        as f32
                                        / 4.0)
                                        as u8;
                                    let average_block = ((l1.get_block_level()
                                        + l2.get_block_level()
                                        + l3.get_block_level()
                                        + l4.get_block_level())
                                        as f32
                                        / 4.0)
                                        as u8;

                                    let light_level =
                                        LightLevel::from_sky_and_block(average_sky, average_block);

                                    (b1, b2, b3, light_level)
                                } else {
                                    (0, 0, 0, state_provider.get_light_level(pos))
                                };

                                Vertex {
                                    position: [
                                        fpos.x + model_vertex.position[0],
                                        fpos.y + model_vertex.position[1],
                                        fpos.z + model_vertex.position[2],
                                    ],
                                    uv: model_vertex.tex_coords,
                                    normal: face.normal.to_array(),
                                    color,
                                    uv_offset: 0,
                                    lightmap_coords: light_level.byte,
                                    ao: 3 - (b1 + b2 + b3),
                                }
                            })
                            .flat_map(Vertex::compressed),
                    );
                    baked_layer.indices.extend(
                        INDICES
                            .iter()
                            .flat_map(|index| (index + (vec_index as u32)).to_ne_bytes()),
                    );
                };

            let mut add_face = |face: &BlockModelFace, dir: Direction| {
                let color = if face.tint_index != -1 {
                    state_provider.get_block_color(pos + section_offset, face.tint_index)
                } else {
                    0xffffffff
                };

                // Culling is the *model's* decision before it is the neighbour's - see
                // `face_is_culled`, which is where the declared `cullface` is read.
                if face_is_culled(face, block_manager, block_state, state_provider, pos) {
                    faces_culled += 1;
                    return;
                }

                faces_drawn += 1;

                // The light comes from the plane the face is *on*, which is what the bucket direction
                // is for; it is not a culling question.
                let light_level: LightLevel = state_provider.get_light_level(pos + dir.to_vec());
                add_quad(face, light_level, dir, color);
            };

            model_mesh.west.iter().for_each(|face| {
                add_face(face, Direction::West);
            });
            model_mesh.east.iter().for_each(|face| {
                add_face(face, Direction::East);
            });
            model_mesh.down.iter().for_each(|face| {
                add_face(face, Direction::Down);
            });
            model_mesh.up.iter().for_each(|face| {
                add_face(face, Direction::Up);
            });
            model_mesh.north.iter().for_each(|face| {
                add_face(face, Direction::North);
            });
            model_mesh.south.iter().for_each(|face| {
                add_face(face, Direction::South);
            });
            model_mesh.any.iter().for_each(|face| {
                let light_level: LightLevel = state_provider.get_light_level(pos);

                let color = if face.tint_index != -1 {
                    state_provider.get_block_color(pos + section_offset, face.tint_index)
                } else {
                    0xffffffff
                };

                add_quad(face, light_level, Direction::Up, color);
            });
        }

        // One entry per *block*, whether or not it had a mesh: a watched state with no mesh at all is
        // one of the four answers this exists to tell apart. See [`WATCHED_BLOCKS`].
        if let Some(slot) = watched {
            WATCHED_BLOCKS[slot].1.note(faces_drawn, faces_culled);
        }
    }

    if let Some(atlas) = atlas {
        bake_fluid_faces(state_provider, atlas, &mut layers);
    }

    layers
}

/// The fluid textures. A fluid has no block model - no elements, no blockstate variant - so nothing
/// else puts its sprites in the block atlas, and the fluid mesher has no texture to sample until
/// they are there: [`crate::mc::MinecraftState::bake_blocks`] allocates them before it uploads the
/// atlas.
pub const FLUID_TEXTURES: [&str; 4] = ["lava_still", "lava_flow", "water_still", "water_flow"];

/// The fluid byte a block carries: which fluid (1 water, 2 lava, 3 some other, 0 none), how much of
/// it there is out of nine, and whether it is falling. The mod writes it and
/// `wgpu_mc_jni::section` reads the same three fields back out of the same byte, so the three
/// shifts and masks here are an ABI: the writer is `describe` in the mod's `Payload`.
pub fn fluid_of(byte: u8) -> (u8, u8, bool) {
    (byte & 0b11, (byte >> 2) & 0b1111, byte & 0b0100_0000 != 0)
}

/// How many blocks the fluid mesher has seen holding a fluid, and how many faces it has drawn for
/// them, over every bake so far.
///
/// Two numbers because they are the two halves of "there is no lava on screen": a block count of zero
/// is a fluid that never reached the baker - the payload's fluid blob, or the states themselves - and
/// blocks counted with no faces drawn is a fluid the mesher saw and could not draw, which is a sprite
/// that is not in the atlas. The JVM side logs both next to the terrain pass, because the Rust side's
/// own log does not reach the game's log file.
static FLUID_BLOCKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static FLUID_QUADS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `(blocks holding a fluid, faces drawn for them)`. See [FLUID_BLOCKS].
pub fn fluid_totals() -> (u64, u64) {
    (
        FLUID_BLOCKS.load(std::sync::atomic::Ordering::Relaxed),
        FLUID_QUADS.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// How high a fluid stands in its own block, in blocks.
///
/// Its amount out of nine, which is Minecraft's rule, except that lava and a falling fluid are drawn
/// to the top of the block: lava is thick enough to fill the block it is in, and a falling fluid is
/// a column rather than a surface. Both come out of the game's own `getOwnHeight`.
fn fluid_height(kind: u8, amount: u8, falling: bool) -> f32 {
    if kind == 2 || falling || amount == 0 {
        1.0
    } else {
        amount.min(9) as f32 / 9.0
    }
}

/// The height of a fluid's surface at one corner of a block, in blocks.
///
/// Minecraft averages the fluid in the four blocks that touch that corner - which is what turns a
/// surface that steps from block to block into one that slopes - and gives the corner the full
/// block when one of those four holds a *different* fluid, because the two do not join. A corner
/// with none of this fluid at all is the full block too.
fn fluid_corner_height<Provider: BlockStateProvider>(
    state_provider: &Provider,
    pos: IVec3,
    kind: u8,
    corner: IVec2,
) -> f32 {
    let base = pos + ivec3(corner.x - 1, 0, corner.y - 1);

    let mut total = 0.0;
    let mut counted = 0;

    for step in [IVec3::ZERO, IVec3::X, IVec3::Z, ivec3(1, 0, 1)] {
        let (other_kind, amount, falling) = fluid_of(state_provider.get_fluid(base + step));

        if other_kind == kind {
            total += fluid_height(kind, amount, falling);
            counted += 1;
        } else if other_kind != 0 {
            return 1.0;
        }
    }

    if counted == 0 {
        1.0
    } else {
        total / counted as f32
    }
}

/// Puts the four corners of a face in the order that turns counter-clockwise around its normal,
/// which is the order the baker's index list and the winding diagnostic both read them in.
///
/// Written as a correction rather than as a table of corner orders per direction: the tables are
/// easy to get backwards - the face comes out drawn inside out and is culled exactly where it
/// should be visible - and the cross product cannot be.
fn wind_quad(
    mut corners: [(glam::Vec3, [u16; 2]); 4],
    normal: glam::Vec3,
) -> [(glam::Vec3, [u16; 2]); 4] {
    let cross = (corners[1].0 - corners[0].0).cross(corners[2].0 - corners[0].0);

    if cross.dot(normal) < 0.0 {
        corners.swap(1, 3);
    }

    corners
}

/// Where one texel of a sprite is in the block atlas, in the sixteenths of a pixel a baked vertex
/// stores: `u` and `v` run 0 to 16 across the sprite with v downwards, as in a block model's face.
fn sprite_uv(sprite: UV, u: u16, v: u16) -> [u16; 2] {
    [sprite.0.0 + u, sprite.0.1 + v]
}

/// The sprites of one fluid and the layer it is drawn in: water is translucent and lava is not.
struct FluidSprites {
    still: UV,
    flow: UV,
}

fn fluid_sprites(atlas: &Atlas, kind: u8, name: &str) -> Option<(RenderLayer, FluidSprites)> {
    let layer = match kind {
        1 => RenderLayer::Transparent,
        2 => RenderLayer::Solid,
        _ => return None,
    };

    let uv_map = atlas.uv_map.read();

    let sprite = |suffix: &str, bit: u64| {
        let path = ResourcePath(format!("minecraft:block/{name}_{suffix}"));
        let sprite = uv_map.get(&path).copied();

        // Reported once per sprite rather than once per bake: a pack without the fluid textures
        // would otherwise write one line per section while the world loads.
        static WARNED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if sprite.is_none() && WARNED.fetch_or(bit, std::sync::atomic::Ordering::Relaxed) & bit == 0
        {
            log::warn!("wgpu-mc: {path} is not in the block atlas, so {name} is not drawn");
        }

        sprite
    };

    let still = sprite("still", 1 << kind)?;
    // The flowing sprite is what a fluid's sides are drawn with, but 26.1 does not hand one out under
    // this name - `minecraft:block/lava_flow` is not a resource the game has - and a fluid drawn with
    // its still sprite on the sides is a whole fluid rather than none at all. The still sprite is the
    // one that has to be there: without it there is no texture to sample and nothing is baked.
    let flow = sprite("flow", 1 << (kind + 2)).unwrap_or(still);

    Some((layer, FluidSprites { still, flow }))
}

/// Bakes the fluids of a section: the lava and the water in it, shaped the way Minecraft's own
/// `LiquidBlockRenderer` shapes them.
///
/// Nothing else does it. A fluid is not a block model - no elements to bake, no variant to look up -
/// so the pass above walks straight past it, and the Rust terrain came out with the lava and the
/// water simply missing, which in a superflat world is the lava lakes and the water.
///
/// The shape: a top face at the height the fluid settled at and only where the block above holds a
/// different fluid, side faces only towards blocks that do not hold the same fluid, clipped to the
/// corners' heights so a sloping surface comes out sloping, a bottom face where the fluid does not
/// continue downwards, and no face at all between two blocks of the same fluid.
fn bake_fluid_faces<Provider: BlockStateProvider>(
    state_provider: &Provider,
    atlas: &Atlas,
    layers: &mut [BakedLayer],
) {
    // Lava is the only fluid with a layer to draw into yet (see the `match` below). The water sprites
    // are still looked up, so that a pack missing them says so once now rather than on the day water is
    // drawn.
    let _water = fluid_sprites(atlas, 1, "water");
    let lava = fluid_sprites(atlas, 2, "lava");

    // The same index list the block baker emits, for the same reason: a mirror in the clip space
    // (see `preprocessing.rs`) turns every triangle over, so the two triangles of a quad are emitted
    // in the order that comes back out wound counter-clockwise.
    const INDICES: [u32; 6] = [0, 3, 1, 1, 3, 2];

    // Water is tinted by the biome it is in, which nothing on this path knows. This is the colour
    // the game uses where no biome says otherwise, packed the way the terrain shader reads the
    // vertex colour back: red in the low byte.
    const WATER_TINT: u32 = 0x00e4_763f;

    let mut add_quad = |layer: RenderLayer,
                        dir: Direction,
                        light: u8,
                        color: u32,
                        corners: [(glam::Vec3, [u16; 2]); 4]| {
        FLUID_QUADS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let baked_layer = &mut layers[layer as usize];
        let first = baked_layer.vertices.len() / Vertex::VERTEX_LENGTH;
        let normal = dir.to_vec().as_vec3().to_array();

        baked_layer.vertices.extend(
            wind_quad(corners, glam::Vec3::from_array(normal))
                .iter()
                .flat_map(|(position, uv)| {
                    Vertex {
                        position: position.to_array(),
                        uv: *uv,
                        normal,
                        color,
                        uv_offset: 0,
                        lightmap_coords: light,
                        // Fluids are not shaded per corner in the game either: a fluid face is
                        // one flat surface, lit by the block it is seen from.
                        ao: 3,
                    }
                    .compressed()
                }),
        );
        baked_layer.indices.extend(
            INDICES
                .iter()
                .flat_map(|index| (index + (first as u32)).to_ne_bytes()),
        );
    };

    for block_index in 0..16 * 16 * 16 {
        let pos = ivec3(block_index & 15, block_index >> 8, (block_index & 255) >> 4);
        let (kind, _, _) = fluid_of(state_provider.get_fluid(pos));

        // Counted before the sprites are looked at: a fluid whose sprite never made it into the atlas
        // is exactly the case the counters exist to tell apart from a fluid that never arrived.
        if kind == 1 || kind == 2 {
            FLUID_BLOCKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        let sprites = match kind {
            // Lava only, for now: water belongs in the translucent layer and no pass of ours draws that
            // layer yet - Minecraft still draws its own water - so baking it would be a second copy of
            // every ocean in the arena, in space the sections that *are* drawn have to share. It comes
            // back the day the translucent pass is taken over.
            1 => continue,
            2 => &lava,
            // 3 is "a fluid this mesher does not know": a modded one, or the empty fluid of a block
            // that has none, which is 0.
            _ => continue,
        };

        let Some((layer, sprites)) = sprites else {
            continue;
        };

        let (fx, fy, fz) = (pos.x as f32, pos.y as f32, pos.z as f32);
        let color = if kind == 1 { WATER_TINT } else { 0x00ff_ffff };

        // The four corners of the block, in the order (0,0), (1,0), (0,1), (1,1) in x and z, each
        // with the height the fluid stands at there. Both the top face and the sides are cut to
        // them, and each one walks four blocks to work out, so they are worked out once.
        let heights = [
            fluid_corner_height(state_provider, pos, kind, IVec2::new(0, 0)),
            fluid_corner_height(state_provider, pos, kind, IVec2::new(1, 0)),
            fluid_corner_height(state_provider, pos, kind, IVec2::new(0, 1)),
            fluid_corner_height(state_provider, pos, kind, IVec2::new(1, 1)),
        ];
        let corner = [
            (fx, fz),
            (fx + 1.0, fz),
            (fx, fz + 1.0),
            (fx + 1.0, fz + 1.0),
        ];

        // The surface, where the fluid ends: no face between two blocks of the same fluid, which is
        // also what keeps a lake from being drawn a block at a time.
        if fluid_of(state_provider.get_fluid(pos + IVec3::Y)).0 != kind {
            let light = state_provider.get_light_level(pos + IVec3::Y).byte;

            add_quad(
                *layer,
                Direction::Up,
                light,
                color,
                [
                    (
                        vec3(fx, fy + heights[0], fz),
                        sprite_uv(sprites.still, 0, 0),
                    ),
                    (
                        vec3(fx, fy + heights[2], fz + 1.0),
                        sprite_uv(sprites.still, 0, 16),
                    ),
                    (
                        vec3(fx + 1.0, fy + heights[3], fz + 1.0),
                        sprite_uv(sprites.still, 16, 16),
                    ),
                    (
                        vec3(fx + 1.0, fy + heights[1], fz),
                        sprite_uv(sprites.still, 16, 0),
                    ),
                ],
            );
        }

        // The sides, towards each block that does not hold the same fluid: lava against stone is a
        // wall of lava, lava against lava is nothing at all.
        for (dir, first, second) in [
            (Direction::North, 0, 1),
            (Direction::South, 2, 3),
            (Direction::West, 0, 2),
            (Direction::East, 1, 3),
        ] {
            let neighbour = pos + dir.to_vec();

            if fluid_of(state_provider.get_fluid(neighbour)).0 == kind {
                continue;
            }

            let (low, high) = (heights[first], heights[second]);

            if low <= 0.0 && high <= 0.0 {
                continue;
            }

            let light = state_provider.get_light_level(neighbour).byte;
            let bottom = |v: f32| (16.0 - 16.0 * v.clamp(0.0, 1.0)) as u16;

            add_quad(
                *layer,
                dir,
                light,
                color,
                [
                    (
                        vec3(corner[first].0, fy, corner[first].1),
                        sprite_uv(sprites.flow, 0, 16),
                    ),
                    (
                        vec3(corner[second].0, fy, corner[second].1),
                        sprite_uv(sprites.flow, 16, 16),
                    ),
                    (
                        vec3(corner[second].0, fy + high, corner[second].1),
                        sprite_uv(sprites.flow, 16, bottom(high)),
                    ),
                    (
                        vec3(corner[first].0, fy + low, corner[first].1),
                        sprite_uv(sprites.flow, 0, bottom(low)),
                    ),
                ],
            );
        }

        // The underside, where the fluid does not carry on into the block below - lava pouring over
        // an edge rather than a lake.
        if fluid_of(state_provider.get_fluid(pos - IVec3::Y)).0 != kind {
            let light = state_provider.get_light_level(pos - IVec3::Y).byte;

            add_quad(
                *layer,
                Direction::Down,
                light,
                color,
                [
                    (vec3(fx, fy, fz), sprite_uv(sprites.still, 0, 0)),
                    (vec3(fx, fy, fz + 1.0), sprite_uv(sprites.still, 0, 16)),
                    (
                        vec3(fx + 1.0, fy, fz + 1.0),
                        sprite_uv(sprites.still, 16, 16),
                    ),
                    (vec3(fx + 1.0, fy, fz), sprite_uv(sprites.still, 16, 0)),
                ],
            );
        }
    }
}

#[cfg(test)]
mod winding_tests {
    use super::*;
    use crate::render::pipeline::Vertex;
    use glam::vec3;

    /// The winding check answers for a quad whose answer is known.
    ///
    /// This is the diagnostic that had to exist before anything could be said about "some sections are
    /// inside out": the check itself was the part that could be wrong, and it reported nothing at all
    /// the first time it ran - which looked exactly like a mesh that was fine. A quad in the plane
    /// `y = 0`, wound so that its triangle's cross product points the way its own normal does, has to
    /// come out outward; the same four vertices in the other order, inward.
    #[test]
    fn a_quad_is_counted_by_which_way_its_triangle_turns() {
        let up = vec3(0.0, 1.0, 0.0);

        let vertex = |position: [f32; 3]| Vertex {
            position,
            uv: [0, 0],
            normal: up.to_array(),
            color: 0xffff_ffff,
            uv_offset: 0,
            lightmap_coords: 0,
            ao: 0,
        };

        let layer_of = |positions: [[f32; 3]; 4]| {
            let mut layer = BakedLayer::default();
            for position in positions {
                layer
                    .vertices
                    .extend_from_slice(&vertex(position).compressed());
            }

            vec![layer, BakedLayer::default(), BakedLayer::default()]
        };

        // (0,0,0) -> (0,0,1) -> (1,0,1): the cross product of the first two edges is +y, which is the
        // normal these vertices carry.
        let outward = layer_of([
            [0.0, 0.0, 0.0],
            [0.0, 0.0, 1.0],
            [1.0, 0.0, 1.0],
            [1.0, 0.0, 0.0],
        ]);
        let counts = count_winding(&outward);
        assert_eq!(counts.checked, 1, "one quad was checked");
        assert_eq!(
            counts.outward, 1,
            "the quad wound the way its normal points"
        );

        // The same quad the other way round.
        let inward = layer_of([
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 1.0],
            [0.0, 0.0, 1.0],
            [1.0, 0.0, 0.0],
        ]);
        let counts = count_winding(&inward);
        assert_eq!(counts.checked, 1, "one quad was checked");
        assert_eq!(counts.outward, 0, "the quad wound against its own normal");
        assert_eq!(
            counts.inward_by_direction[Direction::Up as usize],
            1,
            "and it is counted against the direction its normal names"
        );
    }

    /// A fluid face is put in its drawing order by its corners rather than by a table, and this is
    /// the property that has to hold: whichever way round the four corners arrive, the quad has to
    /// come out one the diagnostic counts as outward.
    ///
    /// It is worth a test of its own because it cost a day of "the terrain is inside out": a quad
    /// whose triangles turn against its own normal is a face the pass culls exactly where it should
    /// be seen, and the same mistake is one swap away in every one of the six directions.
    #[test]
    fn a_fluid_face_is_wound_the_way_it_is_seen_from() {
        let layer_of = |direction: Direction, corners: [[f32; 3]; 4]| {
            let normal = direction.to_vec().as_vec3().to_array();
            let wound = wind_quad(
                corners.map(|corner| (glam::Vec3::from_array(corner), [0u16, 0u16])),
                glam::Vec3::from_array(normal),
            );

            let mut layer = BakedLayer::default();

            for (position, _) in wound {
                layer.vertices.extend_from_slice(
                    &Vertex {
                        position: position.to_array(),
                        uv: [0, 0],
                        normal,
                        color: 0xffff_ffff,
                        uv_offset: 0,
                        lightmap_coords: 0,
                        ao: 0,
                    }
                    .compressed(),
                );
            }

            vec![layer, BakedLayer::default(), BakedLayer::default()]
        };

        let faces: [(Direction, [[f32; 3]; 4]); 6] = [
            (
                Direction::Up,
                [
                    [0.0, 1.0, 0.0],
                    [1.0, 1.0, 0.0],
                    [1.0, 1.0, 1.0],
                    [0.0, 1.0, 1.0],
                ],
            ),
            (
                Direction::Down,
                [
                    [0.0, 0.0, 0.0],
                    [1.0, 0.0, 0.0],
                    [1.0, 0.0, 1.0],
                    [0.0, 0.0, 1.0],
                ],
            ),
            (
                Direction::North,
                [
                    [0.0, 0.0, 0.0],
                    [1.0, 0.0, 0.0],
                    [1.0, 1.0, 0.0],
                    [0.0, 1.0, 0.0],
                ],
            ),
            (
                Direction::South,
                [
                    [0.0, 0.0, 1.0],
                    [1.0, 0.0, 1.0],
                    [1.0, 1.0, 1.0],
                    [0.0, 1.0, 1.0],
                ],
            ),
            (
                Direction::West,
                [
                    [0.0, 0.0, 0.0],
                    [0.0, 0.0, 1.0],
                    [0.0, 1.0, 1.0],
                    [0.0, 1.0, 0.0],
                ],
            ),
            (
                Direction::East,
                [
                    [1.0, 0.0, 0.0],
                    [1.0, 0.0, 1.0],
                    [1.0, 1.0, 1.0],
                    [1.0, 1.0, 0.0],
                ],
            ),
        ];

        for (direction, corners) in faces {
            // The corners as they come, and the same four the other way round - which is the order a
            // face picked off a neighbour the other way round would arrive in.
            let reversed = [corners[0], corners[3], corners[2], corners[1]];

            for corners in [corners, reversed] {
                let counts = count_winding(&layer_of(direction, corners));
                assert_eq!(counts.checked, 1, "{direction:?}: one quad was checked");
                assert_eq!(
                    counts.outward, 1,
                    "{direction:?} came out wound against its own normal"
                );
            }
        }
    }
}

/// The face test, state by state. See [`face_is_hidden`].
#[cfg(test)]
mod face_culling_tests {
    use super::*;
    use crate::mc::Block;
    use crate::mc::block::{BlockMeshVertex, BlockstateKey};
    use indexmap::map::IndexMap;

    /// A full cube's worth of bits, which is what a model whose every face sits on the block boundary
    /// gets - glass, ice and leaves all have one.
    const FULL_CUBE: u8 = 0b0011_1111;

    /// A block id and the mask its model culls with.
    ///
    /// The ids are indices into the manager's `blocks`, and `face_flags` is keyed by the packed state
    /// key - `(block << 16) | augment`, the same packing the section palette carries.
    fn registry(blocks: &[(u16, u8)], flags: &[(u16, FaceFlags)]) -> BlockManager {
        let mut manager = BlockManager::new();

        for (id, cull) in blocks {
            let mut variants = IndexMap::new();
            variants.insert(
                Vec::new(),
                vec![Arc::new(ModelMesh {
                    north: vec![],
                    south: vec![],
                    west: vec![],
                    east: vec![],
                    up: vec![],
                    down: vec![],
                    any: vec![],
                    cull: *cull,
                })],
            );

            manager
                .blocks
                .insert(format!("block{id}"), Block::Variants(variants));
        }

        for (id, flags) in flags {
            manager.face_flags.insert((*id as u32) << 16, *flags);
        }

        manager
    }

    fn state(id: u16) -> ChunkBlockState {
        ChunkBlockState::State(BlockstateKey {
            block: id,
            augment: 0,
        })
    }

    /// What a stone-like state says: its shape is the full block, so every face is occluded, and it
    /// hides nothing against its own kind (`skipRendering` is false by default).
    fn stone() -> FaceFlags {
        FaceFlags {
            occlusion: FULL_CUBE,
            self_hide: 0,
        }
    }

    /// What glass, ice or a leaf block says: a full-cube *model* that occludes nothing at all, and
    /// `skipRendering` true against its own kind.
    fn glass() -> FaceFlags {
        FaceFlags {
            occlusion: 0,
            self_hide: FULL_CUBE,
        }
    }

    /// Two full cubes of the same kind: the face between them is not drawn, which is most of what the
    /// test does and what every block in the world relies on.
    #[test]
    fn a_face_against_an_occluding_neighbour_is_left_out() {
        let manager = registry(
            &[(0, FULL_CUBE), (1, FULL_CUBE)],
            &[(0, stone()), (1, stone())],
        );

        for dir in [
            Direction::Up,
            Direction::Down,
            Direction::North,
            Direction::South,
            Direction::West,
            Direction::East,
        ] {
            assert!(
                face_is_hidden(&manager, state(0), state(1), dir),
                "{dir:?}: two full blocks share a hidden face"
            );
        }
    }

    /// The bug this rule replaced: a full-cube model that does not occlude - glass, ice, leaves,
    /// every plant - used to have its neighbour's face culled, because the model *looks* like a
    /// solid cube. The world showed its insides wherever one of them touched anything.
    #[test]
    fn a_face_against_glass_is_drawn() {
        let manager = registry(
            &[(0, FULL_CUBE), (1, FULL_CUBE)],
            &[(0, stone()), (1, glass())],
        );

        for dir in [
            Direction::Up,
            Direction::Down,
            Direction::North,
            Direction::South,
            Direction::West,
            Direction::East,
        ] {
            assert!(
                !face_is_hidden(&manager, state(0), state(1), dir),
                "{dir:?}: stone's face against glass is drawn, however full glass's model is"
            );
        }
    }

    /// `skipRendering` against the same kind: the faces between two glass blocks are left out, which
    /// is what keeps a wall of glass from being drawn twice over.
    #[test]
    fn glass_against_glass_hides_its_own_face() {
        let manager = registry(&[(0, FULL_CUBE)], &[(0, glass())]);

        assert!(
            face_is_hidden(&manager, state(0), state(0), Direction::North),
            "two glass blocks share one face"
        );

        // The same state against a *different* kind that hides its own faces just as much - two
        // colours of stained glass, say - is a face that is drawn: `skipRendering` speaks for one
        // block against its own kind, and the mask is a per-state answer, so the most it can say is
        // "the same state".
        let mixed = registry(
            &[(0, FULL_CUBE), (1, FULL_CUBE)],
            &[(0, glass()), (1, glass())],
        );
        assert!(
            !face_is_hidden(&mixed, state(0), state(1), Direction::North),
            "two different glass blocks are not the same state"
        );
    }

    /// A neighbour whose occluding shape covers only part of the face - a slab, a stair - is not an
    /// occluder here, and the face is drawn. Vanilla hides a few of those by intersecting the two
    /// shapes; every one of them is a face between two blocks, where nothing can see it.
    #[test]
    fn a_partial_occluder_draws_the_face() {
        let partial = FaceFlags {
            occlusion: 0,
            self_hide: 0,
        };

        let manager = registry(&[(0, FULL_CUBE), (1, FULL_CUBE)], &[(1, partial)]);

        assert!(!face_is_hidden(&manager, state(0), state(1), Direction::Up));
    }

    /// A state the JVM never described has no bits at all, and the conservative reading of that is
    /// "occludes nothing": a registry without the flags is a world with every face drawn rather than
    /// a world with holes in it.
    #[test]
    fn a_state_with_no_flags_occludes_nothing() {
        let manager = registry(&[(0, FULL_CUBE), (1, FULL_CUBE)], &[]);

        assert!(!face_is_hidden(&manager, state(0), state(1), Direction::Up));
    }

    /// Air, and a neighbour whose model is missing, leave the face alone.
    #[test]
    fn a_neighbour_with_no_model_draws_the_face() {
        let manager = registry(&[(0, FULL_CUBE)], &[(0, stone())]);

        assert!(!face_is_hidden(
            &manager,
            state(0),
            ChunkBlockState::Air,
            Direction::Up
        ));
        assert!(
            !face_is_hidden(&manager, state(0), state(7), Direction::Up),
            "block 7 has no model, so there is no geometry to cull against"
        );
    }

    /// A world for one face at the origin: the block it belongs to, air above it, and stone in every
    /// other direction - so "which neighbour was tested" is readable off the answer.
    struct OneBlock {
        id: u16,
    }

    impl BlockStateProvider for OneBlock {
        fn get_state(&self, pos: IVec3) -> ChunkBlockState {
            if pos == IVec3::ZERO || pos == IVec3::Y {
                ChunkBlockState::Air
            } else {
                state(self.id)
            }
        }

        fn get_light_level(&self, _pos: IVec3) -> LightLevel {
            LightLevel::from_sky_and_block(15, 0)
        }

        fn is_section_empty(&self, _rel_pos: IVec3) -> bool {
            false
        }

        fn get_block_color(&self, _pos: IVec3, _tint_index: i32) -> u32 {
            0xffff_ffff
        }
    }

    /// A face's own declaration comes first: one with no `cullface` is drawn whatever is beside it.
    ///
    /// That is the model asking to be seen - a plant's cross, a pane's edge, the inside of a mushroom
    /// cap - and a baker that culls it because the neighbour happens to be a full block is a hole in
    /// the world with nothing in the log to explain it.
    #[test]
    fn a_face_with_no_cullface_is_never_culled() {
        let manager = registry(&[(0, FULL_CUBE), (1, FULL_CUBE)], &[(1, stone())]);
        let provider = OneBlock { id: 1 };

        let face = BlockModelFace {
            vertices: [BlockMeshVertex {
                position: glam::Vec3::ZERO,
                tex_coords: [0, 0],
            }; 4],
            normal: glam::Vec3::Y,
            tint_index: -1,
            animation_uv_offset: 0,
            cull: None,
            layer: RenderLayer::Solid,
        };

        assert!(
            !face_is_culled(&face, &manager, state(0), &provider, IVec3::ZERO),
            "the model declared nothing to cull it against, and stone is right there to the north"
        );

        // And the same face, declaring the direction the stone is in, *is* culled - which is what
        // says the difference above is the declaration and not the state flags.
        let declared = BlockModelFace {
            cull: Some(Direction::North),
            ..face
        };

        assert!(face_is_culled(
            &declared,
            &manager,
            state(0),
            &provider,
            IVec3::ZERO
        ));
    }

    /// The direction tested is the one the face *declared*, not the plane its geometry is on: a face
    /// on a block boundary has both the same, and everything else does not.
    #[test]
    fn the_declared_direction_is_the_one_tested() {
        let manager = registry(&[(0, FULL_CUBE), (1, FULL_CUBE)], &[(1, stone())]);
        let provider = OneBlock { id: 1 };

        // North is stone and up is air, so a face that declares `up` has nothing to be culled by -
        // where the same face would be hidden if the direction were read off its geometry.
        let face = BlockModelFace {
            vertices: [BlockMeshVertex {
                position: glam::Vec3::ZERO,
                tex_coords: [0, 0],
            }; 4],
            normal: glam::Vec3::Y,
            tint_index: -1,
            animation_uv_offset: 0,
            cull: Some(Direction::Up),
            layer: RenderLayer::Solid,
        };

        assert!(!face_is_culled(
            &face,
            &manager,
            state(0),
            &provider,
            IVec3::ZERO
        ));

        let north = BlockModelFace {
            cull: Some(Direction::North),
            ..face
        };

        assert!(face_is_culled(
            &north,
            &manager,
            state(0),
            &provider,
            IVec3::ZERO
        ));
    }
}

/// Sizing the arena, and the refusals that come out of it. See [`SectionStorage::set_pool`] and
/// [`SectionStorage::refused`].
#[cfg(test)]
mod arena_tests {
    use super::*;

    /// How many `u32` slots one quad takes: four vertices of four words each, and six indices.
    const QUAD_SLOTS: u32 = 4 * 4 + 6;

    /// A pool that holds exactly `quads` quads and not one more, so a test can say what "full" means.
    fn pool_for(quads: u32) -> u32 {
        quads * QUAD_SLOTS
    }

    /// One layer of `quads` quads, in the shape the baker produces: four vertices and six indices,
    /// each a `u32`.
    fn layer(quads: usize) -> BakedLayer {
        BakedLayer {
            vertices: vec![0u8; quads * 4 * 4 * 4],
            indices: vec![0u8; quads * 6 * 4],
        }
    }

    /// Puts one section into the arena the way the feed does - allocate, then publish - and answers
    /// whether the pool had room for it.
    fn put(storage: &mut SectionStorage, pos: IVec3, quads: usize) -> bool {
        let layers = [layer(quads)];

        match storage.allocate(pos, &layers) {
            Some((section, freed)) => {
                storage.insert(pos, section);
                storage.defer_free(freed);
                true
            }
            None => false,
        }
    }

    /// The arena is sized once, while it is empty - which is what makes a render distance report
    /// cheap on joining a world and impossible later.
    #[test]
    fn the_pool_can_only_be_set_while_the_arena_is_empty() {
        let mut storage = SectionStorage::new(pool_for(4));

        assert!(
            storage.set_pool(pool_for(2)),
            "an empty arena takes a new size"
        );
        assert_eq!(storage.pool_slots(), pool_for(2));

        assert!(put(&mut storage, IVec3::new(0, 0, 0), 1), "it fits");

        assert!(
            !storage.set_pool(pool_for(1)),
            "a section is stored, so the allocator cannot be moved under it"
        );
        assert_eq!(
            storage.pool_slots(),
            pool_for(2),
            "and the arena keeps the size it was built with"
        );

        // Cleared, and the new size takes: this is what makes the report on joining a world work,
        // since `setLevel` clears the arena at that moment.
        storage.forget();
        assert!(storage.set_pool(pool_for(1)));
        assert_eq!(storage.pool_slots(), pool_for(1));
    }

    /// A section that does not fit is remembered by position, not just counted - the JVM is told
    /// which one it may not record as sent.
    #[test]
    fn a_refused_section_is_remembered_and_handed_over_once() {
        // Room for one section of one quad, and not for a second one.
        let mut storage = SectionStorage::new(pool_for(1));

        assert!(put(&mut storage, IVec3::new(0, 0, 0), 1));

        let refused = IVec3::new(1, 0, 0);
        assert!(!put(&mut storage, refused, 1), "the pool is full");

        assert_eq!(storage.refused(), vec![refused]);
        assert!(
            storage.refused().is_empty(),
            "handing them over is a drain, so the JVM is not told twice"
        );
    }

    /// Forgetting the arena forgets the refusals with it: they describe positions in a world that
    /// has just been left, and the JVM clears its own record at the same moment.
    #[test]
    fn clearing_the_arena_clears_the_refusals() {
        let mut storage = SectionStorage::new(pool_for(1));

        assert!(put(&mut storage, IVec3::new(0, 0, 0), 1));
        assert!(!put(&mut storage, IVec3::new(1, 0, 0), 1));

        storage.forget();

        assert!(storage.refused().is_empty());
        assert_eq!(storage.len(), 0, "and the arena is empty as well");
    }

    /// The pool follows the render distance, and the render distance is capped.
    #[test]
    fn a_pool_grows_with_the_render_distance() {
        assert!(arena_slots(8) < arena_slots(16));
        assert!(arena_slots(16) < arena_slots(24));
        assert_eq!(
            arena_slots(64),
            arena_slots(200),
            "the const fn caps its own input, so the slider's far end is one arena"
        );
        assert!(
            ARENA_SLOTS < arena_slots(16),
            "the pool a session starts with is a placeholder for a world that has not reported its \
             render distance yet, not the size of a real one"
        );
    }

    /// Growing the pool keeps every range that is already handed out.
    ///
    /// That is the whole reason the arena can grow at all: the new pool is the old one with more space
    /// after it, so a bigger arena is a buffer copy rather than a re-bake of the world - and a section
    /// that did not fit is one whose geometry is otherwise stuck at whatever it was. See
    /// [`SectionStorage::grow_pool`].
    #[test]
    fn growing_the_pool_keeps_what_is_already_allocated() {
        let mut storage = SectionStorage::new(pool_for(1));

        assert!(put(&mut storage, IVec3::new(0, 0, 0), 1));

        let stored = |storage: &SectionStorage| {
            storage
                .iter()
                .next()
                .and_then(|(_, section)| section.layers[0].clone())
                .expect("the section that fits is in the arena")
        };

        let before = stored(&storage);

        assert!(
            !put(&mut storage, IVec3::new(1, 0, 0), 1),
            "one quad of pool, one quad in it"
        );

        assert!(storage.grow_pool(pool_for(4)));
        assert_eq!(storage.pool_slots(), pool_for(4));

        assert!(
            !storage.grow_pool(pool_for(4)),
            "a request that does not widen the pool is not a growth"
        );

        let after = stored(&storage);

        assert_eq!(after.vertex_range, before.vertex_range);
        assert_eq!(after.index_range, before.index_range);

        assert!(
            put(&mut storage, IVec3::new(1, 0, 0), 1),
            "and the section that did not fit now does"
        );
    }

    /// A refused section does not cost the pool the layers of itself that *did* fit.
    ///
    /// The leak this used to be: a two-layer section whose first layer found room and whose second did
    /// not was dropped whole, and the ranges the first layer had taken were never given back - so a
    /// full arena got fuller with every refusal, which is the one failure mode this whole mechanism
    /// exists to survive.
    #[test]
    fn a_refused_allocation_gives_back_what_it_took() {
        // Room for one quad in the solid layer and none at all for the cutout one: the first layer
        // allocates, the second is refused.
        let mut storage = SectionStorage::new(pool_for(1));

        let free_before = storage.free_slots();

        let layers = [layer(1), layer(1)];

        assert!(
            storage.allocate(IVec3::new(0, 0, 0), &layers).is_none(),
            "the cutout layer has nowhere to go"
        );

        assert_eq!(
            storage.free_slots(),
            free_before,
            "the solid layer's range came back with the refusal"
        );
        assert_eq!(storage.used_slots(), 0);
    }
}
