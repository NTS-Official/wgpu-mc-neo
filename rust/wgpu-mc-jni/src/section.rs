//! The section feed: what the JVM hands over for one rebuild, and what is kept of it.
//!
//! One rebuild of one section needs the 27 sections around it. What the JVM sends is Minecraft's own
//! data, not a copy shaped for this side:
//!
//!  - the block storage **exactly as `SimpleBitStorage` holds it** - its raw longs and the five
//!    numbers that find a value in them (`valuesPerLong`, `mask`, `divideMul`, `divideAdd`,
//!    `divideShift`) - plus a **palette translation table** whose entries are the Rust block keys,
//!    indexed by the *Minecraft* palette index the storage decodes to. Two entries may name the same
//!    key (a waterlogged variant, say), which is what keeps this side from renumbering anything: the
//!    JVM no longer rebuilds a palette and re-packs a storage per rebuild - 4096 `getAll` calls and a
//!    fresh `SimpleBitStorage` every time - it hands over what it already has;
//!  - the two light layers, as raw nibble arrays, but **only for the sections whose light changed**
//!    since the last call - see [`WorldSections`];
//!  - a mask of the sections the JVM believes this side already has, so a cache that has been
//!    trimmed can ask for those again instead of baking against holes.
//!
//! Everything arrives in one call, in one buffer, written by the JVM into reusable native memory -
//! see `RustChunkBake` on that side for the writer and [`Payload::parse`] here for the reader. The
//! buffer belongs to the caller and is reused by the next rebuild, so everything kept here is copied
//! out of it before the call returns.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use glam::{IVec2, IVec3};
use once_cell::sync::Lazy;
use parking_lot::RwLock;

use wgpu_mc::mc::block::{BlockstateKey, ChunkBlockState};
use wgpu_mc::mc::chunk::{BlockStateProvider, LightLevel};

use crate::pia::PackedIntegerArray;

/// The first four bytes of a payload, so a buffer written by a different build is refused rather
/// than read as if it were this one.
pub const PAYLOAD_MAGIC: u32 = 0x574D_5332; // "WMS2": the payload carries a fluid byte per palette entry

/// The word pair that ends a payload which carries visibility answers: the magic, then the world.
///
/// A trailer rather than a record, and found from the **end** rather than from an offset in the
/// header, because the blobs in front of it are variable length - a fixed offset would have to be
/// reserved out of the blob area for it, and the end is where a trailer belongs anyway. A payload
/// without one is not an error: it is what a writer that does not send one produces.
pub const VISIBILITY_MAGIC: u32 = 0x574D_5356; // "WMSV"

/// The bit that marks a slot as answered, above the 36 the pairs themselves use.
///
/// Without it "nothing is visible from this section" and "this payload said nothing about this
/// section" are the same 36 zero bits - and they are opposite instructions: the first is a section
/// you cannot see out of, the second is one nobody has judged yet. A section treated as the first
/// when it is the second is a hole in the world.
pub const VISIBILITY_PRESENT: u64 = 1 << 63;

/// The 36 bits of the pairs: `from * 6 + to`, which is `VisibilitySet`'s own indexing.
pub const VISIBILITY_BITS: u64 = (1 << 36) - 1;

/// Words per slot in the trailer: the low half of the pair, then the high half.
const VISIBILITY_WORDS_PER_SECTION: usize = 2;

/// The whole trailer in words: the pairs, then the magic and the world.
const VISIBILITY_TRAILER_WORDS: usize = SECTIONS * VISIBILITY_WORDS_PER_SECTION + 2;

/// Whether the "a trailer arrived" line has been printed, so that it is printed once per run.
///
/// The counter is not in a diagnostic report, because there is nothing to report: nothing draws from
/// these answers yet, so the only thing a number could say is whether the two sides agree about the
/// trailer - and one line says that, while a count once a second would not say it any better.
static VISIBILITY_SEEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// How many sections one call describes: the 3x3x3 around the one being rebuilt.
pub const SECTIONS: usize = 27;

/// Bytes in one light layer: 4096 nibbles, indexed by `(y << 8) | (z << 4) | x`.
pub const LIGHT_BYTES: usize = 2048;

/// The bits a bake call answers with: which of the 27 section records it refused, and whether the
/// caller has to send the whole neighbourhood again.
///
/// A refused index is one whose record was written for another *generation* - the JVM built the
/// payload against a world this side has since forgotten (see [`section_generation`]) - so it was not
/// applied and the JVM must not record it as sent.
pub const REJECTED_MASK: u32 = 0x07FF_FFFF;

/// The `bakeSections` answer's resync bit: this side is missing part of the neighbourhood the JVM
/// counts as sent, so the caller clears its bookkeeping and sends everything again.
pub const RESYNC: u32 = 1 << 27;

const HEADER_WORDS: usize = 8;
const BLOCK_RECORD_WORDS: usize = 16;
const LIGHT_RECORD_WORDS: usize = 4;

/// Which world a record describes, in the last word of its record.
///
/// Both record shapes were one word short of the boundary they are laid out on, so this costs no
/// space: the block record is 16 words and used 15, the light record 4 and used 3.
const BLOCK_RECORD_GENERATION_WORD: usize = 15;
const LIGHT_RECORD_GENERATION_WORD: usize = 3;

/// Which world the section cache describes.
///
/// Bumped by [`bump_section_generation`] when the level changes and the bake is forgotten, and read
/// by every payload: a record stamped with an older generation describes the world that was thrown
/// away, and applying it would put those blocks under the new world's coordinates - which is what a
/// chunk build that was already running when the player changed dimension looks like.
static SECTION_GENERATION: AtomicU32 = AtomicU32::new(0);

/// The world the section cache is describing. See [`SECTION_GENERATION`].
pub fn section_generation() -> u32 {
    SECTION_GENERATION.load(Ordering::Relaxed)
}

/// Forgets which world the cache describes, and answers the new generation.
///
/// Called from `clearSections`, and the number it returns is what the JVM stamps its own records
/// with - one counter, owned here, so the two sides cannot disagree about which world they are
/// describing.
pub fn bump_section_generation() -> u32 {
    SECTION_GENERATION.fetch_add(1, Ordering::Relaxed) + 1
}

/// One section's block data, decoded the way Minecraft would decode it.
#[derive(Debug)]
pub struct SectionBlocks {
    /// Minecraft's storage, copied whole: the longs and the numbers that index them.
    pub storage: PackedIntegerArray,
    /// `palette[minecraft index]` is the Rust block key.
    pub palette: Box<[u32]>,
    /// `fluids[minecraft index]`, in the encoding [`fluid_of`] reads. A block state's *fluid* is not
    /// in its model - lava and water have no model elements at all - so this is the only thing that
    /// says a section holds a fluid, and it comes over with the palette because it is a property of
    /// the state rather than of the position.
    pub fluids: Box<[u8]>,
}

/// How many bytes one section's model variants are: one per position, in the same storage order as the
/// palette - `x | z << 4 | y << 8`, Minecraft's own.
///
/// **The game picks that variant, and it is not a property of the state.** A blockstate whose variant
/// is a *list* - the lily pad's four quarter turns, the weights of a grass tuft - is chosen per
/// position by `ModelBlockRenderer#tesselateBlock`: `random.setSeed(blockState.getSeed(pos))` and then
/// `WeightedVariants#collectParts` -> `WeightedList#getRandomOrThrow`. The choice is a pure function of
/// the state and the position, so the JVM computes it with the game's own `WeightedList` and its own
/// `RandomSource` while it assembles the payload, and this side only reads the answer - which is why
/// there is no random number generator here and no weights: the index already accounts for both.
pub const VARIANTS_BYTES: usize = 4096;

/// The header word the variant blob's offset is written in. Zero - no blob - is every payload the JVM
/// writes for a section whose models are not lists, and it is what keeps this channel free.
///
/// Words 6 and 7 of the header were unused, which is why this costs no record space: the block record
/// is full (all sixteen words have a meaning), and a second record shape for one byte per position
/// would be a layout change to every payload rather than a field in one of them.
pub const VARIANT_OFFSET_WORD: usize = 6;

/// The fluid a palette entry's byte describes: kind, `amount`, falling.
///
/// The kind is 0 for "no fluid", 1 water, 2 lava, 3 anything else - a mod's fluid is a fluid this
/// mesher has no textures for, and it is better left to Minecraft than drawn as a guess. The amount is
/// MC's `FluidState#getAmount`: 8 for a source, lower for a flowing block, and the height a fluid
/// surface sits at is `amount / 9` - which is MC's own `getOwnHeight`, so the arithmetic is the same
/// on both sides.
///
/// [`SectionBlocks::fluid`] hands the raw byte on, so nothing decodes a byte with this yet: it is the
/// written-down form of the encoding that doc comment points at.
#[allow(dead_code)]
pub fn fluid_of(byte: u8) -> (u8, u8, bool) {
    (byte & 0b11, (byte >> 2) & 0b1111, byte & 0b0100_0000 != 0)
}

impl SectionBlocks {
    /// The Rust block key at a position **inside this section**, or `None` when the storage names a
    /// palette entry that is not there - which air is, rather than a panic.
    pub fn key(&self, x: i32, y: i32, z: i32) -> Option<BlockstateKey> {
        let index = self.storage.get(x, y, z);
        self.palette
            .get(index as usize)
            .copied()
            .map(BlockstateKey::from)
    }

    /// The fluid at a position **inside this section**, in the encoding [`fluid_of`] reads.
    ///
    /// A palette entry the JVM did not describe is "no fluid": an older payload, or a state whose
    /// fluid could not be asked for, is a section that draws its blocks and no fluid rather than one
    /// that draws a fluid it made up.
    pub fn fluid(&self, x: i32, y: i32, z: i32) -> u8 {
        let index = self.storage.get(x, y, z);
        self.fluids.get(index as usize).copied().unwrap_or(0)
    }
}

/// One section's two light layers, as the nibble arrays Minecraft's light engine holds.
#[derive(Debug)]
pub struct SectionLight {
    pub block: Box<[u8]>,
    pub sky: Box<[u8]>,
}

/// The light of every section this side has been told about.
///
/// Light is sent once per change rather than once per rebuild: a rebuild of one section needs the
/// light of 27, and re-sending all of them was 108 KB of copying, 54 array allocations and 54 JNI
/// calls *per rebuild*, for data that changes when a torch is placed rather than when a chunk is
/// re-meshed. What is kept here is the last version of each section's light; the JVM keeps the
/// matching "what have I already sent" state, and [`WorldSections::verify`] is how the two are
/// reconciled when this side has dropped something the JVM still counts as sent.
#[derive(Default)]
pub struct WorldSections {
    /// The block data of every section the JVM has sent, which is what a bake reads its neighbours
    /// from. One `Arc` per section, replaced whole when the JVM sends a new version, so a bake that
    /// is already running keeps the version it started with.
    blocks: HashMap<IVec3, Arc<SectionBlocks>>,
    /// The light of the same sections, kept separately because it changes on its own schedule: the
    /// JVM sends a section's blocks every time they change but its light only when the light does.
    light: HashMap<IVec3, Arc<SectionLight>>,
    /// Which model variant each position of a section uses, kept **separately from its blocks** for
    /// the same reason the light is.
    ///
    /// The JVM sends a section's blocks when they change and its variants on *every* payload about it,
    /// because a section is usually sent long before it is baked - as a neighbour of whatever was being
    /// built next to it - and by the time its own turn comes its blocks compare equal and no record is
    /// written. Hanging the variants off the block record, which is what this did first, therefore lost
    /// them for every section that was not first sent as its own target: measured, 92% of the lily pads
    /// drawn still at variant 0 while the JVM's own picks were a flat 149/136/158/130 across the four.
    variants: HashMap<IVec3, Arc<[u8]>>,
    /// Bumped every time a section is stored, so trimming can tell what has been in use.
    tick: u64,
    last_trim: u64,
}

/// How many sections may be held before a trim is considered.
const CACHE_SOFT_LIMIT: usize = 4096;

/// How often a trim is considered, in calls: the scan walks every cached section, so it is not worth
/// doing on every rebuild.
const TRIM_EVERY: u64 = 256;

/// How far a section may be from the one being baked, in chunks, before its light is dropped. A bake
/// only ever looks one section away, so anything beyond this is only kept for bakes that have not
/// happened yet - and the JVM re-sends whatever a trim drops, because a section of the 27 that the
/// JVM counts as sent but this side does not have is a resync (see [`WorldSections::verify`]).
const TRIM_RADIUS: i32 = 48;

pub static WORLD: Lazy<RwLock<WorldSections>> = Lazy::new(|| RwLock::new(WorldSections::default()));

impl WorldSections {
    /// Stores one section's block data, replacing whatever was there.
    pub fn set_blocks(&mut self, pos: IVec3, blocks: Arc<SectionBlocks>) {
        self.blocks.insert(pos, blocks);
        self.tick += 1;
    }

    pub fn remove_blocks(&mut self, pos: IVec3) {
        self.blocks.remove(&pos);
    }

    pub fn blocks(&self, pos: IVec3) -> Option<Arc<SectionBlocks>> {
        self.blocks.get(&pos).cloned()
    }

    /// Stores the light of one section, replacing whatever was there.
    pub fn set_light(&mut self, pos: IVec3, light: Arc<SectionLight>) {
        self.light.insert(pos, light);
        self.tick += 1;
    }

    pub fn remove_light(&mut self, pos: IVec3) {
        self.light.remove(&pos);
    }

    pub fn light(&self, pos: IVec3) -> Option<Arc<SectionLight>> {
        self.light.get(&pos).cloned()
    }

    /// Stores one section's model variants, replacing whatever was there. See [`WorldSections::variants`].
    pub fn set_variants(&mut self, pos: IVec3, variants: Arc<[u8]>) {
        self.variants.insert(pos, variants);
        self.tick += 1;
    }

    /// The variants of one section, which only the section being rebuilt ever has: the JVM sends them
    /// for the middle of the 27 and for no other slot.
    pub fn variants(&self, pos: IVec3) -> Option<Arc<[u8]>> {
        self.variants.get(&pos).cloned()
    }

    /// Whether every section the JVM counts as sent is actually here.
    ///
    /// `mask` is the JVM's word on it: bit `i`, in the `sx + sy*3 + sz*9` order the payload uses,
    /// means "you already have this one, I did not send it". When that is not true - because the
    /// light cache was trimmed, or because this is a fresh world - the caller has to ask for the
    /// whole neighbourhood instead of baking against holes, which is what `false` means here.
    /// Which of the sections the JVM counts as sent are not here, as `(blocks, light)` masks.
    ///
    /// Bits, not a boolean, because *what* went missing is the whole diagnosis: a section whose
    /// blocks are gone is one this side dropped, and a claim for a section that was never sent at all
    /// means the two sides disagree about the order their calls landed in.
    pub fn missing(&self, target: IVec3, known_blocks: u32, known_light: u32) -> (u32, u32) {
        let mut blocks = 0;
        let mut light = 0;

        for index in 0..SECTIONS {
            let bit = 1 << index;
            if known_blocks & bit == 0 && known_light & bit == 0 {
                continue;
            }

            let pos = target + neighbour_offset(index);

            if known_blocks & bit != 0 && !self.blocks.contains_key(&pos) {
                blocks |= bit;
            }

            if known_light & bit != 0 && !self.light.contains_key(&pos) {
                light |= bit;
            }
        }

        (blocks, light)
    }

    /// Drops the light of sections far from the one being baked, once there is enough of it to be
    /// worth the scan.
    pub fn trim(&mut self, target: IVec3) {
        if self.len() < CACHE_SOFT_LIMIT || self.tick - self.last_trim < TRIM_EVERY {
            return;
        }

        self.last_trim = self.tick;

        let center = IVec2::new(target.x, target.z);
        let far =
            |pos: &IVec3| (IVec2::new(pos.x, pos.z) - center).abs().max_element() > TRIM_RADIUS;

        self.blocks.retain(|pos, _| !far(pos));
        self.light.retain(|pos, _| !far(pos));
        // The variants are trimmed with them: a section the JVM re-sends carries its variants again on
        // every payload about it, so dropping one costs a re-bake rather than a wrong angle.
        self.variants.retain(|pos, _| !far(pos));
    }

    pub fn len(&self) -> usize {
        self.blocks
            .len()
            .max(self.light.len())
            .max(self.variants.len())
    }

    /// Read by the trim's own test rather than by the trim, which walks the maps it keeps directly.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty() && self.light.is_empty() && self.variants.is_empty()
    }

    /// Drops every section, for a world this side is no longer describing.
    ///
    /// The tick counters stay where they are: they are only a clock for the trim, and a fresh world
    /// that happens to be smaller than the soft limit should not be trimmed on the first bake.
    pub fn clear(&mut self) {
        self.blocks.clear();
        self.light.clear();
        self.variants.clear();
    }
}

/// The offset of neighbour `index` from the section being baked, in the order the payload uses:
/// x fastest, then y, then z - the order `RenderSectionRegion` and `bake_layers` both use.
pub fn neighbour_offset(index: usize) -> IVec3 {
    IVec3::new(
        (index % 3) as i32 - 1,
        ((index / 3) % 3) as i32 - 1,
        (index / 9) as i32 - 1,
    )
}

/// The payload slot of the section being rebuilt: the middle of the 3x3x3, which is index 13 in the
/// order [`neighbour_offset`] indexes - `x` fastest, then `y`, then `z`, so `(1, 1, 1)` before the
/// offsets are centred. Both the order and the middle are pinned by tests in this module.
pub const CENTER: usize = 13;

/// The key Minecraft knows a section by: `SectionPos.asLong(x, y, z)`.
///
/// The JVM side keys its "what have I sent" table by exactly this, so a position can be handed over
/// as the key itself and taken out of that table without being unpacked first - one number per
/// section, and no chance of the two sides disagreeing about a layout only one of them implements.
///
/// The layout is Minecraft's own: x in the high 22 bits, z in the next 22, y in the low 20, each
/// masked to its width so a negative coordinate packs as the complement `SectionPos.x/y/z` sign
/// extends back.
pub fn section_key(pos: IVec3) -> i64 {
    ((pos.x as i64 & 0x3F_FFFF) << 42)
        | ((pos.z as i64 & 0x3F_FFFF) << 20)
        | (pos.y as i64 & 0xF_FFFF)
}

/// The position a key written by [`section_key`] names: Minecraft's own `SectionPos.x/y/z`.
///
/// The inverse of the function above, and written as Minecraft writes it - each field shifted up so its
/// own sign bit is the top bit of the `i64`, then shifted back down arithmetic, which is what makes a
/// negative coordinate come out negative rather than as a huge positive one. `y` is **twenty** bits and
/// the other two are twenty-two: a world is 24 million blocks across and only two million tall, and the
/// asymmetry is the first thing to get wrong here.
///
/// The widths are not `& 0x3F_FFFF` masks, because a mask gives an unsigned answer for a negative
/// coordinate - the two forms agree only on the values that are not interesting.
pub fn section_pos(key: i64) -> IVec3 {
    // The `<< 0` is Minecraft's own `SectionPos.x`: the three decoders are one expression with three
    // shifts, and writing it out is what keeps them comparable to each other and to the game.
    #[allow(clippy::identity_op)]
    IVec3::new(
        (key << 0 >> 42) as i32,
        (key << 44 >> 44) as i32,
        (key << 22 >> 42) as i32,
    )
}

/// One parsed call: what arrived with it.
#[derive(Debug, Default)]
pub struct Payload {
    /// The 27 slots, `Some` where the payload carried block data for that neighbour.
    pub blocks: [Option<SectionBlocks>; SECTIONS],
    /// Which slots came with block data. A slot that is absent and not in
    /// [`Payload::known_blocks`] is air, which is what a section that is not loaded is.
    pub present: u32,
    /// Which slots the payload says to forget: the section became air, or it was unloaded.
    pub absent: u32,
    /// Which slots were written for another world. Their records are dropped rather than applied -
    /// see [`Payload::apply`] - and the caller is told which they were.
    pub stale: u32,
    /// Which slots the JVM believes this side already has block data for.
    pub known_blocks: u32,
    /// The light that changed, by slot.
    pub light: Vec<(usize, Arc<SectionLight>)>,
    /// Which slots the payload says to forget the light of.
    pub light_absent: u32,
    /// Which slots the JVM believes this side already has light for.
    pub known_light: u32,
    /// What each slot's occlusion graph resolved to, where the payload answered for it: which faces of
    /// the section you can see which other faces from, as `from * 6 + to` over DOWN, UP, NORTH,
    /// SOUTH, WEST, EAST - `VisibilitySet`'s own pair indexing, so nothing has to agree about a
    /// second one.
    ///
    /// `None` is "not answered", which is every slot of a payload that carries no trailer. Nothing
    /// reads these yet: they are carried to the world cache and stored there, and the walk that will
    /// read them (`wgpu-mc`'s `render::section_graph`) is written and tested but not wired to this.
    pub visibility: [Option<u64>; SECTIONS],
    /// The target section's model variants, if this payload carries them. See [`VARIANTS_BYTES`] for
    /// what they are and [`WorldSections::variants`] for where they go.
    ///
    /// One blob per *payload* rather than one per record, because exactly one of the 27 slots is the
    /// section being rebuilt ([`CENTER`]) and it is the only one whose mesh is built here: a
    /// neighbour's variants would be bytes nobody reads.
    pub variants: Option<Box<[u8]>>,
}

impl Payload {
    /// Reads a payload written by `RustChunkBake`.
    ///
    /// `generation` is the world this side is describing: a record stamped with anything else is
    /// reported in [`Payload::stale`] and otherwise ignored, because the blocks it carries belong to
    /// a world that was thrown away - a level change while a chunk build was already running.
    ///
    /// Every field is read as little-endian bytes out of the buffer, so the writer does not have to
    /// align anything and this cannot fault: a length or an offset that runs past the end of the
    /// buffer is `None`, which the caller reports as a dropped bake rather than as a panic on a JNI
    /// frame.
    pub fn parse(bytes: &[u8], generation: u32) -> Option<Self> {
        let magic = read_word(bytes, 0);
        if magic != PAYLOAD_MAGIC {
            log::error!(
                "wgpu-mc: a section payload with magic {magic:#x} is not one this build writes; \
                 dropping it"
            );
            return None;
        }

        let block_count = read_word(bytes, 2) as usize;
        let light_count = read_word(bytes, 3) as usize;

        if block_count > SECTIONS || light_count > SECTIONS {
            log::error!(
                "wgpu-mc: a section payload describes {block_count} block and {light_count} light \
                 sections, more than the {SECTIONS} a bake covers; dropping it"
            );
            return None;
        }

        let mut payload = Payload {
            known_blocks: read_word(bytes, 4),
            known_light: read_word(bytes, 5),
            ..Default::default()
        };

        let mut cursor = HEADER_WORDS;
        for _ in 0..block_count {
            let record = bytes.get(cursor * 4..(cursor + BLOCK_RECORD_WORDS) * 4)?;
            cursor += BLOCK_RECORD_WORDS;

            let index = read_word(record, 0) as usize;
            if index >= SECTIONS {
                continue;
            }

            if read_word(record, BLOCK_RECORD_GENERATION_WORD) != generation {
                payload.stale |= 1 << index;
                continue;
            }

            if read_word(record, 1) == 0 {
                payload.absent |= 1 << index;
                continue;
            }

            let bits = read_word(record, 2);
            let size = read_word(record, 3);
            let values_per_long = read_word(record, 4) as i32;
            let mask = (read_word(record, 5) as u64 | ((read_word(record, 6) as u64) << 32)) as i64;
            let divide_mul = read_word(record, 7) as i32;
            let divide_add = read_word(record, 8) as i32;
            let divide_shift = read_word(record, 9) as i32;
            let palette_len = read_word(record, 10) as usize;
            let palette_offset = read_word(record, 11) as usize;
            let longs_len = read_word(record, 12) as usize;
            let longs_offset = read_word(record, 13) as usize;
            let fluids_offset = read_word(record, 14) as usize;

            let palette_bytes =
                bytes.get(palette_offset..palette_offset.checked_add(palette_len * 4)?)?;
            let palette: Box<[u32]> = (0..palette_len)
                .map(|i| read_word(palette_bytes, i))
                .collect();

            // One byte per palette entry, beside the keys rather than inside them: the key is Rust's
            // (block, variant) pair and has no room for anything else.
            let fluids: Box<[u8]> = bytes
                .get(fluids_offset..fluids_offset.checked_add(palette_len)?)?
                .into();

            let long_bytes = bytes.get(longs_offset..longs_offset.checked_add(longs_len * 8)?)?;
            let longs: Box<[i64]> = (0..longs_len).map(|i| read_long(long_bytes, i)).collect();

            payload.blocks[index] = Some(SectionBlocks {
                storage: PackedIntegerArray::from_parts(
                    longs,
                    values_per_long,
                    bits as i32,
                    mask,
                    divide_mul,
                    divide_add,
                    divide_shift,
                    size as i32,
                ),
                palette,
                fluids,
            });

            payload.present |= 1 << index;
        }

        // The target's model variants, which ride in one header word and one blob and go into the world
        // **beside** its blocks rather than inside them: the payload that carries them usually carries
        // no block record for the target at all. See [`WorldSections::variants`].
        let variant_offset = read_word(bytes, VARIANT_OFFSET_WORD) as usize;

        if variant_offset != 0 {
            match bytes.get(variant_offset..variant_offset.saturating_add(VARIANTS_BYTES)) {
                Some(blob) if blob.len() == VARIANTS_BYTES => {
                    payload.variants = Some(blob.into());
                }
                _ => {
                    log::warn!(
                        "wgpu-mc: a payload names its model-variant blob at {variant_offset}, which is \
                         not {VARIANTS_BYTES} bytes inside it; the section's models are drawn at their \
                         first variant"
                    );
                }
            }
        }

        // The light records start after the *whole* block region, not after the block records that
        // are actually in this payload: the writer packs them from a fixed offset so that a payload
        // carrying fewer than 27 block records - which is every payload that is a diff - still has
        // its light in the same place. Reading them from where the blocks happened to end was a
        // payload that parsed as garbage, or did not parse at all, whenever the diff was smaller
        // than a full neighbourhood.
        let mut cursor = HEADER_WORDS + SECTIONS * BLOCK_RECORD_WORDS;

        for _ in 0..light_count {
            let record = bytes.get(cursor * 4..(cursor + LIGHT_RECORD_WORDS) * 4)?;
            cursor += LIGHT_RECORD_WORDS;

            let index = read_word(record, 0) as usize;
            if index >= SECTIONS {
                continue;
            }

            if read_word(record, LIGHT_RECORD_GENERATION_WORD) != generation {
                payload.stale |= 1 << index;
                continue;
            }

            let block_offset = read_word(record, 1) as usize;
            let sky_offset = read_word(record, 2) as usize;

            // No blob: this section has no light any more - it is not loaded - so what is kept for it
            // is dropped rather than replaced with a layer of zeroes, which would be a claim that the
            // section is dark.
            if block_offset == 0 || sky_offset == 0 {
                payload.light_absent |= 1 << index;
                continue;
            }

            let block = bytes
                .get(block_offset..block_offset.checked_add(LIGHT_BYTES)?)?
                .to_vec();
            let sky = bytes
                .get(sky_offset..sky_offset.checked_add(LIGHT_BYTES)?)?
                .to_vec();

            payload.light.push((
                index,
                Arc::new(SectionLight {
                    block: block.into_boxed_slice(),
                    sky: sky.into_boxed_slice(),
                }),
            ));
        }

        payload.read_visibility(bytes, generation);

        Some(payload)
    }

    /// Reads the visibility trailer, if this payload has one.
    ///
    /// The last two words are the magic and the world; the 54 before them are 27 pairs. A trailer
    /// stamped with another world is dropped whole rather than per slot, because the coordinates it
    /// describes are not the ones the coordinates it names are about to be - the same reason a record
    /// from another world is refused, and the same stamp decides it.
    fn read_visibility(&mut self, bytes: &[u8], generation: u32) {
        // A payload length is a whole number of words by construction; anything else is a payload that
        // was not written by this writer, and reading a trailer out of the middle of a byte is worse
        // than reading none.
        if !bytes.len().is_multiple_of(4) {
            return;
        }

        let words = bytes.len() / 4;
        if words < VISIBILITY_TRAILER_WORDS {
            return;
        }

        let magic_at = words - 2;

        if read_word(bytes, magic_at) != VISIBILITY_MAGIC
            || read_word(bytes, magic_at + 1) != generation
        {
            return;
        }

        let entries = magic_at - SECTIONS * VISIBILITY_WORDS_PER_SECTION;

        let mut answered = 0;

        for (index, slot) in self.visibility.iter_mut().enumerate() {
            let low = read_word(bytes, entries + index * VISIBILITY_WORDS_PER_SECTION) as u64;
            let high = read_word(bytes, entries + index * VISIBILITY_WORDS_PER_SECTION + 1) as u64;
            let pair = low | (high << 32);

            if pair & VISIBILITY_PRESENT != 0 {
                *slot = Some(pair & VISIBILITY_BITS);
                answered += 1;
            }
        }

        // Once per run: this is the one thing that says the JVM's occlusion answers crossed, and it is
        // the absence of this line that says they did not.
        if answered > 0 && !VISIBILITY_SEEN.swap(true, std::sync::atomic::Ordering::Relaxed) {
            log::info!(
                "wgpu-mc: the first section payload carrying visibility answers arrived: {answered} \
                 of {SECTIONS} sections answered"
            );
        }
    }

    /// Applies this payload to the world cache, and answers which of the 27 slots it refused.
    ///
    /// The refused ones are exactly the records that arrived stamped with another generation: the
    /// world they describe was forgotten while the chunk build that wrote them was running, and
    /// installing them would put the old world's blocks at coordinates the new one is about to use.
    /// Nothing else about the call changes - a payload carrying a mix of both is applied for the
    /// records that are still current - and the caller hands the mask back to the JVM so that those
    /// sections are not recorded as sent, which is what makes the next rebuild carry them again.
    pub fn apply(&mut self, world: &mut WorldSections, target: IVec3) -> u32 {
        for (index, slot) in self.blocks.iter_mut().enumerate() {
            let pos = target + neighbour_offset(index);

            if self.absent & (1 << index) != 0 {
                world.remove_blocks(pos);
            } else if let Some(blocks) = slot.take() {
                world.set_blocks(pos, Arc::new(blocks));
            }
        }

        for (index, light) in &self.light {
            world.set_light(target + neighbour_offset(*index), light.clone());
        }

        // **The variants go in beside the blocks, not with them**, and the target is the only section
        // they are ever sent for. See [`WorldSections::variants`] for why they cannot ride on the
        // block record: the payload that carries them usually carries no record for the target at all.
        if let Some(variants) = self.variants.take() {
            world.set_variants(target + neighbour_offset(CENTER), variants.into());
        }

        for index in 0..SECTIONS {
            if self.light_absent & (1 << index) != 0 {
                world.remove_light(target + neighbour_offset(index));
            }
        }

        // Only the slots the payload actually answered for, which is what keeps a section's visibility
        // from being replaced by the silence of a payload that carried no trailer. The answers are kept
        // on the render side rather than here, because the walk that reads them lives there - beside
        // the frustum and the game's own visible list - and this loop is their only writer.
        for (index, pair) in self.visibility.iter().enumerate() {
            if let Some(pair) = *pair {
                wgpu_mc::mc::visibility::set(target + neighbour_offset(index), pair);
            }
        }

        self.stale
    }
}

fn read_word(bytes: &[u8], index: usize) -> u32 {
    let start = index * 4;
    match bytes.get(start..start + 4) {
        Some(word) => u32::from_le_bytes([word[0], word[1], word[2], word[3]]),
        None => 0,
    }
}

fn read_long(bytes: &[u8], index: usize) -> i64 {
    let start = index * 8;
    i64::from_le_bytes([
        bytes[start],
        bytes[start + 1],
        bytes[start + 2],
        bytes[start + 3],
        bytes[start + 4],
        bytes[start + 5],
        bytes[start + 6],
        bytes[start + 7],
    ])
}

/// The block and light of the 27 sections around one bake, resolved once, before it is queued.
///
/// Resolved up front on purpose: the world cache is written by the JVM's chunk-build threads while a
/// bake runs, and a bake that saw a section change halfway through would mesh the seam between two
/// versions of the world.
pub struct CachedBlockstateProvider {
    pub blocks: [Option<Arc<SectionBlocks>>; SECTIONS],
    pub light: [Option<Arc<SectionLight>>; SECTIONS],
    /// The target section's model variants, or `None` for a section that has no list-variant block in
    /// it. See [`VARIANTS_BYTES`].
    pub variants: Option<Arc<[u8]>>,
    pub air: BlockstateKey,
}

impl CachedBlockstateProvider {
    /// The slot of the 27 that a *section-relative* position falls in.
    ///
    /// The baker works in section-local coordinates and reaches one block outside the middle
    /// section, so `-1` is the neighbour below and `16` the one above - which is what the `>> 4`
    /// turns into a section offset. The same arithmetic is in `MinecraftBlockstateProvider` and in
    /// the Java feed, which is where the payload's index order comes from.
    fn slot(pos: IVec3) -> usize {
        let section: IVec3 = (pos >> 4) + IVec3::ONE;
        (section.x + section.y * 3 + section.z * 9).clamp(0, 26) as usize
    }
}

impl BlockStateProvider for CachedBlockstateProvider {
    fn get_state(&self, pos: IVec3) -> ChunkBlockState {
        let Some(blocks) = &self.blocks[Self::slot(pos)] else {
            return ChunkBlockState::Air;
        };

        let Some(key) = blocks.key(pos.x & 15, pos.y & 15, pos.z & 15) else {
            return ChunkBlockState::Air;
        };

        if key == self.air {
            ChunkBlockState::Air
        } else {
            ChunkBlockState::State(key)
        }
    }

    fn get_light_level(&self, pos: IVec3) -> LightLevel {
        let Some(light) = &self.light[Self::slot(pos)] else {
            return LightLevel::from_sky_and_block(0, 0);
        };

        // The nibble packing the light engine uses: `(y << 8) | (z << 4) | x`, low nibble first.
        let packed = (((pos.y & 15) << 8) | ((pos.z & 15) << 4) | (pos.x & 15)) as usize;
        let shift = (packed & 1) << 2;
        let index = packed >> 1;

        let sky = (light.sky[index] >> shift) & 0b1111;
        let block = (light.block[index] >> shift) & 0b1111;

        LightLevel::from_sky_and_block(sky, block)
    }

    fn get_fluid(&self, pos: IVec3) -> u8 {
        let Some(blocks) = &self.blocks[Self::slot(pos)] else {
            return 0;
        };

        blocks.fluid(pos.x & 15, pos.y & 15, pos.z & 15)
    }
    fn is_section_empty(&self, rel_pos: IVec3) -> bool {
        if rel_pos.abs().cmpgt(IVec3::ONE).any() {
            return true;
        }

        self.blocks[Self::slot(rel_pos * 16)].is_none()
    }

    /// Which model variant the position's block uses, from the blob the payload carried for the
    /// section being rebuilt. See [`SectionBlocks::variants`].
    ///
    /// **The middle slot is the only one asked**, and this is why the blob is stored there alone: the
    /// bake places meshes for the section it was queued for, and a neighbour's variant would be an
    /// answer nobody reads. A section with no blob - every section whose models are single entries -
    /// answers `0`, which is the first variant and what this side drew before the blob existed.
    fn get_model_variant(&self, pos: IVec3) -> u8 {
        let Some(variants) = &self.variants else {
            return 0;
        };

        // Minecraft's own storage order, the same one the palette is indexed by and the same one the
        // JVM writes the blob in. A position outside the section would be a *neighbour's* block, whose
        // variant is the caller's mistake rather than this one's - the index masks, so it answers
        // something in range rather than panicking on the bake thread.
        let index = ((pos.y & 15) << 8 | (pos.z & 15) << 4 | (pos.x & 15)) as usize;

        variants.get(index).copied().unwrap_or(0)
    }

    fn get_block_color(&self, _pos: IVec3, _tint_index: i32) -> u32 {
        0xffff_ffff
    }
}

#[cfg(test)]
mod queue_footprint_tests {
    use super::*;
    use crate::MAX_QUEUED_BAKES;

    /// **What one queued bake actually holds**, which is the number `MAX_QUEUED_BAKES` is a bound on.
    ///
    /// The comment beside that constant said "each queued bake owns 27 sections' worth of palettes,
    /// storages and light layers, which is a few hundred kilobytes" - and that is not what the type
    /// does. A bake is handed `Arc`s from the section cache (`SectionWorld::blocks`, which is
    /// `get(..).cloned()`, a refcount bump), so what it *owns* is 27 pointers for the blocks and 27 for
    /// the light. The palettes and storages are shared with the cache and with every other bake that
    /// wants the same neighbour, and they are freed when the last of those lets go.
    ///
    /// This test is the arithmetic written down rather than estimated: the size of the struct, and what
    /// a full queue of them costs. It is here so that the next person to think about this constant has
    /// the measurement instead of a plausible sentence.
    #[test]
    fn a_queued_bake_holds_pointers_and_not_sections() {
        let unit = size_of::<CachedBlockstateProvider>();

        // 27 `Option<Arc<..>>` for the blocks, 27 for the light, and the air key. Each `Option<Arc>` is
        // one pointer: `Arc` is non-null, so the niche makes the `Option` free.
        let pointers = 2 * SECTIONS * size_of::<Option<Arc<SectionBlocks>>>();
        assert_eq!(
            size_of::<Option<Arc<SectionLight>>>(),
            size_of::<Option<Arc<SectionBlocks>>>(),
            "the two arrays are the same shape"
        );

        assert!(
            unit >= pointers && unit <= pointers + 32,
            "the provider is {unit} bytes, and {pointers} of those are the two pointer arrays plus at \
             most a few bytes of padding and the air key"
        );

        // The light of one section is the thing that would dominate if it were owned: two 2048-byte
        // nibble arrays. It is not owned, and this is the size that says so.
        assert_eq!(
            size_of::<SectionLight>(),
            2 * size_of::<Box<[u8]>>(),
            "a light layer is two boxed slices, so a `SectionLight` is two fat pointers"
        );

        println!(
            "one queued bake is {unit} bytes; a full queue of {MAX_QUEUED_BAKES} is {} bytes",
            unit * MAX_QUEUED_BAKES
        );

        // **And what one of those pointers points at**, which is the other half of the question: the
        // flattening idea is to replace the pointers with a per-bake copy of the neighbourhood's cells,
        // and this is what it would be copying. A section is 4096 cells; at four bytes a cell - a key
        // and a light nibble pair, before any of the flags a mesher also asks for - one neighbour is
        // twice what all 54 pointers cost, and there are 26 of them.
        let section_cells = 16 * 16 * 16;
        let flattened_neighbourhood = 26 * section_cells * 4;

        assert!(
            flattened_neighbourhood > 26 * unit,
            "a flattened neighbourhood of {flattened_neighbourhood} bytes is not cheaper than the 26 \
             shared pointers it would replace ({unit} bytes for the whole provider)"
        );

        println!(
            "flattening 26 neighbours at 4 bytes a cell would be {flattened_neighbourhood} bytes per \
             bake, {} times the pointers it replaces",
            flattened_neighbourhood / unit
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The world the fixture's records are written for. A test that wants a stale record writes
    /// another number into it.
    pub(super) const TEST_GENERATION: u32 = 3;

    /// One section's worth of what the JVM writes, so the reader is tested against a writer rather
    /// than against itself.
    pub(super) struct BlockFixture<'a> {
        index: usize,
        bits: u32,
        values_per_long: u32,
        mask: i64,
        longs: &'a [i64],
        palette: &'a [u32],
        fluids: &'a [u8],
    }

    pub(super) fn payload(
        blocks: &[BlockFixture<'_>],
        light: &[(usize, Vec<u8>, Vec<u8>)],
    ) -> Vec<u8> {
        // Every record is stamped with the world the fixture describes; a test that wants a stale one
        // patches the word afterwards, which is also the shape of the race it stands for.
        let generation = TEST_GENERATION;

        // The two record regions are fixed, exactly as `Payload` writes them: 27 block slots, then 27
        // light slots, then the blobs. Writing the light after however many block records there happen
        // to be is what the reader used to assume, and it is the bug this fixture now cannot repeat.
        let records_at = HEADER_WORDS;
        let light_at = records_at + SECTIONS * BLOCK_RECORD_WORDS;
        let mut blob = (light_at + SECTIONS * LIGHT_RECORD_WORDS) * 4;

        let mut bytes = vec![0u8; blob];
        let put = |bytes: &mut Vec<u8>, at: usize, value: u32| {
            bytes[at * 4..at * 4 + 4].copy_from_slice(&value.to_le_bytes());
        };

        put(&mut bytes, 0, PAYLOAD_MAGIC);
        put(&mut bytes, 2, blocks.len() as u32);
        put(&mut bytes, 3, light.len() as u32);

        for (i, block) in blocks.iter().enumerate() {
            let at = records_at + i * BLOCK_RECORD_WORDS;
            let palette_offset = blob;
            let longs_offset = (blob + block.palette.len() * 4).div_ceil(8) * 8;

            bytes.resize(longs_offset + block.longs.len() * 8, 0);

            for (j, value) in block.palette.iter().enumerate() {
                bytes[palette_offset + j * 4..palette_offset + j * 4 + 4]
                    .copy_from_slice(&value.to_le_bytes());
            }
            for (j, value) in block.longs.iter().enumerate() {
                bytes[longs_offset + j * 8..longs_offset + j * 8 + 8]
                    .copy_from_slice(&value.to_le_bytes());
            }

            put(&mut bytes, at, block.index as u32);
            put(&mut bytes, at + 1, 1);
            put(&mut bytes, at + 2, block.bits);
            put(&mut bytes, at + 3, 4096);
            put(&mut bytes, at + 4, block.values_per_long);
            put(&mut bytes, at + 5, block.mask as u32);
            put(&mut bytes, at + 6, (block.mask as u64 >> 32) as u32);
            // One fluid byte per palette entry, in its own blob beside the keys - the same shape the
            // JVM writes.
            let fluids_offset = longs_offset + block.longs.len() * 8;
            bytes.resize(fluids_offset + block.palette.len(), 0);

            for (j, value) in block.fluids.iter().enumerate() {
                bytes[fluids_offset + j] = *value;
            }

            put(&mut bytes, at + 10, block.palette.len() as u32);
            put(&mut bytes, at + 11, palette_offset as u32);
            put(&mut bytes, at + 12, block.longs.len() as u32);
            put(&mut bytes, at + 13, longs_offset as u32);
            put(&mut bytes, at + 14, fluids_offset as u32);
            put(&mut bytes, at + BLOCK_RECORD_GENERATION_WORD, generation);

            blob = fluids_offset + block.palette.len();
        }

        for (i, (index, block, sky)) in light.iter().enumerate() {
            let at = light_at + i * LIGHT_RECORD_WORDS;

            bytes.resize(blob + LIGHT_BYTES * 2, 0);
            bytes[blob..blob + LIGHT_BYTES].copy_from_slice(block);
            bytes[blob + LIGHT_BYTES..blob + LIGHT_BYTES * 2].copy_from_slice(sky);

            put(&mut bytes, at, *index as u32);
            put(&mut bytes, at + 1, blob as u32);
            put(&mut bytes, at + 2, (blob + LIGHT_BYTES) as u32);
            put(&mut bytes, at + LIGHT_RECORD_GENERATION_WORD, generation);

            blob += LIGHT_BYTES * 2;
        }

        bytes
    }

    /// Sixteen four-bit values in one long, with the divide constants zeroed - which is the identity
    /// for the first sixteen positions and so is enough to exercise the decode, the palette
    /// translation and the reader together.
    pub(super) fn nibbles<'a>(
        longs: &'a [i64],
        palette: &'a [u32],
        fluids: &'a [u8],
    ) -> BlockFixture<'a> {
        BlockFixture {
            index: 13,
            bits: 4,
            values_per_long: 16,
            mask: 0xf,
            longs,
            palette,
            fluids,
        }
    }

    #[test]
    fn a_payload_round_trips_its_block_data() {
        let key = (7u32 << 16) | 3;
        let longs = [0x10i64]; // the second nibble is 1, so the position (1, 0, 0)
        let palette = [0u32, key];

        let bytes = payload(&[nibbles(&longs, &palette, &[0, 0b0000_1010])], &[]);
        let parsed = Payload::parse(&bytes, TEST_GENERATION).expect("a payload this build writes");

        assert_eq!(parsed.present, 1 << 13);

        let section = parsed.blocks[13].as_ref().expect("the section it carried");
        assert_eq!(section.key(0, 0, 0), Some(BlockstateKey::from(0u32)));
        assert_eq!(section.key(1, 0, 0), Some(BlockstateKey::from(key)));

        // The fluid channel rides on the same palette index as the block key: kind 2 (lava) with
        // amount 2, which is a flowing block rather than a source.
        assert_eq!(section.fluid(1, 0, 0), 0b0000_1010);
        assert_eq!(
            section.fluid(0, 0, 0),
            0,
            "the entry with no fluid byte carries none"
        );
        assert_eq!(
            section.key(2, 0, 0),
            Some(BlockstateKey::from(0u32)),
            "a long that was never written decodes to the palette's first entry"
        );
    }

    #[test]
    fn a_palette_index_with_no_entry_is_air_rather_than_a_panic() {
        let longs = [0x50i64]; // the second nibble is 5, past the end of the table
        let palette = [0u32, 1];

        let bytes = payload(&[nibbles(&longs, &palette, &[0, 0b0000_1010])], &[]);
        let parsed = Payload::parse(&bytes, TEST_GENERATION).expect("a payload this build writes");
        let section = parsed.blocks[13].as_ref().expect("the section it carried");

        // Index 5 is past the end of a two-entry table.
        assert_eq!(section.key(1, 0, 0), None);
    }

    #[test]
    fn a_payload_round_trips_its_light() {
        let mut block = vec![0u8; LIGHT_BYTES];
        let mut sky = vec![0u8; LIGHT_BYTES];

        // Light 15 in the low nibble of the first byte, 7 in the high one.
        block[0] = 0xf7;
        sky[0] = 0x0f;

        let bytes = payload(&[], &[(13, block, sky)]);
        let parsed = Payload::parse(&bytes, TEST_GENERATION).expect("a payload this build writes");

        assert_eq!(parsed.light.len(), 1);
        assert_eq!(parsed.light[0].0, 13);
        assert_eq!(parsed.light[0].1.block[0], 0xf7);
        assert_eq!(parsed.light[0].1.sky[0], 0x0f);
    }

    #[test]
    fn light_survives_a_payload_that_carries_fewer_than_a_full_neighbourhood() {
        // The shape every diff has: a couple of sections, the rest "you already have them". The light
        // records sit at a fixed offset past all 27 block slots, so a reader that walked on from where
        // the block records ended read the wrong bytes - which is how a diff turned into a payload
        // that did not describe what had changed.
        let longs = [0i64];
        let palette = [0u32, 1];

        let mut block = vec![0u8; LIGHT_BYTES];
        let sky = vec![0u8; LIGHT_BYTES];
        block[0] = 0x3f;

        let bytes = payload(&[nibbles(&longs, &palette, &[])], &[(0, block, sky)]);
        let parsed = Payload::parse(&bytes, TEST_GENERATION).expect("a payload this build writes");

        assert_eq!(
            parsed.present,
            1 << 13,
            "the fixture's block record is index 13"
        );
        assert_eq!(parsed.light.len(), 1);
        assert_eq!(parsed.light[0].0, 0);
        assert_eq!(parsed.light[0].1.block[0], 0x3f);
    }

    #[test]
    fn a_visibility_trailer_is_read_from_the_end_and_stored() {
        // A payload with no records at all - both counts are zero - so the header and the trailer are
        // the whole of it. That is what makes this a test of the trailer rather than of a reader that
        // happens to walk past one.
        let mut bytes = Vec::new();

        for word in [PAYLOAD_MAGIC, 0, 0, 0, 0, 0, 0, 0] {
            bytes.extend_from_slice(&word.to_le_bytes());
        }

        // The section being rebuilt - slot 13, whose offset is (0, 0, 0) - answered for, with one pair
        // set: DOWN sees UP, which is `0 * 6 + 1`.
        let pair = VISIBILITY_PRESENT | (1 << 1);

        for index in 0..SECTIONS {
            let value: u64 = if index == CENTER { pair } else { 0 };

            bytes.extend_from_slice(&(value as u32).to_le_bytes());
            bytes.extend_from_slice(&((value >> 32) as u32).to_le_bytes());
        }

        bytes.extend_from_slice(&VISIBILITY_MAGIC.to_le_bytes());
        bytes.extend_from_slice(&7u32.to_le_bytes());

        let mut payload = Payload::parse(&bytes, 7).expect("a payload with a trailer parses");
        assert_eq!(payload.visibility[CENTER], Some(1 << 1));
        assert_eq!(
            payload.visibility[0], None,
            "a slot the payload did not answer for"
        );

        wgpu_mc::mc::visibility::clear();

        let mut world = WorldSections::default();
        payload.apply(&mut world, IVec3::new(0, 0, 0));
        assert_eq!(
            wgpu_mc::mc::visibility::get(IVec3::new(0, 0, 0)),
            Some(1 << 1)
        );
        assert_eq!(
            wgpu_mc::mc::visibility::get(IVec3::new(-1, -1, -1)),
            None,
            "a neighbour the trailer said nothing about"
        );
        // The stamp decides, the same way it decides for a record: a trailer written for a world that
        // was thrown away names coordinates the ones it is read at are not about any more.
        let elsewhere = Payload::parse(&bytes, 8).expect("the payload still parses");
        assert_eq!(elsewhere.visibility[CENTER], None);

        // And a payload with no trailer is the older writer rather than an error: nothing is answered
        // for, and nothing is refused over it.
        let header_only = Payload::parse(&bytes[..32], 7).expect("a payload without a trailer");
        assert_eq!(header_only.visibility[CENTER], None);
    }

    #[test]
    fn a_payload_from_another_build_is_refused() {
        let mut bytes = payload(&[], &[]);
        bytes[0] = 0x00;

        assert!(Payload::parse(&bytes, TEST_GENERATION).is_none());
    }

    /// A record written for a world this side has forgotten is refused rather than applied.
    ///
    /// This is the level change: a chunk build was already running when the player left the world, and
    /// its payload arrives after the cache was cleared. Installing it would put the old world's blocks
    /// under coordinates the new world is about to use, and the JVM would go on believing they were
    /// sent - so those sections would never be sent again.
    #[test]
    fn a_record_from_another_world_is_refused_and_not_applied() {
        let key = (7u32 << 16) | 3;
        let longs = [0x10i64];
        let palette = [0u32, key];

        let mut bytes = payload(
            &[nibbles(&longs, &palette, &[])],
            &[(0, vec![9; LIGHT_BYTES], vec![0; LIGHT_BYTES])],
        );

        // Patch the block record's generation word, and the light record's, to the world before this
        // one. Word 15 of the first block record, word 3 of the first light record.
        let block_generation_at = (HEADER_WORDS + BLOCK_RECORD_GENERATION_WORD) * 4;
        let light_generation_at =
            (HEADER_WORDS + SECTIONS * BLOCK_RECORD_WORDS + LIGHT_RECORD_GENERATION_WORD) * 4;
        bytes[block_generation_at..block_generation_at + 4]
            .copy_from_slice(&(TEST_GENERATION - 1).to_le_bytes());
        bytes[light_generation_at..light_generation_at + 4]
            .copy_from_slice(&(TEST_GENERATION - 1).to_le_bytes());

        let mut parsed =
            Payload::parse(&bytes, TEST_GENERATION).expect("a payload this build writes");

        assert_eq!(
            parsed.stale,
            (1 << 13) | 1,
            "the block record is the fixture's index 13 and the light record is index 0"
        );
        assert_eq!(
            parsed.present, 0,
            "a refused record is not a present section"
        );
        assert_eq!(parsed.absent, 0, "and it is not a section to forget either");
        assert!(parsed.light.is_empty(), "nor is its light applied");

        let mut world = WorldSections::default();
        let rejected = parsed.apply(&mut world, IVec3::new(0, 0, 0));

        assert_eq!(
            rejected,
            (1 << 13) | 1,
            "the caller is told which slots it refused"
        );
        assert!(
            world.is_empty(),
            "and nothing of the old world reached the cache the new one reads"
        );
    }

    /// The records a payload carries for the *current* world are applied, and the answer says so.
    #[test]
    fn a_record_from_this_world_is_applied_and_accepted() {
        let key = (7u32 << 16) | 3;
        let longs = [0x10i64];
        let palette = [0u32, key];

        let bytes = payload(&[nibbles(&longs, &palette, &[])], &[]);
        let mut parsed =
            Payload::parse(&bytes, TEST_GENERATION).expect("a payload this build writes");

        let mut world = WorldSections::default();
        let rejected = parsed.apply(&mut world, IVec3::new(0, 0, 0));

        assert_eq!(rejected, 0);
        assert_eq!(
            world
                .blocks(neighbour_offset(13))
                .map(|blocks| blocks.key(1, 0, 0)),
            Some(Some(BlockstateKey::from(key))),
            "the section the payload carried is in the cache, at the slot its index names"
        );
    }

    #[test]
    fn a_payload_that_runs_off_its_own_end_is_refused() {
        let longs = [0i64];
        let palette = [0u32, 1];

        let mut bytes = payload(&[nibbles(&longs, &palette, &[])], &[]);
        bytes.truncate(bytes.len() - 4);

        assert!(
            Payload::parse(&bytes, TEST_GENERATION).is_none(),
            "a record whose blob is not in the buffer is dropped, not read past the end"
        );
    }

    #[test]
    fn light_that_was_sent_verifies_and_light_that_was_dropped_does_not() {
        let mut world = WorldSections::default();
        let target = IVec3::new(0, 0, 0);

        assert_eq!(
            world.missing(target, 1 << 13, 1 << 13),
            (1 << 13, 1 << 13),
            "nothing is cached yet"
        );
        assert_eq!(
            world.missing(target, 0, 0),
            (0, 0),
            "an empty mask asks for nothing"
        );

        world.set_light(
            target + neighbour_offset(13),
            Arc::new(SectionLight {
                block: vec![0; LIGHT_BYTES].into_boxed_slice(),
                sky: vec![0; LIGHT_BYTES].into_boxed_slice(),
            }),
        );

        // Only the light was sent, so that is the only mask that may claim anything: a block mask
        // that says "already yours" without the block data behind it is exactly what has to resync.
        assert_eq!(world.missing(target, 0, 1 << 13), (0, 0));
        assert_eq!(world.missing(target, 1 << 13, 0), (1 << 13, 0));

        world.remove_light(target + neighbour_offset(13));
        assert_eq!(
            world.missing(target, 0, 1 << 13),
            (0, 1 << 13),
            "a section this side no longer has has to be re-sent"
        );
    }

    #[test]
    fn the_neighbour_order_is_x_fastest_then_y_then_z() {
        // The order `RenderSectionRegion` and the JVM writer use; a mismatch would put another
        // chunk's blocks in a section.
        assert_eq!(neighbour_offset(0), IVec3::new(-1, -1, -1));
        assert_eq!(neighbour_offset(1), IVec3::new(0, -1, -1));
        assert_eq!(neighbour_offset(3), IVec3::new(-1, 0, -1));
        assert_eq!(neighbour_offset(9), IVec3::new(-1, -1, 0));
        assert_eq!(neighbour_offset(13), IVec3::new(0, 0, 0));
        assert_eq!(neighbour_offset(26), IVec3::new(1, 1, 1));

        // And the middle is the slot the bake calls *about*: a reject bit for it is the answer "the
        // section you offered was not baked", which is the only one of the 27 that is about that
        // section rather than about its neighbours.
        assert_eq!(
            neighbour_offset(CENTER),
            IVec3::ZERO,
            "CENTER is the middle slot"
        );
        assert_eq!(CENTER, 13);
    }

    #[test]
    fn a_section_slot_is_found_from_a_position_one_block_outside_it() {
        // The middle section, and the six neighbours the baker reaches into.
        assert_eq!(CachedBlockstateProvider::slot(IVec3::new(0, 0, 0)), 13);
        assert_eq!(CachedBlockstateProvider::slot(IVec3::new(-1, 0, 0)), 12);
        assert_eq!(CachedBlockstateProvider::slot(IVec3::new(16, 0, 0)), 14);
        assert_eq!(CachedBlockstateProvider::slot(IVec3::new(0, -1, 0)), 10);
        assert_eq!(CachedBlockstateProvider::slot(IVec3::new(0, 16, 0)), 16);
        assert_eq!(CachedBlockstateProvider::slot(IVec3::new(0, 0, -1)), 4);
        assert_eq!(CachedBlockstateProvider::slot(IVec3::new(0, 0, 16)), 22);
    }

    /// The packed section key is Minecraft's own layout, so the JVM can use it as the key it already
    /// keeps - and the decode here is written the way `SectionPos.x/y/z` read it back.
    #[test]
    fn a_section_key_is_what_section_pos_as_long_writes() {
        // The `<< 0` is Minecraft's own `SectionPos.x`: the three decoders are the same expression
        // with different shifts, and writing it out is what keeps them comparable.
        #[allow(clippy::identity_op)]
        fn x(key: i64) -> i32 {
            (key << 0 >> 42) as i32
        }
        fn y(key: i64) -> i32 {
            (key << 44 >> 44) as i32
        }
        fn z(key: i64) -> i32 {
            (key << 22 >> 42) as i32
        }

        assert_eq!(section_key(IVec3::new(0, 0, 0)), 0);
        assert_eq!(
            section_key(IVec3::new(1, 0, 0)),
            1 << 42,
            "x is the high field"
        );
        assert_eq!(section_key(IVec3::new(0, 1, 0)), 1, "y is the low one");
        assert_eq!(
            section_key(IVec3::new(0, 0, 1)),
            1 << 20,
            "z sits between them"
        );
        assert_eq!(
            section_key(IVec3::new(-1, -1, -1)),
            -1,
            "the widest negative coordinate in every field is every bit set"
        );

        // The fields are *signed*: twenty-two bits of x and z and twenty of y, so a coordinate at the
        // top of its field is the largest positive one and not the largest unsigned value - which is
        // what the first version of this test got wrong.
        for pos in [
            IVec3::new(0, 0, 0),
            IVec3::new(-1, -1, -1),
            IVec3::new(17, -4, 299),
            IVec3::new(-1234, 250, -56),
            IVec3::new(0x1F_FFFF, 0x7_FFFF, 0x1F_FFFF),
            IVec3::new(-0x20_0000, -0x8_0000, -0x20_0000),
        ] {
            let key = section_key(pos);
            assert_eq!(
                IVec3::new(x(key), y(key), z(key)),
                pos,
                "{pos:?} did not survive the round trip the JVM reads it with"
            );

            // And the **production** decoder is those three expressions, so the occlusion graph's own
            // keys - which arrive as `SectionPos.asLong` straight off each `RenderSection` - come back
            // as the positions the arena is keyed by. A hand-written copy of the shifts in the JNI
            // function is how `y` came to be twenty-one bits wide for one revision of it.
            assert_eq!(
                section_pos(key),
                pos,
                "{pos:?} did not survive section_pos, which is what setVisibleSections decodes with"
            );
        }
    }

    /// The writer classifies **both halves of each fluid**, which is the bug that made every lava fall
    /// invisible: a fluid is two registered objects - a source and a flowing one - and `Fluid#isSame` is
    /// identity, so a *flowing* block answers `FLOWING_LAVA` and `FLOWING_LAVA.isSame(LAVA)` is false.
    ///
    /// Classified against the source alone, every flowing block came out as kind 3 - "a fluid this
    /// mesher does not know" - and the mesher skips those, so a lava lake was drawn only where it was
    /// still and a lava fall was not drawn at all. The picture was reported as "there are gaps between
    /// the stepped flowing lava in a lava fall; the flowing state is wrong on the Rust side".
    ///
    /// This is a test on the writer's *source*, in the same spirit as the ones that check every setting
    /// has a translation and every declaration has an implementation: the mistake is a missing name, and
    /// a missing name is exactly what a test can see. It cannot run the game's registries, so what it
    /// asserts is that both halves are named.
    #[test]
    fn the_writer_knows_both_halves_of_every_fluid() {
        let Some(writer) = wgpu_mc_modtree::rust_chunk_bake_kt() else {
            wgpu_mc_modtree::skip("the fluid-halves check");
            return;
        };

        // Comments dropped, because the file and this test both quote the mistake.
        let code = writer
            .lines()
            .map(str::trim_start)
            .filter(|line| {
                !line.starts_with('*') && !line.starts_with("//") && !line.starts_with("/*")
            })
            .collect::<Vec<_>>()
            .join("\n");

        for name in [
            "Fluids.WATER",
            "Fluids.FLOWING_WATER",
            "Fluids.LAVA",
            "Fluids.FLOWING_LAVA",
        ] {
            assert!(
                code.contains(name),
                "`fluidByte` no longer names {name}, so a block carrying that half of the fluid is \
                 classified as one this mesher does not know - and skipped"
            );
        }

        // And the byte's third field, which says *which* half it is: the mesher draws the two halves with
        // one set of sprites and asks the game's `isSame` question with this bit - the same object above a
        // block, a neighbour that affects the flow, what a corner averages.
        assert!(
            code.contains("0b0100_0000"),
            "`fluidByte` no longer writes the flowing bit, so the mesher cannot tell a source from the \
             flowing half of the same fluid: the surfaces and the flow directions come out as one liquid \
             where the game steps"
        );
    }
}

/// The model-variant channel: one byte per position of the section being rebuilt, sent because the
/// game's own mesher picks between a blockstate's models per *position*. See
/// [`SectionBlocks::variants`] for why the JVM makes the choice.
#[cfg(test)]
mod variant_tests {
    use super::tests::{TEST_GENERATION, nibbles, payload};
    use super::*;
    use std::sync::Arc;

    /// **The blob reaches the provider at the position the game's index counts to.** Minecraft's
    /// storage order is `(y << 8) | (z << 4) | x`, and both sides have to agree about it or a lily pad
    /// is turned to a rotation that belongs to a block four rows away.
    #[test]
    fn a_payload_carries_the_targets_variants_and_the_provider_answers_by_position() {
        let longs = [0i64];
        let palette = [0u32];
        let mut bytes = payload(&[nibbles(&longs, &palette, &[0])], &[]);

        // The blob goes where the JVM's cursor would put it - after everything already written - and is
        // named by header word 6.
        let offset = bytes.len();
        let mut blob = vec![0u8; VARIANTS_BYTES];
        blob[0] = 2;
        blob[(5 << 8) | (3 << 4) | 1] = 7;
        bytes.extend_from_slice(&blob);
        bytes[VARIANT_OFFSET_WORD * 4..VARIANT_OFFSET_WORD * 4 + 4]
            .copy_from_slice(&(offset as u32).to_le_bytes());

        let parsed = Payload::parse(&bytes, TEST_GENERATION).expect("a payload this build writes");

        assert_eq!(parsed.variants.as_ref().expect("the blob")[0], 2);

        let provider = CachedBlockstateProvider {
            blocks: parsed.blocks.map(|section| section.map(Arc::new)),
            light: Default::default(),
            variants: parsed.variants.map(Arc::from),
            air: BlockstateKey::from(0u32),
        };

        assert_eq!(provider.get_model_variant(IVec3::new(0, 0, 0)), 2);
        assert_eq!(provider.get_model_variant(IVec3::new(1, 5, 3)), 7);
        assert_eq!(
            provider.get_model_variant(IVec3::new(2, 0, 0)),
            0,
            "a position the blob left at zero is the first variant, which is what this side drew before \
             the channel existed"
        );
    }

    /// A payload with no blob names none, and every position answers the first variant - which is the
    /// whole of what this channel costs a section that has no list-variant block in it.
    #[test]
    fn a_payload_without_the_word_names_no_variants() {
        let longs = [0i64];
        let palette = [0u32];

        let bytes = payload(&[nibbles(&longs, &palette, &[0])], &[]);
        let parsed = Payload::parse(&bytes, TEST_GENERATION).expect("a payload this build writes");

        assert!(parsed.variants.is_none());

        let provider = CachedBlockstateProvider {
            blocks: parsed.blocks.map(|section| section.map(Arc::new)),
            light: Default::default(),
            variants: None,
            air: BlockstateKey::from(0u32),
        };

        assert_eq!(provider.get_model_variant(IVec3::new(0, 0, 0)), 0);
    }

    /// **A blob that runs off the end of the payload is not a payload this side drops.** The blocks in
    /// it are as good as any other's; what is lost is the section's rotations, which is a wrong angle
    /// rather than a hole in the world.
    #[test]
    fn a_blob_offset_past_the_end_is_ignored_rather_than_fatal() {
        let longs = [0i64];
        let palette = [0u32];
        let mut bytes = payload(&[nibbles(&longs, &palette, &[0])], &[]);
        let past_the_end = (bytes.len() + 64) as u32;
        bytes[VARIANT_OFFSET_WORD * 4..VARIANT_OFFSET_WORD * 4 + 4]
            .copy_from_slice(&past_the_end.to_le_bytes());

        let parsed =
            Payload::parse(&bytes, TEST_GENERATION).expect("the blocks are still readable");

        assert_eq!(parsed.present, 1 << CENTER);
        assert!(parsed.variants.is_none());
    }
}
