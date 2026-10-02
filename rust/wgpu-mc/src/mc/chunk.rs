//! # Everything regarding minecraft chunks
//!
//! This handles storing the state of all blocks in a chunk, as well as baking the chunk mesh
//!
//! # Chunk sections?
//!
//! Minecraft splits chunks into 16-block tall pieces called chunk sections, for
//! rendering purposes.
use arrayvec::ArrayVec;
use glam::{IVec2, IVec3, Vec3, Vec3Swizzles, ivec3, vec3};
use range_alloc::RangeAllocator;
use std::collections::HashMap;
use std::fmt::Debug;
use std::ops::Range;
use std::sync::Arc;

use crate::WmRenderer;
use crate::mc::BlockManager;
use crate::mc::block::{BlockModelFace, ChunkBlockState, FaceFlags, ModelMesh, game_bits};
use crate::mc::direction::Direction;
use crate::mc::resource::ResourcePath;
use crate::render::atlas::Atlas;
use crate::render::pipeline::{BLOCK_ATLAS, UV_ANIMATED, UV_GAME_ATLAS, Vertex};
use crate::texture::UV;

pub const CHUNK_WIDTH: usize = 16;
pub const CHUNK_AREA: usize = CHUNK_WIDTH * CHUNK_WIDTH;
pub const CHUNK_HEIGHT: usize = 384;
pub const CHUNK_SECTION_HEIGHT: usize = 16;
pub const SECTION_VOLUME: usize = CHUNK_AREA * CHUNK_SECTION_HEIGHT;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LightLevel {
    pub byte: u8,
}

impl LightLevel {
    pub const fn from_sky_and_block(sky: u8, block: u8) -> Self {
        Self {
            byte: (sky << 4) | (block & 0b1111),
        }
    }

    /// The brighter of two light levels, **component by component** - the game's own
    /// `LightCoordsUtil.max`:
    ///
    /// ```java
    /// public static int max(int coords1, int coords2) {
    ///     return pack(Math.max(block(coords1), block(coords2)), Math.max(sky(coords1), sky(coords2)));
    /// }
    /// ```
    ///
    /// Not the larger of the two bytes: a cell lit by a torch (`block 14, sky 0`) and a cell in daylight
    /// (`block 0, sky 15`) pack as `0xe0` and `0x0f`, and the byte that is larger by that comparison is
    /// the *dark* one - the sky nibble is in the high half. Taking the maximum of the two nibbles
    /// separately is what keeps both.
    pub fn brightest(self, other: LightLevel) -> LightLevel {
        Self::from_sky_and_block(
            self.get_sky_level().max(other.get_sky_level()),
            self.get_block_level().max(other.get_block_level()),
        )
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

    /// **Which model variant the block at this position uses**, or `0` for the first one.
    ///
    /// A blockstate whose variant is a *list* - the lily pad's four quarter turns, a grass tuft's
    /// weights - is one mesh per entry, and the game picks between them **per position**:
    /// `ModelBlockRenderer#tesselateBlock` does `random.setSeed(blockState.getSeed(pos))` and lets
    /// `WeightedVariants#collectParts` draw from a `WeightedList`. Neither the seed arithmetic nor the
    /// weights are reproduced here; the JVM has the game's own `WeightedList` and `RandomSource`, so it
    /// computes the *index* and sends it with the section. See `section::SectionBlocks::variants`.
    ///
    /// `0` is the default because that is what this side drew before the answer existed: a provider
    /// that knows nothing about variants - every test - draws the first entry of a list, which is a
    /// lily pad pointing the way a lily pad points in a world that never picks.
    fn get_model_variant(&self, _pos: IVec3) -> u8 {
        0
    }

    /// **The colour a fluid's faces are tinted by**, which is not the block tint and not a constant.
    ///
    /// The game asks a *fluid* model, not a block one: `FluidRenderer#tesselate` calls
    /// `model.fluidTintSource().colorInWorld(fluidState, blockState, level, pos)`, where the model comes
    /// from `FluidStateModelSet#get(fluidState)`. For water that source is `FluidTintSources.water()`,
    /// and what it returns is **the biome's water colour** - so an ocean, a swamp and a cold river are
    /// three different colours in the game.
    ///
    /// This path used one constant for all of them (`WATER_TINT`), which is the default this returns -
    /// so a provider that knows nothing about biomes, which is every test, draws the water the renderer
    /// drew before this existed.
    ///
    /// The colour arrives in the game's own packing, ARGB with **red in the low byte**, which is the
    /// packing the block tint already arrives in and the one the vertex format reads back. See
    /// [`scale_rgb`].
    fn get_fluid_color(&self, _pos: IVec3) -> u32 {
        WATER_TINT
    }
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
    /// One section's vertices and indices, in u32 slots: about 80 KB, or 909 quads.
    ///
    /// It was **16 000**, and the water is why it is not any more. The measurement above is the pool at
    /// **79%** with only the solid and cutout layers in it, and the transparent layer is a whole layer
    /// more: an ocean section is a surface, a floor and however many risers the shoreline has, and the
    /// fluid mesher has already culled the faces inside the water (it skips a side towards the same
    /// fluid and a top with the same fluid above it), so what is added is bounded by the *surface* of
    /// the water rather than by its volume. Vanilla sizes the same thing from the other end - its
    /// translucent section buffer is 786 432 bytes against 4 194 304 for each of the two opaque ones, a
    /// fifth - and the water is most of what that layer holds.
    ///
    /// A fifth on top of 16 000 is 19 200, and 20 000 is that rounded up: the danger is not the average
    /// section but a *column* that is all water from the surface to the floor, where the average is the
    /// only thing this constant can describe. What it does not cover is `WmRenderer::grow_arena`, which
    /// doubles the pool the moment a section does not fit - see `REFUSED`.
    const SLOTS_PER_SECTION: u32 = 20_000;

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

/// One range, and the arena buffer it is an offset into.
///
/// The pair travels together through every free path - deferred, refused, trimmed, replaced - because a
/// range given back to the wrong pool is a range that pool will hand out while the buffer holding it
/// still does. See [`SectionRanges::buffer`].
pub type ReleasedRange = (u32, Range<u32>);

/// One layer as the renderer needs it to draw: the arena to bind, the indices to draw, and the vertex to
/// draw them from.
///
/// Named rather than written out because it is built in the gather and consumed in the draw loop, and
/// three positional fields of which two are `Range<u32>` is exactly the shape that gets passed in the
/// wrong order - a mistake that draws a section out of another section's bytes rather than failing.
pub type DrawnLayer = (u32, Range<u32>, Range<u32>);

#[derive(Clone)]
pub struct SectionRanges {
    pub vertex_range: Range<u32>,
    pub index_range: Range<u32>,
    /// **Which arena buffer these ranges are offsets into.**
    ///
    /// The arena was one buffer until it turned out that one is not enough: `arena_cap_slots` is the
    /// device's `max_buffer_size / 4`, so the ceiling was 1.31 GB on the machine this was measured on -
    /// and a 32-chunk view needs about 33,800 sections at a measured 19,251 slots each, which is 2.6 GB.
    /// The arena was half the size it needed to be, and sections it could not hold were sections neither
    /// renderer drew.
    ///
    /// The buffer is carried *with* the ranges rather than looked up, because a draw needs all three
    /// together: the index buffer to draw from, the storage buffer the shader reads the vertices out of,
    /// and the range within them. See `RenderGraph::render`, which rebinds when this changes.
    pub buffer: u32,
}

/// One arena buffer's allocator and its size.
///
/// A list of these is what replaced a single pool: every entry is a buffer of up to the device's
/// `max_buffer_size`, so the arena's ceiling is that size times how many of these there are rather than
/// the size of one. See [`SectionStorage::add_arena`].
struct SectionArena {
    allocator: RangeAllocator<u32>,
    pool: u32,
}

impl SectionArena {
    fn new(slots: u32) -> Self {
        SectionArena {
            allocator: RangeAllocator::new(0..slots),
            pool: slots,
        }
    }
}

///The struct representing a Chunk section, with various render layers, split into sections
pub struct SectionStorage {
    storage: HashMap<IVec3, Section>,
    /// One allocator per arena buffer, index for index with the renderer's list of buffers.
    ///
    /// A section names the buffer it lives in ([`SectionRanges::buffer`]), so index `i` here and buffer
    /// `i` in `Scene` are the same arena - the two are kept in step by [`SectionStorage::add_arena`] and
    /// `WmRenderer::grow_arena`, which are the only things that add either.
    arenas: Vec<SectionArena>,
    /// The arena a new section is tried in first, so consecutive allocations land in the same buffer and
    /// the draw path rebinds as little as possible. A hint, not a rule: a section that does not fit here
    /// is tried in the others.
    current_arena: usize,
    /// Ranges waiting to be given back, one bucket per frame that may still be in flight.
    ///
    /// A range stops being *stored* one frame and is reused the next, and the frame that still draws it
    /// may still be on the GPU: a `queue.write_buffer` is only ordered against submissions made after
    /// it. So a range is parked for as many frames as the renderer is allowed to have in flight - the
    /// same number the present paces itself by, because that is the first frame whose submission is
    /// known to have been waited for. See [`SectionStorage::free_deferred`].
    ///
    /// Each entry carries the arena it came from, because there is more than one now: a parked range
    /// given back to the wrong pool is a range that pool will hand out while the buffer that holds it
    /// still does. See [`SectionRanges::buffer`].
    deferred: Vec<Vec<ReleasedRange>>,
    /// The bucket this frame's ranges are parked in.
    deferred_head: usize,
    /// How many buckets there are. See [`SectionStorage::set_deferred_depth`].
    deferred_depth: usize,
    /// Whether the last [`SectionStorage::allocate_ranges`] ran out of every arena.
    refused: bool,
    /// **Whether a growth request has been clamped by the device's buffer limit**, which is the state
    /// the JVM has to know about.
    ///
    /// A refusal on its own is recoverable: `pending_arena_growth` doubles the pool and the section is
    /// re-offered. This is what says the doubling has nowhere to go - the arena is at
    /// `Scene::arena_cap_slots`, the device's own `max_buffer_size`, and from there every further
    /// refusal is permanent for as long as the world needs more room than the device will make.
    ///
    /// It exists because the JVM decides whether to drop Minecraft's mesh *before* the bake is queued,
    /// on a chunk-build thread that cannot see any of this. A section this side has no room for is a
    /// section the game should draw, and there was nothing to tell it so. See
    /// [`SectionStorage::at_capacity`] and `RustChunkBake`.
    at_capacity: bool,
    /// The sections an allocation was refused for since the JVM last asked, as their positions.
    ///
    /// The boolean above says "this allocation failed"; this says *which* ones, because the two sides
    /// have to agree about it: a section the JVM has been told was baked, and which the arena had no
    /// room for, is a hole nothing will ever fill - its rebuild has already happened, and a rebuild
    /// only carries what changed. So the positions are kept until [`SectionStorage::refused`] hands
    /// them over, and the JVM forgets what it had recorded for them.
    ///
    /// A set with no cap. It was a `Vec` capped at 4096 that dropped what it could not hold, and that
    /// cap was the hole: a section refused while the pool was exhausted, past the cap, was counted and
    /// forgotten - never handed over, never re-offered, and drawn by neither side. A refusal leaves its
    /// section in `storage`, so there is nothing to cap: these are positions that are already in the
    /// map, and the set cannot outgrow it.
    refused_pending: std::collections::HashSet<IVec3>,
    /// **The baked geometry of sections the arena had no room for**, waiting for a slot.
    ///
    /// This is the one place a bake's output outlives the frame that produced it, and it exists
    /// because dropping that output is what made a refusal expensive. The drain used to throw the
    /// layers away and hand the position to the JVM, whose only recovery was to have Minecraft
    /// compile all 4096 positions of the section again and send a 27-section payload back over JNI -
    /// for geometry this side had already computed and was holding at that moment.
    ///
    /// What it costs is memory the arena was about to hold anyway: the vertices and indices of one
    /// section, which is exactly what [`BakedLayer`] is. [`SectionStorage::parked_bytes`] sums it, and
    /// [`PARKED_LIMIT`] bounds how many of them may wait at once.
    ///
    /// A refusal that parks a section **takes it out of [`Self::refused_pending`]**, because the two
    /// mean opposite things to the JVM: a refused section is one it has to mesh itself, and a parked
    /// one is one this side is still going to draw.
    parked: HashMap<IVec3, ParkedSection>,
    /// The next [`ParkedSection::seq`]. A counter rather than a clock: the queue is only ever
    /// compared with itself, and a clock would make the order depend on how long a frame took.
    parked_seq: u64,
    width: i32,
}

/// One section's baked geometry, held until the arena has room for it.
///
/// See [`SectionStorage::park`] for why it is held rather than dropped.
struct ParkedSection {
    /// When it was parked, so the oldest can be offered the next free slot first. Oldest first
    /// because a section that has been waiting is one whose ground has been stale for longer.
    seq: u64,
    layers: Vec<BakedLayer>,
}

/// How many refused sections may hold their baked geometry while they wait for a slot.
///
/// A bound rather than none, because the queue is fed by the rate the world changes and drained by
/// the rate the arena grows. A player flying through new terrain while the pool is exhausted would
/// otherwise park geometry for every section they pass, and the memory that was supposed to be the
/// arena's would be this side's instead. Past this the refusal goes the old way - the layers are
/// dropped and the JVM is told - which costs a rebuild rather than a hole.
///
/// **The number is a memory bound, not a latency one**, which is the opposite of
/// `MAX_QUEUED_BAKES` in `wgpu-mc-jni`: a bake waiting in that queue holds 54 pointers, and a section
/// waiting here holds its vertices and its indices. What one of those costs is not a constant this
/// side knows, so the honest figure is the one the report prints - [`SectionStorage::parked_bytes`] -
/// and this is the count that keeps it in the tens of megabytes rather than the hundreds.
pub const PARKED_LIMIT: usize = 256;

impl SectionStorage {
    pub fn new(range: u32) -> Self {
        SectionStorage {
            storage: HashMap::new(),
            width: 0,
            arenas: vec![SectionArena::new(range)],
            current_arena: 0,
            deferred: vec![Vec::new()],
            deferred_head: 0,
            deferred_depth: 1,
            refused: false,
            at_capacity: false,
            refused_pending: std::collections::HashSet::new(),
            parked: HashMap::new(),
            parked_seq: 0,
        }
    }
    /// Narrows or widens the pool to a render distance, which is only possible while it is empty.
    ///
    /// A range allocator cannot be resized under live allocations: every stored section's ranges would
    /// have to be handed out again, and the data they point at is in the buffer. The render distance is
    /// therefore taken from the *first* report, which is a frame or two after the window opens and long
    /// before the world has anything to bake - and a later change is refused here rather than corrupting
    /// what is already drawn.
    pub fn set_pool(&mut self, slots: u32) -> bool {
        if !self.storage.is_empty() {
            return false;
        }

        self.arenas = vec![SectionArena::new(slots)];
        self.current_arena = 0;
        self.refused = false;
        self.at_capacity = false;
        self.deferred.clear();
        self.deferred_head = 0;

        true
    }

    /// Adds an arena buffer, which is how the pool grows now. See [`Self::grow_pool`].
    ///
    /// The caller is responsible for creating the matching GPU buffer in the same position - index `i`
    /// here is buffer `i` in `Scene`, and `WmRenderer::grow_arena` is the only other half.
    pub fn add_arena(&mut self, slots: u32) {
        self.arenas.push(SectionArena::new(slots));
    }

    /// How many arena buffers there are. See [`Self::add_arena`].
    pub fn arena_count(&self) -> usize {
        self.arenas.len()
    }

    /// How many slots the pool has, across every arena.
    pub fn pool_slots(&self) -> u32 {
        self.arenas.iter().map(|arena| arena.pool).sum()
    }

    /// How many slots of the pool are handed out right now.
    ///
    /// The number that says how close the arena is to refusing: `pool_slots - used_slots` is what a
    /// section has to fit into, and a run that is refusing sections is one where that is small or
    /// fragmented. Reported on the terrain line beside the refusal count - see [`LARGEST_SECTION`] for
    /// the other half of the same question.
    pub fn used_slots(&self) -> u32 {
        self.pool_slots().saturating_sub(self.free_slots())
    }

    /// How many slots of the pool are free, counted across the free list rather than as one run.
    pub fn free_slots(&self) -> u32 {
        self.arenas
            .iter()
            .map(|arena| arena.allocator.total_available())
            .sum()
    }

    /// Adds one more arena buffer of this many slots, and answers whether that was allowed.
    ///
    /// **This replaced growing a single buffer in place, and it is the better trade twice over.** The
    /// old growth allocated a bigger buffer and copied the old one into it (`copy_buffer_to_buffer`),
    /// which is a full pass over everything already meshed; appending an arena costs one allocation and
    /// no copy at all, because every range that was handed out still names the buffer it was handed out
    /// in. See [`SectionRanges::buffer`].
    ///
    /// The ceiling is therefore `max_buffer_size` times how many arenas there are, rather than the size
    /// of one - which is what the arena needed: a 32-chunk view wants about 2.6 GB and one buffer stops
    /// at 1.31 GB on the machine this was measured on, so half the world could not be held and the
    /// sections that did not fit were drawn by neither renderer.
    ///
    /// **`slots` comes from the caller's own shortfall**, not from the device's ceiling: an arena sized to
    /// the ceiling is a request for the ceiling, and one that was 17 GB on the machine that found this
    /// killed the process in `create_buffer`. See `WmRenderer::grow_arena_if_asked`.
    ///
    /// `false` when the list is already at `limit`: the caller decides what the limit is, because it is
    /// the one that knows how much video memory it is willing to ask the driver for.
    pub fn grow_pool(&mut self, limit: usize, slots: u32) -> bool {
        if self.arenas.len() >= limit {
            return false;
        }

        self.arenas.push(SectionArena::new(slots));

        true
    }

    /// **Says that the arena is at the device's buffer limit**, so a refusal is not one that growth can
    /// answer. See [`SectionStorage::at_capacity`].
    ///
    /// Set by `WmRenderer::grow_arena` when the request it was handed had to be clamped to
    /// `Scene::arena_cap_slots` - the only place that knows the request has nowhere to go. Cleared when
    /// a pool is set afresh for a world, because a new world may need less than the device will make.
    pub fn set_at_capacity(&mut self, at: bool) {
        self.at_capacity = at;
    }

    /// Whether a refusal is one growth cannot answer. See [`SectionStorage::at_capacity`].
    pub fn at_capacity(&self) -> bool {
        self.at_capacity
    }

    /// How many refused sections are waiting to be handed over. See [`SectionStorage::refused`].
    ///
    /// Read by `terrain_arena_at_capacity`, which is where the JVM asks whether this side is keeping up.
    /// A refusal with the pool twenty per cent free is the case that made this necessary: an arena is a
    /// pool of contiguous ranges, so one that has room in total and none in the size class being asked
    /// for refuses exactly as a full one does, and `at_capacity` stays false through all of it.
    pub fn refusals_waiting(&self) -> usize {
        self.refused_pending.len()
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
    /// be told about and re-bake (see `refused_pending`) - for a frame that is already gone.
    pub fn forget(&mut self) {
        // Collected first, because the loop borrows the storage and the frees borrow the arenas: the
        // ranges carry `(buffer, range)` together, which is the whole reason a range can be given back
        // to the right arena after the section that named it is gone.
        let mut freed = Vec::new();

        for section in self.storage.values() {
            for ranges in section.layers.iter().flatten() {
                freed.push((ranges.buffer, ranges.vertex_range.clone()));
                freed.push((ranges.buffer, ranges.index_range.clone()));
            }
        }

        for (buffer, range) in freed {
            self.free_in(buffer, range);
        }

        self.storage.clear();

        // The refusals described positions in the world that has just been left behind: the JVM is
        // clearing its own record of them at the same moment (`forgetAll`), and handing them over
        // afterwards would have it forget sections of the *new* world that share a coordinate. They
        // are counted as dropped rather than forgotten, so the sum the diagnostic checks still closes.
        //
        // This is the one way a refusal is still lost, and it is not a hole: the world it belonged to
        // is gone, and the new world's sections are offered from scratch.
        REFUSED_DROPPED.fetch_add(
            self.refused_pending.len() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        self.refused_pending.clear();

        // The geometry waiting for a slot described the world that was just left, so it goes with
        // the refusals above and for the same reason: the new world's sections are offered from
        // scratch. Not counted as dropped the way a refusal is - a parked section was never handed to
        // the JVM, so nothing in that sum refers to it.
        self.parked.clear();
        self.parked_seq = 0;
    }
    /// How far the arena reaches from the camera, in chunks.
    pub fn width(&self) -> i32 {
        self.width
    }

    pub fn set_width(&mut self, w: i32) {
        self.width = w;
    }
    /// Drops the sections beyond the view, and **hands their positions back to the caller**.
    ///
    /// The ranges go to the deferred free list as always, but the sections themselves have to be
    /// reported: the JVM suppresses Minecraft's own mesh for every section it believes Rust holds, so a
    /// section dropped here and not reported is one **neither renderer draws** - and it never comes back
    /// either, because the JVM counts it as already sent and the next rebuild therefore carries nothing
    /// for it. That is the same hole as a refusal, by a third route; see `refused_pending`, which is
    /// where a refusal goes, and `RustChunkBake.forgetRefused`, which is what acts on it.
    ///
    /// Without this the trim was also invisible to the two sides' accounting: the arena's storage shrank
    /// while `rustHas` did not, so the claim and the fact drifted apart by exactly what the player flew
    /// past.
    pub fn trim(&mut self, pos: IVec2) -> Vec<IVec3> {
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
                        deferred.push((l.buffer, l.vertex_range.clone()));
                        deferred.push((l.buffer, l.index_range.clone()));
                    }
                }
            }
        }
        let storage = &self.storage;
        let mut abandoned = 0u64;

        // **And the geometry waiting for a slot goes with the section it was baked for.** A parked
        // section is one the JVM still counts as drawn by this side, so dropping it silently is the
        // hole this file has three separate comments about: nothing draws it and nothing ever will,
        // because the JVM believes it is here. Reporting the position is what makes Minecraft's own
        // mesh take over for it.
        //
        // Only the positions the loop above did not already report, which is exactly the parked ones
        // with no section in the storage - and the predicate is the same one, so a parked position
        // that *is* stored is reported once, by that loop.
        self.parked.retain(|k, _| {
            let dist = (k.xz() - pos).abs();
            let radius = self.width + 2;

            if dist.x > radius || dist.y > radius {
                if !storage.contains_key(k) {
                    to_remove.push(*k);
                }

                abandoned += 1;

                return false;
            }

            true
        });

        PARKED_TRIMMED.fetch_add(abandoned, std::sync::atomic::Ordering::Relaxed);

        to_remove.iter().for_each(|pos| {
            self.storage.remove(pos);
        });
        self.defer_free(deferred);

        to_remove
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
    ) -> Option<(Section, Vec<ReleasedRange>)> {
        let mut freed = Vec::new();

        if let Some(previous_section) = self.storage.get(&pos) {
            for layer in &previous_section.layers {
                if let Some(l) = layer.as_ref() {
                    // With the arena they belong to: a range given back to the wrong pool is a range
                    // that pool hands out while the buffer holding it still does.
                    freed.push((l.buffer, l.vertex_range.clone()));
                    freed.push((l.buffer, l.index_range.clone()));
                }
            }
        }

        let section = self.allocate_ranges(baked_layers);

        if self.refused {
            // **Marked, not listed.** The section is left in the storage exactly as it was (see the
            // note below), so the storage is already the set of sections that need re-offering - and a
            // list beside it is a list that can disagree with it. It did: past `REFUSED_LIMIT`
            // positions were counted and thrown away, so a section refused while the pool was
            // exhausted was never reported, never re-offered, and stayed a 16x16x16 hole until the
            // player happened to dirty it. There is no cap now because there is nothing to cap: the
            // flag is a field on a section that is already in the map.
            self.refused_pending.insert(pos);

            return None;
        }

        // Published, so it is no longer waiting for one. `insert` clears the flag as well - this is
        // for the case below, where the section was refused on one frame and accepted on the next
        // without the map ever being touched in between.
        self.refused_pending.remove(&pos);

        Some((section, freed))
    }

    /// **Records that the trim dropped a section**, so the other side stops claiming it.
    ///
    /// The same channel a refusal goes down, and deliberately: both mean "this renderer is not drawing
    /// this section any more", and the JVM's response to either is the same - drop it from `sent` and
    /// `rustHas`, so Minecraft's mesh is the fallback again and the next rebuild carries its blocks.
    ///
    /// Sharing the channel is `mem::take`-shaped on the reading end, which is what makes it affordable
    /// to report every section a long flight leaves behind. The alternative - leaving them claimed -
    /// is what made the claim and the fact drift apart by exactly what the player flew past, and a
    /// section claimed but not held is drawn by neither renderer.
    pub fn forget_trimmed(&mut self, pos: IVec3) {
        self.refused_pending.insert(pos);
    }

    /// Hands over the sections an allocation was refused for, and forgets them.
    ///
    /// Called from the JVM once a tick: what it does with them is drop the record of having sent
    /// them, so the next rebuild of each carries its blocks again and the section gets another
    /// chance at the pool.
    /// **A drain of the pending set rather than of a list that could overflow.** These are sections
    /// already in the storage - a refusal leaves its section exactly as it was - so the set of them is
    /// bounded by the arena's own size and cannot lose an entry the way the capped list this replaced
    /// could. See [`Self::allocate`] for what that cost: a refusal past the old cap was counted and
    /// dropped, which is a section nothing would ever offer again.
    pub fn refused(&mut self) -> Vec<IVec3> {
        let refused: Vec<IVec3> = self.refused_pending.drain().collect();

        REFUSED_REPORTED.fetch_add(refused.len() as u64, std::sync::atomic::Ordering::Relaxed);

        refused
    }

    /// **Keeps a section's baked geometry until the arena has room for it**, rather than letting the
    /// refused allocation throw it away.
    ///
    /// The refusal this answers is the one where everything already went right: the section was
    /// baked, the geometry is in hand, and the pool had no range to put it in. Dropping it there is
    /// what turns a memory problem into a CPU one - the JVM has to be told, Minecraft compiles the
    /// section's 4096 positions again, a 27-section payload crosses JNI, and the result is offered
    /// back to a pool that is exactly as full as it was a moment ago.
    ///
    /// Answers whether it was kept. It is **not** kept in two cases, and both fall back to the old
    /// path: when the arena is at the device's limit ([`Self::at_capacity`]), where there is no later
    /// slot to wait for and Minecraft's own mesh has to take over for good; and when
    /// [`PARKED_LIMIT`] sections are already waiting, where keeping another would be the queue
    /// growing without bound while the player flies.
    ///
    /// **A parked section is removed from [`Self::refused_pending`]**, and that removal is what keeps
    /// the two recoveries from running at once: the JVM is not told, so it goes on counting the
    /// section as sent and goes on suppressing Minecraft's mesh - which is right, because this side
    /// still means to draw it. Left in the set, the section would be meshed by Minecraft *and*
    /// published here, which is one section drawn twice.
    ///
    /// **A section the arena has never held is parked the same way, and it is not the hole the notes
    /// in this file warn about.** A first bake is only offered after the JVM has stopped drawing the
    /// section itself, so the alternative to waiting is not Minecraft's mesh - it is a rebuild that
    /// has to travel through the game's compiler and JNI before anything is drawn at all, which is
    /// strictly slower than the growth this refusal asks for on the same frame.
    pub fn park(&mut self, pos: IVec3, layers: Vec<BakedLayer>) -> bool {
        if self.at_capacity {
            return false;
        }

        // A second entry for a position already waiting is always allowed and costs nothing: the
        // newer geometry is the one that should land, and the older is superseded rather than queued
        // behind it. Two bakes of one section between two free slots is one section, not two.
        if !self.parked.contains_key(&pos) && self.parked.len() >= PARKED_LIMIT {
            return false;
        }

        // **A replacement keeps its place in the queue.** The geometry is newer, but the section has
        // been waiting since it was first parked, and sending it to the back would be punishing it
        // for having changed - the one section in the queue whose ground the player is most likely to
        // be looking at.
        let seq = match self.parked.get(&pos) {
            Some(waiting) => waiting.seq,
            None => {
                self.parked_seq += 1;

                self.parked_seq
            }
        };

        self.parked.insert(pos, ParkedSection { seq, layers });

        // See the note above: parked is the opposite of refused.
        self.refused_pending.remove(&pos);

        PARKED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        true
    }

    /// Hands back everything waiting for a slot, oldest first, and forgets it.
    ///
    /// Drained rather than peeked at, because every one of them is about to be offered the arena
    /// again - and the ones that still do not fit are parked by the same code that parked them the
    /// first time (see the section drain in `wgpu-mc/src/lib.rs`), which is also what puts them back
    /// in order behind whatever arrived in the meantime.
    pub fn take_parked(&mut self) -> Vec<(IVec3, Vec<BakedLayer>)> {
        if self.parked.is_empty() {
            return Vec::new();
        }

        let mut waiting: Vec<(IVec3, u64, Vec<BakedLayer>)> = self
            .parked
            .drain()
            .map(|(pos, parked)| (pos, parked.seq, parked.layers))
            .collect();

        waiting.sort_by_key(|(_, seq, _)| *seq);

        waiting
            .into_iter()
            .map(|(pos, _, layers)| (pos, layers))
            .collect()
    }

    /// How many sections are holding their geometry, waiting for a slot. For the report.
    pub fn parked_len(&self) -> usize {
        self.parked.len()
    }

    /// How many bytes of baked geometry the parked sections are holding. For the report.
    ///
    /// The honest figure for what this queue costs, rather than an estimate per section: sections
    /// differ by an order of magnitude between solid stone and leaves, so this is the sum of what is
    /// actually held.
    pub fn parked_bytes(&self) -> usize {
        self.parked
            .values()
            .flat_map(|parked| parked.layers.iter())
            .map(|layer| layer.vertices.len() + layer.indices.len())
            .sum()
    }

    /// Gives one range back to the arena it came from.
    ///
    /// The buffer index travels with the range everywhere for this reason: a range is an offset into
    /// *one* arena, and giving it back to another is a range the allocator believes is free while the
    /// buffer that holds it still does - which is a section overwritten by the next one that lands there.
    fn free_in(&mut self, buffer: u32, range: Range<u32>) {
        if let Some(arena) = self.arenas.get_mut(buffer as usize) {
            arena.allocator.free_range(range);
        }
    }

    /// Queues ranges to be freed once the frames that may still draw them are done. See [`Self::allocate`].
    pub fn defer_free(&mut self, ranges: Vec<ReleasedRange>) {
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
            for (buffer, range) in bucket {
                self.free_in(buffer, range);
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

        for (buffer, range) in bucket {
            self.free_in(buffer, range);
        }
    }

    /// Publishes an allocated section, which is what makes the next frame draw it.
    ///
    /// Clears the pending-refusal mark, which is the other half of "a refusal leaves the section as it
    /// was": a section refused on one frame and accepted on the next is in the arena, so it must not be
    /// handed to the JVM as refused any more - that would drop its `sent` record and rebuild a section
    /// that is already drawn.
    pub fn insert(&mut self, pos: IVec3, section: Section) {
        self.storage.insert(pos, section);
        self.refused_pending.remove(&pos);
    }

    fn allocate_ranges(&mut self, baked_layers: &[BakedLayer]) -> Section {
        // A full arena is a state this has to survive rather than panic on: the ranges come out of one
        // fixed pool, and a render distance whose sections do not fit in it is a *policy* problem -
        // see `arena_slots` for how the pool is sized, and `allocate` for what happens when it is not
        // enough. Panicking here ended the game (the panic hook throws, and the JVM aborts on a panic
        // through a native frame), which is not a trade a renderer gets to make.
        self.refused = false;

        let mut layers: Vec<Option<SectionRanges>> = Vec::with_capacity(baked_layers.len());

        // **Which arena a section goes in is decided once, for all its layers.** A layer is drawn with
        // the arena its ranges name and the draw loop rebinds when that changes, so splitting one
        // section's layers across two arenas would rebind between two draws of the same section for no
        // reason. The arena that took the first layer is offered to the rest.
        let mut chosen: Option<u32> = None;

        for layer in baked_layers {
            // An empty layer is one that draws nothing; `allocate_range(0)` is not a no-op either, it
            // is an assertion, so the two halves of a layer are checked together.
            if layer.vertices.is_empty() || layer.indices.is_empty() {
                layers.push(None);
                continue;
            }

            // The arenas to try, in order: the one this section is already in, then the one the last
            // section went into, then the rest - so consecutive sections land together and the draw
            // loop's rebinds stay few.
            let first = chosen.unwrap_or(self.current_arena as u32) as usize;
            let mut placed = None;

            for step in 0..self.arenas.len() {
                let arena_index = (first + step) % self.arenas.len();
                let arena = &mut self.arenas[arena_index];

                let Ok(vertices) = arena
                    .allocator
                    .allocate_range(layer.vertices.len() as u32 / 4)
                else {
                    continue;
                };

                let Ok(indices) = arena
                    .allocator
                    .allocate_range(layer.indices.len() as u32 / 4)
                else {
                    // Give the vertices back before trying the next arena: a range the allocator still
                    // believes is handed out is a piece of that arena that never comes back.
                    arena.allocator.free_range(vertices);
                    continue;
                };

                placed = Some((arena_index as u32, vertices, indices));
                break;
            }

            let Some((buffer, vertices, indices)) = placed else {
                // Nowhere held it. `refuse` gives back everything this section already took.
                self.refuse(&mut layers);
                return Section { layers };
            };

            // Sticky for the rest of this section, and a hint for the next one.
            chosen = Some(buffer);
            self.current_arena = buffer as usize;

            layers.push(Some(SectionRanges {
                vertex_range: vertices,
                index_range: indices,
                buffer,
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
        // Collected first: the loop borrows the layers and the frees borrow the arenas, and the ranges
        // carry the buffer they belong to - which is what makes this give each range back to the arena
        // it actually came from rather than to a pool that never held it.
        let mut freed = Vec::new();

        for layer in layers.iter_mut() {
            if let Some(ranges) = layer.take() {
                freed.push((ranges.buffer, ranges.vertex_range));
                freed.push((ranges.buffer, ranges.index_range));
            }
        }

        for (buffer, range) in freed {
            self.free_in(buffer, range);
        }

        self.refused = true;
        report_full_arena(self.pool_slots(), self.used_slots());
    }
    pub fn iter(&self) -> std::collections::hash_map::Iter<'_, IVec3, Section> {
        self.storage.iter()
    }

    /// Whether the arena is holding anything to draw for this section.
    ///
    /// "Holds" rather than "is in the map": a section whose layers were all taken by a range it could not
    /// get is present with nothing in it, and the question a walk asks is whether a camera looking at the
    /// section would see anything - which is the same question the draw loop answers when it finds no
    /// layer to record.
    pub fn holds(&self, pos: IVec3) -> bool {
        self.storage
            .get(&pos)
            .is_some_and(|section| section.layers.iter().any(|layer| layer.is_some()))
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
/// **One way to lose one now, and it is by design: a level change** clears the pending set along with
/// the arena it describes, and that is not a hole - the world it belonged to is gone. There used to be
/// a second, a cap on the list, and it *was* a hole: a refusal past it was counted here and forgotten
/// by both sides. The pending set is unbounded because its members are sections that are already in
/// the storage, so there is nothing left to cap. What is still not by design is a refusal that neither
/// reaches the JVM nor appears here.
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

/// How many refused sections kept their baked geometry instead of being handed to the JVM.
///
/// This is the counter that says the expensive recovery did not happen. Every one of these is a
/// section Minecraft was *not* asked to compile again, a 27-section payload that did not cross JNI,
/// and a section whose `rustHas` stayed true - so it is also the number of round trips this queue
/// removed. Read beside [REFUSED]: a run where the two are close is a run where parking is not
/// keeping up with the refusals, and the arena growth is the thing to look at.
static PARKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many parked sections were dropped because the player left them behind. See [`SectionStorage::trim`].
///
/// The other half of the same diagnostic: geometry that is parked and then abandoned was held for
/// nothing, and a number that is large next to [PARKED] means the queue is filling with sections the
/// view has already moved past - which is memory spent to no end, since their positions are reported
/// to the JVM on the way out and Minecraft meshes them from scratch.
static PARKED_TRIMMED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// See [PARKED].
pub fn sections_parked() -> u64 {
    PARKED.load(std::sync::atomic::Ordering::Relaxed)
}

/// See [PARKED_TRIMMED].
pub fn sections_parked_abandoned() -> u64 {
    PARKED_TRIMMED.load(std::sync::atomic::Ordering::Relaxed)
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

/// The mesh a state is drawn with, at the variant the world chose for the position it stands at.
///
/// `variant` is the game's own per-position choice among a blockstate's list of models, and it is
/// ignored by every state whose variant is not a list. See
/// [`BlockStateProvider::get_model_variant`].
#[inline]
fn get_block(
    block_manager: &BlockManager,
    state: ChunkBlockState,
    variant: u8,
) -> Option<Arc<ModelMesh>> {
    let key = match state {
        ChunkBlockState::Air => return None,
        ChunkBlockState::State(key) => key,
    };

    block_manager
        .blocks
        .get_index(key.block as usize)?
        .1
        .get_model(key.augment, variant)
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
        ChunkBlockState::State(key) => match block_manager.face_flags.get(&key.pack()) {
            Some(flags) => {
                if phases_on() {
                    phase_flags_hit.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }

                *flags
            }
            None => {
                // **Counted, and the count is the answer to a whole class of "nothing is culled".** The
                // table is filled by the JVM (`cacheBlockStates`), so a run that never got one - a test,
                // the offline bench, a world entered before the block cache finished - has an empty table,
                // every state falls back to "occludes nothing", and the picture is a section drawn in full
                // with `phase_face_hidden` at zero and no clue which of the two it was.
                if phases_on() {
                    phase_flags_miss.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }

                FaceFlags::default()
            }
        },
    }
}

/// **The game's `ModelBlockRenderer#forceOpaque`**, which is the whole of what the leaves switch does:
///
/// ```java
/// public static boolean forceOpaque(boolean cutoutLeaves, BlockState blockState) {
///     return !cutoutLeaves && blockState.getBlock() instanceof LeavesBlock;
/// }
/// ```
///
/// Split out from [`bake_layers`] with both answers handed in, so that the combination can be tested
/// without moving either global - the option is a `static` the settings path writes and the class check
/// is a table the JVM fills, and neither can be reached from a test. The truth table is the contract, and
/// the two halves fail in different directions: a leaf sent to the solid layer when the player asked for
/// cut-out leaves is a leaf with its transparent gaps filled in, while a non-leaf sent there is a plant
/// or a pane drawn without its alpha test at all.
fn force_opaque(cutout_leaves: bool, is_leaves: bool) -> bool {
    !cutout_leaves && is_leaves
}

/// Whether a block darkens the corners around it, which is the game's own `getShadeBrightness`.
///
/// Minecraft's ambient-occlusion corner is the average of four `getShadeBrightness` samples, and that
/// method is `state.isCollisionShapeFullBlock(...) ? 0.2F : 1.0F` - so the question is "does this block
/// fill its whole block", and *not* "is it air" (a torch, a plant, a slab, a pane, a fluid are all next
/// to a corner without darkening it) and not "does it occlude" either: `IceBlock` and `TransparentBlock`
/// are both full cubes whose occlusion shape is empty, and only glass overrides the method to `1.0`. Ice
/// darkens corners in vanilla and glass does not, and the two are the same shape - which is why this is
/// the block's own answer, read on the JVM side and carried in [`FaceFlags::shades`], rather than
/// anything derived from the masks here.
#[inline]
fn shades_corners(block_manager: &BlockManager, state: ChunkBlockState) -> bool {
    face_flags(block_manager, state).shades
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
    let phases = phases_on();

    if neighbour.is_air() {
        return false;
    }

    if phases {
        phase_neighbour_state.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    // No model is a neighbour drawn as something else - the fallback block - so the geometry test
    // has nothing to say about it and the face is kept.
    //
    // The neighbour's *variant* is asked for as 0, and that is deliberate: this is a culling question
    // and every entry of a variant list answers it the same way - a lily pad's four quarter turns all
    // cull the same six faces, because turning a quad does not move it off its plane. The world's own
    // choice is not even available here: this runs for the neighbour a face looks at, and the blob the
    // payload carries describes only the section being rebuilt.
    let Some(mesh) = get_block(block_manager, neighbour, 0) else {
        return false;
    };

    if (mesh.cull >> dir.opposite() as u8) & 1 != 1 {
        return false;
    }

    let flags = face_flags(block_manager, neighbour);

    if phases {
        phase_face_flags.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    if flags.occludes(dir.opposite()) {
        return true;
    }

    // **The second lookup is real and it is the one this path is made of.** `state` and `neighbour` are
    // different states - the `state == neighbour` test on this line is what says so - so the `&&`
    // short-circuits exactly when the first lookup has already answered `false` for occlusion, which is
    // the common case for a full cube against a full cube of another kind. Both tables it reads are the
    // same `HashMap<u32, FaceFlags>`; see [`BAKE_PHASES`] for what that costs, because "the hash is the
    // hot part" was a guess until the counters were put in and it is worth knowing either way.
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
        // **Counted, because "no `cullface`" and "a `cullface` and a neighbour that does not hide it"
        // are the same picture and different bugs.** The first is a model that was read without its
        // `cullface`, which is every face of every cube - and it reads as a section that is drawn in
        // full, at the cost of a neighbour lookup per face, with `phase_face_hidden` at zero and
        // nothing to say which of the two it was. See [`BAKE_PHASES`].
        if phases_on() {
            phase_face_no_cullface.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

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
/// The format is `Vertex::compressed`'s, and the two have to agree: the position is sixteen bits an axis
/// at 1/2048 of a block - x and y in the halves of the first word, z in the low half of the second - and
/// the normal is three bits. `None` for anything that does not decode, which is a vertex this diagnostic
/// has nothing to say about rather than a reason to stop.
fn read_vertex(bytes: &[u8]) -> Option<(glam::Vec3, glam::Vec3)> {
    use glam::Vec3;

    if bytes.len() < 16 {
        return None;
    }

    let half = |low: u8, high: u8| u16::from_le_bytes([low, high]) as f32 / 2048.0;

    let position = Vec3::new(
        half(bytes[0], bytes[1]),
        half(bytes[2], bytes[3]),
        half(bytes[4], bytes[5]),
    );

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

/// **Whether a block's biome tint is multiplied in: `false` is a diagnostic, not a setting.**
///
/// A grass block's side is `texture * tint * light`, and the tint is the biome's colour - so the side of one
/// grass block is the one place all three are visible at once, and a player measured it in both renderers.
/// The three channels of the difference were **not proportional** (`R 1.43, G 1.65, B 0.93`), which rules out
/// any single brightness: a shade, an ambient-occlusion term or a lightmap value moves all three together.
///
/// That leaves the tint and the texture sample, and this constant is how the first is switched off. With it
/// `false` every face is baked untinted - so a grass side comes out the grey-green of the texture itself, and
/// comparing *that* against the game answers the question the arithmetic could not: if the two agree, the
/// tint is what differs; if they still do not, the texture being sampled is.
///
/// The tint is not simply dropped: `scale_rgb(color, color)` is the square, which is what doubling the tint
/// would be - that was the first guess and the numbers do not support it, so this is written as a plain
/// replacement with white and the original colour is left in the log by `helperGetBlockColor`.
///
/// `true` is the only correct value for a release.
pub const TINT: bool = true;
/// Whether a sprite's level-of-detail floor is applied, or the sampler's own choice is left alone.
///
/// **`false` is the state that existed before the floor and it is a *comparison*, not a leftover.**
///
/// The history is the reason it exists. A player's runs found the fluid shimmer gone at an `atlas_lod_bias`
/// of `-4` and present at `0` - which says the sampler is choosing a level that is wrong for a moving
/// sprite, and that a four-level shift cancels it. That bias was then reset to `0` for a reason that has
/// since been fixed, and this side replaced it with a per-sprite floor: `bias = lod_bias - floor`, where
/// `floor` is `log2(sprite texels) - 2` - also a negative shift, also large, but **not the same number**.
///
/// A player reported the shimmer's return, and the two candidates are "the floor is the right idea with the
/// wrong depth" and "the floor does not reach the face at all". Turning it off separates them without a
/// rebuild, and it also restores the `-4` setting to being the only bias in play - so the one run that
/// answered this question the first time can be repeated.
///
/// `true` is the intended behaviour; see `vertex::uv_flags`' floor field for what it is for.
pub const LOD_FLOOR: bool = false; // THE COMPARISON: sampling with no offset of ours at all

/// The brightness vanilla bakes into a face's vertex colour, per direction.
///
/// This is `CardinalLighting.DEFAULT` (`net.minecraft.world.level.CardinalLighting`): down 0.5, up
/// 1.0, north and south 0.8, west and east 0.6. `BlockModelLighter#prepareQuadFlat` writes it as a
/// grey `Color` and `#prepareQuadAmbientOcclusion` scales the corner light by it, and since
/// `DefaultVertexFormat.BLOCK` has no normal, that colour is the *only* place the direction of a face
/// reaches the shader. A baker that leaves it out draws every side face at the brightness of a top
/// one - the world reads as overexposed from the side, and nothing about the geometry says why.
///
/// **This is the fluid path's half of it, and the block path no longer calls it**: a model face carries its
/// own `shade` flag now (see [`crate::mc::block::BlockModelFace::shade`]) and goes through
/// `face_brightness`, which reads the dimension's table and honours the flag. A fluid face has no model and
/// no flag, so it keeps the shaded answer - which is what the game's own `FluidRenderer` does.
fn face_shade(dir: Direction) -> f32 {
    // **The table, not the six numbers.** This used to be a `match` over the overworld's values, and the
    // dimension's table now lives behind `face_brightness` - so a copy of the six here would be a second
    // place the brightness could be wrong, which is exactly what the table was introduced to remove.
    crate::mc::block::face_brightness(dir, true)
}

/// A face's colour with its red, green and blue bytes scaled by `factor`.
///
/// The packing is the one the vertex format reads back - red in the low byte, blue in the third -
/// and the top byte is left where it was, so a tint that arrives with no alpha keeps having none.
fn scale_rgb(color: u32, factor: f32) -> u32 {
    let scale = |byte: u32| ((byte as f32 * factor).round().clamp(0.0, 255.0)) as u32;
    let r = scale(color & 0xff);
    let g = scale((color >> 8) & 0xff);
    let b = scale((color >> 16) & 0xff);

    (color & 0xff00_0000) | (b << 16) | (g << 8) | r
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

#[cfg(test)]
mod face_shade_tests {
    use super::*;

    /// The six values are the game's own, because the picture is compared against the game's world.
    ///
    /// A side face one factor too bright is not something a unit test can see - it is a world that
    /// looks overexposed beside vanilla - so the table itself is what is pinned here, against
    /// `CardinalLighting.DEFAULT`.
    #[test]
    fn the_face_shade_is_the_one_the_game_bakes_into_the_colour() {
        for (direction, expected) in [
            (Direction::Down, 0.5),
            (Direction::Up, 1.0),
            (Direction::North, 0.8),
            (Direction::South, 0.8),
            (Direction::West, 0.6),
            (Direction::East, 0.6),
        ] {
            assert_eq!(face_shade(direction), expected, "{direction:?}");
        }
    }

    /// An upward face is the one that does not move: whatever else the table says, up is full
    /// brightness, which is why a bug here shows on the sides and not on the tops.
    #[test]
    fn an_upward_face_keeps_its_colour() {
        assert_eq!(
            scale_rgb(0xffff_ffff, face_shade(Direction::Up)),
            0xffff_ffff
        );
        assert_eq!(
            scale_rgb(0x00e4_763f, face_shade(Direction::Up)),
            0x00e4_763f
        );
    }

    /// Scaling touches the three colour bytes and leaves the fourth alone - red in the low one.
    #[test]
    fn a_scaled_colour_keeps_its_packing() {
        assert_eq!(scale_rgb(0xffff_ffff, 0.6), 0xff99_9999);
        assert_eq!(scale_rgb(0xffff_ffff, 0.5), 0xff80_8080);
        // A tint is scaled rather than replaced: the water colour at half brightness.
        assert_eq!(scale_rgb(0x00e4_763f, 0.5), 0x0072_3b20);
    }
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

/// How many tinted faces took their colour from the block's own cache instead of asking the game again.
///
/// **One counter per mechanism in the cost of a per-face tint callback**, because they are removed by
/// different changes and a run should say which one is doing the work: this one is the several faces of one
/// block sharing one colour, and [`TINTED_FACES_CULLED`] is the cull that now runs before the tint. The
/// other half of the story is the fall in `wgpu_mc_jni`'s tint count, which is the callbacks that are left.
///
/// See `bake_layers` for the cache itself, and `face_is_culled` for the order in `add_face`.
pub static TINT_MEMO_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many culled faces carried a tint index, and so would have asked for a colour before this was moved.
pub static TINTED_FACES_CULLED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Takes both counters, for the JVM's once-a-second atlas line. See [`TINT_MEMO_HITS`].
pub fn take_tint_face_counts() -> (u64, u64) {
    (
        TINT_MEMO_HITS.swap(0, std::sync::atomic::Ordering::Relaxed),
        TINTED_FACES_CULLED.swap(0, std::sync::atomic::Ordering::Relaxed),
    )
}

/// **Where a bake's time actually goes, counted and - for two of the columns - timed.**
///
/// The benchmark in `examples/mesher_bench.rs` measures a whole section and can say *that* it is slow.
/// It cannot say which part is slow, and the three candidates are all plausible from the outside: the
/// 4096-block loop, the model lookup and the face-flag lookup it does per block, the cull test it does
/// per face, and the vertex emission it does for the faces that survive. This is the same measurement
/// at a finer grain, so that a change can be aimed rather than guessed at.
///
/// # Why it is off by default, and why the switch is a bool rather than a level
///
/// A counter is `fetch_add(1, Relaxed)` on a shared cache line, which is free on one thread and a
/// contended read-modify-write on the bake pool. **The two timed columns are the reason it cannot just
/// be left on**: an `Instant::now()` pair is a clock read, and at one pair per face it is a measurable
/// share of the thing it is measuring - a profiler that changes the answer is worse than none.
///
/// So the switch is one `bool`, read once per call rather than per counter, and a bake with it off pays
/// a predictable branch and nothing else. It is set by the benchmark rather than by a setting: this is a
/// development instrument, and the run that wants it knows which scenario it is running.
pub static BAKE_PHASES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the phase counters are on. See [`BAKE_PHASES`].
#[inline]
fn phases_on() -> bool {
    BAKE_PHASES.load(std::sync::atomic::Ordering::Relaxed)
}

macro_rules! phase_counters {
    ($($name:ident => $doc:literal,)*) => {
        $(
            #[doc = $doc]
            // **`static` and lowercase on purpose.** Every other counter in this file is
            // `SCREAMING_CASE`, but these are named after the `BakePhases` fields they back - so that
            // the macro writes one name rather than two, and a reader can go from a column in the
            // benchmark's output to the line that fills it without a second lookup. The lint is
            // suppressed here rather than at each of the fourteen.
            #[allow(non_upper_case_globals)]
            static $name: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        )*

        /// One bake's worth of [`BAKE_PHASES`], as the benchmark and the log line read it.
        #[derive(Clone, Copy, Debug, Default)]
        pub struct BakePhases {
            $(
                #[doc = $doc]
                pub $name: u64,
            )*
        }

        impl BakePhases {
            /// Reads every counter and zeroes it, so two calls measure two windows rather than one
            /// running total.
            ///
            /// **A window is what this gives, and a bake is not the only thing in one.** The counters
            /// are process-wide rather than per bake, so the caller brackets what it wants: the
            /// benchmark takes once after a whole repeat, which is what makes its per-section figures
            /// averages over that repeat. The first version of this cleared them at the top of every
            /// bake instead, which is the same thing read one section too late - every bake wiped what
            /// the section before it had counted, and a run reported all zeroes.
            pub fn take() -> Self {
                Self {
                    $($name: $name.swap(0, std::sync::atomic::Ordering::Relaxed),)*
                }
            }
        }
    };
}

phase_counters! {
    phase_cull_calls => "Every `face_is_culled` call, which is the population the cull time is over.",
    phase_face_hidden => "Of those, the ones the neighbour hid.",
    phase_face_no_cullface => "Of those, the ones whose model declared no `cullface` at all - the answer is `false` before the world is looked at.",
    phase_face_emitted => "Faces that survived the cull and were written out.",
    phase_fluid_face => "Faces the fluid mesher emitted.",
    phase_block_loop => "Blocks the 4096-block loop actually examined.",
    phase_blocks_with_a_model => "Blocks that resolved to a model, which is the model lookup.",
    phase_model_cull_bit => "Blocks whose model declared at least one `cullface`.",
    phase_neighbour_state => "Neighbour states read, which is one provider call per culled face.",
    phase_face_flags => "Face-flag table lookups, the `HashMap<u32, FaceFlags>` probe.",
    phase_flags_hit => "Face-flag lookups that found a row, so the state's masks were used.",
    phase_flags_miss => "Face-flag lookups that found nothing, so the state occludes and hides nothing.",
    phase_vertices_written => "Vertices pushed into a layer, four per emitted face.",
    phase_quad_sort => "Quads the translucent sort looked at.",
    phase_quads_moved => "Quads the translucent sort actually moved.",
    phase_nanos_in_cull => "Nanoseconds in `face_is_culled`, **only while the switch is on**.",
    phase_nanos_in_emit => "Nanoseconds in the emission of a surviving face, same caveat.",
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

    // **One biome tint per colour a block actually needs**, keyed by the model's own tint index and
    // cleared per block. Neither half of that is decoration:
    //
    //  * *per block*, because the question is a property of the **position** - the game's answer for grass
    //    is the biome at that block, and a section on a biome boundary holds two colours - so a cache that
    //    outlived the block would have to be keyed by position as well, and one that lived shorter would
    //    miss the repeat it exists for;
    //  * *per tint index*, because the same block asks for the same index from several faces: a grass block
    //    is one `up` face plus four side overlays, all `tintindex: 0`, and leaves are six faces of it. The
    //    answer cannot change between them, and each one used to be a JNI callback into the JVM's
    //    `BlockColors` - a `BlockPos`, a palette decode and a biome lookup, per face.
    //
    // `clear` rather than a fresh map: the allocation is kept, and a block with no tinted face never inserts
    // at all, which is most of the world.
    let mut tints: HashMap<i32, u32> = HashMap::new();

    for block_index in 0..16 * 16 * 16 {
        let pos = ivec3(block_index & 15, block_index >> 8, (block_index & 255) >> 4);

        tints.clear();

        let fpos = vec3(pos.x as f32, pos.y as f32, pos.z as f32);

        let block_state: ChunkBlockState = state_provider.get_state(pos);

        // Where this block actually stands, which for a plant is not where it was placed: the game
        // gives every one of them a random nudge derived from its own coordinates, so that a field is a
        // field and not a grid. See [`FaceFlags::block_offset`], which is `BlockBehaviour#getOffset`.
        //
        // The hash takes the **world** coordinates - a section knows its own origin and not the
        // block's, so the section offset is added back here, and the `y` it is hashed with is zero
        // whatever the block's height is. See [`block_seed`].
        let flags = face_flags(block_manager, block_state);

        if phases_on() {
            phase_block_loop.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            phase_face_flags.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        let offset = if flags.has_offset() {
            let world = ivec3(
                section_offset.x + pos.x,
                section_offset.y + pos.y,
                section_offset.z + pos.z,
            );

            flags.block_offset(world.x, world.z)
        } else {
            Vec3::ZERO
        };

        // Which watched block this is, if any: counted per block rather than per face, so the cost is
        // one short scan and three atomics for the handful of blocks in the list. See
        // [`WATCHED_BLOCKS`] for what the three numbers are for.
        let watched = watched_slot(&block_manager.watched, block_state);
        let mut faces_drawn = 0u32;
        let mut faces_culled = 0u32;

        // The variant the game picked for *this* position, which is what decides the mesh when the
        // state's variant is a list. See [`BlockStateProvider::get_model_variant`].
        let variant = state_provider.get_model_variant(pos);

        if let Some(model_mesh) = get_block(block_manager, block_state, variant) {
            if phases_on() {
                phase_blocks_with_a_model.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                if model_mesh.cull != 0 {
                    phase_model_cull_bit.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
            // **`ModelBlockRenderer#forceOpaque`, on this side of the fence.** The game sends a leaf
            // block's faces to the *solid* layer when the player has leaves cut out turned off
            // (`GraphicsPreset`'s `Fast`, `Options#cutoutLeaves`), whatever the leaf sprite says - and the
            // solid layer has no alpha test, so the texture's transparent gaps are filled by its own
            // colour and the block reads as a solid mass.
            //
            // This side takes a face's layer from its sprite (`Atlas::sprite_layer`), where leaves are
            // cut out because their texture has holes - so without this line the option did nothing here
            // and `Fast` and `Fancy` drew exactly the same leaves.
            //
            // Resolved once per block rather than once per face, because it is a property of the state.
            //
            // **And from the `flags` already read above, not from a second lookup.** The old call was
            // `state_is_leaves(block_manager, block_state)`, which is `face_flags(...).leaves` - the same
            // table, the same key, one line after the first read of it. Unconditional, so it was one
            // extra hash per block in the whole section whether the block had leaves or not.
            let opaque_leaves = force_opaque(crate::mc::block::cutout_leaves(), flags.leaves);
            // The winding, and the one thing about a baked quad that nothing else can tell you is
            // wrong: the pass culls back faces with a front face of counter-clockwise, and this backend
            // gives Minecraft's shaders OpenGL's clip space (`preprocessing.rs`) - which *mirrors* the
            // clip space, and a mirror turns every triangle over. The model's own vertex order, drawn
            // through that flip, comes out inside out: the faces that point away from the camera are the
            // ones kept, so the terrain shows its insides and loses its outsides. Reversing the two
            // triangles puts the outside back; the same triangles in the other order are
            // `[1, 3, 0, 2, 3, 1]`.
            const INDICES: [u32; 6] = [0, 3, 1, 1, 3, 2];
            let mut add_quad = |face: &BlockModelFace,
                                _light_level: LightLevel,
                                dir: Direction,
                                color: u32| {
                // The face's own share of the light, which is a property of the direction and not
                // of where the block is: see `face_shade`.
                //
                // **And the model's own `shade` flag, which is the game's other branch.** `BlockModelLighter`
                // is `scaleColor(quad.materialInfo().shade() ? cardinalLighting.byFace(direction) :
                // cardinalLighting.up())`, and this side had only the first half until the flag was added to
                // the vendored model schema: a face the model turns shading off for is drawn at `up`, not at
                // its direction's value. `BlockModelFace::shade` is that flag, and `false` is the usual case
                // for a cross-shaped plant.
                //
                // A face in the `any` bucket is the *other* way a model turns shading off - its variant has
                // no direction to speak of - and it arrives here with `Direction::Up`, which is already the
                // same answer, so the two agree rather than doubling up.
                //
                // `TINT` off replaces every block's biome tint with white. A player measured a grass block's
                // side and the three channels of the difference were not proportional - which no single
                // brightness can produce - so the two candidates are the tint and the texture sample, and
                // only one of them can be switched off. See [`TINT`].
                let color = if TINT { color } else { 0xffff_ffff };
                let color = scale_rgb(color, crate::mc::block::face_brightness(dir, face.shade));

                let baked_layer = &mut layers[if opaque_leaves {
                    crate::mc::block::FACES_FORCED_OPAQUE
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                    RenderLayer::Solid
                } else {
                    face.layer
                } as usize];
                let vec_index = baked_layer.vertices.len() / Vertex::VERTEX_LENGTH;

                let dir_vec = dir.to_vec();

                baked_layer.vertices.extend(
                    (0..4)
                        .map(|vert_index| {
                            let model_vertex = face.vertices[vert_index as usize];

                            let (occluders, light_level) =
                                if crate::mc::block::uses_ambient_occlusion(
                                    model_mesh.ambient_occlusion,
                                    model_mesh.any.is_empty(),
                                ) {
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

                                    // The four blocks Minecraft averages for this corner
                                    // (`BlockModelLighter#prepareQuadAmbientOcclusion`): the neighbour
                                    // across the face at the corner, the two blocks beside it, and the
                                    // block the face looks at - `shade0`, `shade1`, the two corner
                                    // samples and `shadeCenter`, one `getShadeBrightness` each.
                                    //
                                    // The count is what the vertex carries, not a brightness: the
                                    // brightness is `1 - 0.2 * count`, which is the same average, and
                                    // counting it here keeps the curve in one place - the shader -
                                    // where a wrong number is one line rather than a re-bake.
                                    // **Four state reads and four table lookups, and this is where the
                                    // ambient-occlusion corner is actually paid for** - it is the one
                                    // part of emission that reaches back into the world. `phase_face_flags`
                                    // says what it costs, which is why the counter is worth having: on a
                                    // solid section none of this runs at all, because no face survives.
                                    let b1 =
                                        shades_corners(block_manager, state_provider.get_state(p1))
                                            as u8;
                                    let b2 =
                                        shades_corners(block_manager, state_provider.get_state(p2))
                                            as u8;
                                    let b3 =
                                        shades_corners(block_manager, state_provider.get_state(p3))
                                            as u8;
                                    let b4 = shades_corners(
                                        block_manager,
                                        state_provider.get_state(pos + dir_vec),
                                    ) as u8;

                                    if phases_on() {
                                        phase_face_flags
                                            .fetch_add(4, std::sync::atomic::Ordering::Relaxed);
                                        phase_neighbour_state
                                            .fetch_add(4, std::sync::atomic::Ordering::Relaxed);
                                    }

                                    let l1 = state_provider.get_light_level(p1);
                                    let l2 = state_provider.get_light_level(p2);
                                    let l3 = state_provider.get_light_level(p3);
                                    let l4 = state_provider.get_light_level(pos + dir_vec);

                                    // The game's smooth lighting, per vertex: the three cells around the corner
                                    // and the cell in front of the face, with the game's rule for the zeroes
                                    // between them. See [`smooth_blend`] - this was a plain average of the four,
                                    // which is what made a corner read darker than the game's.
                                    let light_level = smooth_blend([l1, l2, l3], l4);

                                    (b1 + b2 + b3 + b4, light_level)
                                } else {
                                    (0, state_provider.get_light_level(pos))
                                };

                            Vertex {
                                position: [
                                    fpos.x + model_vertex.position[0] + offset.x,
                                    fpos.y + model_vertex.position[1] + offset.y,
                                    fpos.z + model_vertex.position[2] + offset.z,
                                ],
                                uv: model_vertex.tex_coords,
                                normal: face.normal.to_array(),
                                color,
                                // Which atlas those coordinates are in, decided when the face was
                                // baked: a face whose sprite the game animates is baked with the
                                // game's own coordinates and draws from the game's atlas. See
                                // `UV_GAME_ATLAS`.
                                uv_flags: face.uv_flags,
                                lightmap_coords: light_level.byte,
                                // How many of the four blocks around this corner fill their whole
                                // block, which is what darkens it: see `shades_corners` for the
                                // four and the shader for the curve.
                                ao: occluders,
                            }
                        })
                        .flat_map(Vertex::compressed),
                );

                if phases_on() {
                    phase_vertices_written.fetch_add(4, std::sync::atomic::Ordering::Relaxed);
                }
                baked_layer.indices.extend(
                    INDICES
                        .iter()
                        .flat_map(|index| (index + (vec_index as u32)).to_ne_bytes()),
                );
            };

            // The colour this block's faces are tinted by, asked of the game once per tint index.
            //
            // See the note on `tints` at the top of the section loop for why the scope is a block and what
            // the callback costs. `-1` is the model's own "no tint" and never leaves this side.
            let mut tint_of = |index: i32| -> u32 {
                if index == -1 {
                    return 0xffff_ffff;
                }

                if let Some(color) = tints.get(&index) {
                    TINT_MEMO_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                    return *color;
                }

                let color = state_provider.get_block_color(pos + section_offset, index);
                tints.insert(index, color);

                color
            };

            let mut add_face = |face: &BlockModelFace, dir: Direction| {
                // **Culled first, because a cull is free and a tint is a callback.** The order here used to
                // be the other way round - the colour was asked for at the top of this closure and the face
                // was then found to be culled - so every hidden face in a section paid for a colour nobody
                // would ever see. The two questions are independent: `face_is_culled` reads the model's own
                // `cullface` and the neighbour's state, and a colour cannot change either.
                //
                // The clock read below is why [`BAKE_PHASES`] is a switch: it costs a real share of the
                // thing it times, so a run that wants the split asks for it and a run that does not pays
                // one predictable branch.
                let phases = phases_on();
                let cull_started = phases.then(std::time::Instant::now);

                let culled = face_is_culled(face, block_manager, block_state, state_provider, pos);

                // **Counted whether it culled or not**, because the counter's job is to be the
                // denominator of the two `ns/` figures below it - and a count of the faces that *were*
                // hidden is not the population the cull time was spent on. Counted inside the `if
                // phases` for the reason everything else here is: a shared atomic per face is not free.
                if phases {
                    if let Some(started) = cull_started {
                        phase_nanos_in_cull.fetch_add(
                            started.elapsed().as_nanos() as u64,
                            std::sync::atomic::Ordering::Relaxed,
                        );
                    }

                    phase_cull_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }

                if culled {
                    faces_culled += 1;

                    if phases {
                        phase_face_hidden.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }

                    if face.tint_index != -1 {
                        TINTED_FACES_CULLED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }

                    return;
                }

                faces_drawn += 1;

                if phases {
                    phase_face_emitted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }

                let color = tint_of(face.tint_index);

                // The light comes from the plane the face is *on*, which is what the bucket direction
                // is for; it is not a culling question.
                let light_level: LightLevel = state_provider.get_light_level(pos + dir.to_vec());

                let emit_started = phases.then(std::time::Instant::now);

                add_quad(face, light_level, dir, color);

                if let Some(started) = emit_started {
                    phase_nanos_in_emit.fetch_add(
                        started.elapsed().as_nanos() as u64,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                }
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

                let color = tint_of(face.tint_index);

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
        bake_fluid_faces(
            block_manager,
            state_provider,
            atlas,
            &mut layers,
            section_offset,
        );
    }

    // **The translucent layer's quads are put in back-to-front order, which is the one thing about a
    // translucent mesh that cannot be decided later.** Blending is order-dependent and there is no depth
    // test to hide it: two panes of glass or two surfaces of water in one section blend in whatever
    // order they are drawn, so the order has to be the camera's.
    //
    // This is `SectionCompiler#compile`'s `mesh.sortQuads(builders.buffer(layer), vertexSorting)` and the
    // game's own rule, which is `VertexSorting.DISTANCE_TO_ORIGIN` for a perspective projection:
    //
    //   * the point sorted by is the quad's **centroid**, and the game's own centroid is the midpoint of
    //     its **first and third vertices** (`MeshData#unpackQuadCentroids`: `x0` and `x1` are the first
    //     vertex and the one two strides later, then averaged);
    //   * the order is **descending** by squared distance - `VertexSorting#byDistance` sorts with
    //     `Floats.compare(keys[o2], keys[o1])`, which puts the largest first, so the farthest quad is
    //     drawn first;
    //   * and the coordinates are **section-relative**, because that is the space a section's vertices
    //     are baked in.
    //
    // Sorting the *index buffer* rather than the vertices is the half of this worth copying: a quad's
    // four vertices are the same four whichever order they are visited in, and the six indices of a quad
    // are already relative within it (see the `INDICES` constant), so reordering the six-index groups
    // reorders the quads and nothing else has to move.
    //
    // **Only the translucent layer.** The solid and cutout layers are order-independent - they are
    // opaque, so the depth test decides - and sorting them would be work with no effect.
    sort_translucent_quads(&mut layers[RenderLayer::Transparent as usize]);

    layers
}

/// Puts a layer's quads in back-to-front order, for the camera at the section's origin.
///
/// The rule is the game's; see the call site for where each half of it comes from. The key is the quad
/// centroid, recovered from the packed vertices rather than tracked beside them: a vertex's position is
/// the first four bytes of its sixteen, so the first and third vertices of a quad are two loads each and
/// the two extra `u8`s of a `256` are the game's own behaviour too - it reads the same bytes as floats
/// and gets zero for the same case.
///
/// A quad is `4 * VERTEX_LENGTH` bytes and a quad's indices are six `u32`s, so both lists divide exactly;
/// a layer whose vertex buffer is not a whole number of quads is left alone, because a sort cannot fix a
/// buffer that does not describe quads and reordering it would attach each quad's indices to another
/// quad's vertices.
fn sort_translucent_quads(layer: &mut BakedLayer) {
    const VERTEX_LENGTH: usize = crate::render::pipeline::Vertex::VERTEX_LENGTH;
    const QUAD_VERTICES: usize = 4;
    const QUAD_INDICES: usize = 6;

    let quad_bytes = VERTEX_LENGTH * QUAD_VERTICES;
    let index_bytes = QUAD_INDICES * size_of::<u32>();

    if layer.vertices.is_empty() {
        return;
    }

    if !layer.vertices.len().is_multiple_of(quad_bytes)
        || layer.indices.len() != layer.vertices.len() / quad_bytes * index_bytes
    {
        return;
    }

    let quads = layer.vertices.len() / quad_bytes;

    // The centroid of a quad, in the packed units the vertex writes its position in: **sixteen bits an
    // axis at 1/2048 of a block**, x and y in the halves of the first word and z in the low half of the
    // second. See `Vertex::compressed`. The scale is not applied because this is only ever compared with
    // itself - the order of two quads is the same in steps as in blocks - and the integers stay exact.
    let centroid = |quad: usize| -> i32 {
        let first = quad * quad_bytes;
        // The third vertex is two vertices further on, which is the pair the game averages.
        let third = first + VERTEX_LENGTH * 2;

        // `low` is the byte the axis starts at, and each one is the low half of a little-endian word.
        let axis = |low: usize| -> i32 {
            let at = |base: usize| {
                u16::from_le_bytes([layer.vertices[base + low], layer.vertices[base + low + 1]])
                    as i32
            };

            (at(first) + at(third)) / 2
        };

        let (x, y, z) = (axis(0), axis(2), axis(4));

        x * x + y * y + z * z
    };

    // **One decode per quad, not one per comparison.**
    //
    // The comparator used to call `centroid` on both of its arguments, so every comparison decoded four
    // vertices' worth of bytes - and a comparison happens `O(quads log quads)` times, which for the
    // 1,504 quads a solid ice section emits is seventeen thousand comparisons against fifteen hundred
    // quads. The keys are the same values either way and the order is the same on them, so this is a
    // change of *how often the work is done* and not of what the answer is.
    //
    // `clone` of the index list is not needed here because the sort now reorders a key vector rather
    // than the indices themselves; the move loop below still needs the original order to read from, so
    // `source` stays.
    let mut keyed: Vec<(i32, usize)> = (0..quads).map(|quad| (centroid(quad), quad)).collect();

    // **Counted, because this sort is the one part of baking whose cost is not linear in the faces.**
    // `phase_quads_moved` is what says whether it is worth doing at all: a layer that is already in
    // order pays for the sort and moves nothing.
    let phases = phases_on();

    if phases {
        phase_quad_sort.fetch_add(quads as u64, std::sync::atomic::Ordering::Relaxed);
    }

    keyed.sort_by(|(a, _), (b, _)| {
        // Descending, so the farthest quad is drawn first. `cmp` rather than `partial_cmp` because these
        // are integers with a total order, and a comparator that is not total is a sort whose result is
        // not reproducible - the game is explicit about wanting a total one too (`Floats.compare`).
        //
        // `sort_by` is stable, so two quads at the same distance keep the order they were baked in -
        // which is the game's own behaviour too, and it is why the key is compared and not the pair.
        b.cmp(a)
    });

    let source = layer.indices.clone();

    let mut moved = 0u64;

    for (position, (_, quad)) in keyed.iter().enumerate() {
        if *quad != position {
            moved += 1;
        }

        let from = quad * index_bytes;
        let to = position * index_bytes;

        layer.indices[to..to + index_bytes].copy_from_slice(&source[from..from + index_bytes]);
    }

    if phases {
        phase_quads_moved.fetch_add(moved, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The fluid textures. A fluid has no block model - no elements, no blockstate variant - so nothing
/// else puts its sprites in the block atlas, and the fluid mesher has no texture to sample until
/// they are there: [`crate::mc::MinecraftState::bake_blocks`] allocates them before it uploads the
/// atlas.
pub const FLUID_TEXTURES: [&str; 4] = ["lava_still", "lava_flow", "water_still", "water_flow"];

/// The fluid a block carries: which fluid (1 water, 2 lava, 3 some other, 0 none), how much of it there
/// is out of nine, and **which half of the fluid it is** - the source object or the flowing one. The mod
/// writes it and `wgpu_mc_jni::section` reads the same three fields back out of the same byte, so the
/// shifts and masks here are an ABI: the writer is `fluidByte` in the mod's `RustChunkBake`.
///
/// The third field used to be documented as "falling", which no writer ever set: a falling fluid is not a
/// different fluid to the game - its `FALLING` property is on the *state*, and its type is the flowing
/// object like any spreading block. What the game does distinguish is the source from the flowing half,
/// which is exactly what [`same_fluid`] needs. See the mod's `fluidByte` for the writer's half of it.
pub fn fluid_of(byte: u8) -> (u8, u8, bool) {
    (byte & 0b11, (byte >> 2) & 0b1111, byte & 0b0100_0000 != 0)
}

/// The game's `LightCoordsUtil.smoothBlend`: the light one vertex takes from the four cells around its
/// corner.
///
/// ```java
/// public static int smoothBlend(int neighbor1, int neighbor2, int neighbor3, int center) {
///     if (sky(center) > 2 || block(center) > 2) {
///         if (sky(neighbor1) == 0) neighbor1 |= center & 0xFF0000;      // and the block channel, and the
///         if (block(neighbor1) == 0) neighbor1 |= center & 0xFF;        // same for the other two
///         ...
///     }
///     return neighbor1 + neighbor2 + neighbor3 + center >> 2 & 16711935;
/// }
/// ```
///
/// The rule in the middle is the one this side did not have, and it is why its corners came out darker than
/// the game's: **one of the four cells around a corner of a face is usually the inside of a solid block**,
/// and the light stored inside a solid block is nothing at all - so an average of the four is dragged down
/// by cells that no light reaches and no camera sees. The game lifts every zero it finds up to the value of
/// the cell in front of the face, whenever that cell holds any light at all (more than 2 of 15), and only
/// then averages. A cell that has some light of its own keeps it: this is a floor under the zeroes, not a
/// maximum.
///
/// The average is the plain one, per channel, floored (`>> 2` of the four nibbles packed one per byte) -
/// which is what the sum of four nibbles cannot carry into the other channel.
fn smooth_blend(neighbours: [LightLevel; 3], center: LightLevel) -> LightLevel {
    // `sky(center) > 2 || block(center) > 2`: dim light is not worth lifting anything to, and lifting a
    // zero to a two would move a corner by a step the game does not move it by.
    let lift = center.get_sky_level() > 2 || center.get_block_level() > 2;

    let neighbour = |neighbour: LightLevel| {
        if !lift {
            return neighbour;
        }

        LightLevel::from_sky_and_block(
            if neighbour.get_sky_level() == 0 {
                center.get_sky_level()
            } else {
                neighbour.get_sky_level()
            },
            if neighbour.get_block_level() == 0 {
                center.get_block_level()
            } else {
                neighbour.get_block_level()
            },
        )
    };

    let lifted = neighbours.map(neighbour);

    let average = |channel: fn(&LightLevel) -> u8| {
        let sum =
            lifted.iter().map(channel).map(u16::from).sum::<u16>() + u16::from(channel(&center));

        (sum / 4) as u8
    };

    LightLevel::from_sky_and_block(
        average(LightLevel::get_sky_level),
        average(LightLevel::get_block_level),
    )
}

/// The brighter of two light levels, which is `LightCoordsUtil.max` and not the larger byte. See
/// [`LightLevel::brightest`], which the fluid faces are lit by.
#[cfg(test)]
mod light_level_tests {
    use super::*;

    /// The two nibbles are maxima of their own: a cell lit by a torch and a cell in daylight are each
    /// brighter than the other in one half.
    ///
    /// Worth a test because the packed byte makes the naive answer *wrong in the dark direction*: the
    /// sky light is the high nibble, so `0x0f` (sky 0, block 15) compares as *smaller* than `0xe0`
    /// (sky 14, block 0) and taking the larger byte would light a torch-lit cell with sky light instead.
    #[test]
    fn the_brighter_of_two_levels_is_taken_per_component() {
        let torch = LightLevel::from_sky_and_block(0, 15);
        let daylight = LightLevel::from_sky_and_block(15, 0);

        assert_eq!(torch.byte, 0x0f);
        assert_eq!(daylight.byte, 0xf0);
        assert!(
            daylight.byte > torch.byte,
            "the naive comparison says daylight"
        );

        assert_eq!(
            torch.brightest(daylight),
            LightLevel::from_sky_and_block(15, 15),
            "and the answer keeps both: full sky *and* full block"
        );

        // A covered lava cell: block light 15 in its own cell, nothing in the block above it - which is
        // the pair the fluid's surface is lit by, and the reason lava under a block is not black.
        assert_eq!(
            LightLevel::from_sky_and_block(7, 15).brightest(LightLevel::from_sky_and_block(0, 0)),
            LightLevel::from_sky_and_block(7, 15)
        );

        assert_eq!(torch.brightest(torch), torch);
    }

    /// The game's smooth-light rule, on the cells that make it matter: one of the four around a corner is
    /// usually the inside of a solid block, and that cell holds no light at all.
    ///
    /// Without the lifting, that zero is averaged in like any other sample and the corner comes out darker
    /// than the game draws it - which is what "the shadow in corners is darker than the game's" is.
    #[test]
    fn a_dark_cell_beside_a_lit_one_is_lifted_to_it() {
        let lit = LightLevel::from_sky_and_block(0, 14);
        let dark = LightLevel::from_sky_and_block(0, 0);

        assert_eq!(
            smooth_blend([dark; 3], lit),
            lit,
            "the three zeroes take the lit cell's value, so the average is that value"
        );

        // A cell with some light of its own keeps it: this is a floor under the zeroes, not a maximum.
        assert_eq!(
            smooth_blend([LightLevel::from_sky_and_block(0, 4), dark, dark], lit),
            LightLevel::from_sky_and_block(0, 11),
            "`(4 + 14 + 14 + 14) / 4` is 11 and a half, floored"
        );

        // Nothing lit in front of the face is nothing to lift the zeroes to.
        assert_eq!(smooth_blend([dark; 3], dark), dark);
        assert_eq!(
            smooth_blend(
                [dark, dark, LightLevel::from_sky_and_block(0, 1)],
                LightLevel::from_sky_and_block(0, 1)
            ),
            dark,
            "a cell holding 1 of 15 is not worth lifting anything to - the game lifts only above 2"
        );

        // The two channels are lifted on their own: daylight raises the sky light of its neighbours and
        // leaves their block light where it was.
        assert_eq!(
            smooth_blend(
                [LightLevel::from_sky_and_block(0, 0); 3],
                LightLevel::from_sky_and_block(15, 0)
            ),
            LightLevel::from_sky_and_block(15, 0)
        );

        assert_eq!(
            smooth_blend(
                [
                    LightLevel::from_sky_and_block(0, 9),
                    LightLevel::from_sky_and_block(0, 0),
                    LightLevel::from_sky_and_block(0, 0)
                ],
                LightLevel::from_sky_and_block(15, 0)
            ),
            LightLevel::from_sky_and_block(15, 2),
            "each channel on its own: the sky is lifted to the lit cell's 15, and the block light has \
             nothing to be lifted to - the cell in front of the face holds none - so the 9 is averaged \
             with three zeroes and floored to 2"
        );
    }
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

/// How high a fluid stands in its own block, in blocks - Minecraft's own answer, which is
/// `FluidState#getOwnHeight` with one case lifted:
///
/// ```java
/// // FluidRenderer#getHeight(level, fluidType, pos, state, fluidState)
/// if (fluidType.isSame(fluidState.getType())) {
///     BlockState above = level.getBlockState(pos.above());
///     return fluidType.isSame(above.getFluidState().getType()) ? 1.0F : fluidState.getOwnHeight();
/// }
/// ```
///
/// so a block whose own fluid is directly under more of it is the whole block - a column is one column,
/// not a stack of surfaces - and everything else is `amount / 9`.
///
/// **Nothing about the fluid changes that**, and this function used to say otherwise: lava was drawn at
/// the full block "because lava is thick enough to fill the block it is in", and a falling fluid at the
/// full block "because it is a column rather than a surface". Neither is in the game. A lava *source* is
/// amount 8 - `FluidState#getAmount` is 8 for a source and for a falling fluid, never 9 - so vanilla
/// draws it at `8/9`, one ninth of a block below the top, and that ninth is the whole difference between
/// a lava lake that reads as a liquid surface and a lava lake that reads as a floor of lava-coloured
/// cubes. The column case is real, and it is the `same_above` argument rather than a flag: the same
/// fluid in the block above is what lifts a block to the full height, whether it is falling or not.
///
/// `amount == 0` with a fluid kind is not something the payload produces - a fluid that is there has an
/// amount - so it falls back to the full block, which is what this returned for everything.
fn fluid_height(amount: u8, same_above: bool) -> f32 {
    if same_above || amount == 0 {
        1.0
    } else {
        amount.min(9) as f32 / 9.0
    }
}

/// Whether two fluid bytes hold **the same fluid object**, which is what the game's `isSame` asks and is
/// not the same question as "the same kind of fluid":
///
/// ```java
/// // Fluid
/// public boolean isSame(Fluid other) { return other == this; }
/// ```
///
/// A fluid is two registered objects, a source and a flowing one, and a block answers whichever its level
/// came from (`LiquidBlock`'s state cache: level 0 is `getSource(false)`, levels 1 to 7 and the falling
/// level 8 are `getFlowing(...)`). So `FLOWING_LAVA.isSame(LAVA)` is false, and vanilla's height and flow
/// arithmetic treats a source beside a flowing block as **two liquids that do not join**: the flowing half
/// is not "the same fluid above" for the source, `affectsFlow` is false between them, and a corner between
/// them takes the other one as a zero.
///
/// This side draws the two halves with the same sprite - they are one liquid to look at - but asks this
/// question wherever vanilla does, which is why the byte carries the bit. Getting that wrong is what "the
/// lava's flowing state still does not match the game" was: the surfaces and the flow directions came out
/// as one continuous liquid where vanilla steps.
pub fn same_fluid(kind: u8, flowing: bool, other_kind: u8, other_flowing: bool) -> bool {
    kind != 0 && kind == other_kind && flowing == other_flowing
}

/// What one block contributes to a corner sample: the game's `FluidRenderer#getHeight`, sentinel and all.
///
/// ```java
/// private float getHeight(BlockAndTintGetter level, Fluid fluidType, BlockPos pos, BlockState state,
///                         FluidState fluidState) {
///     if (fluidType.isSame(fluidState.getType())) {                       // the *same object*
///         BlockState aboveState = level.getBlockState(pos.above());
///         return fluidType.isSame(aboveState.getFluidState().getType()) ? 1.0F : fluidState.getOwnHeight();
///     } else {
///         return !state.isSolid() ? 0.0F : -1.0F;
///     }
/// }
/// ```
///
/// So a block answers one of four things:
///
/// - `1.0` - it holds **this fluid object** with the same one above it, which is a column;
/// - `amount / 9` - it holds this fluid object without it above;
/// - `0.0` - it does not hold it and is **not solid**: air, a plant, a different fluid, and the *other
///   half of this same fluid*. That is the weight-one zero that tapers a fluid's edge;
/// - `-1.0` - it does not hold it and *is* solid. The average drops those entirely, and it is what keeps
///   a fluid's surface at its own height where a stone wall rises beside it.
///
/// `blocks_motion` stands in for `isSolid()` here, which is `block != COBWEB && block != BAMBOO_SAPLING &&
/// isSolid()`: the two agree for everything that matters to a fluid - air, stone, and a fluid itself,
/// which is not solid - and differ for a cobweb and a bamboo sapling, which this then treats as a 0.0
/// sample rather than dropping.
fn sampled_height<Provider: BlockStateProvider>(
    state_provider: &Provider,
    block_manager: &BlockManager,
    pos: IVec3,
    kind: u8,
    flowing: bool,
) -> f32 {
    let (other_kind, amount, other_flowing) = fluid_of(state_provider.get_fluid(pos));

    if same_fluid(kind, flowing, other_kind, other_flowing) {
        let (above_kind, _, above_flowing) = fluid_of(state_provider.get_fluid(pos + IVec3::Y));

        return fluid_height(amount, same_fluid(kind, flowing, above_kind, above_flowing));
    }

    if face_flags(block_manager, state_provider.get_state(pos)).blocks_motion {
        -1.0
    } else {
        0.0
    }
}

/// The game's `FluidRenderer#addWeightedHeight`, into a `(sum, weight)` pair:
///
/// ```java
/// private void addWeightedHeight(float[] weightedHeight, float height) {
///     if (height >= 0.8F) {
///         weightedHeight[0] += height * 10.0F;
///         weightedHeight[1] += 10.0F;
///     } else if (height >= 0.0F) {
///         weightedHeight[0] += height;
///         weightedHeight[1]++;
///     }
/// }
/// ```
///
/// A height of `0.8` or more counts **ten times**, and a negative one - the `-1.0` a solid block answers
/// with - is dropped. That weighting is the whole reason a fluid's surface does not sag towards its edges:
/// the block's own height outweighs the zeroes beside it ten to one.
fn add_weighted_height(total: &mut (f32, f32), height: f32) {
    if height >= 0.8 {
        total.0 += height * 10.0;
        total.1 += 10.0;
    } else if height >= 0.0 {
        total.0 += height;
        total.1 += 1.0;
    }
}

/// The height of a fluid's surface at one corner of a block, in blocks - the game's
/// `FluidRenderer#calculateAverageHeight`:
///
/// ```java
/// private float calculateAverageHeight(BlockAndTintGetter level, Fluid type, float heightSelf,
///                                      float height2, float height1, BlockPos cornerPos) {
///     if (!(height1 >= 1.0F) && !(height2 >= 1.0F)) {
///         float[] weightedHeight = new float[2];
///         if (height1 > 0.0F || height2 > 0.0F) {
///             float heightCorner = this.getHeight(level, type, cornerPos);
///             if (heightCorner >= 1.0F) {
///                 return 1.0F;
///             }
///             this.addWeightedHeight(weightedHeight, heightCorner);
///         }
///         this.addWeightedHeight(weightedHeight, heightSelf);
///         this.addWeightedHeight(weightedHeight, height1);
///         this.addWeightedHeight(weightedHeight, height2);
///         return weightedHeight[0] / weightedHeight[1];
///     } else {
///         return 1.0F;
///     }
/// }
/// ```
///
/// `side_first` and `side_second` are the two blocks that share this corner with `pos` - north and west
/// for the north-west corner, north and east for the north-east one, and so on - and they are sampled
/// from the *same* fluid object as `pos`, which is the caller's job. The diagonal block is only sampled
/// at all when one of the two sides holds something, which is why a corner out in the open does not
/// average in the block diagonally behind it.
///
/// This used to be a plain mean of the corner blocks that held the same fluid, which is both a different
/// weighting *and* a different set of blocks: it left out the zeroes air and a different fluid contribute,
/// so a surface was level where the game's tapers at an open edge, and it returned the full block for a
/// corner touching another fluid, where the game gives a `0.0` or drops the sample.
fn fluid_corner_height<Provider: BlockStateProvider>(
    state_provider: &Provider,
    block_manager: &BlockManager,
    pos: IVec3,
    kind: u8,
    flowing: bool,
    self_height: f32,
    corner: IVec2,
) -> f32 {
    // The two blocks that share this corner with `pos`: the sign of the corner's own y is which way they
    // lie, and the corner's four blocks are `pos` plus those two plus the diagonal between them.
    let step_x = if corner.x == 0 { -IVec3::X } else { IVec3::X };
    let step_z = if corner.y == 0 { -IVec3::Z } else { IVec3::Z };

    let side_x = sampled_height(state_provider, block_manager, pos + step_x, kind, flowing);
    let side_z = sampled_height(state_provider, block_manager, pos + step_z, kind, flowing);

    if side_x >= 1.0 || side_z >= 1.0 {
        return 1.0;
    }

    let mut total = (0.0, 0.0);

    if side_x > 0.0 || side_z > 0.0 {
        let corner_height = sampled_height(
            state_provider,
            block_manager,
            pos + step_x + step_z,
            kind,
            flowing,
        );

        if corner_height >= 1.0 {
            return 1.0;
        }

        add_weighted_height(&mut total, corner_height);
    }

    add_weighted_height(&mut total, self_height);
    add_weighted_height(&mut total, side_x);
    add_weighted_height(&mut total, side_z);

    if total.1 == 0.0 {
        1.0
    } else {
        total.0 / total.1
    }
}

/// How high a fluid stands, and the one thing that was wrong about it. See [`fluid_height`].
#[cfg(test)]
mod fluid_height_tests {
    use super::*;

    /// A source block is eight ninths of a block, and that ninth is the difference between a surface and
    /// a cube.
    ///
    /// This is the reported picture: a lava lake drawn with every block at the full height has no
    /// surface at all - the sides and the top are one unbroken shape and the lava reads as a floor of
    /// lava-coloured blocks, which is exactly what "lava looks like a cube" describes. The game draws a
    /// source at `8/9` for every fluid there is, lava included.
    #[test]
    fn a_source_block_is_eight_ninths_of_a_block() {
        assert_eq!(fluid_height(8, false), 8.0 / 9.0);
        assert_ne!(
            fluid_height(8, false),
            1.0,
            "which is what a lava lake drawn at the full block looks like it is not"
        );
    }

    /// What *does* fill a block is more of the same fluid above it - the game's `same_above`, and the
    /// reason a falling column is not a stack of surfaces. Not a property of the fluid: this used to be
    /// two special cases, one for lava and one for a "falling" flag the payload never even set.
    #[test]
    fn the_block_is_full_only_under_more_of_the_same_fluid() {
        assert_eq!(fluid_height(8, true), 1.0);
        assert_eq!(
            fluid_height(4, true),
            1.0,
            "even a thin layer, with more of it above"
        );

        for amount in 1..=8u8 {
            assert_eq!(fluid_height(amount, false), amount as f32 / 9.0);
        }
    }

    /// A fluid whose amount never arrived is the full block, which is what the old form returned for
    /// everything: the payload cannot produce it, and a guess in the other direction would sink a fluid
    /// into its own block.
    #[test]
    fn a_fluid_with_no_amount_is_left_at_the_full_block() {
        assert_eq!(fluid_height(0, false), 1.0);
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

/// One point of a fluid's sprite, in the sixteenths of a pixel a baked vertex stores.
///
/// Nothing here is a pixel coordinate, and that is the point: the game's own fluid renderer spells every
/// coordinate it uses as a *fraction* of the sprite - `TextureAtlasSprite#getU(0.5F)` is the sprite's
/// middle whatever its size in pixels - and the two atlases a face can be baked for pack that sprite at
/// different sizes. `chunk.rs`'s old form added whole pixels to a rectangle's corner, which is the same
/// picture for the sprite sizes vanilla ships (`water_flow.png` is 32x32 a frame, and the half-sprite
/// offsets are 16 pixels) and a different one for every pack that ships something else.
///
/// The offsets a fluid face uses are exactly the game's:
///
/// | face | sprite | offsets |
/// | --- | --- | --- |
/// | top, still water or lava | `*_still` | the whole sprite, `0..1` both ways |
/// | top, flowing | `*_flow` | a quarter of the sprite turned to the flow, `0.5 ± (cos ± sin) * 0.25` |
/// | sides | `*_flow` | `u` 0 to 0.5, `v` from `(1 - height) * 0.5` at the surface to 0.5 at the bottom |
/// | underside | `*_still` | the whole sprite |
///
/// Which of the two a top face uses is the fluid's own flow: the still sprite when `FluidState#getFlow`
/// is zero - a lake - and a rotated quarter of the flowing sprite when it is not, which is what makes a
/// stream read as a current rather than as a square of texture. See [`fluid_flow`] and
/// [`flowing_top_offsets`].
#[derive(Clone, Copy)]
struct FluidSprite {
    /// This side's rectangle for the sprite: the whole frame *strip* when the game animates it.
    atlas: UV,
    /// Where the game put the sprite - one frame of it - when the game animates it and the terrain pass
    /// has its atlas bound. Faces baked with those coordinates animate for free. See [`UV_GAME_ATLAS`].
    game: Option<[f32; 4]>,
    /// Whether the game animates this sprite, which is what the shader's level-of-detail handling is scoped
    /// to. Read from the sprite's `.mcmeta` alongside the rectangle, in the same call. See [`Self::flags`].
    animated: bool,
    /// **The coarsest level of the game's chain this sprite may be sampled from**, which is the floor the
    /// shader applies and the reason the lava stops shimmering.
    ///
    /// **This field did not exist, and that is why the shimmer came back after it had been fixed.** The
    /// block-model path takes all three answers out of `SpriteInAtlas` at once; the fluid path took two of
    /// them - `rect` and `animated` - so `sprite_level_floor(None, true)` answered **zero**, the shader's
    /// `bias` stayed at the setting's own value, and a fluid face returned to the sampler's automatic level
    /// choice. Which is exactly what the `-4` offset used to stand in for.
    ///
    /// The order of events is worth keeping, because it is why the omission survived a fix that was measured
    /// as working: setting `UV_ANIMATED` is what *enabled* the shader's floor branch for fluids, so it also
    /// enabled the branch that reads this field. Before it the branch was dead and the missing field cost
    /// nothing; after it, the field was the whole difference between a floor and none.
    level_cap: Option<u32>,
}

impl FluidSprite {
    /// The rectangle **one frame** of this sprite covers in this side's atlas.
    ///
    /// An animated sprite is packed as a vertical strip of square frames - `lava_flow.png` is 32x512,
    /// sixteen frames of 32x32, and this side packs the whole strip - while every offset a fluid face
    /// uses is a fraction of *one* frame. Taking the strip's full height would squeeze all sixteen
    /// frames onto the face; the frames are square, so one frame is as tall as the strip is wide. A
    /// sprite that is not a strip is its own frame.
    fn frame(&self) -> (u16, u16, u16, u16) {
        let (x0, y0) = self.atlas.0;
        let (x1, y1) = self.atlas.1;

        let width = x1.saturating_sub(x0);
        let height = y1.saturating_sub(y0).min(width);

        (x0, y0, x0 + width, y0 + height)
    }

    /// One point of this sprite, from an offset across and down it: `0.0` is its top-left corner and
    /// `1.0` its bottom-right. This is `TextureAtlasSprite#getU`/`#getV`, which is what the game's own
    /// fluid renderer passes.
    fn at(&self, u: f32, v: f32) -> [u16; 2] {
        match self.game {
            // The game's own coordinates, in the game's own atlas: `0..1` over the rectangle, stored as
            // the sixteen bits filled edge to edge. See `UV_GAME_ATLAS`.
            Some(rect) => [
                game_bits(rect[0] + u * (rect[2] - rect[0])),
                game_bits(rect[1] + v * (rect[3] - rect[1])),
            ],
            None => {
                let (x0, y0, x1, y1) = self.frame();

                [
                    (x0 as f32 + u * (x1 - x0) as f32).round() as u16,
                    (y0 as f32 + v * (y1 - y0) as f32).round() as u16,
                ]
            }
        }
    }

    /// Which atlas a face of this sprite is baked for, and whether it moves. See [`UV_GAME_ATLAS`] and
    /// `Vertex::uv_flags`' `UV_ANIMATED`.
    ///
    /// **`UV_ANIMATED` was missing here and that is the whole of a long search.** The block-model path sets
    /// it from the sprite's `.mcmeta` (`face_data`), and the shader's level-of-detail floor, its
    /// animated-minified sampler and its offset are all gated on `in.animated == 1u` - so every one of them
    /// was inert for the fluids. Five separate changes to how a minified animated face is sampled were
    /// measured as reaching the faces they were written for (counters, not reasoning) and none of them was
    /// on the fluid path at all: this function answered `UV_GAME_ATLAS` alone, and a fluid with a game
    /// rectangle was therefore indistinguishable from a *static* sprite on the game's atlas.
    ///
    /// The two are separate flags rather than one because they answer different questions - which texture,
    /// and does it move - and the fluid case is the one where they come apart: a fluid with no game
    /// rectangle is this side's frozen copy and does not move either.
    fn flags(&self) -> u32 {
        let flags = match (self.game.is_some(), self.animated) {
            (true, true) => UV_GAME_ATLAS | UV_ANIMATED,
            (true, false) => UV_GAME_ATLAS,
            _ => 0,
        };

        // **Counted here because nothing else counts these, and a player reported the lava shimmering
        // again.** `FACES_ANIMATED` and `FACES_LOD_FLOORED` are the block-model path's counters and a fluid
        // face never touches them, so "did the fluid get the animated flag" - the one fact the entire fluid
        // shimmer fix rests on - was invisible in every log this renderer produces. A shader that is gated
        // on `in.animated == 1u` and a bake that may or may not set it is a pair that has to be measured
        // rather than reasoned about; this is the measurement.
        if self.animated && self.game.is_some() {
            FLUID_FACES_ANIMATED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            // **And whether this face got a floor at all, which is the number that was silently zero.**
            // `sprite_level_floor` answers zero for a sprite nothing registered a cap for, and a zero floor
            // *is* no floor - so "the flag arrived" and "the floor arrived" are two separate facts and the
            // shimmer needed both. See the `level_cap` field for how long the second was missing while the
            // first was reported as present.
            match crate::mc::block::sprite_level_floor_for(self.level_cap, true) {
                0 => FLUID_FACES_UNFLOORED.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                _ => FLUID_FACES_FLOORED.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            };
        } else if self.game.is_some() {
            FLUID_FACES_STATIC.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        flags
    }
}

/// **How many fluid faces were baked with `UV_ANIMATED`, and how many without.** See [`FluidSprite::flags`].
///
/// The pair rather than one number, because the question is which of the two a fluid landed in: all-static
/// is the shimmer, all-animated is the fix, and either is a fact a picture cannot be asked about.
pub static FLUID_FACES_ANIMATED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// How many fluid faces were baked from a game rectangle **without** the animated flag. See
/// [`FLUID_FACES_ANIMATED`].
pub static FLUID_FACES_STATIC: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// **How many animated fluid faces got a non-zero level-of-detail floor**, and how many got zero.
///
/// The second of these is the regression counter: a zero floor is no floor, and it is what the missing
/// `level_cap` field produced for every fluid face in the world while `FLUID_FACES_ANIMATED` read as
/// healthy. Reported beside it so the two cannot be confused again. See [`FluidSprite::flags`].
pub static FLUID_FACES_FLOORED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many animated fluid faces were baked with **no** floor. See [`FLUID_FACES_FLOORED`].
pub static FLUID_FACES_UNFLOORED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// How short of a whole block a fluid is drawn when it is falling: the game's `0.8888889F`, which is
/// `8/9` - the height of a source block. See [`fluid_flow`], the one place it is used.
const MAX_FLUID_HEIGHT: f32 = 0.8888889;

/// Which way a fluid is flowing across its own surface, in blocks of x and z - the game's own
/// `FlowingFluid#getFlow`, and `(0, 0)` when it is standing still.
///
/// ```java
/// for (Direction direction : Direction.Plane.HORIZONTAL) {     // NORTH, EAST, SOUTH, WEST
///     FluidState neighbourFluid = level.getFluidState(pos.relative(direction));
///     if (this.affectsFlow(neighbourFluid)) {                  // empty, or the same fluid
///         float neighborHeight = neighbourFluid.getOwnHeight();
///         float distance = 0.0F;
///         if (neighborHeight == 0.0F) {
///             if (!level.getBlockState(neighbourPos).blocksMotion()) {
///                 FluidState belowNeighborState = level.getFluidState(neighbourPos.below());
///                 if (this.affectsFlow(belowNeighborState)) {
///                     neighborHeight = belowNeighborState.getOwnHeight();
///                     if (neighborHeight > 0.0F) {
///                         distance = fluidState.getOwnHeight() - (neighborHeight - 0.8888889F);
///                     }
///                 }
///             }
///         } else if (neighborHeight > 0.0F) {
///             distance = fluidState.getOwnHeight() - neighborHeight;
///         }
///         if (distance != 0.0F) { flowX += direction.getStepX() * distance; flowZ += direction.getStepZ() * distance; }
///     }
/// }
/// return new Vec3(flowX, 0.0, flowZ).normalize();
/// ```
///
/// So it is the *gradient* of the fluid's own height over its four horizontal neighbours, and the one
/// thing that is not a height is the `blocksMotion` case: a neighbour with no fluid of its own that a
/// fluid can flow *past* (air, a plant - anything without collision) is looked through to the block
/// under it, and the fluid there counts as one block's height lower. That is what makes a stream's
/// surface point at the edge it is about to fall over.
///
/// The falling case of the game's function is left out: the downward part of `getFlow` is chosen by the
/// fluid's own `FALLING` property, which this side's payload does not carry. What it would add is a
/// vertical component, and the only thing the caller uses is the horizontal *direction*.
///
/// `affectsFlow` is the game's `neighbourFluid.isEmpty() || neighbourFluid.getType().isSame(this)`, which
/// is *object* identity: the **other half** of this same fluid - a source beside a flowing block - does not
/// affect the flow, and neither does a different fluid. Getting that wrong is the other half of "the lava's
/// flowing state still does not match the game": the flow vector decides which way the rotated quarter of
/// the flowing sprite points, so a neighbour that should not count changes the *direction of the pattern*.
///
/// The result is normalized the way the game normalizes it - `Vec3#normalize` answers zero for a vector
/// that is already zero rather than dividing by it - because the caller's first question is whether
/// either component is zero.
fn fluid_flow<Provider: BlockStateProvider>(
    state_provider: &Provider,
    block_manager: &BlockManager,
    pos: IVec3,
    kind: u8,
    flowing: bool,
    own_amount: u8,
) -> (f32, f32) {
    let own_height = fluid_height(own_amount, false);
    let (mut flow_x, mut flow_z) = (0.0f64, 0.0f64);

    // The game's `Direction.Plane.HORIZONTAL`, in the order it iterates: north, east, south, west.
    for dir in [
        Direction::North,
        Direction::East,
        Direction::South,
        Direction::West,
    ] {
        let neighbour = pos + dir.to_vec();
        let (neighbour_kind, neighbour_amount, neighbour_flowing) =
            fluid_of(state_provider.get_fluid(neighbour));

        // `affectsFlow`: `neighbourFluid.isEmpty() || neighbourFluid.getType().isSame(this)` - a fluid
        // flows towards nothing and towards the same fluid object. An **empty** neighbour affects the
        // flow (it is what the "is there fluid below it" case below is about), a different fluid does
        // not, and neither does the other half of this one.
        if neighbour_kind != 0 && !same_fluid(kind, flowing, neighbour_kind, neighbour_flowing) {
            continue;
        }

        let neighbour_height = if neighbour_kind == 0 {
            0.0
        } else {
            fluid_height(neighbour_amount, false)
        };

        let distance = if neighbour_height == 0.0 {
            // No fluid in the neighbour at all: can the fluid flow *past* it? If it can - air, a plant,
            // anything without collision - then the fluid below that block is what this one is heading
            // for, and it counts as one block down less the eighth of a block a falling fluid is drawn
            // short by (`MAX_FLUID_HEIGHT`, the `0.8888889` in the game's line).
            if face_flags(block_manager, state_provider.get_state(neighbour)).blocks_motion {
                0.0
            } else {
                let below = neighbour - IVec3::Y;
                let (below_kind, below_amount, below_flowing) =
                    fluid_of(state_provider.get_fluid(below));

                // `affectsFlow` again, and `getOwnHeight` of an *empty* fluid is zero: a neighbour with
                // nothing under it either leaves the distance at zero.
                if below_kind != 0 && !same_fluid(kind, flowing, below_kind, below_flowing) {
                    0.0
                } else {
                    let below_height = if below_kind == 0 {
                        0.0
                    } else {
                        fluid_height(below_amount, false)
                    };

                    if below_height > 0.0 {
                        own_height - (below_height - MAX_FLUID_HEIGHT)
                    } else {
                        0.0
                    }
                }
            }
        } else {
            own_height - neighbour_height
        };

        if distance != 0.0 {
            let step = dir.to_vec();

            flow_x += step.x as f64 * distance as f64;
            flow_z += step.z as f64 * distance as f64;
        }
    }

    // `Vec3#normalize`: zero stays zero rather than becoming a division by it, and the caller's first
    // question is whether both components are zero.
    let length = (flow_x * flow_x + flow_z * flow_z).sqrt();

    if length < 1e-4 {
        (0.0, 0.0)
    } else {
        ((flow_x / length) as f32, (flow_z / length) as f32)
    }
}

/// The four corner offsets a **flowing** fluid's top face is drawn with: a quarter of the flowing
/// sprite, turned by the flow's angle.
///
/// `FluidRenderer#tesselate` asks `FluidState#getFlow` for the top face of a fluid, and if either
/// horizontal component is not zero it draws the face as a quarter of the *flowing* sprite turned to
/// point along the flow:
///
/// ```java
/// float angle = (float)Mth.atan2(flow.z, flow.x) - (float)(Math.PI / 2);
/// float s = Mth.sin(angle) * 0.25F;
/// float c = Mth.cos(angle) * 0.25F;
/// u00 = sprite.getU(0.5F + (-c - s)); v00 = sprite.getV(0.5F + (-c + s));
/// u01 = sprite.getU(0.5F + (-c + s)); v01 = sprite.getV(0.5F + (c + s));
/// u10 = sprite.getU(0.5F + (c + s));  v10 = sprite.getV(0.5F + (c - s));
/// u11 = sprite.getU(0.5F + (c - s));  v11 = sprite.getV(0.5F + (-c - s));
/// ```
///
/// and those four go to the corners in the order north-west, south-west, south-east, north-east, which
/// is the order this mesher passes its four heights in. The offsets are fractions of the sprite, so they
/// go to [`FluidSprite::at`] as they are; they stay inside `0.146 .. 0.854` for every angle, because
/// `|c|` and `|s|` are at most a quarter.
fn flowing_top_offsets(flow_x: f32, flow_z: f32) -> [(f32, f32); 4] {
    let angle = flow_z.atan2(flow_x) - std::f32::consts::FRAC_PI_2;
    let s = angle.sin() * 0.25;
    let c = angle.cos() * 0.25;
    let middle = 0.5;

    [
        (middle + (-c - s), middle + (-c + s)),
        (middle + (-c + s), middle + (c + s)),
        (middle + (c + s), middle + (c - s)),
        (middle + (c - s), middle + (-c - s)),
    ]
}

/// The fluid mesher's worlds, built by hand: what the two test modules below put in front of it.
#[cfg(test)]
mod fluid_fixtures {
    use super::*;
    use crate::mc::block::BlockstateKey;

    /// A fluid byte in the payload's own packing: which fluid, how much of nine, and whether it is the
    /// **flowing** half of that fluid - bit 6, which is the third field `fluid_of` reads.
    pub fn fluid(kind: u8, amount: u8) -> u8 {
        kind | (amount << 2)
    }

    /// The same for the other half of a fluid: `Fluids.FLOWING_LAVA` rather than `Fluids.LAVA`.
    pub fn flowing(kind: u8, amount: u8) -> u8 {
        fluid(kind, amount) | 0b0100_0000
    }

    /// A source block of either fluid: `FluidState#getAmount` is 8 for a source and for a falling
    /// fluid, and never 9 - see `fluid_height`.
    pub const SOURCE: u8 = 8;

    /// Solid ground, which stops a fluid, and a plant, which does not: the two answers
    /// `blocksMotion` gives and the only thing in [`fluid_flow`] that is not a height.
    pub const SOLID: u16 = 1;
    pub const PLANT: u16 = 2;

    fn state_of(id: u16) -> ChunkBlockState {
        ChunkBlockState::State(BlockstateKey {
            block: id,
            augment: 0,
        })
    }

    /// A world holding exactly the fluids and the blocks given, and air everywhere else.
    pub struct FluidWorld {
        pub fluids: Vec<(IVec3, u8)>,
        pub blocks: Vec<(IVec3, u16)>,
    }

    impl FluidWorld {
        pub fn new(fluids: &[(IVec3, u8)], blocks: &[(IVec3, u16)]) -> Self {
            Self {
                fluids: fluids.to_vec(),
                blocks: blocks.to_vec(),
            }
        }

        /// A source of `kind` at the origin, and whatever else the test gives it.
        pub fn with(kind: u8, fluids: &[(IVec3, u8)], blocks: &[(IVec3, u16)]) -> Self {
            let mut all = vec![(IVec3::ZERO, fluid(kind, SOURCE))];
            all.extend_from_slice(fluids);

            Self::new(&all, blocks)
        }
    }

    impl BlockStateProvider for FluidWorld {
        fn get_state(&self, pos: IVec3) -> ChunkBlockState {
            match self.blocks.iter().find(|(at, _)| *at == pos) {
                Some((_, id)) => state_of(*id),
                None => ChunkBlockState::Air,
            }
        }

        fn get_fluid(&self, pos: IVec3) -> u8 {
            self.fluids
                .iter()
                .find(|(at, _)| *at == pos)
                .map(|(_, byte)| *byte)
                .unwrap_or(0)
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

    pub fn manager() -> BlockManager {
        let mut manager = BlockManager::new();

        for (id, blocks_motion) in [(SOLID, true), (PLANT, false)] {
            manager.face_flags.insert(
                (id as u32) << 16,
                FaceFlags {
                    occlusion: if blocks_motion { 0b0011_1111 } else { 0 },
                    self_hide: 0,
                    shades: blocks_motion,
                    blocks_motion,
                    offset_max_y: 0.0,
                    offset_xz: false,
                    leaves: false,
                },
            );
        }

        manager
    }
}

/// The top face of a flowing fluid: which way it runs, and how the flowing sprite is turned to match.
#[cfg(test)]
mod fluid_flow_tests {
    use super::fluid_fixtures::*;
    use super::*;
    use std::f32::consts::FRAC_1_SQRT_2;

    /// Which way the fluid at the origin runs, in the world the test built.
    ///
    /// The fixture's fluids are all **sources** - the byte's third field says which half of a fluid a block
    /// holds, and a test that wants a flowing one says so - which is also the fluid object the block above
    /// it has to be for `same_above` to lift it.
    fn flow(world: &FluidWorld) -> (f32, f32) {
        fluid_flow(world, &manager(), IVec3::ZERO, 2, false, SOURCE)
    }

    /// A lava source is `8/9` tall in every direction, so there is no slope and nothing to flow down:
    /// a lake's surface is the still sprite, whole.
    ///
    /// This is the picture the flow test exists for as much as the flowing one: a lake drawn with the
    /// flowing sprite would be a pattern of quarter-sprite squares sweeping across still water.
    #[test]
    fn a_level_surface_does_not_flow() {
        let mut around = Vec::new();

        for dir in [
            Direction::North,
            Direction::East,
            Direction::South,
            Direction::West,
        ] {
            around.push((dir.to_vec(), fluid(2, SOURCE)));
        }

        assert_eq!(flow(&FluidWorld::with(2, &around, &[])), (0.0, 0.0));
    }

    /// A source with nothing beside it at all - air, and air below that - has no slope either: the
    /// game's own `getFlow` reads the *difference* between heights, and there is nothing to differ
    /// from.
    #[test]
    fn a_source_with_nothing_beside_it_does_not_flow() {
        assert_eq!(flow(&FluidWorld::with(2, &[], &[])), (0.0, 0.0));
    }

    /// A neighbour holding less of the same fluid is downhill, and the flow points at it: the game
    /// takes `ownHeight - neighbourHeight` and normalizes, so one lower neighbour gives a unit vector
    /// straight at it.
    #[test]
    fn a_fluid_runs_towards_the_neighbour_that_is_lower() {
        let east = flow(&FluidWorld::with(2, &[(IVec3::X, fluid(2, 4))], &[]));

        assert!(
            east.0 > 0.99,
            "east is +x, and the lower block is east: {east:?}"
        );
        assert_eq!(east.1, 0.0, "and it is not going along z at all");

        let north = flow(&FluidWorld::with(2, &[(-IVec3::Z, fluid(2, 4))], &[]));

        assert!(
            north.1 < -0.99,
            "north is -z, which is the direction the game's own `Direction.North` steps in: {north:?}"
        );
        assert_eq!(north.0, 0.0);
    }

    /// A fluid whose neighbour holds none is *not* flowing anywhere by itself - unless that neighbour
    /// is a block the fluid flows past, in which case the fluid is heading for the edge to fall over
    /// it, and what it compares itself against is the fluid under that block.
    ///
    /// This is the whole of the `blocksMotion` step, and the pair of answers below is what says so:
    /// the same world with a solid block in the same place has no flow at all.
    #[test]
    fn a_fluid_runs_towards_an_edge_it_can_pour_over() {
        // Air to the east, and a lava source in the block below that air.
        let falling = flow(&FluidWorld::with(
            2,
            &[(IVec3::X - IVec3::Y, fluid(2, SOURCE))],
            &[],
        ));

        assert!(
            falling.0 > 0.99,
            "the fluid beside it is air and the fluid under that is one block down: it pours east: {falling:?}"
        );

        // The same, with a plant to the east rather than air: a fluid flows past a block with no
        // collision exactly as it flows past air.
        let past_a_plant = flow(&FluidWorld::with(
            2,
            &[(IVec3::X - IVec3::Y, fluid(2, SOURCE))],
            &[(IVec3::X, PLANT)],
        ));

        assert!(
            past_a_plant.0 > 0.99,
            "a plant has no collision, so the fluid goes past it: {past_a_plant:?}"
        );

        // And with a solid block there instead: the fluid looks nowhere, so it goes nowhere.
        let into_a_wall = flow(&FluidWorld::with(
            2,
            &[(IVec3::X - IVec3::Y, fluid(2, SOURCE))],
            &[(IVec3::X, SOLID)],
        ));

        assert_eq!(
            into_a_wall,
            (0.0, 0.0),
            "a solid neighbour is a wall, and the fluid under it is not reachable"
        );
    }

    /// A fluid beside a *different* fluid does not flow towards it: lava next to water is two liquids
    /// that do not join, which is the game's own `affectsFlow`.
    #[test]
    fn a_fluid_ignores_the_other_fluid() {
        assert_eq!(
            flow(&FluidWorld::with(2, &[(IVec3::X, fluid(1, 4))], &[])),
            (0.0, 0.0),
            "water beside lava is not downhill, it is a different liquid"
        );
    }

    /// The two are normalized, so a fluid with two lower neighbours runs along the diagonal between
    /// them rather than towards whichever the loop reached last.
    #[test]
    fn two_lower_neighbours_give_the_diagonal_between_them() {
        let (x, z) = flow(&FluidWorld::with(
            2,
            &[(IVec3::X, fluid(2, 4)), (-IVec3::Z, fluid(2, 4))],
            &[],
        ));

        assert!(
            (x - FRAC_1_SQRT_2).abs() < 0.001 && (z + FRAC_1_SQRT_2).abs() < 0.001,
            "east and north are both downhill, so the flow is the normalized sum: {x}, {z}"
        );
    }

    /// The four offsets are the corners of a square half the sprite across, turned by the flow's
    /// angle: that *is* "a quarter of the flowing sprite", and the square's centre stays the sprite's
    /// middle however it is turned.
    ///
    /// The side is 0.5 whatever the angle, which is the part that is easy to get wrong: the game's
    /// `sin * 0.25` and `cos * 0.25` are *half* of the quarter's side in each axis, so the four
    /// offsets walk `0.25` either side of the middle and the quarter is `0.5` across.
    #[test]
    fn the_flowing_top_face_is_a_quarter_of_the_sprite_turned_by_the_angle() {
        let angles: Vec<f32> = (0..16).map(|step| step as f32 * 0.4).collect();

        for angle in angles {
            let offsets = flowing_top_offsets(angle.cos(), angle.sin());

            for (index, (u, v)) in offsets.iter().enumerate() {
                let next = offsets[(index + 1) % 4];

                let side = ((next.0 - u).powi(2) + (next.1 - v).powi(2)).sqrt();

                assert!(
                    (side - 0.5).abs() < 1e-5,
                    "a quarter of the sprite is half of it across, at every angle: {angle} gave {side}"
                );

                assert!(
                    *u > 0.14 && *u < 0.86 && *v > 0.14 && *v < 0.86,
                    "and it stays inside the sprite: {u}, {v}"
                );
            }

            let middle = (
                offsets.iter().map(|(u, _)| u).sum::<f32>() / 4.0,
                offsets.iter().map(|(_, v)| v).sum::<f32>() / 4.0,
            );

            assert!(
                (middle.0 - 0.5).abs() < 1e-5 && (middle.1 - 0.5).abs() < 1e-5,
                "the quarter turns about the sprite's middle: {middle:?}"
            );
        }
    }

    /// The two axes the game's own arithmetic has to get right, spelled out: a flow along +x comes
    /// out as the sprite's middle half unrotated, and a flow along +z as the same square.
    ///
    /// That is not a tautology - it is what says the offsets are indexed the way the corners are,
    /// north-west first - and the diagonal case below is the one that shows the turn itself.
    #[test]
    fn the_axes_and_the_diagonal() {
        let x = flowing_top_offsets(1.0, 0.0);
        let z = flowing_top_offsets(0.0, 1.0);

        for (u, v) in x.iter().chain(z.iter()) {
            assert!(
                ((u - 0.25).abs() < 1e-5 || (u - 0.75).abs() < 1e-5)
                    && ((v - 0.25).abs() < 1e-5 || (v - 0.75).abs() < 1e-5),
                "the middle half of the sprite, corner for corner: {u}, {v}"
            );
        }

        let diagonal = flowing_top_offsets(FRAC_1_SQRT_2, FRAC_1_SQRT_2);

        assert!(
            (diagonal[0].0 - 0.5).abs() < 0.001 && (diagonal[0].1 - 0.1464).abs() < 0.001,
            "a 45 degree flow turns the quarter onto its point, north-west corner first: {:?}",
            diagonal[0]
        );

        assert!(
            (diagonal[2].0 - 0.5).abs() < 0.001 && (diagonal[2].1 - 0.8536).abs() < 0.001,
            "and the opposite corner is opposite it: {:?}",
            diagonal[2]
        );
    }
}

/// The fluid mesher's geometry: the quads it emits for a world built by hand. See
/// [`bake_fluid_faces_with`], which exists so that this can be a test rather than a screenshot.
#[cfg(test)]
mod fluid_geometry_tests {
    use super::fluid_fixtures::*;
    use super::*;

    /// The fluid sprites, at rectangles that do not matter here: this is about where the vertices are.
    ///
    /// Both fluids are present and they are in **different layers** - water blends and lava does not,
    /// which is the one thing about a fluid this side of the sprite lookup decides - because which layer
    /// a fluid lands in is what says which pass draws it. See `the_water_lands_in_the_transparent_layer`.
    fn sprites() -> [Option<(RenderLayer, FluidSprites)>; 2] {
        let sprite = FluidSprite {
            atlas: ((0, 0), (16, 16)),
            game: None,
            animated: false,
            level_cap: None,
        };

        let both = || FluidSprites {
            still: sprite,
            flow: sprite,
        };

        [
            Some((RenderLayer::Transparent, both())),
            Some((RenderLayer::Solid, both())),
        ]
    }

    /// One vertex of a baked layer, as the position inside the section, in blocks.
    ///
    /// The format holds **sixteen bits an axis at 1/2048 of a block** - `Vertex::axis_to_2048ths`, and the
    /// shader's `f32(v1 & 0xffffu) * 0.00048828125` on the other side of it. A flag bit an axis used to say
    /// "sixteen blocks", because a byte could not; see `Vertex::compressed` for what that byte became.
    fn position(vertex: &[u8]) -> [f32; 3] {
        let half = |low: usize, high: usize| {
            u16::from_le_bytes([vertex[low], vertex[high]]) as f32 / 2048.0
        };

        [half(0, 1), half(2, 3), half(4, 5)]
    }

    /// Every quad of a layer, as its four corners in blocks.
    ///
    /// `chunks_exact` rather than the `as_chunks` clippy suggests, for the same reason the `quads` walk
    /// above gives: `as_chunks` is a nightly-only inherent method on slices, and the two agree on the
    /// length this hands them.
    #[allow(clippy::chunks_exact_to_as_chunks)]
    fn quads(layer: &BakedLayer) -> Vec<[[f32; 3]; 4]> {
        layer
            .vertices
            .chunks_exact(Vertex::VERTEX_LENGTH)
            .map(position)
            .collect::<Vec<_>>()
            .chunks_exact(4)
            .map(|quad| [quad[0], quad[1], quad[2], quad[3]])
            .collect()
    }

    /// The four corners of a quad with its lowest two and its highest two separated, which is what a
    /// side face is: a bottom edge and a top edge.
    fn edges(quad: &[[f32; 3]; 4]) -> ([f32; 2], [f32; 2]) {
        let mut heights = quad.map(|corner| corner[1]);
        heights.sort_by(f32::total_cmp);

        (
            heights[..2].try_into().unwrap(),
            heights[2..].try_into().unwrap(),
        )
    }

    /// Bakes the fluids of one section-sized world and answers the quads of one layer.
    ///
    /// The layer is named rather than assumed because the two fluids go in different ones - see
    /// `the_water_lands_in_the_transparent_layer` - and most of what is below is about lava's shape.
    fn bake_layer(world: &FluidWorld, layer: RenderLayer) -> Vec<[[f32; 3]; 4]> {
        let mut layers = vec![BakedLayer::default(); 3];

        // The section at the origin: these fixtures build one section's worth of blocks and ask what
        // came out, so the world position a fluid asks its tint for is the section-relative one.
        bake_fluid_faces_with(&manager(), world, &sprites(), &mut layers, IVec3::ZERO);

        quads(&layers[layer as usize])
    }

    /// The same, for the lava layer, which is what every shape test below is about.
    fn bake(world: &FluidWorld) -> Vec<[[f32; 3]; 4]> {
        bake_layer(world, RenderLayer::Solid)
    }

    /// **Where the water goes, which is what makes it drawable at all.**
    ///
    /// Water and lava are the same mesher and the same shapes; the whole of what separates them is the
    /// layer they are baked into, and that is decided by [`fluid_sprites`] from the fluid's kind. Lava
    /// is opaque and goes in the solid layer, which is one of the two the graph's *opaque* pass draws.
    /// Water blends, so it goes in the transparent layer - and a transparent layer that nothing draws is
    /// water that is baked and invisible, which is exactly the state this was in for as long as water was
    /// skipped here.
    ///
    /// So this asserts both halves: the water is in the transparent layer, and it is **not** in the solid
    /// one. The second half is the one that catches a mesher that ignored the layer it was handed: a
    /// water block drawn in the opaque pass is water with no blending, which is an ocean you cannot see
    /// the bottom of rather than an error.
    #[test]
    fn the_water_lands_in_the_transparent_layer() {
        // A source of water, with solid ground under it so that it has a floor and its sides are walls.
        let world = FluidWorld::with(
            1,
            &[],
            &[
                (IVec3::new(0, -1, 0), SOLID),
                (IVec3::new(1, 0, 0), SOLID),
                (IVec3::new(-1, 0, 0), SOLID),
                (IVec3::new(0, 0, 1), SOLID),
                (IVec3::new(0, 0, -1), SOLID),
            ],
        );

        let transparent = bake_layer(&world, RenderLayer::Transparent);
        let solid = bake_layer(&world, RenderLayer::Solid);

        assert!(
            !transparent.is_empty(),
            "a water source with a floor and four walls has five faces to draw and none were baked \
             into the layer water belongs in"
        );

        assert!(
            solid.is_empty(),
            "and none of them belong in the solid layer, which cannot blend: {} quad(s) of water were \
             baked there",
            solid.len()
        );

        // The same world with lava in it is the other way round, which is what says the split is the
        // fluid's and not the world's.
        let lava = FluidWorld::with(
            2,
            &[],
            &[
                (IVec3::new(0, -1, 0), SOLID),
                (IVec3::new(1, 0, 0), SOLID),
                (IVec3::new(-1, 0, 0), SOLID),
                (IVec3::new(0, 0, 1), SOLID),
                (IVec3::new(0, 0, -1), SOLID),
            ],
        );

        assert!(
            bake_layer(&lava, RenderLayer::Solid).len() == transparent.len(),
            "the two fluids have the same shape, so with the same neighbours they bake the same number \
             of faces - in their own layers"
        );
        assert!(bake_layer(&lava, RenderLayer::Transparent).is_empty());
    }

    /// **A flat pool has no faces between its own blocks, and that is what says a camera inside it sees
    /// through to whatever is beyond.**
    ///
    /// Vanilla draws a side face between two blocks of one fluid only where one of them stands above the
    /// other - the *riser* of a step. Two blocks at one level leave no band, and the face is not drawn, so
    /// from inside a pool the water beyond it is invisible: there is nothing there to look through. This
    /// measures that on a five-by-five pool one block deep with a wall around it: five faces under it,
    /// eight wall-facing sides, and **nothing else** - no face between any two of the pool's own blocks.
    ///
    /// A pool that comes out with faces between its own blocks is a pool a player inside can see out of,
    /// which is what "the waterfall shows through the water surface" is.
    #[test]
    fn a_flat_pool_has_no_faces_between_its_own_blocks() {
        let mut fluids = Vec::new();
        let mut blocks = Vec::new();

        // A five-by-five pool of source water, **three blocks deep**, with a solid floor under it. Depth is
        // the part that matters: a block with water above it is a whole block tall rather than 8/9, so a
        // one-deep pool is a different set of heights from a real one.
        //
        // And **one column is one block shorter** - a step, which is what a waterfall's foot is. That is the
        // case the riser logic exists for, and the case a flat pool says nothing about.
        for x in -2i32..=2 {
            for z in -2i32..=2 {
                let top = if x == 2 && z == 2 { 2 } else { 3 };

                for y in 0i32..top {
                    fluids.push((IVec3::new(x, y, z), fluid(1, SOURCE)));
                }

                blocks.push((IVec3::new(x, -1, z), SOLID));
            }
        }

        let world = FluidWorld::new(&fluids, &blocks);
        let quads = bake_layer(&world, RenderLayer::Transparent);

        // `sampled_height` is 8/9 for a source, so the surface sits a sixteenth below the block top.
        let surface = 8.0 / 9.0;
        let leak = |a: f32, b: f32| (a - b).abs() < 1.0e-4;

        // **No quad may lie in a plane between two of the pool's own blocks.** Those are the four inner
        // planes of x and of z, and the count of faces on them is the count of ways a camera inside the
        // pool can see out of it. A rim face is a different thing entirely and is not asked about.
        let mut interior = Vec::new();

        for quad in &quads {
            let planes = [
                (0, IVec3::new(-1, 0, 0), IVec3::new(0, 0, 0)),
                (1, IVec3::new(0, -1, 0), IVec3::new(0, 0, 0)),
                (2, IVec3::new(0, 0, -1), IVec3::new(0, 0, 0)),
            ];

            for (axis, _a, _b) in planes {
                for at in -1i32..=0 {
                    let on = quad
                        .iter()
                        .all(|corner| (corner[axis] - at as f32).abs() < 1.0e-4);

                    // A *side* face on that plane, and not the surface or the floor: its two heights are
                    // not all the same.
                    let is_side = quad.iter().any(|corner| !leak(corner[1], quad[0][1]));

                    if on && is_side {
                        interior.push((axis, at, quad[0]));
                    }
                }
            }

            let _ = surface;
        }

        assert!(
            interior.is_empty(),
            "a flat pool drew {} face(s) between its own blocks, which a camera inside it sees straight \
             through - the first is {:#?}",
            interior.len(),
            interior.first()
        );
    }

    /// **The surface of a lake is visible from inside it, and that is a second face.**
    ///
    /// A fluid's top face is baked pointing *up*, and this renderer culls back faces - so a player under
    /// water looking at the surface is looking at the back of it and sees nothing at all. Vanilla draws
    /// that surface twice (`FluidRenderer`'s `addBackFace`, fed by
    /// `FluidState#shouldRenderBackwardUpFace`), and the condition is a three-by-three one block up: if
    /// any of those nine is neither this fluid nor a solid block, the surface gets a copy wound the other
    /// way. This is the report it is here for:
    ///
    /// > 在水里向上看看不见水面的纹理
    ///
    /// Both halves are asserted, because either one alone is a picture that is wrong in a different way:
    /// a back face that is always drawn is a doubled surface (which is invisible, since a quad and its
    /// mirror occupy the same pixels, but it is two draws and two blends for every lake in the world),
    /// and one that is never drawn is the report above.
    #[test]
    fn a_fluid_surface_is_drawn_from_below_when_it_can_be_seen_from_below() {
        // A single source with air around it: the block above and all eight of its neighbours are air,
        // which is neither this fluid nor solid, so the surface is drawn both ways. One bottom face, four
        // sides, a top face and its mirror - seven quads rather than six.
        let open = FluidWorld::new(&[(IVec3::ZERO, fluid(1, SOURCE))], &[]);

        let quads = bake_layer(&open, RenderLayer::Transparent).len();

        assert_eq!(
            quads, 7,
            "a fluid surface in the open is drawn from above and from below - the block above it is air, \
             so a player inside the fluid can see it; {quads} quad(s) were baked"
        );

        // The same fluid with all nine of the cells above it made solid: there is no view from above to
        // be had - a player under it is under a roof - so the second copy is not baked. One bottom, four
        // sides, one top.
        let mut blocks = vec![(IVec3::new(0, -1, 0), SOLID)];
        for dx in -1..=1 {
            for dz in -1..=1 {
                blocks.push((IVec3::new(dx, 1, dz), SOLID));
            }
        }

        let roofed = FluidWorld::new(&[(IVec3::ZERO, fluid(1, SOURCE))], &blocks);
        let quads = bake_layer(&roofed, RenderLayer::Transparent).len();

        assert_eq!(
            quads, 6,
            "a fluid under a solid roof still has its four sides and its surface - and the surface there \
             is the one face, because the nine cells above it are all solid; {quads} quad(s) were baked"
        );
    }

    /// The wall of one block in one direction, as the quad that was baked for it - the plane it lies
    /// in, the two spans it covers and the block's own height band.
    fn wall_of(
        quads: &[[[f32; 3]; 4]],
        pos: IVec3,
        axis: usize,
        high: bool,
    ) -> Option<[[f32; 3]; 4]> {
        let (x, y, z) = (pos.x as f32, pos.y as f32, pos.z as f32);

        // The plane is the block's low or high boundary on that axis, and the other horizontal axis is
        // the span the face covers.
        let (plane, span) = if axis == 0 {
            (x + if high { 1.0 } else { 0.0 }, [z, z + 1.0])
        } else {
            (z + if high { 1.0 } else { 0.0 }, [x, x + 1.0])
        };

        quads.iter().copied().find(|quad| {
            let on_plane = quad
                .iter()
                .all(|corner| if axis == 0 { corner[0] } else { corner[2] } == plane);
            let in_span = quad.iter().all(|corner| {
                let other = if axis == 0 { corner[2] } else { corner[0] };
                other == span[0] || other == span[1]
            });
            let in_band = quad
                .iter()
                .all(|corner| corner[1] == y || corner[1] == y + 1.0);

            on_plane && in_span && in_band
        })
    }

    /// **The bug this test is for.** A lava fall came out with a gap between every pair of its blocks.
    ///
    /// Every block of a falling column stands a whole block tall - the same fluid is directly above it -
    /// and for a block like that the game does **no corner averaging at all**:
    ///
    /// ```java
    /// // FluidRenderer#tesselate
    /// float heightSelf = this.getHeight(level, type, pos, blockState, fluidState);
    /// if (heightSelf >= 1.0F) {
    ///     heightNorthEast = 1.0F;
    ///     heightNorthWest = 1.0F;
    ///     heightSouthEast = 1.0F;
    ///     heightSouthWest = 1.0F;
    /// } else {
    ///     ... the four calculateAverageHeight calls ...
    /// }
    /// ```
    ///
    /// This mesher averaged them anyway, so a full-height block's corner was pulled down by whatever was
    /// diagonally beside it - the thin spreading lava at the foot of the fall, the step it poured over -
    /// and its side faces stopped below its own top. The block above starts at the block boundary, so the
    /// difference between the two is an open slit in the wall, repeated at every block of the column:
    /// "in a lava fall the stepped flowing lava has gaps between it".
    ///
    /// The slit is a *triangle*, not a full-height hole - one corner of the face reaches the block top and
    /// the other does not - which is why the assertion is on **both** top corners of every wall rather
    /// than on the wall's extent.
    #[test]
    fn every_wall_of_a_full_block_of_fluid_reaches_its_own_top() {
        // A column of lava at (0, 2, 0) down to (0, 0, 0) - a fall - with a thin spreading flow
        // diagonally beside its middle block: `amount` 3 is `3/9` of a block and it has no fluid above
        // it, so it is exactly the neighbour that used to drag the middle block's corner down.
        let world = FluidWorld::with(
            2,
            &[
                (ivec3(0, 1, 0), fluid(2, 8)),
                (ivec3(0, 2, 0), fluid(2, 8)),
                (ivec3(1, 1, 0), fluid(2, 3)),
            ],
            &[],
        );

        let quads = bake(&world);

        // Both of the column's lower blocks are a whole block tall, and every wall they draw has to run
        // from the bottom of its own block to the top of it. The middle block has no east wall: the thin
        // flow is right there, and a face between two blocks of one fluid is not drawn.
        for (pos, walls) in [
            (
                ivec3(0, 0, 0),
                vec![(0, false), (0, true), (2, false), (2, true)],
            ),
            (ivec3(0, 1, 0), vec![(0, false), (2, false), (2, true)]),
        ] {
            for (axis, high) in walls {
                let quad = wall_of(&quads, pos, axis, high).unwrap_or_else(|| {
                    panic!("no wall for {pos:?} on axis {axis}, high {high}: {quads:?}")
                });

                let (bottom, top) = edges(&quad);

                assert_eq!(
                    bottom, [pos.y as f32; 2],
                    "the wall of {pos:?} has to start at the bottom of its own block"
                );
                assert_eq!(
                    top,
                    [pos.y as f32 + 1.0; 2],
                    "and reach the top of it: a whole block of fluid is a whole block tall, whatever is \
                     diagonally beside it. A corner short of this is a slit between this block and the \
                     one above it, which is the gap that was reported in a lava fall."
                );
            }
        }
    }

    /// A block that is *not* a whole block tall is the other half of the same rule: its corners are
    /// averaged, which is what makes a lake's surface slope and a stream's surface follow its flow.
    ///
    /// The two are the same code path in the game - `heightSelf >= 1.0F` is the branch - so a test for
    /// the one is only worth anything next to a test for the other.
    #[test]
    fn a_thin_layer_of_fluid_still_averages_its_corners() {
        // A source with a thinner neighbour: `4/9` of a block beside `8/9` of one, both with air above.
        let world = FluidWorld::with(2, &[(ivec3(1, 0, 0), fluid(2, 4))], &[]);

        let quads = bake(&world);

        // The shared corner of a `8/9` block beside a `4/9` one is the plain mean of the two, which is
        // this side's average rather than the game's weighted one - see the fluid section of the README.
        // It shows on the source's **north** wall: the east wall is not drawn at all, because the block
        // beside it holds the same fluid.
        let expected = (8.0 / 9.0 + 4.0 / 9.0) / 2.0;

        // `wall_of` bands a face to its own block, which only holds for a face that is a whole block
        // tall: this one's top edge is the two averaged heights, so it is found by its floor instead.
        let wall = quads
            .iter()
            .find(|quad| {
                quad.iter().all(|corner| corner[2] == 0.0)
                    && quad
                        .iter()
                        .all(|corner| corner[0] == 0.0 || corner[0] == 1.0)
                    && quad.iter().filter(|corner| corner[1] == 0.0).count() == 2
            })
            .expect("the source's north wall");

        let tops: Vec<f32> = wall
            .iter()
            .map(|corner| corner[1])
            .filter(|y| *y > 0.5)
            .collect();

        assert!(
            tops.iter().any(|y| (*y - expected).abs() < 0.07),
            "the shared corner is the average of the two heights, {expected}, and neither the block's \
             own height nor a whole block: {tops:?}"
        );

        assert!(
            tops.iter().any(|y| *y > expected + 0.05),
            "and the far corner is the taller of the two - a face that came out flat would mean the \
             average had been skipped: {tops:?}"
        );
    }

    /// **The other half of the same report.** A purely vertical fall came out complete and a *stepped*
    /// one still had cracks, which is the difference between the two rules this mesher applies to a face
    /// between two blocks of one fluid.
    ///
    /// Where a fall lands, the falling column's last block is a whole block tall - the same fluid is
    /// above it - and the lava it lands on is not: that block has air above it, so its height is its
    /// `amount/9` and its corners are averaged. The two surfaces therefore do **not** meet at the edge
    /// between them: the landing's surface is below the column's, and the band between the two is the
    /// riser of the step. The face on that band is the one this mesher used to skip for holding the same
    /// fluid, and skipping it is what left a crack along the foot of every drop.
    #[test]
    fn the_riser_between_a_full_block_and_a_lower_one_of_the_same_fluid_is_drawn() {
        // A fall landing on a step: a falling column at (1, 3, 0) and (1, 2, 0) - the lower of which has
        // the same fluid above it, so it is a whole block tall - and the lava it lands in at (2, 2, 0),
        // which is thinner and has air above it. The terrain that holds the step up is solid, so the
        // only faces at the edge between them are the fluid's own. (Everything is at a positive `y`:
        // the mesher walks the section's own 0..15, which is where a section's blocks live.)
        let world = FluidWorld::new(
            &[
                (ivec3(1, 3, 0), fluid(2, 8)),
                (ivec3(1, 2, 0), fluid(2, 8)),
                (ivec3(2, 2, 0), fluid(2, 6)),
            ],
            &[(ivec3(1, 1, 0), SOLID), (ivec3(2, 1, 0), SOLID)],
        );

        let quads = bake(&world);

        // The face at the step: the plane the two blocks share, x = 2, and it has to reach the **top of
        // the column's block** - a face that stopped short of it is the crack. It is a band rather than
        // the whole block: its floor is the landing's own surface where they meet, which the game's
        // weighted average has already lifted towards the column, and everything below that is inside the
        // landing's fluid and is not drawn at all.
        let step = quads
            .iter()
            .find(|quad| quad.iter().all(|corner| corner[0] == 2.0))
            .filter(|quad| {
                quad.iter()
                    .all(|corner| corner[2] == 0.0 || corner[2] == 1.0)
            })
            .filter(|quad| quad.iter().any(|corner| corner[1] == 3.0))
            .unwrap_or_else(|| {
                panic!(
                    "no face reaching the top of the column's block at the step, so the band above the \
                     landing's surface is open - which is the crack. The quads were {quads:?}"
                )
            });

        let (bottom, top) = edges(step);

        assert_eq!(top, [3.0, 3.0], "the face reaches the column's surface");
        assert!(
            bottom[0] < 3.0 && bottom[1] < 3.0,
            "and it is a band: it starts at the landing's surface, which is below the column's - a face \
             spanning the whole block would be geometry inside the landing's fluid, which is what a \
             camera inside the lava sees. It starts at {bottom:?}"
        );
        assert!(
            bottom[0] > 2.0 && bottom[1] > 2.0,
            "the floor of the band is the landing's surface, above the floor of its block: {bottom:?}"
        );
    }

    /// The face is *not* drawn where there is nothing to see, which is what keeps a lake from paying
    /// four extra faces for every block it holds: two blocks of one fluid at one level share the corner
    /// heights at the edge between them, so the face lies inside the fluid.
    #[test]
    fn a_face_inside_a_lake_is_not_drawn() {
        // Two sources side by side, both with air above and nothing beside them: `8/9` each.
        let world = FluidWorld::with(2, &[(ivec3(1, 0, 0), fluid(2, 8))], &[]);

        let quads = bake(&world);

        let shared = quads.iter().find(|quad| {
            quad.iter().all(|corner| corner[0] == 1.0)
                && quad
                    .iter()
                    .all(|corner| corner[2] == 0.0 || corner[2] == 1.0)
        });

        assert!(
            shared.is_none(),
            "the two blocks hold one fluid at one height, so the face between them is inside it:             {shared:?}"
        );
    }
}

/// The fluid *object*, which is the question the game's `isSame` asks and the one this mesher used to get
/// wrong - see [`same_fluid`].
#[cfg(test)]
mod fluid_identity_tests {
    use super::fluid_fixtures::*;
    use super::*;

    /// A fluid is two registered objects, a source and a flowing one, and `isSame` is identity: neither
    /// half is the other, and both are themselves.
    #[test]
    fn the_two_halves_of_a_fluid_are_two_fluids() {
        assert!(same_fluid(2, false, 2, false), "a source is a source");
        assert!(same_fluid(2, true, 2, true), "and the flowing half itself");
        assert!(!same_fluid(2, false, 2, true), "but not each other");
        assert!(!same_fluid(2, true, 2, false));
        assert!(!same_fluid(1, true, 2, true), "and water is not lava");
        assert!(
            !same_fluid(0, false, 0, false),
            "and no fluid is not a fluid"
        );
    }

    /// A flow does not run towards the **other half** of its own fluid, because the game does not consider
    /// it the same liquid. That is the difference the report was about - the flow vector decides which way
    /// the rotated quarter of the flowing sprite points, so a neighbour that should not count turns the
    /// pattern.
    ///
    /// The two worlds below are the same shape: a flowing block with a source to its east. The source is
    /// *lower* than the block, so if the halves were one fluid there would be a slope to flow down and the
    /// answer would be a unit vector east; the game's answer is nothing at all, and the test asserts the
    /// difference rather than a zero that two different reasons could produce.
    #[test]
    fn a_flow_ignores_the_other_half_of_its_own_fluid() {
        let across_the_halves = FluidWorld::new(
            &[(IVec3::ZERO, flowing(2, 8)), (ivec3(1, 0, 0), fluid(2, 3))],
            &[],
        );

        assert_eq!(
            fluid_flow(&across_the_halves, &manager(), IVec3::ZERO, 2, true, 8),
            (0.0, 0.0),
            "a source is not the fluid this block is, so it is not downhill whatever its height"
        );

        // The same two blocks with the *same* half on both sides do flow, which is what says the test
        // above is about the fluid object and not about a reading that ignores neighbours.
        let both_flowing = FluidWorld::new(
            &[
                (IVec3::ZERO, flowing(2, 8)),
                (ivec3(1, 0, 0), flowing(2, 3)),
            ],
            &[],
        );

        let (x, z) = fluid_flow(&both_flowing, &manager(), IVec3::ZERO, 2, true, 8);

        assert!(
            x > 0.99 && z == 0.0,
            "the lower neighbour is east and the flow runs downhill towards it: {x}, {z}"
        );
    }

    /// The game's weighted corner average: a height of `0.8` or more counts **ten** times, a zero counts
    /// once, and a **solid** block is dropped instead of counted.
    ///
    /// The two cases below are chosen so that both weights show up in one number each: a lone `8/9` block
    /// in air has two zeroes beside it, and the same block against stone has the stone dropped - so the
    /// corner comes out at `8/9 * 10/12` and `8/9 * 10/11`.
    ///
    /// This used to be a plain mean of the corner blocks that held the same fluid, which is a different
    /// weighting *and* a different set: air was left out of it entirely, so a surface was level where the
    /// game's tapers at an open edge.
    #[test]
    fn a_corner_is_the_weighted_average_of_its_samples() {
        let manager = manager();

        let in_the_open = FluidWorld::with(2, &[], &[]);
        let corner = fluid_corner_height(
            &in_the_open,
            &manager,
            IVec3::ZERO,
            2,
            false,
            8.0 / 9.0,
            IVec2::new(0, 0),
        );

        let two_zeroes = (8.0 / 9.0) * 10.0 / 12.0;

        assert!(
            (corner - two_zeroes).abs() < 1e-5,
            "a lone block is pulled down by the two air neighbours it has: {corner}, wanted {two_zeroes}"
        );
        assert!(
            (corner - 8.0 / 9.0).abs() > 0.1,
            "and it is not its own height either: {corner}"
        );

        let against_a_wall = FluidWorld::with(2, &[], &[(ivec3(-1, 0, 0), SOLID)]);
        let corner = fluid_corner_height(
            &against_a_wall,
            &manager,
            IVec3::ZERO,
            2,
            false,
            8.0 / 9.0,
            IVec2::new(0, 0),
        );

        let stone_dropped = (8.0 / 9.0) * 10.0 / 11.0;

        assert!(
            (corner - stone_dropped).abs() < 1e-5,
            "a solid neighbour is dropped from the average rather than counted as a zero - which is what \
             keeps a fluid's surface at its own height against a wall: got {corner}, wanted \
             {stone_dropped}"
        );
        assert!(
            corner > two_zeroes,
            "so a wall holds the surface up where open air lets it sag: {corner} against {two_zeroes}"
        );
    }
}

/// The sprites of one fluid and the layer it is drawn in: water is translucent and lava is not.
struct FluidSprites {
    still: FluidSprite,
    flow: FluidSprite,
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
        let rect = uv_map.get(&path).copied();

        // Reported once per sprite rather than once per bake: a pack without the fluid textures
        // would otherwise write one line per section while the world loads.
        static WARNED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if rect.is_none() && WARNED.fetch_or(bit, std::sync::atomic::Ordering::Relaxed) & bit == 0 {
            log::warn!("wgpu-mc: {path} is not in the block atlas, so {name} is not drawn");
        }

        // The game's rectangle for the same sprite, when there is one to be had: this is what makes a
        // fluid animate, and it is the same decision a block model's face makes - the animated-texture
        // switch, the handed-over atlas, and whether the game animates this sprite. Read here, while
        // the name is in hand, because the face baker is handed the sprite and not its name.
        //
        // **The animation flag comes along, and dropping it here was a longstanding mistake.**
        //
        // A fluid sprite is normally animated - `lava_flow.png.mcmeta` and `water_flow.png.mcmeta` are what
        // make it a strip - but "normally" is not "always", and far more to the point the shader's whole
        // level-of-detail handling is gated on the flag this used to throw away. See `FluidSprite::flags`
        // for what that cost.
        rect.map(|atlas_rect| {
            let game = atlas.game_atlas_rect(&path);

            FluidSprite {
                atlas: atlas_rect,
                game: game.map(|sprite| sprite.rect),
                animated: game.is_some_and(|sprite| sprite.animated),
                // **Taken here, and its absence is why the shimmer returned.** See the field.**
                level_cap: game.and_then(|sprite| sprite.level_cap),
            }
        })
    };

    let still = sprite("still", 1 << kind)?;
    // The flowing sprite is what a fluid's sides - and a moving surface - are drawn with, and it is a
    // resource the game has: `FluidStateModelSet` names `block/lava_still` *and* `block/lava_flow` for
    // lava, so both are stitched into the block atlas and both are animated (`lava_flow.png.mcmeta` is
    // `{"animation":{"frametime":3}}`). It is still looked up as an `Option` because a resource pack is
    // free not to ship one, and a fluid drawn with its still sprite on the sides is a whole fluid rather
    // than none at all. The *still* sprite is the one that has to be there: without it there is no
    // texture to sample and nothing is baked.
    let flow = sprite("flow", 1 << (kind + 2)).unwrap_or(FluidSprite {
        atlas: still.atlas,
        game: still.game,
        animated: still.animated,
        level_cap: still.level_cap,
    });

    Some((layer, FluidSprites { still, flow }))
}

/// Bakes the fluids of a section: the lava and the water in it, shaped the way Minecraft's own
/// `FluidRenderer` shapes them (renamed from `LiquidBlockRenderer` in 26.1).
///
/// Nothing else does it. A fluid is not a block model - no elements to bake, no variant to look up -
/// so the pass above walks straight past it, and the Rust terrain came out with the lava and the
/// water simply missing, which in a superflat world is the lava lakes and the water.
///
/// The two go into different layers, which is the whole of what separates them here: lava is opaque and
/// goes in the solid layer, water blends and goes in the transparent one. See [`fluid_sprites`], which
/// is where that is decided, and `@geo_terrain_translucent` on the render graph's side, which is what
/// draws the transparent layer.
///
/// The shape: a top face at the height the fluid settled at and only where the block above holds a
/// different fluid, side faces only towards blocks that do not hold the same fluid, clipped to the
/// corners' heights so a sloping surface comes out sloping, a bottom face where the fluid does not
/// continue downwards, and no face at all between two blocks of the same fluid.
///
/// The block manager is here for two things: the top face's direction, and whether the nine cells above
/// a fluid are solid enough to hide its surface. The game turns a flowing surface to face along its flow,
/// which is [`fluid_flow`], and that function's one non-height step is the `blocksMotion` of a neighbour
/// state - see [`FaceFlags::blocks_motion`].
fn bake_fluid_faces<Provider: BlockStateProvider>(
    block_manager: &BlockManager,
    state_provider: &Provider,
    atlas: &Atlas,
    layers: &mut [BakedLayer],
    section_offset: IVec3,
) {
    // Indexed by kind - 1, so the geometry below does not have to know a `FluidSprites` from an atlas;
    // see [`bake_fluid_faces_with`], which is what a test calls.
    let sprites = [
        fluid_sprites(atlas, 1, "water"),
        fluid_sprites(atlas, 2, "lava"),
    ];

    bake_fluid_faces_with(
        block_manager,
        state_provider,
        &sprites,
        layers,
        section_offset,
    );
}

/// The colour water is tinted when nothing knows which biome it is in.
///
/// `BiomeColors.getAverageWaterColor` for the default biome, which is what the game uses where no biome
/// says otherwise. It is the fallback of [`BlockStateProvider::get_fluid_color`] rather than the answer,
/// so the renderer still draws water on a path that has no biome data - which is every test, and was the
/// whole renderer before the tint was threaded through.
///
/// The alpha byte is not part of the tint. The shader builds the vertex colour as
/// `vec4(red, green, blue, 1.0)` and the alpha that reaches the blend is the *texture's*, so this byte
/// goes nowhere - it is `00` only because it is unused rather than because it means anything. **The two
/// halves are not interchangeable and were briefly confused**: `0x00e4763f` is a colour, `0x00ffffff` is
/// white, and white water is what this looks like when the colour half is mistaken for the alpha half.
pub const WATER_TINT: u32 = 0x00e4_763f;

/// Whether a fluid's **top face** is also drawn facing the other way - the one thing that makes the
/// surface of a lake visible from inside it.
///
/// This is the game's `FluidState#shouldRenderBackwardUpFace`, and the shape of it is what says what it
/// is for:
///
/// ```java
/// for (int ox = -1; ox <= 1; ox++) {
///     for (int oz = -1; oz <= 1; oz++) {
///         BlockPos offset = above.offset(ox, 0, oz);
///         FluidState fluidState = level.getFluidState(offset);
///         if (!fluidState.getType().isSame(this.getType()) && !level.getBlockState(offset).isSolidRender()) {
///             return true;
///         }
///     }
/// }
/// ```
///
/// Three by three, one block **up**: if any of those nine is neither this fluid nor a solid block, the
/// surface gets a second copy of itself wound the other way. The case that answer is for is a player
/// under water looking up - the top face as baked points *up*, this renderer culls back faces, so from
/// below it is the back of a surface and nothing is drawn at all. A lake you cannot see the surface of
/// from inside is the report this was written for.
///
/// The condition is what keeps it from being unconditional: a fluid with a solid roof over it - one that
/// is entirely enclosed - has no view from above to spoil, and the check skips the work there.
fn fluid_wants_back_face<Provider: BlockStateProvider>(
    state_provider: &Provider,
    block_manager: &BlockManager,
    pos: IVec3,
    kind: u8,
    flowing: bool,
) -> bool {
    let mut wants = false;

    // `pos.above()` and then the nine around it, so the block that decided the top face has one at all
    // (`!same_above`) is one of the nine asked about here.
    for (dx, dz) in [
        (-1, -1),
        (-1, 0),
        (-1, 1),
        (0, -1),
        (0, 0),
        (0, 1),
        (1, -1),
        (1, 0),
        (1, 1),
    ] {
        let at = pos + IVec3::new(dx, 1, dz);
        let (other_kind, _, other_flowing) = fluid_of(state_provider.get_fluid(at));

        if same_fluid(kind, flowing, other_kind, other_flowing) {
            continue;
        }

        // "Solid render" is the game's `BlockState#isSolidRender` - a full block that hides what is
        // behind it - which this side carries as the face flags' occlusion mask rather than as a
        // separate flag: a block that occludes all six of its neighbours is exactly one that renders
        // solid. See [`FaceFlags::occlusion`].
        let flags = face_flags(block_manager, state_provider.get_state(at));

        if flags.occlusion != 0b0011_1111 {
            wants = true;
        }
    }

    wants
}

/// The fluid mesher's geometry, for a caller that already has the sprites.
///
/// Split out from [`bake_fluid_faces`] so that the shape of a fluid can be tested against a synthetic
/// world: everything below the sprite lookup is arithmetic on block states and fluid bytes, and the
/// lookup is the only part that needs a GPU atlas. The alternative was what this file did for a while -
/// a fluid fall that came out with seams in it, argued about from the code and from screenshots rather
/// than from the quads.
fn bake_fluid_faces_with<Provider: BlockStateProvider>(
    block_manager: &BlockManager,
    state_provider: &Provider,
    sprites: &[Option<(RenderLayer, FluidSprites)>; 2],
    layers: &mut [BakedLayer],
    section_offset: IVec3,
) {
    // The same index list the block baker emits, for the same reason: a mirror in the clip space
    // (see `preprocessing.rs`) turns every triangle over, so the two triangles of a quad are emitted
    // in the order that comes back out wound counter-clockwise.
    const INDICES: [u32; 6] = [0, 3, 1, 1, 3, 2];

    // Water is tinted by the biome it is in, and this is the colour the game uses where no biome says
    // otherwise - see [`BlockStateProvider::get_fluid_color`], which is what replaces it when the provider
    // knows. Packed the way the terrain shader reads the vertex colour back: red in the low byte. See
    // [`WATER_TINT`].

    let mut add_quad = |layer: RenderLayer,
                        dir: Direction,
                        light: u8,
                        color: u32,
                        uv_flags: u32,
                        corners: [(glam::Vec3, [u16; 2]); 4],
                        back_face: bool| {
        FLUID_QUADS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        if phases_on() {
            phase_fluid_face.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        // A fluid face is shaded by its direction exactly as a block face is - the game's `FluidRenderer`
        // writes `tint * up * (north | west)` for a side, `tint * up` for a top and `tint * down` for a
        // bottom, which is `CardinalLighting.DEFAULT.byFace` in every case.
        let color = scale_rgb(color, face_shade(dir));

        let baked_layer = &mut layers[layer as usize];
        let first = baked_layer.vertices.len() / Vertex::VERTEX_LENGTH;
        let normal = dir.to_vec().as_vec3().to_array();

        // The face, wound the way this renderer culls it.
        let wound = wind_quad(corners, glam::Vec3::from_array(normal));

        // And, when it is asked for, **its mirror as well** - the same four points the other way round,
        // which is the same surface facing the other way, sent as a second quad of its own. Reversed
        // *after* the winding and appended rather than replacing it, because both sides are drawn: the
        // front for everyone above the surface, the back for a player inside it. See
        // [`fluid_wants_back_face`].
        //
        // Two quads and not one turned inside out, which is the mistake this was written as first: a
        // `reverse()` of the four corners in place rearranges one quad's vertices and appends nothing, so
        // the surface was still drawn from one side only and the quad count did not move.
        let mut quads: Vec<(glam::Vec3, [u16; 2])> = wound.to_vec();

        if back_face {
            quads.extend(wound.iter().rev().copied());
        }

        baked_layer
            .vertices
            .extend(quads.iter().flat_map(|(position, uv)| {
                Vertex {
                    position: position.to_array(),
                    uv: *uv,
                    normal,
                    color,
                    // Whichever atlas this fluid's sprite was baked for: the game's, when it animates
                    // it and the pass has that atlas - which is what makes a lava fall move - and this
                    // side's copy of the frame otherwise. See `FluidSprite`.
                    uv_flags,
                    lightmap_coords: light,
                    // Fluids are not shaded per corner in the game either: a fluid face is
                    // one flat surface, lit by the block it is seen from - so no corner of it is
                    // darkened, which is a count of zero.
                    ao: 0,
                }
                .compressed()
            }));
        // One set of six indices per quad that was written, so the back face's four vertices at `first +
        // 4` are indexed as well as the front's at `first`. A vertex buffer with eight vertices and six
        // indices draws one quad and leaves the other in the arena unread.
        for quad in 0..quads.len() / 4 {
            let base = first as u32 + (quad as u32) * 4;

            baked_layer.indices.extend(
                INDICES
                    .iter()
                    .flat_map(|index| (index + base).to_ne_bytes()),
            );
        }
    };

    for block_index in 0..16 * 16 * 16 {
        let pos = ivec3(block_index & 15, block_index >> 8, (block_index & 255) >> 4);
        let (kind, amount, flowing) = fluid_of(state_provider.get_fluid(pos));

        // Counted before the sprites are looked at: a fluid whose sprite never made it into the atlas
        // is exactly the case the counters exist to tell apart from a fluid that never arrived.
        if kind == 1 || kind == 2 {
            FLUID_BLOCKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        let sprites = match kind {
            1 => &sprites[0],
            2 => &sprites[1],
            // 3 is "a fluid this mesher does not know": a modded one, or the empty fluid of a block
            // that has none, which is 0.
            _ => continue,
        };

        let Some((layer, sprites)) = sprites else {
            continue;
        };

        let (fx, fy, fz) = (pos.x as f32, pos.y as f32, pos.z as f32);
        // **Water is asked for its colour; lava is not.** The game tints water by the biome it is in and
        // leaves lava alone, and that is exactly what the `kind` test is: `FluidTintSources.water()` is
        // water's, while the lava model carries no tint source at all - `FluidStateModelSet` builds
        // `LAVA_MODEL` with `null` where `WATER_MODEL` passes the water source - so white is lava's
        // answer and not a placeholder for one.
        //
        // Asked per block rather than per section because the biome is a per-column property: a section
        // on a biome boundary holds two colours, and holding both is the thing the constant this replaced
        // could never do. It is one call per fluid block, on the bake thread, where a block face's tint
        // is already asked per face.
        let color = if kind == 1 {
            state_provider.get_fluid_color(pos + section_offset)
        } else {
            0x00ff_ffff
        };

        // The light every face of this fluid is lit by, and the reason it is not the light of the block
        // the face is *towards*.
        //
        // The game's fluid renderer asks its own `getLightCoords`, which is the brightest of the fluid's
        // **own** cell and the cell above it, component by component:
        //
        // ```java
        // // FluidRenderer
        // private int getLightCoords(BlockAndTintGetter level, BlockPos pos) {
        //     return LightCoordsUtil.max(LevelRenderer.getLightCoords(level, pos),
        //                                LevelRenderer.getLightCoords(level, pos.above()));
        // }
        // ```
        //
        // so the fluid's own block is half of the answer wherever the face is. This side read a single
        // neighbour: the block above for the surface, the block the side faces for a side - and a *solid*
        // block has no light in it at all, which is why lava with a block over it came out black:
        //
        // > when there is a block above the lava, the lava goes black - you can just about make out the
        // > texture, the brightness is very low
        //
        // A lava cell holds block light 15 of its own (the light engine writes a block's emission into
        // its own cell), so the brightest of the two is 15 and a covered lava surface is lit by the lava
        // itself, which is what the game shows.
        let own_light = state_provider.get_light_level(pos);
        let above_light = state_provider.get_light_level(pos + IVec3::Y);

        // The top face and the sides both read `getLightCoords(pos)`: the fluid's own cell, or the one
        // above it.
        let light_here = own_light.brightest(above_light).byte;

        // What is above this block, which is two of the decisions below: whether there is a surface here
        // at all, and how tall the block is. "This fluid" is the game's `isSame` - the same *object* - so
        // the other half of the same liquid counts as a different fluid here, see [`same_fluid`].
        let (above_kind, _, above_flowing) = fluid_of(state_provider.get_fluid(pos + IVec3::Y));
        let same_above = same_fluid(kind, flowing, above_kind, above_flowing);

        // The block's own height - and the one case in which the game does **not** average the corners.
        //
        // ```java
        // // FluidRenderer#tesselate
        // float heightSelf = this.getHeight(level, type, pos, blockState, fluidState);
        // if (heightSelf >= 1.0F) {
        //     heightNorthEast = 1.0F;
        //     heightNorthWest = 1.0F;
        //     heightSouthEast = 1.0F;
        //     heightSouthWest = 1.0F;
        // } else {
        //     ... calculateAverageHeight for each of the four corners ...
        // }
        // ```
        //
        // A block that stands a whole block tall - which is every block of a falling column, because the
        // same fluid is directly above it - has **no averaging at all**: all four corners are the top of
        // its own block. Averaging them anyway is what pulled the walls of a lava fall apart. A
        // full-height block's corner was dragged down by a lower neighbour diagonally beside it (the
        // spreading lava at the foot of the fall, the step it poured over), so its side faces stopped
        // short of its own top while the block above started at the block boundary - and the difference
        // between the two is a slit you can see straight through, repeated at every block of the column:
        //
        // > in a lava fall the stepped flowing lava has gaps between it
        //
        // The heights are also *not* averaged for the surface of such a block, so a column's top face is
        // flat at the full block - which is what the game draws.
        let own_height = fluid_height(amount, same_above);

        // The four corners of the block, in the order (0,0), (1,0), (0,1), (1,1) in x and z, each
        // with the height the fluid stands at there. Both the top face and the sides are cut to
        // them, and each one walks four blocks and two weights to work out, so they are worked out once.
        let heights = if own_height >= 1.0 {
            [1.0; 4]
        } else {
            [
                fluid_corner_height(
                    state_provider,
                    block_manager,
                    pos,
                    kind,
                    flowing,
                    own_height,
                    IVec2::new(0, 0),
                ),
                fluid_corner_height(
                    state_provider,
                    block_manager,
                    pos,
                    kind,
                    flowing,
                    own_height,
                    IVec2::new(1, 0),
                ),
                fluid_corner_height(
                    state_provider,
                    block_manager,
                    pos,
                    kind,
                    flowing,
                    own_height,
                    IVec2::new(0, 1),
                ),
                fluid_corner_height(
                    state_provider,
                    block_manager,
                    pos,
                    kind,
                    flowing,
                    own_height,
                    IVec2::new(1, 1),
                ),
            ]
        };
        let corner = [
            (fx, fz),
            (fx + 1.0, fz),
            (fx, fz + 1.0),
            (fx + 1.0, fz + 1.0),
        ];

        // The surface, where the fluid ends: no face between two blocks of the same fluid, which is
        // also what keeps a lake from being drawn a block at a time. "The same fluid" is the game's
        // `isSame`: the same object, so the other half of this liquid has a surface of its own here.
        if !same_above {
            // Which way the surface is running, which decides both the sprite and how it is cut. A
            // surface that is not going anywhere is the whole of the still sprite - `u` and `v` from 0
            // to 1 over it. A *flowing* one is a quarter of the flowing sprite, turned to point along
            // the flow, which is the game's own top face and the reason a stream reads as a current.
            let (flow_x, flow_z) =
                fluid_flow(state_provider, block_manager, pos, kind, flowing, amount);

            let (sprite, offsets) = if flow_x == 0.0 && flow_z == 0.0 {
                (
                    sprites.still,
                    [(0.0, 0.0), (0.0, 1.0), (1.0, 1.0), (1.0, 0.0)],
                )
            } else {
                (sprites.flow, flowing_top_offsets(flow_x, flow_z))
            };

            // The four offsets go to the corners north-west, south-west, south-east, north-east, in
            // that order - the order the four heights are in.
            add_quad(
                *layer,
                Direction::Up,
                light_here,
                color,
                sprite.flags(),
                [
                    (
                        vec3(fx, fy + heights[0], fz),
                        sprite.at(offsets[0].0, offsets[0].1),
                    ),
                    (
                        vec3(fx, fy + heights[2], fz + 1.0),
                        sprite.at(offsets[1].0, offsets[1].1),
                    ),
                    (
                        vec3(fx + 1.0, fy + heights[3], fz + 1.0),
                        sprite.at(offsets[2].0, offsets[2].1),
                    ),
                    (
                        vec3(fx + 1.0, fy + heights[1], fz),
                        sprite.at(offsets[3].0, offsets[3].1),
                    ),
                ],
                fluid_wants_back_face(state_provider, block_manager, pos, kind, flowing),
            );
        }

        // The sides. A neighbour that does not hold this fluid is a wall - lava against stone - and a
        // neighbour that *does* hold it leaves only the **riser of a step**: the band between that
        // block's surface and this one's, and nothing below it.
        //
        // The game draws the whole face for that neighbour - `isNeighborSameFluid` is on the top face
        // alone in 26.1, and a fluid occludes nothing, so a side face is culled only by
        // `isFaceOccludedByNeighbor` and `isFaceOccludedBySelf`:
        //
        // ```java
        // public static boolean shouldRenderFace(FluidState fluidState, BlockState selfState,
        //                                       Direction direction, BlockState otherState) {
        //     return !isNeighborStateHidingOverlay(fluidState, otherState, direction.getOpposite())
        //         && !isFaceOccludedBySelf(selfState, direction);
        // }
        // ```
        //
        // and the whole face is what left the texture of a step's flank visible from *inside* the lava:
        //
        // > the internal culling is off too - inside the lava you can see the texture of flowing lava
        // > that is not exposed to air
        //
        // Which is exactly what the part below the neighbour's surface is: geometry inside the fluid,
        // with a flow texture on it, that nothing can see from outside and a camera *inside* the fluid
        // looks straight at. Vanilla has the same faces, and they are invisible there for the same
        // reason they are invisible here - from outside. So this side draws the band that is exposed and
        // measures both of its edges in the fluid's own height: the neighbour's surface where the two
        // blocks meet, and this block's.
        //
        // Two blocks of one fluid at one level have no band at all - their corner heights come out of the
        // same four blocks - so a lake still pays nothing for this.
        for (dir, first, second, neighbour_first, neighbour_second) in [
            (Direction::North, 0, 1, IVec2::new(0, 1), IVec2::new(1, 1)),
            (Direction::South, 2, 3, IVec2::new(0, 0), IVec2::new(1, 0)),
            (Direction::West, 0, 2, IVec2::new(1, 0), IVec2::new(1, 1)),
            (Direction::East, 1, 3, IVec2::new(0, 0), IVec2::new(0, 1)),
        ] {
            let neighbour = pos + dir.to_vec();
            let (neighbour_kind, _, neighbour_flowing) =
                fluid_of(state_provider.get_fluid(neighbour));

            // The floor of the face: the bottom of this block, or the neighbour's own surface where the
            // neighbour holds the same fluid. The four corner parameters are the ones that name the two
            // ends of the shared edge, in this block's numbering and in the neighbour's.
            //
            // The neighbour's surface is its own - `sampled_height` asks whether *it* has the same fluid
            // above it - and it is measured along the shared edge by the same weighted average this
            // block's corners go through, so the two faces agree about where they meet.
            let (floor_first, floor_second) =
                if same_fluid(kind, flowing, neighbour_kind, neighbour_flowing) {
                    let neighbour_height = sampled_height(
                        state_provider,
                        block_manager,
                        neighbour,
                        neighbour_kind,
                        neighbour_flowing,
                    );

                    (
                        fluid_corner_height(
                            state_provider,
                            block_manager,
                            neighbour,
                            neighbour_kind,
                            neighbour_flowing,
                            neighbour_height,
                            neighbour_first,
                        ),
                        fluid_corner_height(
                            state_provider,
                            block_manager,
                            neighbour,
                            neighbour_kind,
                            neighbour_flowing,
                            neighbour_height,
                            neighbour_second,
                        ),
                    )
                } else {
                    (0.0, 0.0)
                };

            let (low, high) = (heights[first], heights[second]);

            // Nothing to show: this block is not above the neighbour's surface anywhere along the edge.
            if low <= floor_first && high <= floor_second {
                continue;
            }

            // The flowing sprite, in the quarter of it the game's own fluid renderer samples: `u` from 0
            // to 0.5 - one half of the sprite across - and `v` from `(1 - height) * 0.5` at the fluid's
            // surface down to 0.5, the sprite's middle. The two halves are what makes the flow pattern
            // tile across a face whose height depends on where the fluid settled, and a riser is the same
            // mapping over a shorter face.
            let v_surface = |height: f32| (1.0 - height.clamp(0.0, 1.0)) * 0.5;

            add_quad(
                *layer,
                dir,
                light_here,
                color,
                sprites.flow.flags(),
                [
                    (
                        vec3(corner[first].0, fy + floor_first, corner[first].1),
                        sprites.flow.at(0.0, v_surface(floor_first)),
                    ),
                    (
                        vec3(corner[second].0, fy + floor_second, corner[second].1),
                        sprites.flow.at(0.5, v_surface(floor_second)),
                    ),
                    (
                        vec3(corner[second].0, fy + high, corner[second].1),
                        sprites.flow.at(0.5, v_surface(high)),
                    ),
                    (
                        vec3(corner[first].0, fy + low, corner[first].1),
                        sprites.flow.at(0.0, v_surface(low)),
                    ),
                ],
                // A side face is drawn once: the game adds no back face to one, because a fluid's sides
                // are only ever seen from outside it.
                false,
            );
        }

        // The underside, where the fluid does not carry on into the block below - lava pouring over
        // an edge rather than a lake.
        if fluid_of(state_provider.get_fluid(pos - IVec3::Y)).0 != kind {
            // The game's `getLightCoords(pos.below())` - the cell below, or the fluid's own.
            let light = own_light
                .brightest(state_provider.get_light_level(pos - IVec3::Y))
                .byte;

            add_quad(
                *layer,
                Direction::Down,
                light,
                color,
                sprites.still.flags(),
                [
                    (vec3(fx, fy, fz), sprites.still.at(0.0, 0.0)),
                    (vec3(fx, fy, fz + 1.0), sprites.still.at(0.0, 1.0)),
                    (vec3(fx + 1.0, fy, fz + 1.0), sprites.still.at(1.0, 1.0)),
                    (vec3(fx + 1.0, fy, fz), sprites.still.at(1.0, 0.0)),
                ],
                false,
            );
        }
    }
}

/// The fluid sprites' offsets, which are fractions of a sprite rather than pixels of an atlas. See
/// [`FluidSprite`].
#[cfg(test)]
mod fluid_sprite_tests {
    use super::*;

    /// A sprite in this side's atlas, for a game that is not animating it.
    fn here(u0: u16, v0: u16, u1: u16, v1: u16) -> FluidSprite {
        FluidSprite {
            atlas: ((u0, v0), (u1, v1)),
            game: None,
            animated: false,
            level_cap: None,
        }
    }

    /// A sprite the game animates, at the given rectangle of the game's own atlas.
    ///
    /// `animated` is true because that is what the name says and because the fixture exists to test the
    /// animated case: it asserted `flags() == UV_GAME_ATLAS` for a long time, which is the assertion that let
    /// the fluid path forget the flag while every test stayed green.
    /// `level_cap` is `Some`, because a sprite the game animates is exactly the sprite whose chain the
    /// game built extra levels for - and the fluid path reading two of these three fields and not the third
    /// is what let the shimmer come back. A fixture that leaves it `None` cannot catch that.
    fn in_the_games_atlas(rect: [f32; 4]) -> FluidSprite {
        FluidSprite {
            atlas: ((0, 0), (16, 16)),
            game: Some(rect),
            animated: true,
            level_cap: Some(5),
        }
    }

    /// **A fluid sprite carries `UV_ANIMATED` *and* a floor, and the second is the one that went missing.**
    ///
    /// This is the regression test for a fix that was measured as working and then came back, and the shape
    /// of the mistake is worth stating because no counter could see it: `UV_ANIMATED` is what makes the
    /// shader *read* the level-of-detail floor, and the floor is zero unless the sprite's `level_cap`
    /// reached `sprite_level_floor`. Both facts were required and only the first was counted -
    /// `FLUID_FACES_ANIMATED` read as healthy for every fluid face in the world while the floor was zero,
    /// and the shimmer the `-4` offset used to hide came straight back.
    ///
    /// A 32-texel `lava_flow` is five levels of its own detail and keeps four of them, so its floor is one.
    #[test]
    fn an_animated_fluid_gets_a_floor_and_not_merely_the_flag() {
        let lava = in_the_games_atlas([0.25, 0.5, 0.5, 0.75]);

        assert_eq!(
            lava.flags(),
            UV_GAME_ATLAS | UV_ANIMATED,
            "the flag is the half that was already there"
        );

        assert_eq!(
            crate::mc::block::sprite_level_floor(lava.level_cap, lava.animated),
            1,
            "a 32-texel sprite has five levels and keeps four, so nothing coarser than level 1 - and the \
             depth is the variable: at two kept the floor was 3, which a player measured as not enough, and \
             the `-4` bias that did work reaches level 0 on a chain this short"
        );

        // **And whether it is applied at all is a separate question, which is `LOD_FLOOR`.** The arithmetic
        // is right either way; the flag is what says whether the shader is handed it, and it is off while the
        // two candidate explanations for the shimmer are told apart. See that constant.
        assert_eq!(
            crate::mc::block::sprite_level_floor_for(lava.level_cap, lava.animated),
            if LOD_FLOOR { 1 } else { 0 },
            "the accessor the bake calls follows `LOD_FLOOR`, and the arithmetic above does not"
        );

        // And the same sprite with nothing registered for it, which is what the fluid path used to build:
        // the flag set, and a floor of zero. Named so the difference is visible in the test as well as in
        // the field's docs.
        assert_eq!(
            crate::mc::block::sprite_level_floor(None, true),
            0,
            "no cap is no floor, which is the state every fluid face was in"
        );
    }

    /// An animated sprite is packed as a strip of square frames, and every offset a fluid face uses is
    /// a fraction of **one** frame. Taking the strip's whole height is what puts sixteen frames on one
    /// face - which is what a naive `rect.min + offset` does the moment the offsets are fractions.
    #[test]
    fn one_frame_of_a_strip_is_one_frame() {
        // `lava_flow.png` is 32x512: sixteen frames of 32x32.
        let flow = here(64, 128, 96, 640);

        assert_eq!(flow.at(0.0, 0.0), [64, 128], "the frame's own corner");
        assert_eq!(
            flow.at(1.0, 1.0),
            [96, 160],
            "the frame is square - the strip is 32 wide - so its bottom is 32 down, not 512"
        );
        assert_eq!(
            flow.at(0.5, 0.5),
            [80, 144],
            "and its middle is the middle of frame 0"
        );
    }

    /// The offsets the fluid mesher passes are the ones it passed before, for the sprite sizes vanilla
    /// ships: `water_flow.png` is 32 pixels a frame, so half of it is the sixteen pixels the old
    /// pixel-based form added. The rewrite was not supposed to move anything about a vanilla pack.
    #[test]
    fn the_vanilla_offsets_land_where_they_always_did() {
        // The strip this side packs for `water_flow.png`, 32x1024.
        let flow = here(200, 300, 232, 1324);

        // The four corners of a side face at full height.
        assert_eq!(flow.at(0.0, 0.5), [200, 316]);
        assert_eq!(flow.at(0.5, 0.5), [216, 316]);
        assert_eq!(flow.at(0.5, 0.0), [216, 300]);
        assert_eq!(flow.at(0.0, 0.0), [200, 300]);

        // Which is `rect.min + (u, v)` for the old whole-pixel offsets, `u` 0 and 16, `v` 16 and 0.
        let old = |u: u16, v: u16| [200 + u, 300 + v];
        assert_eq!(flow.at(0.0, 0.5), old(0, 16));
        assert_eq!(flow.at(0.5, 0.5), old(16, 16));
        assert_eq!(flow.at(0.0, 0.0), old(0, 0));

        // And a face clipped by the fluid's height: the surface corner sits at `(1 - height) * 0.5`,
        // which for a 32-pixel frame is the `16 - 16 * height` the old form computed.
        for height in [0.0f32, 0.25, 0.5, 0.875, 1.0] {
            let old_v = (16.0 - 16.0 * height) as u16;
            let v = (1.0 - height) * 0.5;

            assert_eq!(
                flow.at(0.0, v),
                old(0, old_v),
                "a fluid standing {height} of a block high"
            );
        }
    }

    /// A face the game animates carries the game's coordinates and the flag that says so, in the game's
    /// own units: `0..1` over the game's rectangle, in the sixteen bits a vertex holds them in.
    #[test]
    fn an_animated_sprite_is_baked_for_the_games_atlas() {
        let still = in_the_games_atlas([0.25, 0.5, 0.5, 0.75]);

        assert_eq!(
            still.flags(),
            UV_GAME_ATLAS | UV_ANIMATED,
            "a fluid the game animates is **both** of the flags: on the game's atlas, and moving - and this \
             assertion used to read `UV_GAME_ATLAS` alone, which is how the flag went missing for fluids"
        );
        assert_eq!(still.at(0.0, 0.0), [16384, 32768], "the game's own corner");
        assert_eq!(still.at(1.0, 1.0), [32768, 49151], "and its opposite one");

        assert_eq!(
            here(0, 0, 16, 16).flags(),
            0,
            "a sprite this side drew from its own copy says nothing about the game's atlas"
        );
    }
}

#[cfg(test)]
mod shading_tests {
    /// The corner curve the shader applies, against the average it stands for.
    ///
    /// Vanilla's corner brightness is the mean of **four** samples - the two side neighbours, the
    /// diagonal block and the block the face looks at - each `0.2` for a block that fills its whole block
    /// and `1.0` for everything else (`BlockModelLighter#prepareQuadAmbientOcclusion` reads
    /// `BlockBehaviour#getShadeBrightness` four times and multiplies by 0.25). The vertex carries only
    /// the *count* of those four that are `0.2`, and the fragment shader turns that back into a
    /// brightness with `1 - 0.2 * count`.
    ///
    /// This is that identity over every count there is. It is what makes an integer in the vertex format
    /// equivalent to the float the game bakes - and it is the check that breaks if either half moves: a
    /// curve that starts at `0.6` instead of `1.0` (which is what this renderer drew for months, and
    /// what "the ambient occlusion is too weak" was), or a fifth sample counted into the vertex.
    #[test]
    fn the_corner_curve_is_the_average_it_stands_for() {
        for count in 0..=4u8 {
            let occluders = count as f32;
            let vanilla = (occluders * 0.2 + (4.0 - occluders) * 1.0) / 4.0;
            let ours = 1.0 - 0.2 * occluders;

            assert!(
                (vanilla - ours).abs() < 1e-6,
                "{count} occluder(s): vanilla's average is {vanilla}, the curve gives {ours}"
            );
        }

        assert!(
            (1.0f32 - 0.2 * 4.0 - 0.2).abs() < 1e-6,
            "a corner with all four samples occluded is 0.2, which is the darkest vanilla draws - and \
             exactly the case the old curve drew at 0.6"
        );
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
            uv_flags: 0,
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

    /// **Reversing a wound quad turns it over, and that is the whole of what the back face relies on.**
    ///
    /// The back face of a fluid surface - the one that makes a lake visible from inside it, see
    /// [`fluid_wants_back_face`] - is the front face's four points written the other way round. If that
    /// did *not* turn the triangle over, the second quad would be culled with the first and the fix would
    /// do nothing at all.
    ///
    /// Winding is a sign, so this asks the two quads for the sign their first triangle has about the
    /// surface's axis. The subtlety it is here for is **which** four points are reversed: [`wind_quad`]
    /// normalises whatever it is handed - it swaps two corners if the first triangle comes out facing away
    /// from the normal - so reversing its *input* is a question it answers by undoing. What has to be
    /// reversed is its *output*. This asserts exactly that, and the first version of this test asserted
    /// the opposite and was wrong.
    #[test]
    fn a_reversed_quad_is_turned_over_and_winds_the_other_way() {
        let top = [
            [0.0, 1.0, 0.0],
            [0.0, 1.0, 1.0],
            [1.0, 1.0, 1.0],
            [1.0, 1.0, 0.0],
        ];

        let signed_area = |corners: [[f32; 3]; 4]| {
            let a = glam::Vec3::from_array(corners[0]);
            let b = glam::Vec3::from_array(corners[1]);
            let c = glam::Vec3::from_array(corners[2]);

            (b - a).cross(c - a).y
        };

        let wound = wind_quad(
            top.map(|corner| (glam::Vec3::from_array(corner), [0u16, 0u16])),
            glam::Vec3::Y,
        );

        let as_corners = |quad: [(glam::Vec3, [u16; 2]); 4]| {
            [
                quad[0].0.to_array(),
                quad[1].0.to_array(),
                quad[2].0.to_array(),
                quad[3].0.to_array(),
            ]
        };

        let front = signed_area(as_corners(wound));
        let back = signed_area(as_corners([wound[3], wound[2], wound[1], wound[0]]));

        assert!(
            front > 0.0,
            "the front face has to come out of `wind_quad` facing its own normal, which for an upward \
             surface is a positive area about `y`: got {front}"
        );

        assert!(
            back < 0.0,
            "the back face is the front face's four points reversed, so it has to wind the other way - \
             that is the only thing that makes it a back face rather than a second copy of the front one, \
             which would be culled with it and leave the surface invisible from below. front {front}, \
             back {back}"
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
                        uv_flags: 0,
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

/// The translucent quad sort. See [`sort_translucent_quads`].
///
/// Worth a test rather than a picture, because **a wrong order here is a wrong blend and nothing else**:
/// the geometry is all present, the depth test passes, and the only difference is which of two surfaces
/// is drawn first. That is the one class of bug this renderer cannot see in a log and a player describes
/// as "the water looks off".
#[cfg(test)]
mod translucent_sort_tests {
    use super::*;
    use crate::render::pipeline::Vertex;

    /// A quad at `origin`, as the packed bytes a baked layer holds: four vertices of sixteen bytes, with
    /// the position in the first three of each.
    fn quad(origin: [u8; 3]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(Vertex::VERTEX_LENGTH * 4);

        for _ in 0..4 {
            let mut vertex = [0u8; Vertex::VERTEX_LENGTH];
            vertex[..3].copy_from_slice(&origin);
            bytes.extend_from_slice(&vertex);
        }

        bytes
    }

    /// A layer of the given quads, with each quad's own indices in the fixture's six-value pattern.
    fn layer(quads: &[[u8; 3]]) -> BakedLayer {
        let mut vertices = Vec::new();
        let mut indices = Vec::new();

        for (number, origin) in quads.iter().enumerate() {
            vertices.extend_from_slice(&quad(*origin));

            for relative in [0u32, 3, 1, 1, 3, 2] {
                indices.extend_from_slice(&(relative + (number as u32) * 4).to_ne_bytes());
            }
        }

        BakedLayer { vertices, indices }
    }

    /// The distance a quad's centroid sits at, which is the key the sort is by.
    fn centroid_distance(vertices: &[u8], position: usize) -> i32 {
        let at = position * Vertex::VERTEX_LENGTH * 4;
        let third = at + Vertex::VERTEX_LENGTH * 2;

        let axis = |channel: usize| -> i32 {
            (vertices[at + channel] as i32 + vertices[third + channel] as i32) / 2
        };

        let (x, y, z) = (axis(0), axis(1), axis(2));

        x * x + y * y + z * z
    }

    /// **The farthest quad comes out first**, which is the whole of the rule: blending is
    /// order-dependent and a translucent surface behind another has to be drawn before it.
    #[test]
    fn the_farthest_quad_is_drawn_first() {
        // Deliberately not in order, so a sort that does nothing cannot pass.
        let mut baked = layer(&[[4, 0, 0], [1, 0, 0], [3, 0, 0]]);

        sort_translucent_quads(&mut baked);

        let order: Vec<usize> = baked
            .indices
            .as_chunks::<{ 6 * 4 }>()
            .0
            .iter()
            .map(|group| {
                let first = u32::from_ne_bytes(group[..4].try_into().expect("four bytes"));
                (first / 4) as usize
            })
            .collect();

        assert_eq!(
            order,
            vec![0, 2, 1],
            "the quads are at x=4, x=1 and x=3 in bake order, so farthest first is the first baked, \
             then the third, then the second"
        );

        // And the keys really are descending, read back out of the buffer the sort produced rather than
        // from the order it was expected to produce.
        let keys: Vec<i32> = order
            .iter()
            .map(|quad| centroid_distance(&baked.vertices, *quad))
            .collect();

        assert!(
            keys.windows(2).all(|pair| pair[0] >= pair[1]),
            "the sort is descending by centroid distance, and read back it is {keys:?}"
        );
    }

    /// **Only the six-index groups move.** A quad's winding is the order of its indices, so a sort that
    /// permuted the values inside a group would turn every translucent face inside out - which is the
    /// failure this checks for, and it is one that would look like a lighting bug rather than a sorting
    /// one.
    #[test]
    fn a_quad_keeps_its_own_indices() {
        let mut baked = layer(&[[2, 0, 0], [9, 0, 0]]);

        sort_translucent_quads(&mut baked);

        for group in baked.indices.as_chunks::<{ 6 * 4 }>().0 {
            let values: Vec<u32> = group
                .as_chunks::<4>()
                .0
                .iter()
                .map(|word| u32::from_ne_bytes(*word))
                .collect();

            // The pattern is relative within the quad, so subtracting the quad's own base leaves the
            // fixture's six values in their fixture order.
            let base = values.iter().copied().min().expect("six values");

            assert_eq!(
                base % 4,
                0,
                "a quad's indices start at its own first vertex"
            );

            let relative: Vec<u32> = values.iter().map(|value| value - base).collect();

            assert_eq!(
                relative,
                vec![0, 3, 1, 1, 3, 2],
                "the winding inside a quad is untouched by the sort"
            );
        }
    }

    /// A layer that does not describe whole quads is left exactly as it is.
    ///
    /// The alternative is worse than not sorting: reordering index groups against a vertex buffer whose
    /// quads are not where the sort assumes attaches each quad's indices to another quad's vertices, so a
    /// buffer this side does not understand is a buffer this side does not touch.
    #[test]
    fn a_layer_that_is_not_whole_quads_is_left_alone() {
        let mut ragged = layer(&[[1, 0, 0]]);
        ragged.vertices.truncate(Vertex::VERTEX_LENGTH * 3);

        let before = ragged.indices.clone();

        sort_translucent_quads(&mut ragged);

        assert_eq!(ragged.indices, before, "nothing moved");

        // And the empty layer, which every section has two of.
        let mut empty = BakedLayer::default();

        sort_translucent_quads(&mut empty);

        assert!(empty.indices.is_empty());
    }
}

/// The leaves switch and the layer it decides. See [`force_opaque`].
///
/// Worth a test rather than a picture, because **the two wrong answers look like two different bugs and
/// neither looks like a switch**: a leaf forced opaque when the player asked for cut-out leaves is a tree
/// that lost its holes, and a non-leaf forced opaque is a pane or a plant drawn with no alpha test - which
/// is not "leaves are wrong", it is "some blocks are wrong", and the connection between them is a single
/// `&&`.
#[cfg(test)]
mod force_opaque_tests {
    use super::*;

    /// The truth table, over both inputs - which is the whole of the function and the whole of the
    /// game's, so anything that is not this is a bug in one direction or the other.
    #[test]
    fn only_a_leaf_with_cut_out_leaves_off_is_forced_opaque() {
        // `cutout_leaves`, `is_leaves`, expected.
        let table = [
            // The default: leaves are leaves, whatever the block is.
            (true, true, false),
            (true, false, false),
            // Fast graphics: a leaf block goes opaque, and nothing else moves.
            (false, true, true),
            (false, false, false),
        ];

        for (cutout_leaves, is_leaves, expected) in table {
            assert_eq!(
                force_opaque(cutout_leaves, is_leaves),
                expected,
                "cutout leaves {cutout_leaves}, a leaf block {is_leaves}"
            );
        }
    }

    /// **The switch only ever moves leaves**, which is the assertion a `&&` written the wrong way round
    /// fails: with the operands swapped, `Fast` would force every non-leaf block opaque and leave the
    /// leaves alone - a world where the trees are fine and the glass, the plants, the ice and the water
    /// are all wrong, which is a bug that would be blamed on the block models.
    #[test]
    fn the_switch_never_forces_a_block_that_is_not_a_leaf() {
        for cutout_leaves in [false, true] {
            assert!(
                !force_opaque(cutout_leaves, false),
                "a block that is not a leaf is forced opaque only when cutout leaves is {cutout_leaves}, \
                 and it must never be"
            );
        }
    }

    /// And the default is on, which is the game's own default and what a player who has never opened the
    /// graphics page sees.
    #[test]
    fn cut_out_leaves_is_on_by_default() {
        assert!(crate::mc::block::cutout_leaves());
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
                    ambient_occlusion: None,
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

    /// What a stone-like state says: its shape is the full block, so every face is occluded, it hides
    /// nothing against its own kind (`skipRendering` is false by default), and it darkens the corners it
    /// touches (`getShadeBrightness` is 0.2 for a block that fills its whole block).
    fn stone() -> FaceFlags {
        FaceFlags {
            occlusion: FULL_CUBE,
            self_hide: 0,
            shades: true,
            // A full cube stops a fluid: `blocksMotion()` is `isSolid()` for everything that is not
            // cobweb or a bamboo sapling, and a collision shape that fills the block is solid.
            blocks_motion: true,
            // A block stands where it was placed unless it asked not to.
            offset_max_y: 0.0,
            offset_xz: false,
            leaves: false,
        }
    }

    /// What glass or a leaf block says: a full-cube *model* that occludes nothing at all,
    /// `skipRendering` true against its own kind, and no darkening - which is the pair that made the
    /// shade flag necessary: ice answers every one of these the same way except the last one, and it
    /// *does* darken corners in vanilla.
    fn glass() -> FaceFlags {
        FaceFlags {
            occlusion: 0,
            self_hide: FULL_CUBE,
            shades: false,
            // `noOcclusion` is about light, not collision, and glass is walked on like any other
            // block: a fluid cannot flow past it either.
            blocks_motion: true,
            offset_max_y: 0.0,
            offset_xz: false,
            leaves: false,
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
            shades: false,
            // A slab is solid - it stops a fluid - and it occludes nothing. The two answers are
            // independent, which is why they are two fields.
            blocks_motion: true,
            offset_max_y: 0.0,
            offset_xz: false,
            leaves: false,
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
            shade: true,
            uv_flags: 0,
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
            shade: true,
            uv_flags: 0,
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

    /// **A refused section's geometry is kept rather than thrown away**, which is the whole point:
    /// the recovery becomes "wait for a free slot" instead of "have Minecraft compile the section
    /// again, send a 27-section payload over JNI and offer the result to a pool that is exactly as
    /// full as it was".
    ///
    /// Drained oldest first, because the section that has been waiting is the one whose ground has
    /// been stale for longer.
    #[test]
    fn a_parked_section_keeps_its_geometry_and_comes_back_oldest_first() {
        let mut storage = SectionStorage::new(pool_for(1));

        // Room for one section of one quad, and not for a second one.
        assert!(put(&mut storage, IVec3::new(0, 0, 0), 1));

        let second = IVec3::new(1, 0, 0);
        let third = IVec3::new(2, 0, 0);
        let layers = vec![layer(1), layer(2)];
        let bytes: usize = layers
            .iter()
            .map(|held| held.vertices.len() + held.indices.len())
            .sum();

        assert!(
            storage.allocate(second, &layers).is_none(),
            "the pool is full"
        );
        assert!(storage.park(second, layers));

        assert_eq!(storage.parked_len(), 1);
        assert_eq!(storage.parked_bytes(), bytes, "held, not estimated");

        assert!(storage.park(third, vec![layer(1)]));

        let waiting = storage.take_parked();

        assert_eq!(
            waiting.iter().map(|(pos, _)| *pos).collect::<Vec<_>>(),
            vec![second, third],
            "oldest first, so the section that has waited longest is offered the next slot"
        );
        assert_eq!(
            waiting[0].1.len(),
            2,
            "and each one comes back with its own geometry rather than a placeholder"
        );
        assert_eq!(waiting[0].1[1].indices.len(), layer(2).indices.len());
        assert_eq!(storage.parked_len(), 0, "taking them forgets them");
        assert_eq!(storage.parked_bytes(), 0);
    }

    /// **A parked section is not a refused one**, and the difference is exactly what the JVM is told.
    ///
    /// A refused section is one this side will not draw, so Minecraft has to mesh it. A parked one is
    /// one this side is *still* going to draw, so telling the JVM about it would have the section
    /// meshed by Minecraft and published here - one section drawn twice.
    #[test]
    fn parking_a_section_takes_it_out_of_the_refusals_the_jvm_hears_about() {
        let mut storage = SectionStorage::new(pool_for(1));

        assert!(put(&mut storage, IVec3::new(0, 0, 0), 1));

        let refused = IVec3::new(1, 0, 0);

        assert!(storage.allocate(refused, &[layer(1)]).is_none());
        assert_eq!(
            storage.refusals_waiting(),
            1,
            "the refusal is recorded first"
        );

        assert!(storage.park(refused, vec![layer(1)]));

        assert_eq!(
            storage.refusals_waiting(),
            0,
            "parked is not refused: the JVM goes on counting this section as sent, because it is \
             still going to be drawn from here"
        );
        assert!(
            storage.refused().is_empty(),
            "so there is nothing to hand over for it"
        );

        let counted = sections_parked();

        assert!(storage.park(IVec3::new(2, 0, 0), vec![layer(1)]));
        assert!(
            sections_parked() > counted,
            "and the run counts it, which is the number that says the expensive recovery did not \
             happen"
        );
    }

    /// Waiting does not become the memory. Past [`PARKED_LIMIT`] sections, and on an arena that has
    /// reached the device's buffer limit, the refusal goes the old way and the JVM is told.
    #[test]
    fn parking_gives_up_at_its_limit_and_at_the_device_limit() {
        let mut storage = SectionStorage::new(pool_for(1));

        assert!(put(&mut storage, IVec3::new(0, 0, 0), 1));

        for index in 0..PARKED_LIMIT {
            let pos = IVec3::new(index as i32 + 1, 0, 0);

            assert!(storage.park(pos, vec![layer(1)]), "parked {index}");
        }

        assert_eq!(storage.parked_len(), PARKED_LIMIT);

        let one_too_many = IVec3::new(-1, 0, 0);

        assert!(
            !storage.park(one_too_many, vec![layer(1)]),
            "the queue is a bound on memory rather than a hope: geometry for every section the \
             player flies past is exactly what an exhausted arena must not accumulate"
        );
        assert_eq!(storage.parked_len(), PARKED_LIMIT);

        // **A replacement keeps its place**, because the section has been waiting since it was first
        // parked and its geometry having changed is not a reason to send it to the back.
        assert!(storage.park(IVec3::new(1, 0, 0), vec![layer(3)]));

        let waiting = storage.take_parked();

        assert_eq!(waiting.len(), PARKED_LIMIT);
        assert_eq!(
            waiting[0].0,
            IVec3::new(1, 0, 0),
            "the replaced entry is still the oldest"
        );
        assert_eq!(waiting[0].1[0].indices.len(), layer(3).indices.len());

        // **An arena at the device's limit has no later slot to wait for**, so nothing waits: the
        // position has to reach the JVM, where Minecraft's own mesh is the fallback for good.
        storage.set_at_capacity(true);

        assert!(!storage.park(one_too_many, vec![layer(1)]));
        assert_eq!(storage.parked_len(), 0);
    }

    /// The trim lets go of the geometry waiting for a slot, and **reports the position**: the JVM
    /// still counts a parked section as drawn by this side, so a parked entry dropped in silence is
    /// the hole this file has three comments about.
    #[test]
    fn the_trim_lets_go_of_geometry_waiting_for_a_slot() {
        let mut storage = SectionStorage::new(pool_for(1));

        storage.set_width(1);

        let near = IVec3::new(0, 0, 0);
        let far = IVec3::new(40, 0, 40);

        assert!(storage.park(near, vec![layer(1)]));
        assert!(storage.park(far, vec![layer(1)]));

        assert_eq!(
            storage.trim(IVec2::new(0, 0)),
            vec![far],
            "one report for the parked section the view left behind, which is what makes Minecraft \
             mesh it"
        );
        assert_eq!(storage.parked_len(), 1, "and the near one still waits");
        assert_eq!(storage.take_parked()[0].0, near);
    }

    /// A parked position that is **also** in the arena is reported once and not twice: the trim's own
    /// loop over the storage reports it, and the sweep over the parked queue has to agree.
    #[test]
    fn a_parked_section_that_is_also_stored_is_reported_once() {
        let mut storage = SectionStorage::new(pool_for(4));

        storage.set_width(1);

        let far = IVec3::new(40, 0, 40);

        assert!(put(&mut storage, far, 1));
        assert!(storage.park(far, vec![layer(1)]));

        assert_eq!(
            storage.trim(IVec2::new(0, 0)),
            vec![far],
            "the section was in the arena, so the arena's loop is the one that reports it"
        );
        assert_eq!(storage.parked_len(), 0, "and its geometry went with it");
    }

    /// Forgetting the arena forgets what was waiting for a slot along with it: it described the world
    /// that has just been left, and the new world's sections are offered from scratch.
    #[test]
    fn clearing_the_arena_clears_what_was_waiting_for_a_slot() {
        let mut storage = SectionStorage::new(pool_for(1));

        assert!(storage.park(IVec3::new(0, 0, 0), vec![layer(1)]));
        assert_eq!(storage.parked_len(), 1);

        storage.forget();

        assert_eq!(storage.parked_len(), 0);
        assert_eq!(storage.parked_bytes(), 0);
        assert!(storage.take_parked().is_empty());
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

        // Growth appends an arena of the size the caller names; the limit is how many there may be.
        assert!(storage.grow_pool(2, pool_for(4)));
        assert_eq!(storage.pool_slots(), pool_for(1) + pool_for(4));

        assert!(
            !storage.grow_pool(2, pool_for(4)),
            "a request past the limit is not a growth"
        );

        let after = stored(&storage);

        assert_eq!(after.vertex_range, before.vertex_range);
        assert_eq!(after.index_range, before.index_range);

        assert!(
            put(&mut storage, IVec3::new(1, 0, 0), 1),
            "and the section that did not fit now does"
        );
    }

    /// **The trim has to report what it dropped**, and this is the test for the third route to a hole.
    ///
    /// A section removed from the arena is one this renderer no longer draws, and the JVM suppresses
    /// Minecraft's mesh for every section it believes Rust holds - so a dropped section nobody is told
    /// about is drawn by neither renderer. It does not come back either: the JVM counts it as sent, so
    /// the next rebuild carries nothing for it. That is the same hole as a refusal and as a dropped
    /// bake, arriving by the trim instead.
    ///
    /// The positions go down the same channel a refusal does, which is what this checks: they come out
    /// of `refused()` and the JVM answers them the way it answers any other.
    #[test]
    fn the_trim_reports_every_section_it_drops() {
        let mut storage = SectionStorage::new(pool_for(64));
        let layers = [layer(1)];

        // One section at the origin and one far away, with a width that puts the second outside it.
        let near = IVec3::new(0, 0, 0);
        let far = IVec3::new(64, 0, 64);

        storage.set_width(8);

        for pos in [near, far] {
            let (section, _) = storage
                .allocate(pos, &layers)
                .unwrap_or_else(|| panic!("{pos:?} should fit"));

            storage.insert(pos, section);
        }

        let dropped = storage.trim(IVec2::new(0, 0));

        assert_eq!(
            dropped,
            vec![far],
            "the section beyond the width is the one the trim drops, and it has to be named"
        );

        // And it is what the other side is handed, through the channel that exists for it.
        for pos in dropped {
            storage.forget_trimmed(pos);
        }

        assert_eq!(
            storage.refused(),
            vec![far],
            "a trimmed section reaches the JVM the way a refused one does, so `rustHas` stops claiming it"
        );

        assert_eq!(
            storage.len(),
            1,
            "and the near section is still held - the trim removed exactly the one it reported"
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

    /// **A refusal is reported however many of them there are**, which is the hole this closed.
    ///
    /// The list of refused positions used to have a cap of 4096 and dropped what it could not hold -
    /// counted in `REFUSED_DROPPED` and forgotten by both sides. A section refused past that cap was
    /// never handed to the JVM, so the JVM kept its record of having sent it, so nothing offered it
    /// again, and Minecraft's own mesh for it stayed suppressed because Rust had been told about it.
    /// Neither side drew it: a 16x16x16 hole that only an unrelated rebuild would ever fill.
    ///
    /// The cap is gone because there is nothing to cap. A refusal leaves its section in the storage, and
    /// the pending set holds positions that are in that map - so the set cannot outgrow the arena.
    #[test]
    fn every_refused_section_is_reported_however_many_there_are() {
        // Enough pool for exactly one quad, so everything that needs a second is refused.
        let mut storage = SectionStorage::new(pool_for(1));
        let layers = [layer(1)];

        // Well past the old cap of 4096.
        let attempts = 6000;
        let mut refused_here = Vec::new();

        for index in 0..attempts {
            let pos = IVec3::new(index % 100, 0, index / 100);

            if storage.allocate(pos, &layers).is_none() {
                refused_here.push(pos);
            }
        }

        assert!(
            refused_here.len() > 4096,
            "the test is only meaningful past the cap this replaced: {} refusals",
            refused_here.len()
        );

        // Sorted as tuples, because `IVec3` has no `Ord` - and sorted rather than compared as sets
        // because a set comparison would not say *how many* were missing, and the count is the whole
        // point of this test.
        let as_tuples = |positions: &mut Vec<IVec3>| {
            let mut tuples: Vec<(i32, i32, i32)> =
                positions.iter().map(|pos| (pos.x, pos.y, pos.z)).collect();

            tuples.sort_unstable();

            tuples
        };

        let reported = as_tuples(&mut storage.refused());
        let refused_here = as_tuples(&mut refused_here);

        assert_eq!(
            reported.len(),
            refused_here.len(),
            "every refusal has to be reported. One the JVM is not told about is a hole nothing will \
             fill: it keeps its record of having sent the section, so nothing offers it again, and \
             Minecraft's own mesh for it stays suppressed."
        );

        assert_eq!(reported, refused_here, "and they are the right sections");

        assert!(
            storage.refused().is_empty(),
            "and the drain is a drain - a second call has nothing left to hand over"
        );
    }

    /// A section that is refused and then accepted stops being reported.
    ///
    /// The other half of the pending set being derived from the storage: a section refused on one frame
    /// and published on the next is in the arena, so handing it to the JVM as refused would drop its
    /// `sent` record and rebuild a section that is already drawn.
    #[test]
    fn a_section_that_fits_on_a_later_frame_stops_being_refused() {
        let mut storage = SectionStorage::new(pool_for(1));
        let layers = [layer(1)];

        // The one quad of pool, taken by a section that keeps it.
        let occupier = IVec3::new(0, 0, 0);
        let (occupier_section, _) = storage
            .allocate(occupier, &layers)
            .expect("the pool starts empty, so the first section fits");

        let held = occupier_section.layers[0]
            .clone()
            .expect("it has a layer, which is what took the pool");

        storage.insert(occupier, occupier_section);

        let pos = IVec3::new(3, 0, 7);

        assert!(
            storage.allocate(pos, &layers).is_none(),
            "and the next one has nowhere to go"
        );
        assert_eq!(storage.refused(), vec![pos], "so it is reported");

        // Room made the way the game makes it: the occupier is walked away from.
        storage.free_in(held.buffer, held.vertex_range);
        storage.free_in(held.buffer, held.index_range);
        storage.storage.remove(&occupier);

        assert!(
            storage.allocate(pos, &layers).is_some(),
            "the same section, with room this time"
        );

        assert!(
            storage.refused().is_empty(),
            "a section that is in the arena is not waiting for one - reporting it here would rebuild \
             geometry that is already drawn"
        );
    }

    /// And the same, for the case where it never went through `allocate` a second time: `insert`
    /// clears the mark, because that is the call that publishes.
    #[test]
    fn publishing_a_section_clears_its_refusal_mark() {
        let mut storage = SectionStorage::new(pool_for(1));
        let layers = [layer(1)];

        let occupier = IVec3::new(0, 0, 0);
        let (occupier_section, _) = storage
            .allocate(occupier, &layers)
            .expect("the pool starts empty");
        storage.insert(occupier, occupier_section);

        let pos = IVec3::new(1, 0, 0);
        assert!(storage.allocate(pos, &layers).is_none());
        assert_eq!(storage.refused(), vec![pos]);

        // Now it is refused and reported, and the section is published directly - which is what
        // `submit_chunk_updates` does once `allocate` has handed it a section.
        let section = Section {
            layers: vec![None, None, None],
        };

        storage.insert(pos, section);

        assert!(
            storage.refused().is_empty(),
            "`insert` is what publishes, so it is what says the section is no longer waiting"
        );
    }
}
