//! Rust implementations of minecraft concepts that are important to us.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64};

use arc_swap::ArcSwap;
use chunk::SectionStorage;
use glam::{IVec2, IVec3, ivec2};
use indexmap::map::IndexMap;
use minecraft_assets::schemas;
use minecraft_assets::schemas::blockstates::multipart::StateValue;
use parking_lot::{Mutex, RwLock};

use crate::mc::entity::{BundledEntityInstances, Entity};
use crate::mc::resource::ResourceProvider;
use crate::render::atlas::{Atlas, TextureManager};
use crate::render::pipeline::BLOCK_ATLAS;
use crate::util::BindableBuffer;
use crate::{Gpu, WmRenderer};

use self::block::ModelMesh;
use self::resource::ResourcePath;

pub mod block;
pub mod chunk;
pub mod direction;
pub mod entity;
pub mod resource;
pub mod visibility;
pub mod world_extent;
/// Take in a block name (not a [ResourcePath]!) and optionally a variant state key, e.g. "facing=north" and format it some way
/// for example, `minecraft:anvil[facing=north]` or `Block{minecraft:anvil}[facing=north]`
pub type BlockVariantFormatter = dyn Fn(&str, Option<&str>) -> String;

pub struct BlockManager {
    /// This maps block state keys to either a [VariantMesh] or a [Multipart] struct. How the keys are formatted
    /// is defined by the user of wgpu-mc. For example `Block{minecraft:anvil}[facing=west]` or `minecraft:anvil#facing=west`
    pub blocks: IndexMap<String, Block>,
    /// What every block *state* the JVM described says about the faces around it, keyed by the packed
    /// [`BlockstateKey`](block::BlockstateKey) - which is the only thing a baked section carries about
    /// a block, so it is the only key this table can use.
    ///
    /// It is filled when the block registry is built (`cacheBlockStates`, on the JNI side) from the
    /// masks `RegistryMixin` computes for each state, and it is what the terrain baker's face test
    /// reads: a neighbour's *state* decides whether a face is drawn, not its model - see
    /// `chunk::face_is_hidden`.
    pub face_flags: HashMap<u32, block::FaceFlags>,

    /// `(block index, slot in the watched list)` for each name in [`chunk::WATCHED_BLOCKS`] this
    /// registry has, resolved once when it is built.
    ///
    /// The baker counts a watched block's faces as it draws them - see [`chunk::WATCHED_BLOCKS`] for
    /// the four answers the three numbers tell apart - and this is how a block *index*, which is all a
    /// baked section carries, is recognised as one of the names somebody asked about.
    pub watched: Vec<(u16, u8)>,
}

impl BlockManager {
    pub fn new() -> Self {
        Self {
            blocks: IndexMap::new(),
            face_flags: HashMap::new(),
            watched: Vec::new(),
        }
    }
}

impl Default for BlockManager {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub enum Block {
    Multipart(Multipart),
    Variants(IndexMap<Vec<(String, StateValue)>, Vec<Arc<ModelMesh>>>),
}

impl Block {
    /// The mesh a variant key's list holds at `variant`, or the list's first entry.
    ///
    /// **`variant` is the game's choice, and it is asked for per position.** A blockstate whose variant
    /// is a list of models is `WeightedVariants` at runtime, and the entry drawn for a given block is
    /// `WeightedList#getRandomOrThrow` on a `RandomSource` seeded with `blockState.getSeed(pos)`. The
    /// JVM has both, so it sends the *index* with the section rather than this side reproducing a
    /// random number generator and a weight table - see the `variants` field of
    /// `section::SectionBlocks`.
    ///
    /// **The list is the same order the JVM counted in**: both are the blockstate's own list, and a
    /// key whose models all failed to bake is dropped whole rather than shortened, so an index cannot
    /// point at a different model than the one the game picked. An index past the end is answered with
    /// the first entry: a list the two sides disagree about the length of is a model drawn at the
    /// variant it would have had without this channel, which is a wrong angle rather than a hole.
    pub fn get_model(&self, key: u16, variant: u8) -> Option<Arc<ModelMesh>> {
        Some(match &self {
            Block::Multipart(multipart) => multipart.keys.read().get_index(key as usize)?.1.clone(),
            Block::Variants(variants) => {
                let meshes = &variants.get_index(key as usize)?.1;

                meshes
                    .get(variant as usize)
                    .or_else(|| meshes.first())?
                    .clone()
            }
        })
    }

    pub fn get_model_by_key<'a>(
        &self,
        key: impl IntoIterator<Item = (&'a str, &'a StateValue)> + Clone,
        resource_provider: &dyn ResourceProvider,
        block_atlas: &Atlas,
        //TODO use this
        _seed: u8,
    ) -> Option<(Arc<ModelMesh>, u16)> {
        let key_map: HashMap<&str, &StateValue> = key.clone().into_iter().collect();

        let key_string = key
            .clone()
            .into_iter()
            .map(|(key, value)| {
                format!(
                    "{}={}",
                    key,
                    match value {
                        StateValue::Bool(bool) =>
                            if *bool {
                                "true"
                            } else {
                                "false"
                            },
                        StateValue::String(string) => string,
                    }
                )
            })
            .collect::<Vec<String>>()
            .join(",");

        match &self {
            Block::Multipart(multipart) => {
                {
                    if let Some(full) = multipart.keys.read().get_full(&key_string) {
                        return Some((full.2.clone(), full.0 as u16));
                    }
                }

                let mesh = multipart.generate_mesh(key, resource_provider, block_atlas)?;

                let mut multipart_write = multipart.keys.write();
                multipart_write.insert(key_string, mesh.clone());

                Some((mesh, multipart_write.len() as u16 - 1))
            }
            Block::Variants(variants) => {
                // A variant whose models all failed to bake is skipped rather than indexed into:
                // `variants` can hold an empty mesh list now that a bad model is dropped instead of
                // taking the registry with it (see `bake_blocks`).
                let full =
                    variants
                        .iter()
                        .enumerate()
                        .find(|(_, (variant_key, model_meshes))| {
                            !model_meshes.is_empty()
                                && variant_key.iter().all(
                                    |(variant_property_key, variant_property_value)| {
                                        key_map
                                            .get(&variant_property_key[..])
                                            .is_some_and(|v| v == &variant_property_value)
                                    },
                                )
                        })?;

                Some((full.1.1.first()?.clone(), full.0 as u16))
            }
        }
    }
}

#[derive(Debug)]
pub struct Multipart {
    pub cases: Vec<schemas::blockstates::multipart::Case>,
    pub keys: RwLock<IndexMap<String, Arc<ModelMesh>>>,
}

impl Multipart {
    /// The mesh a multipart block's cases add up to, or `None` if one of them cannot be baked.
    ///
    /// A multipart mesh is generated on demand - once per state a player actually looks at - so a
    /// model that fails here fails in the middle of the game rather than during the block cache, and
    /// `None` lets the caller fall back to the block it uses for a state with no model.
    pub fn generate_mesh<'a>(
        &self,
        key: impl IntoIterator<Item = (&'a str, &'a schemas::blockstates::multipart::StateValue)>
        + Clone,
        resource_provider: &dyn ResourceProvider,
        block_atlas: &Atlas,
    ) -> Option<Arc<ModelMesh>> {
        let apply_variants = self.cases.iter().filter_map(|case| {
            if case.applies(key.clone()) {
                Some(case.apply.models())
            } else {
                None
            }
        });

        match ModelMesh::bake(
            apply_variants.into_iter().flatten(),
            resource_provider,
            block_atlas,
        ) {
            Ok(mesh) => Some(Arc::new(mesh)),
            Err(err) => {
                log::warn!(
                    "wgpu-mc: a multipart model could not be baked ({err:?}); the state is drawn as \
                     bedrock, the same as one with no model at all"
                );
                None
            }
        }
    }
}

pub enum MultipartOrMesh {
    Multipart(Arc<Multipart>),
    Mesh(Arc<ModelMesh>),
}

/// Multipart models are generated dynamically as they can be too complex
pub struct BlockInstance {
    pub render_settings: block::RenderSettings,
    pub block: MultipartOrMesh,
}

#[derive(Default, Clone)]
pub struct SkyState {
    pub color: [f32; 3],
    pub angle: f32,
    pub brightness: f32,
    pub star_shimmer: f32,
    pub moon_phase: i32,
}

#[derive(Default, Clone)]
pub struct RenderEffectsData {
    pub fog_start: f32,
    pub fog_end: f32,
    pub fog_shape: f32,
    pub fog_color: [f32; 4],
    pub color_modulator: [f32; 4],
    pub dimension_fog_color: [f32; 4],
}

/// What the arena's buffer is used for: written by the section feed, read as vertices and indices by
/// the graph's draws, and bound as a storage buffer by the shader that fetches a section's quads.
///
/// One constant rather than the same flags at every creation site - the pool and the buffers are made
/// together by [`Scene::set_arena_slots`] and `WmRenderer::grow_arena`, and a flag missing from one of
/// those sites is a validation error at the first draw.
///
/// **No `COPY_SRC`.** It was there because a full arena used to grow by allocating a bigger buffer and
/// copying the old one into it; growth appends another buffer now, so nothing copies an arena and the
/// flag would be permission for a thing that does not happen. See [`ARENA_BUFFERS`].
pub(crate) const ARENA_USAGE: wgpu::BufferUsages = wgpu::BufferUsages::COPY_DST
    .union(wgpu::BufferUsages::VERTEX)
    .union(wgpu::BufferUsages::STORAGE)
    .union(wgpu::BufferUsages::INDEX);

/// **The ceiling on what the arena may hold in total**, as policy rather than as a measurement.
///
/// A per-buffer limit is not a memory bound: `ARENA_BUFFERS` of them is four times whatever the device
/// will make in one, and with the arena appending rather than refusing, a player flying forward grew it
/// without an upper limit. That stopped being a bug about holes and became one about memory, so there is a
/// bound stated here and enforced where growth happens (`WmRenderer::grow_arena_if_asked`).
///
/// **It bounds what the pool holds, and that is not the same as what it asks for.** A run died in
/// `create_buffer` with this budget nowhere near being reached: the pool held 312 MB of the 5 GB, and the
/// *request* was `arena_cap_slots` slots - **seventeen gigabytes**, because that ceiling is
/// `max_buffer_size / 4` clamped to a `u32` and this device's `max_buffer_size` is past 16 GB. A budget on
/// the total cannot catch a single oversized allocation; sizing the allocation can, and that is what
/// `grow_arena_if_asked` does now. The other numbers are in [`Scene::arena_memory_budget`].
///
/// The number that actually binds is `Scene::arena_memory_budget` - this, or
/// `max_buffer_size` x [`ARENA_BUFFERS`], whichever is smaller - because this constant alone is a budget
/// for the machine it was written for. On this one the policy is the smaller and prints as:
///
/// ```text
/// wgpu-mc: the section arena: up to 4 buffer(s) of 4294967295 slot(s) (16383 MB each, the device's own
/// `max_buffer_size`), a budget of 4768 MB, of which the device's limits allow 65535 MB
/// ```
///
/// **And no total can be read off a Vulkan device.** `MemoryBudgetThresholds` - the one API for "start
/// returning OOM at a percentage of the native budget" - is implemented for **DX12 only**
/// (`wgpu-hal/src/dx12/device.rs`, reading `DXGI_MEMORY_SEGMENT_GROUP_LOCAL`); the Vulkan backend has no
/// equivalent and neither exposes the budget as a number. That is why `grow_arena_if_asked` also *catches*
/// a refused allocation instead of trusting any of this.
///
/// The number to read in the log is this one being reached:
///
/// ```text
/// Rebuilds: ... N left to Minecraft because the arena is full, ...
/// ```
///
/// That count climbing is the budget, and it is the one number that says whether the arena is holding the
/// view the player asked for.
pub const ARENA_MEMORY_BUDGET: u64 = 5_000_000_000;

/// The arena's budget **for this device**, from the two numbers a device actually gives.
///
/// `max_buffer_size` is a *per-buffer* limit, and [`ARENA_BUFFERS`] of them is the most the device's own
/// limits would let the arena hold; [`ARENA_MEMORY_BUDGET`] is the policy ceiling on top of that, so a
/// device that will make a very large buffer does not get an arena several times that by arithmetic alone.
/// On the machine this was written on the two agreed; here the policy is the smaller and is what binds.
///
/// Separate from the constant and a function of one argument so that the arithmetic can be tested - the
/// interesting cases are the two ends, where either the device or the policy decides.
pub fn arena_memory_budget(max_buffer_size: u64) -> u64 {
    max_buffer_size
        .saturating_mul(ARENA_BUFFERS as u64)
        .min(ARENA_MEMORY_BUDGET)
}

/// **How large an appended arena should be: what the view asks for, not what the device allows.**
///
/// This is the function that decides whether a full arena asks the driver for something it can have. It
/// used to be `Scene::arena_cap_slots` - the device's own `max_buffer_size` over four, clamped to a `u32` -
/// and on the machine this was found on that ceiling is `u32::MAX`, so every growth asked for **17 GB in
/// one buffer**. The driver refused, wgpu treats a refused `create_buffer` as fatal, and the process died
/// with `wgpu error: Out of Memory` and a terrain line as the last thing in the log. The same code on the
/// machine it was written for asked for 2 GB (`536870911` slots, from the historical log line) and worked,
/// which is exactly why the ceiling looked like a reasonable thing to ask for.
///
/// So the size comes from the world instead, in four numbers, each of which is a bound that can be
/// explained on its own:
///
///  * **`target`** - what [`chunk::arena_slots`] says the current render distance wants. The pool should
///    reach this and not more, and it is the number the growth exists for. This is the *size* now rather
///    than the ceiling.
///  * **`largest_section`** - a floor. An arena smaller than the largest section this session has meshed
///    cannot hold one, and holding sections is the whole point.
///  * **`cap_slots`** - the device's per-buffer ceiling, which is still a real limit and the reason the
///    request is clamped at all. On this machine it is `u32::MAX` and therefore not the binding one.
///  * **`budget`** - [`Scene::arena_memory_budget`], less what the pool already holds. The appended arena
///    is what tips the pool over it otherwise, and the budget is the only bound on the *total*.
///
/// `0` means "do not append": nothing is wanted and nothing has been meshed, so there is no size to ask
/// for and the caller should treat the refusal as one growth cannot answer.
pub fn arena_growth_slots(
    target: u32,
    pool_slots: u32,
    largest_section: u32,
    cap_slots: u32,
    budget: u64,
) -> u32 {
    // What the budget still allows, in slots. `u32::MAX` when it allows more than a `u32` can count.
    let room = budget.saturating_sub(pool_slots as u64 * 4) / 4;
    let room = room.min(u32::MAX as u64) as u32;

    target.max(largest_section).min(cap_slots).min(room)
}

/// How many arena buffers the arena may grow to.
///
/// Each is capped at the device's `max_buffer_size`, so this is the arena's ceiling in buffers - and
/// [`ARENA_MEMORY_BUDGET`] is the ceiling that actually matters, because four buffers of 1.31 GB is more
/// than a 32-chunk view needs and more than this renderer should be holding. This one exists so that a
/// device with an unusually large `max_buffer_size` cannot reach the byte budget with a single buffer
/// and then stop growing before a second would have fit.
pub const ARENA_BUFFERS: usize = 4;

/// One terrain pass's own draw buffers, and why one pair for the whole frame is not enough.
///
/// The terrain pass draws its sections with `multi_draw_indexed_indirect`, and a multi-drawn section
/// cannot be handed anything per draw except through a buffer or the indirect record itself - an
/// immediate is set once for the whole call. So each draw's section position and its slot in the arena
/// travel in [`SectionDraw`], the vertex stage reads them by `instance_index`, and the pass writes that
/// buffer once per pass.
///
/// **One pair of buffers per pass rather than one for the frame**, and the reason is where
/// `Queue::write_buffer` lands:
///
/// ```text
/// Calls to `write_buffer()` do *not* submit the transfer to the GPU immediately. They begin GPU
/// execution only on the next call to `Queue::submit()`, just before the explicitly submitted
/// commands.
/// ```
///
/// So every write a frame makes is applied at the head of that frame's submission, before every draw
/// recorded in it. A single buffer written twice in one frame - which is what the opaque group is, two
/// terrain pipelines recorded into one encoder by one `render` call - has the *second* pipeline's
/// records in it when the *first* pipeline's draws execute. They would both draw the last pass's
/// sections, out of the last pass's arena slots. Two buffers are the whole fix: pass A's writes land in
/// A's buffer and pass B's in B's, and the submission's ordering stops mattering.
///
/// The cost is the memory, which is a few hundred kilobytes per pass - see [`SECTION_DRAW_CAPACITY`].
pub const SECTION_DRAW_SLOTS: usize = 3;

/// How many draws one pass may hand to the GPU *indirectly*, which is the capacity of every slot's
/// `DrawIndexedIndirectArgs` buffer: 10,000 records of five `u32`s.
///
/// Above this the pass falls back to one `draw_indexed` per section rather than dropping anything - the
/// indirect buffer is a batching device, not a limit on what can be drawn. It is the number this buffer
/// has always been sized for.
pub const INDIRECT_DRAW_CAPACITY: usize = 10_000;

/// How many draws one pass may make at all, indirect or not, which is the capacity of every slot's
/// [`SectionDraw`] buffer.
///
/// **It is larger than [`INDIRECT_DRAW_CAPACITY`] because the fallback needs it to be.** A pass that
/// exceeds the indirect capacity still draws every section - one call at a time - and each of those
/// calls still reads its own `SectionDraw` out of this buffer, because the shader reads the section's
/// position there in both paths. A common capacity would make the fallback impossible and turn the
/// batched path's limit into a hole in the world.
///
/// Past *this* count the extra draws are dropped and said so in the log, once a second. Four thousand
/// draws is a 32-chunk view's whole surface, so the ceiling is four times a case that has not been
/// reached; it is here so that the failure is a line in a log rather than a read past the end of a
/// buffer.
pub const SECTION_DRAW_CAPACITY: usize = 1 << 14;

/// One terrain draw's arguments, as the vertex stage reads them: `section_draws[instance_index]`.
///
/// Sixteen bytes, four `u32`s, and the layout is the shader's - `SectionDraw` in `terrain.wgsl` and
/// `terrain_solid.wgsl` spells the same four members in the same order. A struct with a `vec3` in it was
/// the other shape this could have had and it is the one to avoid: `vec3<i32>` is sixteen bytes of
/// storage for twelve bytes of data, and three scalars say what the four offsets are without anyone
/// having to know the alignment rule.
///
/// **`word_base` was `@builtin(instance_index)`.** Every section's draw used to pass its arena slot as
/// the instance number, which is exactly what `first_instance` is for in an indirect record - except
/// that `instance_index` now has a job of its own: it is the index of the record being drawn, because a
/// multi-draw cannot be told anything per draw any other way.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct SectionDraw {
    /// The section's position relative to the camera's section, in sections - the number the immediate
    /// used to carry for one draw at a time. See [`Scene::camera_section_pos`].
    pub x: i32,
    pub y: i32,
    pub z: i32,
    /// The u32 slot in the arena the section's vertices start at: the old `instance_index`, and what
    /// `chunk_data` is indexed by in the shader.
    pub word_base: u32,
}

pub struct Scene {
    pub section_storage: RwLock<SectionStorage>,
    /// The section the camera is in, sent by the JVM once per frame with the matrices it belongs to.
    ///
    /// All three axes, and two things read it:
    ///
    ///  - the arena's **trim**, which only needs `x` and `z` - a vertical slice of the world is loaded
    ///    all at once, so nothing is trimmed by height;
    ///  - the terrain pass's **transform**, which is where the precision of the whole renderer is
    ///    decided. A section is drawn at `(section - camera_section) * 16 + a local position`, which is
    ///    a small number, and the view matrix carries the camera's offset *within its own section*. The
    ///    absolute position never appears, and that is the point: `x + 30000` in `f32` has a step of
    ///    0.004 blocks, which is orders of magnitude more than the depth buffer can forgive - and it is
    ///    what made the ground and the shadow lying on it fight over which of them is in front. See
    ///    `TerrainPass.sendCameraMatrices` for the other half, and `@geo_terrain` in
    ///    [`crate::render::graph`] for what is done with it.
    pub camera_section_pos: RwLock<IVec3>,
    /// The camera section the arena was last trimmed against.
    ///
    /// Trimming walks every section the arena holds, so it happens when the camera crosses into
    /// another section rather than once per frame: a frame where the camera stayed put has nothing to
    /// free, and the walk would cost more than the frames it ran on.
    pub trimmed_section_pos: RwLock<IVec2>,
    /// **The arena's buffers, one entry per pool in `SectionStorage`.**
    ///
    /// A list because one buffer is not enough: each is capped at the device's `max_buffer_size` - 1.31 GB
    /// on the machine this was measured on - while a 32-chunk view wants about 2.6 GB of meshed sections.
    /// With one buffer the arena was half the size it needed to be, and every section it could not hold
    /// was one neither renderer drew. See [`ARENA_BUFFERS`] for the limit.
    ///
    /// Index `i` here is `SectionRanges::buffer == i`, and the two lists are kept in step by
    /// `WmRenderer::grow_arena` and `SectionStorage::grow_pool` - the only things that add either. The
    /// list is replaced whole rather than mutated, so a frame still drawing from it keeps its own
    /// reference; index 0 is the buffer a world starts with.
    pub chunk_buffers: ArcSwap<Vec<Arc<BindableBuffer>>>,

    /// The pool size a section did not fit into asked to be grown to, or zero.
    ///
    /// A request rather than a growth, because the two happen in different places: the refusal is
    /// noticed while a frame's bakes are being written into the buffer that is bound right now, and
    /// growing has to happen before that load - a drain that swapped buffers halfway through would
    /// write the rest of its sections into the wrong one. See `WmRenderer::grow_arena`.
    pub pending_arena_growth: AtomicU32,

    /// The largest pool this device can have in **one** buffer, in u32 slots: `max_buffer_size / 4`,
    /// clamped to what a `u32` can count.
    ///
    /// Read once, from the device the arena's buffer is created on. What it caps is a single *appended*
    /// arena - growth is sized by the shortfall and never asks for more than this - and a ceiling rather
    /// than a policy: growth only happens because a section did not fit, so a session that never refuses
    /// never reaches it.
    ///
    /// **`u32::MAX` is a real value here and it was a real bug.** `max_buffer_size` is past 16 GB on the
    /// device this was found on, so the clamp decides and the ceiling is 17 GB - which every growth used to
    /// request in one buffer, because an appended arena was created at this size "because there is no
    /// reason for one to be smaller". The driver refused, and wgpu treats a refused `create_buffer` as
    /// fatal. See `WmRenderer::grow_arena_if_asked`: it is a *cap* now, not a size.
    pub arena_cap_slots: u32,

    /// **How much video memory this arena is allowed to use, on this device.** See
    /// [`arena_memory_budget`] for the arithmetic and [`ARENA_MEMORY_BUDGET`] for why a total cannot be
    /// read off a Vulkan device at all.
    ///
    /// Read beside `max_buffer_size` rather than derived where it is used, because growth happens on the
    /// render thread and the plan should not be recomputed - or drift - per refusal burst. What enforces
    /// it is `WmRenderer::grow_arena_if_asked`.
    pub arena_memory_budget: u64,

    /// **The draw arguments, one pair of buffers per terrain pass.** See [`SECTION_DRAW_SLOTS`] for why
    /// there is a pair per pass rather than one for the frame, and [`SectionDraw`] for what is in them.
    ///
    /// `section_draws[i]` is the storage buffer the vertex stage reads a draw's section position and
    /// arena slot out of, and `indirect_buffers[i]` is the `DrawIndexedIndirectArgs` array the batched
    /// path draws from. Index `i` is a *slot*, not a pipeline: `BoundPipeline::draw_slot` says which
    /// pass owns which, and two passes may not share one - which is the whole of what the slot
    /// assignment has to get right.
    pub section_draws: Vec<Arc<BindableBuffer>>,
    pub indirect_buffers: Vec<Arc<wgpu::Buffer>>,

    pub entity_instances: Mutex<HashMap<String, BundledEntityInstances>>,
    pub sky_state: ArcSwap<SkyState>,

    pub render_effects: ArcSwap<RenderEffectsData>,

    pub depth_texture: RwLock<wgpu::Texture>,

    /// The sections Minecraft's own occlusion culling says are visible this frame, or `None` before the
    /// JVM has ever sent a list.
    ///
    /// **The reason this exists is that a frustum is not occlusion culling.** The terrain pass culled
    /// its sections against the camera's frustum, which is what the game's own renderer does *first* and
    /// what its [`SectionOcclusionGraph`] then throws most of away: the graph walks outward from the
    /// camera through the sections it can actually see - a section is reached only through a neighbour
    /// whose face toward it is not fully opaque - so the ~2,000 sections inside the frustum of a normal
    /// view become the few hundred that are not behind a hill. Drawing the frustum's set means submitting
    /// every section in a cave system, a ravine or a forest floor, and the vertex stage then transforms
    /// geometry the depth test discards. `visibleSections` is that graph's answer, once per frame.
    ///
    /// `None` and `Some(empty)` are **different**, which is why this is an `Option`: `None` is "the JVM
    /// has not told this side anything", where a frustum is the best answer there is, and `Some(empty)`
    /// is "the game looked and saw nothing", where drawing the arena would be drawing exactly what the
    /// game decided not to. A session with the terrain switch off never sends either.
    ///
    /// The positions are absolute section coordinates, the same space [`Scene::camera_section_pos`] and
    /// the arena's keys are in. See `set_visible_sections`.
    ///
    /// [`SectionOcclusionGraph`]: https://minecraft.wiki/w/Occlusion_culling
    pub visible_sections: RwLock<Option<HashSet<IVec3>>>,

    /// How many times the list above has been replaced. See [`Scene::set_visible_sections`].
    ///
    /// **The terrain frame cache keys on this**, because the list is one of the things the gather reads:
    /// a new list is a gather that has to run again. A counter rather than the list itself, because
    /// comparing two `HashSet`s is the same walk as the gather it would be deciding about - see
    /// `TerrainFrameKey` in `render/graph.rs`.
    ///
    /// It is a revision and not a "dirty" flag, so a cache that is several revisions behind cannot be
    /// mistaken for a current one; and it is bumped by the only writer rather than by the reader, so a
    /// list that is set twice in one frame invalidates twice.
    pub visible_sections_revision: AtomicU64,

    /// **The sections the arena has taken over since the list above was last replaced.**
    ///
    /// This is the only thing that tells a *stale* answer apart from an *occluded* one, and without it
    /// the gather has to choose between two wrong answers. `visibleSections` is refilled only when the
    /// camera has turned by more than two degrees or the game's occlusion graph reports a change
    /// (`LevelRenderer#applyFrustum`), so between refills it is a snapshot - and a section this side has
    /// just taken over is exactly what it has not caught up with. Minecraft's mesh for that section has
    /// already been dropped (`SectionCompilerMixin` does that when the bake is taken), so reading "not
    /// named" as "occluded" for it leaves a section neither renderer draws: a 16x16x16 hole, appearing
    /// while terrain streams in. See the README's "the occlusion list is a snapshot".
    ///
    /// A position lands here when it enters the arena - the publish in the section drain, which is where
    /// this side's claim on it becomes true - and the set is **emptied whenever a list arrives**, which
    /// is exactly "the game's answer has caught up". A section in it that the list does not name is
    /// answered by the frustum instead; every other unnamed section is still culled, so the occlusion
    /// culling survives for the steady state, where all of its value is.
    pub sections_since_the_list: RwLock<HashSet<IVec3>>,
}

impl Scene {
    /// Replaces the list of sections the game's own occlusion graph says are visible.
    ///
    /// **The one writer of [`Scene::visible_sections`]**, so that the revision beside it cannot fall out
    /// of step with the list: the two are one fact - what the gather reads - and a second place that
    /// wrote the list without bumping the revision would be a frame drawn from a stale one.
    ///
    /// **The set is kept, and the positions arrive as they are read.** The JVM sends a whole list every
    /// time the game rebuilds it, so the set that holds it is cleared and refilled rather than replaced -
    /// `HashSet::clear` keeps the table's capacity and its control bytes, which is the allocation and the
    /// growth this does not pay twice - and the section positions are taken as an iterator, so the only
    /// thing that happens to a key between the JVM's array and the set is the hashing that a set costs
    /// anyway. Building a set on the JVM's side of this call only to move it in here was one allocation
    /// and one extra pass over the list per rebuild.
    pub fn set_visible_sections<I>(&self, visible: I)
    where
        I: IntoIterator<Item = IVec3>,
    {
        // **And the list has caught up**, so nothing is "too new to have been seen" any more: a section
        // this side took over before this call is the list's business like any other, and if the game's
        // graph still does not name it, it is occluded rather than unseen. See
        // [`Scene::sections_since_the_list`].
        self.sections_since_the_list.write().clear();

        let mut visible = visible.into_iter();

        // The guard is dropped before the revision moves, which is the order that carries the guarantee: a
        // reader decides whether to use its cached gather from the revision and reads the list afterwards, so
        // the new list has to be *visible* before the revision that says it is there. Dropping the guard
        // first makes exactly that ordering true - the list is published, and only then does the count move.
        {
            let mut slot = self.visible_sections.write();

            match slot.as_mut() {
                Some(set) => {
                    set.clear();
                    set.extend(&mut visible);
                }
                // The first list of a session, and the only one that has to build the set.
                None => *slot = Some(visible.collect()),
            }
        }

        self.visible_sections_revision
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Records that the arena has taken one section over. See [`Scene::sections_since_the_list`].
    ///
    /// Called from the section drain at the publish, which is the moment the claim becomes true: before
    /// it the section is not in the arena and the gather cannot reach it, and after it the section is
    /// this side's while the game's list may not have heard of it yet.
    pub fn note_section_taken(&self, pos: IVec3) {
        self.sections_since_the_list.write().insert(pos);
    }

    /// The revision [`Scene::set_visible_sections`] last left behind.
    pub fn visible_sections_revision(&self) -> u64 {
        self.visible_sections_revision
            .load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn new(wm: &WmRenderer, framebuffer_size: wgpu::Extent3d) -> Self {
        // Sized for a large render distance up front, so that a session which never reports one still
        // works, and resized to the one the game reports with [`Scene::set_arena_slots`] - which
        // happens when a world is joined, before anything has been baked. A buffer that is too large
        // is video memory; one that is too small is sections that cannot be baked.
        //
        // The two buffers a terrain pass draws out of, allocated for every slot up front: a slot is
        // claimed when the graph builds its pipelines and a buffer created then would have to be
        // created on a rebuild, which is a resource the frame in flight may still be reading. See
        // `SECTION_DRAW_SLOTS`.
        //
        // `min_binding_size` is left open in the layout, so the whole of each is bound and the shader
        // indexes it by `instance_index`; the arena's storage buffer is declared the same way.
        let section_draws = (0..SECTION_DRAW_SLOTS)
            .map(|_| {
                Arc::new(BindableBuffer::new_deferred(
                    wm,
                    (SECTION_DRAW_CAPACITY * std::mem::size_of::<SectionDraw>()) as u64,
                    wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::STORAGE,
                    "section_draws",
                ))
            })
            .collect::<Vec<_>>();

        let indirect_buffers = (0..SECTION_DRAW_SLOTS)
            .map(|_| {
                Arc::new(wm.gpu.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("terrain indirect draw arguments"),
                    size: (INDIRECT_DRAW_CAPACITY
                        * std::mem::size_of::<wgpu::util::DrawIndexedIndirectArgs>())
                        as u64,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::INDIRECT,
                    mapped_at_creation: false,
                }))
            })
            .collect::<Vec<_>>();

        Self {
            section_storage: RwLock::new(SectionStorage::new(crate::mc::chunk::ARENA_SLOTS)),
            camera_section_pos: RwLock::new(IVec3::ZERO),
            trimmed_section_pos: RwLock::new(ivec2(i32::MAX, i32::MAX)),
            chunk_buffers: ArcSwap::from_pointee(vec![Arc::new(BindableBuffer::new_deferred(
                wm,
                crate::mc::chunk::ARENA_SLOTS as u64 * 4,
                ARENA_USAGE,
                "ssbo",
            ))]),
            pending_arena_growth: AtomicU32::new(0),
            arena_cap_slots: (wm.gpu.device.limits().max_buffer_size / 4).min(u32::MAX as u64)
                as u32,
            arena_memory_budget: arena_memory_budget(wm.gpu.device.limits().max_buffer_size),
            indirect_buffers,
            section_draws,

            entity_instances: Default::default(),
            sky_state: Default::default(),
            render_effects: Default::default(),
            depth_texture: wm
                .gpu
                .device
                .create_texture(&wgpu::TextureDescriptor {
                    label: None,
                    size: framebuffer_size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Depth32Float,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    view_formats: &[],
                })
                .into(),

            // `None` rather than an empty set: nothing has been sent yet, and an empty set means "the
            // game looked and saw nothing", which would draw no terrain at all. See the field.
            visible_sections: RwLock::new(None),
            visible_sections_revision: AtomicU64::new(0),
            sections_since_the_list: RwLock::new(HashSet::new()),
        }
    }

    /// Sizes the arena to a pool of `slots` u32 slots, and answers whether it took.
    ///
    /// The pool and the buffer are one thing and are therefore set in one place: the pool is what the
    /// allocator hands ranges out of, the buffer is where those ranges live, and a pool larger than
    /// the buffer is a write past the end of it. Sizing them apart - which is what this replaced - is
    /// how the buffer came to be fixed at the largest render distance while the pool followed the
    /// game's: the memory was spent either way, and only the *capacity* moved.
    ///
    /// It only takes while the arena is empty, for the reason `SectionStorage::set_pool` gives: a
    /// range allocator cannot be resized under live allocations, and the bytes in the buffer are the
    /// only copy of the geometry that is on screen. `false` is a report that arrived after the world
    /// had been meshed - the arena keeps the size it was built with, and the caller says so.
    pub fn set_arena_slots(&self, wm: &WmRenderer, slots: u32) -> bool {
        if !self.section_storage.write().set_pool(slots) {
            return false;
        }

        // The old buffers go when the last frame that recorded a draw from them is done with them: the
        // pass holds its own reference, and this drops ours.
        self.chunk_buffers
            .store(Arc::new(vec![Arc::new(BindableBuffer::new_deferred(
                wm,
                slots as u64 * 4,
                ARENA_USAGE,
                "ssbo",
            ))]));

        true
    }

    pub fn resize_depth_texture(&self, wm: &WmRenderer, width: u32, height: u32) {
        self.depth_texture.read().destroy();
        *self.depth_texture.write() = wm.gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth32Float,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        })
    }
}

/// Minecraft-specific state and data structures go in here
pub struct MinecraftState {
    pub block_manager: RwLock<BlockManager>,

    pub entity_models: RwLock<HashMap<String, Arc<Entity>>>,

    pub resource_provider: Arc<dyn ResourceProvider>,
    pub texture_manager: TextureManager,

    pub animated_block_buffer: ArcSwap<Option<wgpu::Buffer>>,
    pub animated_block_bind_group: ArcSwap<Option<wgpu::BindGroup>>,
}

impl MinecraftState {
    #[must_use]
    pub fn new(wgpu_state: &Gpu, resource_provider: Arc<dyn ResourceProvider>) -> Self {
        MinecraftState {
            entity_models: RwLock::new(HashMap::new()),

            texture_manager: TextureManager::new(wgpu_state),

            block_manager: RwLock::new(BlockManager::new()),
            resource_provider,

            animated_block_buffer: ArcSwap::new(Arc::new(None)),
            animated_block_bind_group: ArcSwap::new(Arc::new(None)),
        }
    }

    /// Bake blocks from their blockstates
    ///
    /// # Example
    ///
    ///```ignore
    /// # use wgpu_mc::mc::MinecraftState;
    /// # use wgpu_mc::mc::resource::ResourcePath;
    /// # use wgpu_mc::WmRenderer;
    ///
    /// # let minecraft_state: MinecraftState;
    /// # let wm: WmRenderer;
    ///
    /// minecraft_state.bake_blocks(
    ///     &wm,
    ///     [("minecraft:anvil", &ResourcePath("minecraft:blockstates/anvil.json".into()))]
    /// );
    /// ```
    pub fn bake_blocks<'a>(
        &self,
        wm: &WmRenderer,
        block_states: impl IntoIterator<Item = (impl AsRef<str>, &'a ResourcePath)>,
    ) {
        let mut block_manager = self.block_manager.write();
        let atlases = self.texture_manager.atlases.read();
        // Not a panic: nothing registers a block atlas in this build yet - the Fabric module's atlas
        // loader was never ported - and a `#[jni_fn]` frame cannot unwind, so a missing atlas used to
        // be the JVM aborting on the block cache thread. Say what is missing and leave the registry
        // empty; everything that needs block models (the Rust terrain baker) will find it empty and
        // do nothing.
        let Some(block_atlas) = atlases.get(BLOCK_ATLAS) else {
            log::error!(
                "wgpu-mc: no block atlas is registered, so block models cannot be baked - the Rust \
                 terrain path needs one (see `bake_blocks`)"
            );
            return;
        };

        //Figure out which block models there are
        block_states
            .into_iter()
            .for_each(|(block_name, block_state)| {
                // One missing or malformed blockstate file must not take the game down: this runs on
                // a background thread whose panics abort the JVM.
                let Some(json) = self.resource_provider.get_string(block_state) else {
                    log::warn!(
                        "wgpu-mc: {} has no blockstate file; skipping it",
                        block_state.0
                    );
                    return;
                };

                let blockstates: schemas::BlockStates = match serde_json::from_str(&json) {
                    Ok(blockstates) => blockstates,
                    Err(err) => {
                        log::warn!("wgpu-mc: {} could not be read: {err}", block_state.0);
                        return;
                    }
                };

                let block = match &blockstates {
                    schemas::BlockStates::Variants { variants } => {
                        let meshes: IndexMap<Vec<(String, StateValue)>, Vec<Arc<ModelMesh>>> =
                            variants
                                .iter()
                                .filter_map(|(variant_id, variant)| {
                                    let key_iter = if !variant_id.is_empty() {
                                        variant_id
                                            .split(',')
                                            .filter_map(|kv_pair| {
                                                let mut split = kv_pair.split('=');
                                                if kv_pair.is_empty() {
                                                    return None;
                                                }

                                                Some((
                                                    split.next().unwrap().to_string(),
                                                    match split.next().unwrap() {
                                                        "true" => StateValue::Bool(true),
                                                        "false" => StateValue::Bool(false),
                                                        other => StateValue::String(other.into()),
                                                    },
                                                ))
                                            })
                                            .collect::<Vec<_>>()
                                    } else {
                                        vec![]
                                    };

                                    // A variant whose model cannot be baked is dropped rather than
                                    // unwrapped: one bad model in one blockstate file used to abort
                                    // the whole registry, which is the difference between a block
                                    // that does not draw and no terrain at all. The block itself
                                    // stays registered, with the variants that did bake.
                                    let mut meshes = Vec::with_capacity(variant.models().len());
                                    for variation in variant.models() {
                                        match ModelMesh::bake(
                                            std::slice::from_ref(variation),
                                            &*self.resource_provider,
                                            block_atlas,
                                        ) {
                                            Ok(mesh) => meshes.push(Arc::new(mesh)),
                                            Err(err) => {
                                                log::warn!(
                                                    "wgpu-mc: {} variant {variant_id} could not be \
                                                     baked ({err:?}); skipping it",
                                                    block_name.as_ref()
                                                );
                                                return None;
                                            }
                                        }
                                    }

                                    Some((key_iter, meshes))
                                })
                                .collect();

                        Block::Variants(meshes)
                    }
                    schemas::BlockStates::Multipart { cases } => Block::Multipart(Multipart {
                        cases: cases.clone(),
                        keys: RwLock::new(IndexMap::new()),
                    }),
                };

                block_manager
                    .blocks
                    .insert(String::from(block_name.as_ref()), block);
            });

        // The fluids' own textures. Lava and water have no block model to name them, so this is the
        // only place they can enter the atlas - and the fluid mesher draws them, so without this the
        // fluids would be baked untextured. Allocated before the upload, which is what puts them in
        // the texture the pass samples.
        for name in chunk::FLUID_TEXTURES {
            // Two names for the same texture, and they are not interchangeable: the atlas is keyed by
            // the name a *model* would use (`minecraft:block/lava_still`), and the resource provider by
            // the *file* (`minecraft:textures/block/lava_still.png`). Asking the provider for the first
            // is a `None` on every machine - which is what both fluids reported, "cannot be read",
            // while the files sat in the jar the whole time - and a fluid whose sprite never reaches the
            // atlas is a fluid the mesher leaves out. `block.rs` makes the same conversion for the
            // textures its models name.
            let path = ResourcePath(format!("minecraft:block/{name}"));

            if block_atlas.uv_map.read().contains_key(&path) {
                continue;
            }

            let texture_path = path.prepend("textures/").append(".png");

            let Some(bytes) = self.resource_provider.get_bytes(&texture_path) else {
                log::warn!("wgpu-mc: {texture_path} cannot be read, so that fluid is not drawn");
                continue;
            };

            block_atlas.allocate([(&path, &bytes)], &*self.resource_provider);
        }

        block_atlas.upload(wm);
    }
}

/// The one thing a blockstate's variant *list* is picked with. See [`Block::get_model`] for why the
/// index comes from the JVM rather than from a random number generator on this side.
#[cfg(test)]
mod variant_pick_tests {
    use super::*;
    use crate::mc::block::ModelMesh;

    fn mesh(cull: u8) -> ModelMesh {
        ModelMesh {
            north: vec![],
            south: vec![],
            west: vec![],
            east: vec![],
            up: vec![],
            down: vec![],
            any: vec![],
            cull,
            ambient_occlusion: None,
        }
    }

    #[test]
    fn a_list_is_picked_by_the_index_the_jvm_counted() {
        let mut variants: IndexMap<Vec<(String, StateValue)>, Vec<Arc<ModelMesh>>> =
            IndexMap::new();
        variants.insert(
            vec![],
            vec![Arc::new(mesh(1)), Arc::new(mesh(2)), Arc::new(mesh(3))],
        );

        let block = Block::Variants(variants);

        for (index, expected) in [(0u8, 1u8), (1, 2), (2, 3)] {
            assert_eq!(
                block.get_model(0, index).expect("a model").cull,
                expected,
                "variant {index} is entry {expected} of the list"
            );
        }

        // **Past the end is the first entry, not a hole.** A list the two sides disagree about the
        // length of - a blockstate this loader dropped an entry of, a mod's list - would otherwise be a
        // face that is not drawn at all, and a wrong angle is the smaller failure of the two.
        assert_eq!(block.get_model(0, 9).expect("the first entry").cull, 1);

        // And a key no state selected has no model, which is the same answer as before this existed.
        assert!(block.get_model(4, 0).is_none());
    }
}
