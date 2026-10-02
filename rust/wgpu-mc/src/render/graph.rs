use glam::IVec3;
use linked_hash_map::LinkedHashMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use treeculler::{AABB, BVol, Frustum, Vec3};
use wgpu::util::{BufferInitDescriptor, DeviceExt};

use wgpu::{
    BufferUsages, Color, IndexFormat, LoadOp, Operations, RenderPassColorAttachment,
    RenderPassDepthStencilAttachment, RenderPassDescriptor, SamplerBindingType, ShaderStages,
    StoreOp,
};

use crate::WmRenderer;
use crate::mc::Scene;
use crate::mc::SectionDraw;
use crate::mc::chunk::RenderLayer;
use crate::mc::chunk::SectionStorage;
use crate::mc::entity::InstanceVertex;
use crate::mc::resource::ResourcePath;
use crate::render::entity::EntityVertex;
use crate::render::pipeline::{BLOCK_ATLAS, QuadVertex};
use crate::render::section_graph;
use crate::render::section_graph::SectionSource;
use crate::render::section_graph::Visibility;
use crate::render::shader::WgslShader;
use crate::render::shaderpack::{
    BindGroupDef, LonghandResourceConfig, PipelineConfig, ShaderPackConfig,
    ShorthandResourceConfig, TypeResourceConfig,
};
use crate::render::sky::{SkyVertex, SunMoonVertex};
use crate::texture::TextureAndView;
use crate::util::WmArena;

/// What the terrain pass has drawn since the last report, and when that was.
///
/// Diagnostics, and the numbers the terrain path is checked with: "the graph pass ran and drew the
/// arena" is a count here rather than a screenshot, and a frustum built from a matrix convention the
/// culler does not share shows up as everything culled rather than as missing terrain.
/// What the game's own occlusion culling says about one section. See [`RenderGraph::section_visibility`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SectionVisibility {
    /// A named section: submit it.
    Draw,
    /// The game looked and did not name this one: do not.
    OutOfSight,
    /// Nothing has been sent, or the list was sent before this section was here, so the frustum is the
    /// best answer there is.
    AskTheFrustum,
}

/// How many sections asked the frustum because the game's occlusion list was too old to have judged
/// them. See [`RenderGraph::section_visibility`] and `Scene::sections_since_the_list`.
///
/// **This is the number that says the snapshot fix is doing something.** It is not a fault count and
/// not a "these were drawn" count - a section that answers `AskTheFrustum` this way still has to survive
/// the frustum test - it is "the list had no answer for this one, so the frustum was asked instead". A
/// run that streams terrain in shows it moving; a settled world shows zero, because nothing is being
/// taken over and the list has judged everything the arena holds.
static SECTIONS_TOO_NEW: AtomicU64 = AtomicU64::new(0);

/// One section that survived the gather: where it is, what the arena holds for each layer, and how far
/// away it is.
///
/// The two ranges per layer are the ones a draw needs - the index range to draw and the vertex range to
/// draw it *from*, which is the section's slot in the arena - and they are carried here rather than
/// looked up again so that the draw loops never touch the arena's `HashMap`.
#[derive(Debug, Clone)]
struct VisibleSection {
    /// The section's position relative to the camera's section: the number the push constant carries,
    /// and the origin every vertex of the section is placed against. See the terrain pass.
    relative_position: glam::IVec3,
    /// Per [`RenderLayer`], the `(indices, vertices)` the arena holds, or `None` for no layer.
    ///
    /// The third number is **which arena buffer** those two ranges are offsets into. It travels with
    /// them because the draw loop has to rebind the arena - as the index buffer and as the storage buffer
    /// the shader reads vertices from - whenever it changes, and a section drawn against the wrong arena
    /// is a section of somebody else's geometry. See `SectionRanges::buffer`.
    ranges: [Option<crate::mc::chunk::DrawnLayer>; 3],
    /// Distance from the camera's section to this one, squared, in sections. See the sort.
    distance_squared: f32,
}

/// One draw a terrain pass is about to make: the arena it reads, the indices to draw, and which
/// [`SectionDraw`] describes it.
///
/// The gather produces *sections* and a draw needs *draws* - a section with two layers is two of them,
/// and the record a draw reads is per draw rather than per section - so the pass builds this list once
/// and both draw paths below walk it. `instance` is the index of the matching record, which is what the
/// vertex stage receives as `instance_index`: through `first_instance` of an indirect record on the
/// batched path, and through the instance range of a plain `draw_indexed` on the other.
///
/// Which layer a draw belongs to is not here: the list is built layer by layer and the pass keeps the
/// span of each layer's run instead, so a layer's draws are a contiguous slice and the layer is already
/// known where they are walked. See `PassDraws::spans`.
#[derive(Debug)]
struct DrawCall {
    /// Which arena buffer the section lives in. A batched call may only span one arena, because the
    /// index buffer and the bind group are per arena and a `multi_draw` has one of each.
    arena: u32,
    index_range: std::ops::Range<u32>,
    instance: u32,
}

/// `DrawIndexedIndirectArgs` in bytes: what an indirect draw's offset into its buffer is measured in.
const INDIRECT_RECORD_SIZE: u64 = std::mem::size_of::<wgpu::util::DrawIndexedIndirectArgs>() as u64;

/// The terrain pass' immediate, **as a struct rather than as a run of byte offsets**.
///
/// The same members in the same order as the shader's `SectionPosition`, which is the whole of the
/// contract: this side has to write exactly the bytes that struct declares, at the offsets it declares
/// them at. It was a `[u8; 28]` with a `constants[..4]`, `constants[4..8]`, ... table, and this is what
/// that cost - **the level-of-detail bias was dropped from it during the change that removed the
/// section's position from the immediate**, and nothing said so: every member is four bytes and most
/// of them are `f32`, so a field that is never written is a zero and a field written one slot early is
/// another field's value. Both draw a picture. `#[repr(C)]` and `bytemuck` make the offsets
/// `size_of`'s and `align_of`'s business, and the test beside this one pins every member by name
/// against the offsets naga reads out of the shipped shaders.
///
/// See the shader's `SectionPosition` for what each one means and `SectionDraw` for the three that are
/// *not* here - the section's position, which a `multi_draw` cannot be told per draw and which travels
/// in the storage buffer instead.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
struct SectionPositionImmediate {
    /// `0.5` for the cutout layer, `0.01` for the translucent one, and nothing at all for the solid
    /// one's shader, which has no test to make. Set per layer rather than per pass.
    alpha_cutout: f32,
    /// The atlas level-of-detail bias. Read from the setting at the top of every pass, so it is as
    /// current as the frame it is drawn in - see `atlas_lod_bias`.
    lod_bias: f32,
    /// Half a texel of each atlas. **Zero**, and the experiment that made it worth a field is over: a
    /// half-texel shift is a picture shifted by half a texel, which is what a player reported. The two
    /// fields stay because the shader reads them and a constant of zero is the honest way to say so.
    half_texel_ours: f32,
    half_texel_game: f32,
    /// One texel of each atlas, as a fraction of it, which is what the magnification test compares the
    /// coordinates' screen-space derivative against.
    texel_ours: f32,
    texel_game: f32,
    /// Whether the game's `textureFiltering` option is RGSS. A `u32` rather than a `bool`, because the
    /// shader declares a `u32` and Rust's `bool` is one byte where WGSL's is four.
    use_rgss: u32,
}

/// **One frame's terrain work, so the frame's three passes gather, build and upload once between them.**
///
/// The frame is three terrain passes - `terrain_solid`, `terrain` and `translucent_terrain` - and each of
/// them used to walk the whole arena, cull it against the frustum, build its own records and upload them.
/// **The walk and the cull do not depend on which layer is being drawn**, so the three passes produced the
/// same list three times; the first pass of a frame now does it for every terrain pipeline the graph
/// holds, and the other two draw from what it left.
///
/// A field on the graph and a `RefCell` rather than a local, for the reason the list it replaced gave: the
/// vectors keep their capacity between frames, so a few hundred sections a frame is not a few hundred
/// allocations. The graph is shared but only the render thread draws, and a `RefCell` borrow that
/// overlapped would panic rather than corrupt - which is the right failure for scratch.
#[derive(Debug, Default)]
struct TerrainFrame {
    /// What the cached work was built from, or `None` before anything has been built. See
    /// [`TerrainFrameKey`].
    key: Option<TerrainFrameKey>,
    /// The gather's list, in the order the passes that write depth draw it in.
    ///
    /// **Storage order, which is not an order at all - and that is the point.** It is what the batched
    /// path needs: `multi_draw_indexed_indirect` reads one index buffer and one arena bind group per
    /// call, so a call may only span draws of one arena, and the runs are as long as the list keeps
    /// sections of one arena together. Sorting this list would interleave arenas by distance and turn a
    /// handful of calls into one per section or worse. See [`TerrainFrame::sorted`].
    visible: Vec<VisibleSection>,
    /// The same sections **far to near**, for the pass that blends.
    ///
    /// Two lists and not one because the two orders are for two different things and only one of them is
    /// free: a blending layer mixes with what is already in the target, so its order *is* the picture,
    /// while the opaque layers write depth and are ordered by the depth test - but they are 90% of the
    /// draws and their order decides how many batched calls there are. One list would have to pick, and
    /// either choice is a real loss. Built once a frame, which is what the per-pass sort used to be.
    sorted: Vec<VisibleSection>,
    /// One entry per **pass slot**, parallel to `Scene::section_draws` and `Scene::indirect_buffers`.
    ///
    /// Per pass and not per layer because a pass is what owns a buffer and an indirect argument array:
    /// the records a pass draws have to be in the buffer that pass binds, and two passes drawing the same
    /// layer would otherwise write over each other.
    passes: Vec<PassDraws>,
    /// **The gather-level counts, which belong to the frame rather than to a pass.** How many sections
    /// the game's list did not name, how many the frustum rejected, and how many had no layer at all.
    /// They were per pass, which is three walks deep and therefore three times the same number; see
    /// `report_terrain_pass` for what the line says now.
    culled: u64,
    out_of_sight: u64,
}

impl TerrainFrame {
    /// Whether the work about to be built belongs to a **new frame** rather than to a second view inside
    /// the frame already built.
    ///
    /// The distinction decides whether every pass is rebuilt or only the ones that have not drawn yet, so
    /// getting it wrong is not a slow frame: read as "new" when it is not, a pass that has already
    /// recorded its draws has the buffer under it rewritten; read as "not new" when it is, the frame
    /// counter would never clear [`PassDraws::drawn`] and every frame after the first would draw the
    /// first frame's list - one frame's world, frozen, which is what the first version of this did.
    fn starts_a_new_frame(&self, incoming: &TerrainFrameKey) -> bool {
        self.key.map(|built| built.frame) != Some(incoming.frame)
    }
}

/// One terrain pass's share of a frame: the records it uploads and the draws that read them.
#[derive(Debug, Default)]
struct PassDraws {
    records: Vec<SectionDraw>,
    calls: Vec<DrawCall>,
    /// Per layer this pass draws: `(layer, first call, one past the last call)`. The calls of one layer
    /// are a contiguous run of `calls`, which is what lets a layer be handed to `multi_draw` in segments.
    spans: Vec<(usize, usize, usize)>,
    /// Sections the arena held nothing in for each layer, this frame.
    empty: [u64; 3],
    /// **Whether this pass draws in batches**, decided when the records were built rather than when they
    /// are drawn. The switch behind it is a setting, and a setting that moved between the build and the
    /// draw would pick a different loop from the one the indirect arguments were written for.
    batched: bool,
    /// **Whether this pass has already drawn from these records.** It is what makes a rebuild in the
    /// middle of a frame safe rather than corrupting: a pass whose draws are recorded reads its records
    /// out of the buffer at submission time, so rewriting that buffer after the fact would leave the
    /// recorded draw counts and offsets describing a list that is no longer there. A rebuild therefore
    /// leaves a drawn pass alone - its records are still in the arena's own terms, because a section's
    /// ranges are not reused inside the frame they were recorded in (`SectionStorage` defers a freed
    /// range for as many frames as are in flight).
    drawn: bool,
}

/// What [`TerrainFrame`] was built from: **everything the gather reads, and nothing else.**
///
/// Exact rather than a frame number, so a cache hit is a list a fresh gather would have produced
/// character for character - and so that a frame in which the camera has not moved, the world has not
/// been re-meshed and the game's occlusion list has not changed reuses the list instead of rebuilding the
/// same one. The frame counter is the part that cannot be inferred from the world: a section's *ranges*
/// stop being valid after the frames that may still draw them, so a list may not outlive its frame even
/// when everything above it is unchanged.
///
/// Two `f32`s that are equal are equal bit for bit here, and a NaN anywhere fails the comparison and
/// rebuilds - which is the safe direction.
#[derive(Clone, Copy, PartialEq, Debug)]
struct TerrainFrameKey {
    /// The frame the work belongs to. See [`begin_frame`].
    frame: u64,
    /// The frustum's six planes, which is the camera's projection and rotation: a camera that moved or
    /// turned has a different one, so the cull is redone. The matrix itself is not kept because this is
    /// all the culler reads of it.
    frustum: [[f32; 4]; 6],
    /// **The camera's section, and it is not in the frustum.** The view matrix carries the camera's
    /// offset *within* its own section, so two sections with the same offset produce the same matrix and
    /// the same planes - while the boxes being culled are relative to that section and move with it.
    camera_section: glam::IVec3,
    /// Which revision of the game's occlusion list the gather honoured. See
    /// [`Scene::visible_sections_revision`].
    visible: u64,
    /// The model matrix's translation, which the culler is handed and warns about. In the key because it
    /// is an argument of the thing being cached.
    model_translation: [f32; 3],
}

/// How many frames the renderer has begun. See [`begin_frame`].
///
/// A static rather than a field, because "which frame is this" is a fact about the renderer and not about
/// a graph: the frame boundary is a submission, which the command encoder knows about and the graph does
/// not. One process, one renderer, one counter - and the demo that drives a graph by hand leaves it at
/// zero, which simply means its cache lives until something else in the key moves.
static FRAME: AtomicU64 = AtomicU64::new(0);

/// The budgets the flood is cross-checked at **while the setting is off**.
///
/// With `adv_culling` at zero the walk decides nothing, so the cross-check probes these and reports each
/// of them; with it on, the budget it is deciding with is the one reported instead. These four agreeing
/// with each other is how the saturation below them was found.
///
/// A list rather than one value because the question the cross-check answers is *which* budget reproduces
/// the game's own list: what the walk reaches and what it costs both move with this number, and nothing
/// but a measurement says where the two meet. The original's `maxDirectionsChanges` is `advCulling - 1`.
const FLOOD_BUDGETS: [u8; 4] = [4, 8, 16, 127];

/// The second the cross-check last reported in, so that it runs once a second rather than once a frame.
static FLOOD_REPORTED: AtomicU64 = AtomicU64::new(0);

/// The world as the flood sees it: the game's answers, what the arena holds, and the frame's frustum.
///
/// The three are what [`SectionSource`] asks for, and they are gathered here rather than read inside the
/// walk because two of them are behind locks: the walk polls thousands of positions, and a lock per poll
/// would be a lock per section in the world.
struct FloodWorld<'a> {
    /// The game's occlusion answers, by section. Absent means "nobody has said", which stops the walk.
    answers: &'a HashMap<IVec3, u64>,
    /// The arena, for "is there anything to draw here" - read through the guard the caller already holds.
    sections: &'a SectionStorage,
    /// The frame's frustum, which is measured from the camera's own section. See [`Self::in_frustum`].
    frustum: &'a Frustum<f32>,
    /// The section the camera is in, which is the origin every box in the frustum test is measured from.
    camera_section: IVec3,
}

impl SectionSource for FloodWorld<'_> {
    /// The game's answer where there is one, and **open air where there is not**.
    ///
    /// The second half is not a fallback, it is the rule that makes the walk work at all: Minecraft
    /// never compiles a section that is all air, so no payload ever carries an answer for one, and a walk
    /// that stopped at those sections would propagate through rock and be stopped by sky. The original
    /// does not have the problem because it walks its own grid, where every loaded chunk is present and an
    /// air section is present-and-empty; here the only thing that says "this section is air" is that
    /// nobody has ever described it.
    ///
    /// What bounds the walk instead is the world itself: an unanswered section is open only where the
    /// game has a world - [`Self::in_world`], the game's own `ViewArea`, pushed once a frame - and inside
    /// that, the frustum. The order is the box first because it is the cheaper test, and because a
    /// position outside the world is not a cull but the end of the world.
    fn visibility(&self, pos: IVec3) -> Option<Visibility> {
        Some(
            self.answers
                .get(&pos)
                .map_or(Visibility::EVERY_PAIR, |bits| Visibility::from_bits(*bits)),
        )
    }

    /// "Nothing to draw here" is the arena's own answer, and asking it is what keeps the walk's idea of a
    /// section the same as the draw loop's: a section with no layer is one no draw would name.
    fn is_empty(&self, pos: IVec3) -> bool {
        !self.sections.holds(pos)
    }

    /// The world's own edge: the game's `ViewArea`, which is what says whether a position is a section of
    /// this level at all.
    ///
    /// Asked before the frustum, and the reason the walk no longer fills the sky. "Nobody has described
    /// this section" means air *inside* the world and nothing at all outside it, so a walk with only the
    /// frustum for a boundary walks the whole cone - above the build limit, below the level, and out to
    /// the far plane - and pays a poll for every one of those positions. See
    /// `crate::mc::world_extent` for where the box comes from and how it was measured.
    ///
    /// Until the JVM has said anything the box is absent and every position is in the world, which is the
    /// behaviour this walk had before the box existed: a test, or a demo driving a graph by hand, must not
    /// have its walk silently emptied.
    fn in_world(&self, pos: IVec3) -> bool {
        crate::mc::world_extent::get().is_none_or(|extent| extent.holds(pos, self.camera_section))
    }

    /// The same test the gather's own cull uses, in the same space.
    ///
    /// The frustum comes from a view matrix that carries the camera's offset *within its section* and
    /// nothing else, so a box built from the absolute section position would stand thousands of blocks
    /// away from the geometry it describes. Sixteen blocks wide, relative to the camera's section, is
    /// where the shader draws it and therefore where the frustum can judge it.
    fn in_frustum(&self, pos: IVec3) -> bool {
        let rel = pos - self.camera_section;

        let a: Vec3<f32> = [
            rel.x as f32 * 16.0,
            rel.y as f32 * 16.0,
            rel.z as f32 * 16.0,
        ]
        .into();
        let b: Vec3<f32> = a + Vec3::new(16.0, 16.0, 16.0);

        // The plane hint is 0: this walk goes face to face rather than in the arena's iteration order, so
        // there is no "the plane that culled the last one" to hand back.
        AABB::new(a.into_array(), b.into_array())
            .coherent_test_against_frustum(self.frustum, 0)
            .0
    }
}

/// Runs the flood beside the game's own answer, once a second, and reports both.
///
/// **This decides nothing.** The list it compares against is the one this renderer already obeys, and
/// running the two side by side on the same world is the only way to know whether the flood is right
/// before it is allowed to decide anything. What the line says, in order: how many sections the game
/// named, how many of those the arena can actually draw, how many sections the arena holds in all, how
/// many of those have an answer at all - and then, per budget, what the walk reached and how much of it
/// the game agreed with.
fn cross_check_the_flood(scene: &Scene, frustum: &Frustum<f32>, camera_section: IVec3) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);

    if FLOOD_REPORTED.swap(now, Ordering::Relaxed) == now {
        return;
    }

    let culling = ADV_CULLING.load(Ordering::Relaxed).min(16) as u8;

    // The same locks the gather takes, in the same order - the visible list, then the arena - and the
    // answers' own lock after both. Nothing here may take them in another order, or the drain's publish
    // (arena, then the too-new set) and this would be a cycle.
    let visible = scene.visible_sections.read();
    let sections = scene.section_storage.read();

    let Some(list) = visible.as_ref() else {
        log::info!(
            "wgpu-mc: the flood has nothing to compare against yet: the game has sent no visible list"
        );
        return;
    };

    let answers = crate::mc::visibility::snapshot();

    // What the game named *and* the arena can draw: what a flood that was perfect would have to reach. A
    // section the list names that the arena holds nothing for is not a miss - nothing draws it either way.
    let named: std::collections::HashSet<IVec3> = list
        .iter()
        .copied()
        .filter(|pos| sections.holds(*pos))
        .collect();
    let answered = sections
        .iter()
        .filter(|(pos, _)| answers.contains_key(pos))
        .count();

    let mut budgets = String::new();

    // With the walk deciding, the number that matters is the one it is deciding with; while it is off,
    // what is worth reporting is the probe. Either way the line names a budget that something used.
    let to_report: Vec<u8> = match culling {
        0 => FLOOD_BUDGETS.to_vec(),
        culling => vec![culling - 1],
    };

    for budget in to_report {
        // **A world per budget, and its own clock.** The source counts what the world's own box refused,
        // so it cannot be shared between budgets; and with the setting on, the budget reported here is
        // the one the frame decides with - which makes this the cost of the walk the renderer is running,
        // measured, rather than an estimate of it.
        let world = FloodWorld {
            answers: &answers,
            sections: &sections,
            frustum,
            camera_section,
        };

        let started = std::time::Instant::now();
        let flood = section_graph::flood(&world, camera_section, budget);
        let took = started.elapsed();

        let agreed = flood
            .visible
            .iter()
            .filter(|pos| named.contains(*pos))
            .count();

        // **Where the misses are, and why - split in two, because the two need different work.** A section
        // the game named, the arena holds and the walk did not reach is either one this side's own
        // *per-section* frustum test refuses - which is a tighter bound than the game's own octree visit,
        // and one that can be widened - or one no route inside the frustum reaches at all. One number
        // cannot tell those apart, and the answer decides whether the missing reach is a bound or a rule.
        let drawn: std::collections::HashSet<IVec3> = flood.visible.iter().copied().collect();
        let missed_behind_the_frustum = named
            .iter()
            .filter(|pos| !drawn.contains(*pos) && !world.in_frustum(**pos))
            .count();
        let missed_unreachable = named
            .iter()
            .filter(|pos| !drawn.contains(*pos) && world.in_frustum(**pos))
            .count();

        budgets.push_str(&format!(
            " [budget {budget}: {} drawn ({agreed} named, {} not), {} missed ({missed_behind_the_frustum} \
             behind this side's frustum, {missed_unreachable} unreachable), {} polled, {} over budget, {} \
             behind the camera, {} outside the world, {:.2} ms]",
            flood.visible.len(),
            flood.visible.len() - agreed,
            named.len().saturating_sub(agreed),
            flood.polled,
            flood.over_budget,
            flood.out_of_frustum,
            flood.outside_the_world,
            took.as_secs_f64() * 1000.0,
        ));
    }

    // **The box the walk is bounded by, named in the line.** A run whose walk costs something other than
    // the last run's is either a different world or no world at all, and the two are told apart here
    // rather than guessed at: "has not arrived" means this side is walking on the frustum alone.
    let box_line = match crate::mc::world_extent::get() {
        None => {
            "the world's own box has not arrived, so every position in the frustum is walkable air"
                .to_string()
        }
        Some(extent) if !extent.holds_the_layer(camera_section.y) => format!(
            "the camera's own layer {} is outside the level's {}..={}, so the walk is not used this \
             frame",
            camera_section.y, extent.min_section_y, extent.max_section_y
        ),
        Some(extent) => format!(
            "the world's own box is +/-{} chunk(s) around the camera and layers {}..={}",
            extent.horizon, extent.min_section_y, extent.max_section_y
        ),
    };

    log::info!(
        "wgpu-mc: the flood against the game's list: the game named {} section(s), {} of them held by the \
         arena, {} held in all and {answered} of those with an answer; {box_line};{budgets}",
        list.len(),
        named.len(),
        sections.len(),
    );
}

/// Says that a new frame has begun, which is what expires the terrain pass' frame scratch.
///
/// **Called from the one submission point**, `device::flush_shared_encoder`, so a frame is what the
/// encoder says it is. `TerrainFrameKey` also carries everything the gather reads, so a caller that never
/// calls this still cannot draw a stale *list* - only one whose sections' arena ranges may have been
/// handed back, which is why this exists at all.
pub fn begin_frame() {
    FRAME.fetch_add(1, Ordering::Relaxed);
}

static TERRAIN_DRAWN: AtomicU64 = AtomicU64::new(0);
static TERRAIN_CULLED: AtomicU64 = AtomicU64::new(0);
static TERRAIN_EMPTY: AtomicU64 = AtomicU64::new(0);
/// Sections the arena holds that the game's own occlusion graph did not name.
///
/// The number that says whether the graph's list is arriving and doing anything: it is the frustum's
/// count *minus* this one, and a run where this is zero while the frustum culls hard is a run whose
/// list never got there. See [`report_terrain_pass`].
static TERRAIN_OUT_OF_SIGHT: AtomicU64 = AtomicU64::new(0);
static TERRAIN_REPORTED: AtomicU64 = AtomicU64::new(0);

/// How many times the frame's gather ran, how many arena sections it walked, and how many it kept.
///
/// **The three numbers that say whether the frame's work is per frame or per pass.** The gather is one walk
/// of the arena a frame (`rebuild_terrain_frame`), and the sections it keeps are then walked once by each
/// terrain pass that builds records - three in a frame with water in view. `walked` against `visible` is
/// what the gather costs, and `visible` against the gathers is what the per-pass loops cost. See the
/// terrain build line in [`report_terrain_pass`].
static TERRAIN_GATHERS: AtomicU64 = AtomicU64::new(0);
static TERRAIN_WALKED: AtomicU64 = AtomicU64::new(0);
static TERRAIN_VISIBLE: AtomicU64 = AtomicU64::new(0);

/// The most draw calls any one pass has been left with, **for the whole session**.
///
/// Against [`crate::mc::INDIRECT_DRAW_CAPACITY`], this is how close the frame has ever come to the cliff
/// where `PassDraws::batched` goes false and the pass falls back to one draw per section. It is a session
/// maximum rather than a per-second one because the question it answers - "should the capacity be raised" -
/// is about the worst case a run can produce, and because a per-second maximum would be reset by the report
/// that the number is read in.
static TERRAIN_CALLS_PEAK: AtomicU64 = AtomicU64::new(0);

/// Passes that went over [`crate::mc::INDIRECT_DRAW_CAPACITY`] and lost the batched path, and when the last
/// one was reported. See [`report_the_indirect_cap`].
static TERRAIN_OVER_CAPACITY: AtomicU64 = AtomicU64::new(0);
static TERRAIN_OVER_CAPACITY_REPORTED: AtomicU64 = AtomicU64::new(0);

/// The terrain frame's uploads: one per pass with records, one more per batched pass with arguments.
///
/// `Queue::write_buffer` allocates staging memory per call, so this is a count of the allocations a frame
/// makes on the render thread - six of them for a frame that draws all three passes batched. Counted here
/// rather than in `device.rs` because it is the terrain path's own shape that decides the number.
static TERRAIN_UPLOADS: AtomicU64 = AtomicU64::new(0);
static TERRAIN_UPLOAD_BYTES: AtomicU64 = AtomicU64::new(0);

/// How long [`RenderGraph::rebuild_terrain_frame`] takes, and how much of that is the gather.
///
/// **The number that decides whether any of the above is worth changing.** The counters beside these say
/// what the work is proportional to - sections walked, sections kept, uploads made - and none of them says
/// whether the whole thing is 20 µs a frame or 2 ms. Two clock reads a frame, only while the frame is
/// actually rebuilt: a frame the key says is unchanged does not come here at all.
static TERRAIN_BUILD_NANOS: AtomicU64 = AtomicU64::new(0);
static TERRAIN_GATHER_NANOS: AtomicU64 = AtomicU64::new(0);

/// And how much of the build is the per-pass half: the records and their uploads.
///
/// The two halves are the two things that scale with how much of the world is in view, and they are what
/// decides where a change would pay: the gather is one walk of the arena a frame, and the record sets are one
/// walk of the *kept* sections per pass - three of them in a frame with water in view - plus an upload each.
static TERRAIN_RECORDS_NANOS: AtomicU64 = AtomicU64::new(0);

/// Terrain frame rebuilds, and how many of them the submission counter alone forced.
///
/// [`TerrainFrameKey::frame`] advances at every *submission* ([`begin_frame`]), and a presented frame makes
/// more than one of those. Everything else in the key is the camera, the frustum and the occlusion list - so
/// a rebuild whose "same except the frame" twin matches is a rebuild that re-walked the whole arena to
/// produce a list identical to the one it replaced. See the report line in [`report_terrain_pass`].
static TERRAIN_REBUILDS: AtomicU64 = AtomicU64::new(0);
static TERRAIN_REBUILDS_FRAME_ONLY: AtomicU64 = AtomicU64::new(0);

/// The same three counts, per layer.
///
/// A layer that is not on screen has two very different explanations - the arena has nothing in it for
/// those sections, or it has and the pass is not drawing it - and the total above cannot tell them
/// apart. Indexed by `RenderLayer as usize`.
///
/// These are **drained by the report**, and since the opaque group is two pipelines over one geometry
/// that means whichever of them the graph reaches last is the one that reports: `terrain_solid` clears
/// the counters and the report that follows `terrain` then shows the cutout layer's own count and a
/// zero where the solid layer's should be. That reads as a solid layer that never drew, which is why
/// the totals below exist and why the report prints those.
static LAYER_DRAWN: [AtomicU64; 3] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static LAYER_EMPTY: [AtomicU64; 3] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];

/// And the same per layer **since the renderer started**, which nothing drains.
///
/// The whole frame's count rather than the last pipeline's, for the reason above: a report taken at a
/// pipeline boundary cannot see the pass's other pipelines, and the question the numbers are asked -
/// "is the solid layer being drawn at all" - is about the frame. A caller subtracts two reads to get a
/// rate; the report simply prints them.
static LAYER_DRAWN_TOTAL: [AtomicU64; 3] =
    [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static LAYER_EMPTY_TOTAL: [AtomicU64; 3] =
    [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];

/// Drains those six counters, for a caller outside this module.
///
/// Drained rather than read, so the numbers a caller prints are the ones since its last call - which
/// is what makes them readable beside a per-second report.
pub fn take_layer_counts() -> ([u64; 3], [u64; 3]) {
    let mut drawn = [0u64; 3];
    let mut empty = [0u64; 3];

    for layer in 0..3 {
        drawn[layer] = LAYER_DRAWN[layer].swap(0, Ordering::Relaxed);
        empty[layer] = LAYER_EMPTY[layer].swap(0, Ordering::Relaxed);
    }

    (drawn, empty)
}

/// Sections drawn by the terrain pass, in total rather than since the last report.
///
/// The counter above is drained by the report; this one is not, because it is what a caller outside
/// this module asks to decide whether the pass is drawing a world yet - a dump of the terrain layer
/// taken before the arena holds one is a picture of nothing.
static TERRAIN_DRAWN_TOTAL: AtomicU64 = AtomicU64::new(0);

/// How many sections the terrain pass has drawn since the renderer started.
pub fn terrain_sections_drawn() -> u64 {
    TERRAIN_DRAWN_TOTAL.load(Ordering::Relaxed)
}

/// The two pipeline-state diagnostics, which are read when a pipeline is *built*.
///
/// They exist to answer one question about a picture that is inside out: whether the faces are wound
/// the wrong way or the depth test keeps the wrong end of the range. Drawn two-sided, the picture
/// stops depending on the winding at all, which is what tells the two apart; drawn with the depth
/// test the other way round, the faces behind are the ones kept, which is what "the depth values are
/// the wrong way round" would look like. Both are switches on the options screen now, and both were
/// marker files (`wgpu-terrain-no-cull`, `wgpu-terrain-greater-depth`) first.
///
/// They are read by every pipeline the graph builds, not only by the terrain one - which is where
/// the marker was read. The names are the bug they were written for rather than the scope of the
/// switch.
///
/// Unlike the switches that are consulted per draw, these two are *built into* a pipeline, so a
/// change leaves the pipelines already in the graph stale: [`set_pipeline_diagnostics`] reports
/// that, and the caller rebuilds them.
static TERRAIN_NO_CULL: AtomicBool = AtomicBool::new(false);

/// Whether the terrain pass honours the game's occlusion list or draws every section the frustum holds.
///
/// Read per gather rather than baked into a pipeline, because it decides an `if` and not a pipeline
/// state - so unlike [`TERRAIN_NO_CULL`] this one takes effect on the next frame, with nothing rebuilt.
///
/// **On is the faster answer and, while the list is right, the correct one.** It is a switch because the
/// list is a *snapshot*: `LevelRenderer.applyFrustum` is the only thing that refills it, and it runs only
/// when the camera has turned more than two degrees or the occlusion graph reports a change. A section
/// this side has baked and the game's list has not caught up with is skipped here while the game's own
/// mesh for it stays suppressed - and a section neither side draws is a 16x16x16 hole. See
/// `Settings::terrain_occlusion`.
static TERRAIN_OCCLUSION: AtomicBool = AtomicBool::new(true);

/// Sets whether the gather honours the game's occlusion list. See [`TERRAIN_OCCLUSION`].
pub fn set_terrain_occlusion(honour: bool) {
    TERRAIN_OCCLUSION.store(honour, Ordering::Relaxed);
}

/// How many direction changes this renderer's own occlusion walk may take, or zero to draw from the game's
/// own list. See `Settings::adv_culling`.
///
/// Read per gather, like [`TERRAIN_OCCLUSION`] and for the same reason: it decides which list an `if`
/// tests against, so it takes effect on the next frame with nothing rebuilt. Zero is the default and means
/// the game decides - the walk is measured beside that list (see [`cross_check_the_flood`]) rather than
/// trusted in its place.
static ADV_CULLING: AtomicU64 = AtomicU64::new(0);

/// Sets how many direction changes the occlusion walk may take. See [`ADV_CULLING`].
pub fn set_adv_culling(culling: u8) {
    ADV_CULLING.store(culling as u64, Ordering::Relaxed);
}

/// Whether the terrain pass batches its draws, **as the setting asks**. See [`terrain_batches_draws`],
/// which is the answer the draw path uses: this one is only half of it.
static TERRAIN_INDIRECT: AtomicBool = AtomicBool::new(false);

/// Whether this device may batch the terrain pass' draws at all: `device::can_batch_terrain_draws`,
/// minus the backends that may not be asked, resolved where the device is created. See
/// [`terrain_batches_draws`].
///
/// The other half, and it is a separate flag because the two are decided in different places and at
/// different times: the setting is read from the config when the run directory is sent - **which is
/// from the mod constructor, before there is a device** - and this is what the device turned out to be
/// able to do, recorded where the device was created. Resolving the pair where the setting is read is
/// what went wrong the first time this was written: `debug::apply` asked the adapter a question no
/// adapter had answered yet, got "no" every launch, and printed
///
/// ```text
/// indirect draws: execution yes, real batched multi-draw yes, non-zero first instance yes
/// the terrain pass is drawing one section at a time, and this device could batch
/// ```
///
/// - which is the pair of lines that found it.
static TERRAIN_BATCHING_POSSIBLE: AtomicBool = AtomicBool::new(false);

/// Sets whether the terrain pass batches its draws, as the setting asks. See [`terrain_indirect`].
pub fn set_terrain_indirect(batched: bool) {
    TERRAIN_INDIRECT.store(batched, Ordering::Relaxed);
}

/// Records whether this device may batch the terrain pass' draws at all. See
/// [`TERRAIN_BATCHING_POSSIBLE`], which is where the feature bits and the backend are resolved.
pub fn set_terrain_batching_possible(possible: bool) {
    TERRAIN_BATCHING_POSSIBLE.store(possible, Ordering::Relaxed);
}

/// Whether the `terrain_indirect` setting asks for batching: **the setting alone**, without the device.
///
/// One caller, and it is a diagnostic: `device::report_device_capabilities` prints which of the reasons
/// the terrain pass is drawing one section at a time, and "the switch is off" and "the device was not
/// allowed to" are different answers that the combined flag cannot tell apart. See [`terrain_indirect`]
/// for what the draw path reads.
pub fn terrain_indirect_requested() -> bool {
    TERRAIN_INDIRECT.load(Ordering::Relaxed)
}

/// Whether the terrain pass batches its draws: **the setting and the device, in one answer**.
///
/// Read per pass by the draw path, and written once per world by the capability report - so what a run
/// prints about the batching is what the batching did.
pub fn terrain_batches_draws() -> bool {
    terrain_indirect_requested() && TERRAIN_BATCHING_POSSIBLE.load(Ordering::Relaxed)
}

/// Whether this device and this backend may batch draws with `multi_draw_indexed_indirect` at all:
/// **the device answer without the setting**.
///
/// The three indirect feature bits and the DX12 exclusion, resolved where the adapter is - see
/// [`TERRAIN_BATCHING_POSSIBLE`]. This is the second reader of that answer, and it is read per batch
/// rather than per pass: [`terrain_batches_draws`] is about the terrain pass this renderer draws
/// itself, and this is about Minecraft's own repeated draws, which the JNI side batches through the
/// same call. A device whose multi-draw is emulated, or whose backend's is broken, gets one draw at a
/// time there too - so the two readers cannot disagree about what the hardware can do.
pub fn terrain_batching_possible() -> bool {
    TERRAIN_BATCHING_POSSIBLE.load(Ordering::Relaxed)
}

/// Whether the game's block atlas is sampled from its base mip level only.
///
/// **Off, and the argument for leaving it off is in the sampler's own comment** - the game animates
/// every level of that chain, so there is no stale level for a clamp to avoid, and clamping to 0.0 gives
/// up the mip chain instead: a distant lump of lava samples one texel of a 16x16 sprite, which is the
/// aliasing the chain exists to prevent. It is a switch rather than a decision because the case for it
/// is a picture nobody has measured yet - if the distant animated textures shimmer, this is the one-line
/// answer to try, and a run with it on against a run with it off says whether it helped.
///
/// Read when the sampler is built, so it is applied by rebuilding the graph - see
/// `wgpu_mc_jni::debug::rebuild_pipelines_if_stale`, which is the same path the occlusion switch's
/// neighbour takes.
static ATLAS_BASE_MIP_ONLY: AtomicBool = AtomicBool::new(false);

/// Sets whether the game's block atlas is sampled from its base mip level only. See
/// [`ATLAS_BASE_MIP_ONLY`].
pub fn set_atlas_base_mip_only(base_only: bool) {
    ATLAS_BASE_MIP_ONLY.store(base_only, Ordering::Relaxed);
}

/// Whether the game's block atlas is sampled from its base mip level only. See [`ATLAS_BASE_MIP_ONLY`].
pub fn atlas_base_mip_only() -> bool {
    ATLAS_BASE_MIP_ONLY.load(Ordering::Relaxed)
}
static TERRAIN_GREATER_DEPTH: AtomicBool = AtomicBool::new(false);

/// Sets both pipeline-state diagnostics, and says whether either of them changed.
///
/// The answer is what the pipelines are rebuilt on - the ones built with the old answer keep it.
/// Nothing here rebuilds anything itself: this crate has no renderer to rebuild against, and the
/// caller that has one is the caller that owns the graph.
pub fn set_pipeline_diagnostics(no_cull: bool, greater_depth: bool) -> bool {
    let no_cull_changed = TERRAIN_NO_CULL.swap(no_cull, Ordering::Relaxed) != no_cull;
    let depth_changed =
        TERRAIN_GREATER_DEPTH.swap(greater_depth, Ordering::Relaxed) != greater_depth;

    no_cull_changed || depth_changed
}

/// Draws a terrain pass had to leave out because its buffers had no room for a record of them,
/// cumulatively, and the last second one was reported in.
///
/// See [`report_truncated_draws`] for why the line is not behind the diagnostics switch.
static TERRAIN_TRUNCATED: AtomicU64 = AtomicU64::new(0);
static TERRAIN_TRUNCATED_REPORTED: AtomicU64 = AtomicU64::new(0);

/// Says so, once a second, when a pass had more draws than it has room to describe.
///
/// **Not gated on the diagnostics switch**, unlike every other line the terrain pass writes. The ones
/// that are gated are counters - how much was drawn, how much was culled - and this is geometry that
/// was *not* drawn. It cannot happen at the sizes here, and if it ever does the picture is missing
/// sections with nothing else anywhere to say why, which is the one failure this renderer keeps
/// choosing to make loud.
fn report_truncated_draws(truncated: u64) {
    if truncated == 0 {
        return;
    }

    let total = TERRAIN_TRUNCATED.fetch_add(truncated, Ordering::Relaxed) + truncated;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);

    if TERRAIN_TRUNCATED_REPORTED.swap(now, Ordering::Relaxed) == now {
        return;
    }

    log::error!(
        "wgpu-mc: a terrain pass had more draws than the {} its buffers hold and left the rest out; \
         {total} draw(s) dropped so far this session. Raise `SECTION_DRAW_CAPACITY`",
        crate::mc::SECTION_DRAW_CAPACITY
    );
}

/// Whether a pass with this many draw calls can still be recorded as one `multi_draw` argument list.
///
/// **The one place `PassDraws::batched`'s capacity is decided**, so the decision and the warning that reports
/// it cannot come apart: the pass takes the batched path while this is true, and
/// [`report_the_indirect_cap`] is what says so when it is false. The bound is inclusive - a pass with
/// exactly [`crate::mc::INDIRECT_DRAW_CAPACITY`] calls has a slot for every one of them, because the buffers
/// are allocated at that size.
fn the_indirect_path_holds(calls: usize) -> bool {
    calls <= crate::mc::INDIRECT_DRAW_CAPACITY
}

/// Says so, once a second, when a terrain pass had more draws than the indirect path can batch.
///
/// **Not gated on the diagnostics switch**, for the reason [`report_truncated_draws`] is not: this is a step
/// change in what the frame costs - the pass stops writing one argument list and starts submitting one draw
/// per section, on the render thread, for the rest of the frame - and it arrives with no other symptom. A run
/// that hits it looks like "the frame time jumped when I turned toward the open" and nothing else, which is
/// exactly the kind of thing that is worth a line in the log of a run nobody is profiling. The counters on
/// the gated terrain build line say how close the session has come; this says when it arrived.
///
/// It is not an error and the picture is unchanged: the fallback is what this renderer did before the
/// indirect path existed, and the capacity is what bounds the argument buffers. See
/// [`crate::mc::INDIRECT_DRAW_CAPACITY`] and `PassDraws::batched`.
fn report_the_indirect_cap(calls: u64) {
    let total = TERRAIN_OVER_CAPACITY.fetch_add(1, Ordering::Relaxed) + 1;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);

    if TERRAIN_OVER_CAPACITY_REPORTED.swap(now, Ordering::Relaxed) == now {
        return;
    }

    log::warn!(
        "wgpu-mc: a terrain pass has {calls} draw calls, over the {} the indirect path batches, so this \
         frame draws it one draw per section instead; {total} pass(es) over it so far this session. Raise \
         `INDIRECT_DRAW_CAPACITY`, or split the pass into several multi-draws",
        crate::mc::INDIRECT_DRAW_CAPACITY
    );
}

/// Reports what the terrain pass drew, once a second and only while the section diagnostics are on.
///
/// The counters are always kept, because the line they feed is the only place a run says whether the Rust
/// terrain reached the screen at all - but they are *local* `u64`s on the pass's own stack, added to these
/// atomics once each at the end of it. See the terrain branch: a relaxed `fetch_add` per section per layer
/// is a contended read-modify-write for a number read once a second.
///
/// **`culled` and `out of sight` now count sections and not section-layers.** They are decided while the
/// gather runs - once per section, before any layer is chosen - so a two-layer pass divides the old
/// figures by two. The rate is what the line is read for, and the ratio between the two is unchanged.
/// "drawn" and "empty" are still per layer, because they are decided where the layer is drawn.
fn report_terrain_pass() {
    let drawn = TERRAIN_DRAWN.swap(0, Ordering::Relaxed);
    let culled = TERRAIN_CULLED.swap(0, Ordering::Relaxed);
    let empty = TERRAIN_EMPTY.swap(0, Ordering::Relaxed);
    let out_of_sight = TERRAIN_OUT_OF_SIGHT.swap(0, Ordering::Relaxed);

    if !crate::mc::chunk::DIAGNOSTIC_LOGGING.load(Ordering::Relaxed) {
        return;
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);

    if TERRAIN_REPORTED.swap(now, Ordering::Relaxed) == now {
        return;
    }

    // Per layer as well as in total, and the numbers answer different questions about a layer that is
    // not on screen: "drawn" is geometry that reached the GPU, "empty" is a layer the arena has nothing
    // in for that section, "culled" is a section the frustum rejected, and "out of sight" is one the
    // game's own occlusion graph did not name. The last two are the interesting pair: out-of-sight much
    // larger than culled is the game culling properly, and out-of-sight at zero with the frustum culling
    // hard is the graph's list not arriving at all. Those two count sections; the per-layer ones count
    // draws.
    //
    // **`culled`, `out of sight` and `empty` are counted once per frame now, not once per pass.** They
    // are properties of the gather, and there is one gather a frame rather than three - so they are about
    // a third of what they used to be, and their *ratio*, which is what they are read for, is unchanged.
    // `drawn` is still per pass, because a pass is what draws. See `TerrainFrame`.
    //
    // The per-layer figures are the **totals**, not the drained ones: this report runs at a pipeline
    // boundary and the opaque group is two pipelines, so the drained per-layer count would be whichever
    // of the two the graph reached last - a solid layer reported as zero while it drew, which is exactly
    // how this was noticed. The pass-wide counts below are still per report, which is what makes them
    // readable as a rate.
    let per_layer: Vec<String> = [
        RenderLayer::Solid,
        RenderLayer::Cutout,
        RenderLayer::Transparent,
    ]
    .iter()
    .map(|layer| {
        let index = *layer as usize;
        format!(
            "{layer:?} {} drawn {} empty",
            LAYER_DRAWN_TOTAL[index].load(Ordering::Relaxed),
            LAYER_EMPTY_TOTAL[index].load(Ordering::Relaxed)
        )
    })
    .collect();

    log::info!(
        "wgpu-mc: terrain pass: {drawn} section draw(s) - solid and cutout of one pass, transparent of \
         the other -, {out_of_sight} not named by the game's occlusion graph and old enough to believe, \
         {} too new for it to have judged (the list had not been refilled since they were taken over, so \
         the frustum decided them instead), {culled} culled by the frustum, {empty} with no layer at \
         all; per layer: {}",
        SECTIONS_TOO_NEW.load(Ordering::Relaxed),
        per_layer.join(", ")
    );

    // **Faces by atlas, cumulative, so two of these lines are a rate.** The question this answers is
    // whether the two-atlas branch in the terrain shaders is live: `decide_game_atlas` sends every face to
    // the game's atlas once it is bound, so faces still arriving on this side's own after the handover are
    // the fallback, and a session where the second number stops moving is one whose shader could carry a
    // single atlas. See `set_game_block_atlas`, which says the same pair at the handover.
    let (game, ours) = crate::mc::block::atlas_face_counts();

    log::info!(
        "wgpu-mc: faces by atlas, cumulative: {game} from the game's, {ours} from this side's"
    );

    // **What building the frame's draws costs, per second.** The three numbers on the left are the shape of
    // the work: one gather a frame, that many arena sections walked, and that many sections kept - and the
    // kept ones are walked once more by each terrain pass that builds records, which is three of them in a
    // frame with water in view. The two on the right are what that work hands to the GPU: an upload per pass
    // with records and one more per batched pass, each of which is a `Queue::write_buffer` and therefore an
    // allocation of staging memory.
    //
    // `peak` is the session's maximum and never drained, because the question it is read for - how close a
    // run has come to `INDIRECT_DRAW_CAPACITY` - is about the worst frame a run has produced. The rest are
    // per report, which is what makes them a rate.
    let gathers = TERRAIN_GATHERS.swap(0, Ordering::Relaxed);
    let walked = TERRAIN_WALKED.swap(0, Ordering::Relaxed);
    let visible = TERRAIN_VISIBLE.swap(0, Ordering::Relaxed);
    let uploads = TERRAIN_UPLOADS.swap(0, Ordering::Relaxed);
    let uploaded = TERRAIN_UPLOAD_BYTES.swap(0, Ordering::Relaxed) / 1024;
    let peak = TERRAIN_CALLS_PEAK.load(Ordering::Relaxed);
    let over = TERRAIN_OVER_CAPACITY.load(Ordering::Relaxed);
    let build_nanos = TERRAIN_BUILD_NANOS.swap(0, Ordering::Relaxed);
    let gather_nanos = TERRAIN_GATHER_NANOS.swap(0, Ordering::Relaxed);
    let records_nanos = TERRAIN_RECORDS_NANOS.swap(0, Ordering::Relaxed);
    let rebuilds = TERRAIN_REBUILDS.swap(0, Ordering::Relaxed);
    let frame_only = TERRAIN_REBUILDS_FRAME_ONLY.swap(0, Ordering::Relaxed);

    // A second with no gather in it is a second with no world drawn, and the arithmetic would be a panic
    // rather than a zero - which is the shape of a report taken while the world is being entered.
    let per_gather = walked.checked_div(gathers).unwrap_or(0);
    let build_us = build_nanos / 1000;
    let build_per_frame = build_nanos.checked_div(gathers).unwrap_or(0) / 1000;
    let gather_us = gather_nanos / 1000;
    let records_us = records_nanos / 1000;

    log::info!(
        "wgpu-mc: terrain build: {gathers} gather(s) ({per_gather} section(s) walked each, {visible} kept \
         in all - a pass builds records from the kept ones, three of them a frame), {uploads} upload(s) in \
         {uploaded} KB; {build_us} us building it, {build_per_frame} us a gather, {gather_us} us of it the \
         gather and {records_us} us the records and uploads; {rebuilds} rebuild(s), {frame_only} of them \
         because the submission counter moved and nothing else did; the most calls any pass has had is \
         {peak} of the {} the indirect path batches, {over} pass(es) over it so far",
        crate::mc::INDIRECT_DRAW_CAPACITY
    );
}

/// The game's own block atlas, as the pass that draws the terrain samples it.
///
/// Filled by the JVM once the game has stitched one (`WmNative.bindGameBlockAtlas`, called from the
/// block cache right beside the sprite registration, so that the rectangles a face is baked with and
/// the atlas that face samples always come from the same stitch). What is kept is the **view**, not
/// the texture: a `wgpu::TextureView` holds a handle to the texture it was made from, so this side
/// holding one is what keeps the game's atlas alive for as long as the graph samples it.
static GAME_BLOCK_ATLAS: parking_lot::RwLock<Option<Arc<wgpu::TextureView>>> =
    parking_lot::RwLock::new(None);

/// Whether the game's block atlas has been handed over, and so whether a face may be baked with its
/// coordinates. See [`GAME_BLOCK_ATLAS`].
///
/// This is the baker's gate, and it is set by the **handover** rather than by the graph build, because
/// of what the two halves of an animated texture are: the graph can be built again as often as it
/// likes, but a block *model* - and every face in it - is baked once, when the block states are
/// cached, and then drawn from that cache for the rest of the session. A model baked while this was
/// false would keep this side's copy of its sprite forever, however many times the atlas was bound
/// afterwards.
///
/// What makes that safe is the ordering around it, and it is the ordering `bind_game_block_atlas`
/// arranges: the handover marks the graph stale, the graph is replaced at the end of a frame
/// (`rebuild_pipelines_if_stale`), and the arena is fed in that same end-of-frame step - so there is no
/// frame in which a face flagged for the game's atlas is drawn by a pass that has not bound it. Until
/// the handover, every face keeps sampling this side's copy of its sprite, which is exactly what the
/// renderer did before any of this existed. See `Vertex::uv_flags`' `UV_GAME_ATLAS`.
static GAME_ATLAS_BOUND: AtomicBool = AtomicBool::new(false);

/// How wide and tall the game's block atlas is in texels, or `None` before it is handed over.
///
/// **Kept because "half a texel" is not a number without it.** The game's stitcher packs `blocks.png`
/// at whatever size the sprites need and the answer changes between runs - 2048x2048 with five mip
/// levels one launch, 1024x1024 with three the next - so a shift expressed as a fraction of the atlas
/// is a different shift each time. Anything that wants to move a coordinate by a texel has to ask here
/// rather than assume a size.
///
/// Also the sampler's own answer for our atlas, which is always [`crate::render::atlas::ATLAS_DIMENSIONS`]
/// square, so the two are deliberately not mixed: a shift meant for one atlas must be built from that
/// atlas' size.
static GAME_ATLAS_SIZE: parking_lot::RwLock<Option<(u32, u32)>> = parking_lot::RwLock::new(None);

/// The game's block atlas size in texels, or `None` if it has not been handed over. See
/// [`GAME_ATLAS_SIZE`].
pub fn game_atlas_size() -> Option<(u32, u32)> {
    *GAME_ATLAS_SIZE.read()
}

/// Hands over the game's block atlas. See [`GAME_BLOCK_ATLAS`] and [`GAME_ATLAS_BOUND`]. Called from
/// the JVM.
pub fn set_game_block_atlas(view: wgpu::TextureView, size: (u32, u32)) {
    *GAME_BLOCK_ATLAS.write() = Some(Arc::new(view));
    *GAME_ATLAS_SIZE.write() = Some(size);
    GAME_ATLAS_BOUND.store(true, Ordering::Relaxed);

    // **The number the terrain shader's "which atlas" branch turns on, said at the one moment it is a
    // decision.** `decide_game_atlas` sends *every* face here when this is bound and the JVM registered a
    // rectangle for its sprite - the two-atlas branch in `terrain.wgsl` exists for the faces that did not
    // get one, and the only such faces left in a normal session are the ones baked before this call.
    //
    // Both counts are cumulative and never reset, so this line plus the per-second one in
    // `report_terrain_pass` is a *rate*: faces still going to this side's atlas after the handover are
    // the fallback being live, and zero is the axis being removable.
    let (game, ours) = crate::mc::block::atlas_face_counts();

    log::info!(
        "wgpu-mc: the game's block atlas is bound, {}x{}: {game} face(s) are baked for it so far and \
         {ours} for this side's own atlas - the ones that fell back, because the game has no rectangle \
         for their sprite, because the animation is off, or because they were baked before this call",
        size.0,
        size.1,
    );
}

/// The game's block atlas, if the JVM has handed one over.
pub fn game_block_atlas() -> Option<Arc<wgpu::TextureView>> {
    GAME_BLOCK_ATLAS.read().clone()
}

/// Whether a face may be baked with the game's atlas coordinates. See [`GAME_ATLAS_BOUND`].
pub fn game_atlas_bound() -> bool {
    GAME_ATLAS_BOUND.load(Ordering::Relaxed)
}

/// The game's **lightmap**: the 16x16 texture Minecraft builds every frame it needs to, and the one
/// thing that decides how bright a light level is.
///
/// The terrain shader reads it exactly as the game's own `terrain.vsh` does - `sample_lightmap(Sampler2,
/// UV2)`, one fetch per vertex, and the colour interpolated across the quad - so every part of the curve
/// comes from the game: the gamma and brightness options, the day/night sky light, the dimension's
/// ambient light, night vision, the darkness effect. This side's shader used to approximate all of it
/// with `max(sky, block) * 0.7 + 0.3`, which is a straight line through a curve the game had already
/// built, and ignores every one of those.
///
/// Filled by the JVM (`WmNative.bindGameLightmap`). Like the block atlas it is the **view** that is kept,
/// so this side holding one keeps the texture alive; unlike the atlas, the game does not build a new one
/// - it writes into the texture it has - so a handover is a one-off and the values move under it.
static GAME_LIGHTMAP: parking_lot::RwLock<Option<Arc<wgpu::TextureView>>> =
    parking_lot::RwLock::new(None);

/// Hands over the game's lightmap. See [`GAME_LIGHTMAP`]. Called from the JVM.
pub fn set_game_lightmap(view: wgpu::TextureView) {
    *GAME_LIGHTMAP.write() = Some(Arc::new(view));
}

/// The game's lightmap, if the JVM has handed one over.
pub fn game_lightmap() -> Option<Arc<wgpu::TextureView>> {
    GAME_LIGHTMAP.read().clone()
}

/// The lightmap this side draws with when the game has not handed one over: the approximation the
/// shader used before there was a lightmap to sample, baked into a 16x16 texture.
///
/// One texel per light pair - `x` is the block light and `y` the sky light, which is the order
/// Minecraft's own `sample_lightmap` indexes in - and the value is `max(block, sky) / 15 * 0.7 + 0.3` per
/// channel. So a run whose handover failed draws the picture this renderer has always drawn instead of a
/// world lit by whatever a one-texel white texture would say, and the two are one line apart in the log
/// (`wgpu-mc: the game's lightmap is bound to the terrain pass`).
pub fn fallback_lightmap() -> [u8; 16 * 16 * 4] {
    let mut image = [0u8; 16 * 16 * 4];

    for sky in 0..16u32 {
        for block in 0..16u32 {
            let level = (block.max(sky) as f32) / 15.0;
            let grey = ((level * 0.7 + 0.3) * 255.0).round().clamp(0.0, 255.0) as u8;

            // The lightmap texture is `RGBA8`, and a light with no colour to it is grey with a full
            // alpha - the game's own lightmap is not grey, which is the point of sampling its.
            let texel = ((sky * 16 + block) * 4) as usize;
            image[texel] = grey;
            image[texel + 1] = grey;
            image[texel + 2] = grey;
            image[texel + 3] = 0xff;
        }
    }

    image
}

pub trait Geometry: Send + Sync {
    fn render<'graph: 'pass + 'arena, 'pass, 'arena: 'pass>(
        &mut self,
        wm: &WmRenderer,
        render_graph: &'graph RenderGraph,
        bound_pipeline: &'graph BoundPipeline,
        render_pass: &mut wgpu::RenderPass<'pass>,
        arena: &WmArena<'arena>,
    );
}

#[derive(Debug)]
pub enum ResourceBacking {
    Buffer(Arc<wgpu::Buffer>, wgpu::BufferBindingType),
    BufferArray(Vec<Arc<wgpu::Buffer>>),
    Texture2D(Arc<TextureAndView>),
    /// A view of a texture this renderer does not own: the game created it, and the view keeps it
    /// alive for as long as the graph does. See [`set_game_block_atlas`].
    TextureView(Arc<wgpu::TextureView>),
    Sampler(Arc<wgpu::Sampler>),
}

/// What a binding carries, which is all a bind group layout needs to know about it.
///
/// This exists so that the one thing the layout has to *decide* - which stages see the binding - is
/// decided in one place, [`ResourceKind::visibility`], rather than four times in a `match` where three
/// of the arms can be right and the fourth wrong. Which is what happened: see that method.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResourceKind {
    Buffer,
    Storage,
    Texture,
    Sampler,
}

impl ResourceKind {
    /// Which pipeline stages a binding of this kind is visible to: **both, always**.
    ///
    /// Not a choice this side gets to make per resource. Which stage samples a binding is a property of
    /// the *shader*, and the shaders here are the game's and the pack's: Minecraft's own terrain shader
    /// fetches its lightmap in the **vertex** stage - `vertexColor = Color * sample_lightmap(Sampler2,
    /// UV2)` - and this side's terrain shader does the same, because the light has to be interpolated as
    /// a colour rather than looked up per pixel. A texture the vertex stage samples, declared
    /// fragment-only, is not a pipeline that draws differently: it is one wgpu refuses to build, with
    /// the pipeline layout named in the error:
    ///
    /// ```text
    /// In Device::create_render_pipeline, label = 'terrain'
    ///   Error matching ShaderStages(VERTEX) shader requirements against the pipeline
    ///     Shader global ResourceBinding { group: 0, binding: 7 } is not available in the pipeline layout
    ///       Visibility flags don't include the shader stage
    /// ```
    ///
    /// That is exactly what the two texture arms and the sampler arm said for the lightmap, and the
    /// game ended while entering a world - see [`device_call`], which is the other half of this fix.
    /// Minecraft's own pipeline builder gives every binding both stages for the same reason
    /// (`blaze.rs`), and a binding of a kind that only one stage could use does not exist: a uniform, a
    /// storage buffer, a texture and a sampler are all readable from either.
    pub fn visibility(self) -> ShaderStages {
        match self {
            ResourceKind::Buffer
            | ResourceKind::Storage
            | ResourceKind::Texture
            | ResourceKind::Sampler => ShaderStages::VERTEX_FRAGMENT,
        }
    }
}

impl ResourceBacking {
    /// Which kind of binding this backing is. See [`ResourceKind::visibility`].
    pub fn kind(&self) -> ResourceKind {
        match self {
            ResourceBacking::Buffer(..) => ResourceKind::Buffer,
            ResourceBacking::BufferArray(_) => ResourceKind::Storage,
            ResourceBacking::Texture2D(_) | ResourceBacking::TextureView(_) => {
                ResourceKind::Texture
            }
            ResourceBacking::Sampler(_) => ResourceKind::Sampler,
        }
    }

    pub fn get_bind_group_layout_entry(&self, binding: u32) -> wgpu::BindGroupLayoutEntry {
        let visibility = self.kind().visibility();

        match self {
            ResourceBacking::Buffer(_, buffer_ty) => wgpu::BindGroupLayoutEntry {
                binding,
                visibility,
                ty: wgpu::BindingType::Buffer {
                    ty: *buffer_ty,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            ResourceBacking::BufferArray(_buffers) => wgpu::BindGroupLayoutEntry {
                binding,
                visibility,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            ResourceBacking::Texture2D(_) | ResourceBacking::TextureView(_) => {
                wgpu::BindGroupLayoutEntry {
                    binding,
                    visibility,
                    ty: wgpu::BindingType::Texture {
                        // Filterable, because the shaders this graph builds sample with `textureSample`
                        // and the atlas is `Rgba8Unorm`: a layout that says otherwise is not a pipeline
                        // that draws differently, it is one wgpu refuses to create at all.
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                }
            }
            ResourceBacking::Sampler(_) => wgpu::BindGroupLayoutEntry {
                binding,
                visibility,
                // Filtering for the same reason: `textureSample` needs one, and the default sampler
                // this graph binds is a filtering sampler with nearest filtering - non-filtering is
                // a different binding type, not a different filter mode.
                ty: wgpu::BindingType::Sampler(SamplerBindingType::Filtering),
                count: None,
            },
        }
    }

    pub fn get_bind_group_entries(&self, index: u32) -> Vec<wgpu::BindGroupEntry<'_>> {
        match self {
            ResourceBacking::Buffer(buffer, _buffer_ty) => vec![wgpu::BindGroupEntry {
                binding: index,
                resource: wgpu::BindingResource::Buffer(buffer.as_entire_buffer_binding()),
            }],
            ResourceBacking::Texture2D(texture) => vec![wgpu::BindGroupEntry {
                binding: index,
                resource: wgpu::BindingResource::TextureView(&texture.view),
            }],
            ResourceBacking::TextureView(view) => vec![wgpu::BindGroupEntry {
                binding: index,
                resource: wgpu::BindingResource::TextureView(view),
            }],
            ResourceBacking::Sampler(sampler) => vec![wgpu::BindGroupEntry {
                binding: index,
                resource: wgpu::BindingResource::Sampler(sampler),
            }],
            // RenderResource::TextureHandle(handle) => vec![
            //     wgpu::BindGroupEntry {
            //         binding: index,
            //         resource: wgpu::BindingResource::TextureView(handle.),
            //     }
            // ],
            _ => todo!(),
        }
    }
}

#[derive(Debug)]
pub enum WmBindGroup {
    Resource(String),
    Custom(wgpu::BindGroup),
}

#[derive(Debug)]
pub struct BoundPipeline {
    pub pipeline: wgpu::RenderPipeline,
    pub bind_groups: Vec<(u32, WmBindGroup)>,
    pub config: PipelineConfig,
    /// Every immediate this pipeline's layout declares, as `(byte offset, byte size)` in slot order.
    ///
    /// Resolved from `config.immediates` **once, when the pipeline is built**, so that drawing does not
    /// have to: the offsets and the sizes are properties of the layout, and the layout is built here.
    /// What is left for a draw is the data, handed over as a slice parallel to this one - see
    /// [`set_immediates`]. Two `Vec`s of two `u32`s, one per pipeline, once.
    pub immediates: Vec<(u32, u32)>,
    /// Which of the scene's draw-buffer slots this pass writes its records and indirect arguments
    /// into, or `None` when it is not a terrain pass.
    ///
    /// **A slot and not the buffers themselves**, because the buffers belong to the scene - they are
    /// replaced when the scene is - while the slot is a fact about the graph's own pipeline list.
    /// Assigned in [`RenderGraph::create_pipelines`] in the order the config lists terrain pipelines;
    /// see [`crate::mc::SECTION_DRAW_SLOTS`] for why each pass needs one of its own.
    pub draw_slot: Option<usize>,
}

impl BoundPipeline {
    /// Where in the layout the immediate named `resource` sits, or `None` when the pipeline has none.
    ///
    /// A name rather than an index, because a call site asks for the data it has - "the section's
    /// position" - and not for "the second immediate". The lookup is over a list that is one entry long
    /// in every graph this repository ships, and it happens once per pass rather than once per draw.
    pub fn immediate_offset(&self, resource: &str) -> Option<u32> {
        self.config
            .immediates
            .iter()
            .find(|(_, name)| name.as_str() == resource)
            .map(|(offset, _)| *offset as u32)
    }
}

#[derive(Debug)]
pub struct RenderGraph {
    pub config: ShaderPackConfig,
    pub pipelines: LinkedHashMap<String, BoundPipeline>,
    pub resources: HashMap<String, ResourceBacking>,
    /// The frame's terrain: the gather and the draws each pass will issue. See [`TerrainFrame`].
    terrain_frame: std::cell::RefCell<TerrainFrame>,
}

/// What a caught panic said, as one line.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    if let Some(message) = payload.downcast_ref::<&str>() {
        message
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message
    } else {
        "a panic with no message"
    }
}

/// Runs a device call wgpu reports a validation error out of by *panicking*, and answers `None` if it
/// did - with the reason in the log, naming `what`.
///
/// The device does not return an error here: an uncaptured validation error panics inside wgpu, and
/// because the whole renderer runs inside `#[jni_fn]` frames - and a `#[jni_fn]` frame cannot unwind -
/// that panic does not reach a `catch` anywhere: it takes the process with it. The message the player
/// gets is
///
/// ```text
/// panicked at library/core/src/panicking.rs:225:5:
/// panic in a function that cannot unwind
/// ```
///
/// and the game is gone while entering a world, with the actual reason - the wgpu error, above it in
/// the console - being the only thing that says what happened. This is the same treatment the resource
/// and shader arms of `create_pipelines` already give their own failures, for the same reason: a graph
/// that is missing one pipeline still draws the rest of the frame, and the reason belongs in the log
/// rather than in a process that ended.
///
/// It is a guard and not a licence: a pipeline that fails to build is a bug, and the log line is an
/// error.
fn device_call<T>(what: &str, build: impl FnOnce() -> T) -> Option<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(build)) {
        Ok(value) => Some(value),
        Err(payload) => {
            log::error!(
                "wgpu-mc: {what} could not be built, so it is skipped: {}",
                panic_message(&*payload)
            );

            None
        }
    }
}

/// The last second the plane hint reported itself, so a per-frame count is a per-second line.
static PLANE_HINT_REPORTED: AtomicU64 = AtomicU64::new(0);

impl RenderGraph {
    fn create_pipelines(
        &mut self,
        wm: &WmRenderer,
        custom_bind_groups: Option<HashMap<String, &wgpu::BindGroupLayout>>,
        geometry_vertex_layouts: Option<HashMap<String, Vec<wgpu::VertexBufferLayout>>>,
    ) {
        self.pipelines.clear();

        let arena = WmArena::new(1024);

        // Which of the frame's draw-buffer slots a terrain pass has claimed, in the order this config
        // lists its pipelines. See `SECTION_DRAW_SLOTS`: a pass needs a pair of buffers of its own,
        // because every `write_buffer` a frame makes is applied at the head of that frame's submission
        // and two passes sharing one buffer would both draw the second pass's records.
        let mut next_draw_slot = 0usize;

        for (pipeline_name, pipeline_config) in &self.config.pipelines.pipelines {
            // A pipeline whose resources are not all registered is skipped rather than unwrapped: this
            // runs inside a `#[jni_fn]` frame, where a panic aborts the JVM, and the resources a
            // pipeline names can legitimately be missing - the block atlas is registered by a resource
            // reload, and a reload that has not reached it yet leaves the terrain shader with nothing
            // to sample. The rest of the graph is unaffected, and a later reload builds it.
            let mut missing: Option<&String> = None;
            'resources: for def in pipeline_config.bind_groups.values() {
                if let BindGroupDef::Entries(entries) = def {
                    for resource_id in entries.values() {
                        if !self.resources.contains_key(resource_id) {
                            missing = Some(resource_id);
                            break 'resources;
                        }
                    }
                }
            }

            if let Some(missing) = missing {
                log::warn!(
                    "wgpu-mc: the '{pipeline_name}' pipeline of the render graph names {missing}, \
                     which is not registered; skipping it"
                );
                continue;
            }

            let bind_group_layouts = pipeline_config
                .bind_groups
                .iter()
                .map(|(_slot, def)| match def {
                    BindGroupDef::Entries(entries) => {
                        let layout_entries = entries
                            .iter()
                            .map(|(index, resource_id)| {
                                let resource = self.resources.get(resource_id).unwrap();
                                resource.get_bind_group_layout_entry(*index as u32)
                            })
                            .collect::<Vec<wgpu::BindGroupLayoutEntry>>();

                        &*arena.alloc(wm.gpu.device.create_bind_group_layout(
                            &wgpu::BindGroupLayoutDescriptor {
                                label: None,
                                entries: &layout_entries,
                            },
                        ))
                    }
                    BindGroupDef::Resource(resource) => {
                        match (&resource[..], &custom_bind_groups) {
                            ("@bg_ssbo_chunks", _) => wm.bind_group_layouts.get("ssbo").unwrap(),
                            // The draw arguments a terrain pass writes and reads back. A layout of its
                            // own rather than `ssbo`, because it is visible to the vertex stage alone -
                            // see the entry in `create_bind_group_layouts`.
                            ("@bg_section_draws", _) => {
                                wm.bind_group_layouts.get("section_draws").unwrap()
                            }
                            ("@bg_entity", _) => wm.bind_group_layouts.get("entity").unwrap(),
                            (_, Some(custom)) => {
                                if let Some(entry) = custom.get(resource) {
                                    entry
                                } else {
                                    unimplemented!("{}", resource)
                                }
                            }
                            (_, None) => unimplemented!(),
                        }
                    }
                })
                .map(Option::from)
                .collect::<Vec<Option<&wgpu::BindGroupLayout>>>();

            let wm_bind_groups = pipeline_config
                .bind_groups
                .iter()
                .enumerate()
                .map(|(vec_index, (slot, def))| match def {
                    BindGroupDef::Entries(entries) => {
                        let entries = entries
                            .iter()
                            .flat_map(|(index, resource_id)| {
                                let resource = self.resources.get(resource_id).unwrap();
                                resource.get_bind_group_entries(*index as u32)
                            })
                            .collect::<Vec<wgpu::BindGroupEntry>>();

                        let bind_group =
                            wm.gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                                label: None,
                                layout: bind_group_layouts[vec_index].as_ref().unwrap(),
                                entries: &entries,
                            });

                        (*slot as u32, WmBindGroup::Custom(bind_group))
                    }
                    BindGroupDef::Resource(resource) => {
                        (*slot as u32, WmBindGroup::Resource(resource.clone()))
                    }
                })
                .collect::<Vec<(u32, WmBindGroup)>>();

            // The sizes the shaders themselves declare for these, in bytes: an immediate whose
            // layout is smaller than the struct the shader reads out of it is a draw that reads
            // whatever follows in the buffer. `@pc_section_position` is four four-byte members - the
            // section's position, and the alpha cutoff of the layer being drawn - which is what the
            // terrain shader's `SectionPosition` spells out and what the pass writes there.
            let immediate_size: u32 = pipeline_config
                .immediates
                .iter()
                .map(|(index, name)| {
                    if immediate_size_of(name) == 0 {
                        unimplemented!("immediate {index} ({name}) has no size")
                    }

                    immediate_size_of(name)
                })
                .sum();

            // A pipeline that passes immediates needs the device feature for them, and wgpu answers a
            // layout without it with a validation error rather than a `None` - which, on this path, is
            // the process ending. There is no way to draw such a pipeline differently, so it is
            // skipped with the reason.
            if immediate_size > 0
                && !wm
                    .gpu
                    .device
                    .features()
                    .contains(wgpu::Features::IMMEDIATES)
            {
                log::error!(
                    "wgpu-mc: the render graph's '{pipeline_name}' pipeline passes {immediate_size} \
                     byte(s) of immediates per draw, and this device was created without the \
                     `immediates` feature; skipping it"
                );
                continue;
            }

            let Some(layout) =
                device_call(&format!("the '{pipeline_name}' pipeline layout"), || {
                    wm.gpu
                        .device
                        .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                            label: None,
                            bind_group_layouts: &bind_group_layouts,
                            immediate_size,
                        })
                })
            else {
                continue;
            };

            // A pipeline whose shader cannot be read is skipped, for the same reason the resources above
            // are: the alternative is `unwrap` on a `None` inside a `#[jni_fn]` frame, which aborts the
            // JVM - and a graph missing one pipeline still draws the rest of the frame. `init` has no
            // error to report (it is a `None` for "no such resource" and for "not UTF-8"), so the line
            // names the resource it wanted, which is what the reader needs either way.
            //
            // The name is the pipeline's own unless the config names another, because two pipelines can
            // be one shader with two sets of pipeline state: the two terrain passes are, and a pipeline
            // that was left to look for a shader named after itself was skipped in silence.
            let shader_name = pipeline_config.shader.as_deref().unwrap_or(pipeline_name);
            let shader_resource = ResourcePath(format!("wgpu_mc:shaders/{shader_name}.wgsl"));

            // **Which fragment entry point, and the one pipeline state that is really a shader choice.**
            //
            // `terrain.wgsl` has two: `frag`, which picks between the game's atlas and this side's copy
            // with a per-vertex flag, and `frag_game_atlas`, which samples the game's atlas and nothing
            // else. The second is smaller by three `sample_at_level` call sites, one texture and three
            // samplers, and it is the *source* that is smaller - not a branch naga or a driver has to fold
            // away - which is the whole reason it is a second entry point rather than a pipeline constant.
            //
            // It is chosen only when it is certainly right: the game's atlas is bound, and no face in the
            // arena fell back to this side's copy. See `block::faces_are_all_the_games`, which is the bake
            // answering for what it actually did rather than a guess from the settings.
            //
            // The vertex stage stays one entry point: the flag it writes is a varying `frag_game_atlas`
            // does not read, and naga drops the interface a stage does not use. Splitting the vertex stage
            // too would only save two selects per vertex, against a second copy of the hottest code in
            // the renderer.
            let terrain = matches!(
                &pipeline_config.geometry[..],
                "@geo_terrain" | "@geo_terrain_translucent"
            );

            let frag_entry =
                if terrain && game_atlas_bound() && crate::mc::block::faces_are_all_the_games() {
                    "frag_game_atlas"
                } else {
                    "frag"
                };

            // **Said out loud, because it is not visible in a picture.** Which of the two fragment entry
            // points a terrain pass ended up with is a difference in the shader and not in what it draws -
            // both sample the same texture when every face is the game's - so a log line is the only place
            // a run can be read for it. Once per terrain pipeline per graph build, and a graph is built a
            // handful of times a session.
            if terrain {
                log::info!(
                    "wgpu-mc: the '{pipeline_name}' terrain pass samples {} (shader entry point \
                     `{frag_entry}`)",
                    if frag_entry == "frag_game_atlas" {
                        "the game's block atlas and nothing else"
                    } else {
                        "whichever atlas each face was baked for"
                    }
                );
            }

            let Some(shader) = WgslShader::init(
                &shader_resource,
                &*wm.mc.resource_provider,
                &wm.gpu.device,
                frag_entry.into(),
                "vert".into(),
            ) else {
                log::error!(
                    "wgpu-mc: the render graph's '{pipeline_name}' pipeline could not be built: {} is \
                     missing, or is not a readable WGSL source; skipping it",
                    shader_resource.0
                );
                continue;
            };

            let vertex_buffer = match &pipeline_config.geometry[..] {
                "@geo_terrain" | "@geo_terrain_translucent" => vec![],
                "@geo_entities" => vec![EntityVertex::desc(), InstanceVertex::desc()],
                "@geo_quad" => vec![QuadVertex::desc()],
                "@geo_sun_moon" => vec![SunMoonVertex::desc()],
                "@geo_sky_scatter" | "@geo_sky_stars" | "@geo_sky_fog" => {
                    vec![SkyVertex::desc()]
                }
                _ => {
                    match geometry_vertex_layouts
                        .as_ref()
                        .and_then(|layouts| layouts.get(&pipeline_config.geometry))
                    {
                        None => unimplemented!(),
                        Some(layout) => layout.clone(),
                    }
                }
            }
            .into_iter()
            // `Some` per slot because wgpu 30's `VertexState::buffers` is a list of *optional* layouts:
            // the holes it now allows are how a shader reads a buffer it binds by index without every
            // slot below it having to exist. This side has never had a hole - a geometry names its
            // layouts in order - so every entry is a layout.
            .map(Some)
            .collect::<Vec<_>>();

            let label = pipeline_name.to_string();

            let Some(render_pipeline) = device_call(&format!("the '{label}' pipeline"), || {
                wm.gpu
                        .device
                        .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                        label: Some(&label),
                        layout: Some(&layout),
                        vertex: wgpu::VertexState {
                            module: &shader.module,
                            entry_point: Some("vert"),
                            compilation_options: Default::default(),
                            buffers: &vertex_buffer,
                        },
                        primitive: wgpu::PrimitiveState {
                            topology: wgpu::PrimitiveTopology::TriangleList,
                            strip_index_format: None,
                            front_face: wgpu::FrontFace::Ccw,
                            // The two pipeline-state diagnostics, read here because a pipeline's cull
                            // mode is part of the pipeline - see `TERRAIN_NO_CULL`.
                            cull_mode: if TERRAIN_NO_CULL.load(Ordering::Relaxed) {
                                None
                            } else {
                                Some(wgpu::Face::Back)
                            },
                            unclipped_depth: false,
                            polygon_mode: Default::default(),
                            conservative: false,
                        },
                        depth_stencil: pipeline_config.depth.as_ref().map(|_| {
                            wgpu::DepthStencilState {
                                format: wgpu::TextureFormat::Depth32Float,
                                depth_write_enabled: Some(pipeline_config.depth_write),
                                // The other half of that diagnostic - see `TERRAIN_GREATER_DEPTH`. It
                                // asks the depth test the opposite question, which is what "the faces
                                // behind are the ones drawn" would mean if the depth values were the
                                // wrong way round.
                                depth_compare: Some(if TERRAIN_GREATER_DEPTH.load(Ordering::Relaxed) {
                                    wgpu::CompareFunction::Greater
                                } else {
                                    // `DepthStencilState.DEFAULT`, which is what both of the game's
                                    // terrain pipelines are built with: `LESS_THAN_OR_EQUAL`, not
                                    // `LESS`. A face that lands on a plane something else has already
                                    // written - two blocks sharing a boundary, a model's face flush
                                    // with the block below it - is a draw the game keeps and a `LESS`
                                    // test drops, and the pixels it drops are a pattern that follows
                                    // the camera rather than anything in the world.
                                    wgpu::CompareFunction::LessEqual
                                }),
                                stencil: wgpu::StencilState::default(),
                                bias: Default::default(),
                            }
                        }),
                        multisample: Default::default(),
                        fragment: Some(wgpu::FragmentState {
                            module: &shader.module,
                            entry_point: Some("frag"),
                            compilation_options: Default::default(),
                            targets: &pipeline_config
                                .output
                                .iter()
                                .map(|_| {
                                    Some(wgpu::ColorTargetState {
                                        format: match &pipeline_config.output_format[..] {
                                            "bgra8unorm" => wgpu::TextureFormat::Bgra8Unorm,
                                            "bgra8unorm_srgb" => wgpu::TextureFormat::Bgra8UnormSrgb,
                                            "rgba8unorm" => wgpu::TextureFormat::Rgba8Unorm,
                                            "rgba8unorm_srgb" => wgpu::TextureFormat::Rgba8UnormSrgb,
                                            "r16float" => wgpu::TextureFormat::R16Float,
                                            "rgba16float" => wgpu::TextureFormat::Rgba16Float,
                                            other => unimplemented!(
                                                "Unknown output format {other}; the pass would have to \
                                                 be built against the format of the texture it draws \
                                                 into, and a mismatch is a validation error at the first \
                                                 draw"
                                            ),
                                        },
                                        blend: Some(match &pipeline_config.blending[..] {
                                            "alpha_blending" => wgpu::BlendState::ALPHA_BLENDING,
                                            "premultiplied_alpha_blending" => {
                                                wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING
                                            }
                                            "replace" => wgpu::BlendState::REPLACE,
                                            "color_add_alpha_blending" => wgpu::BlendState {
                                                color: wgpu::BlendComponent {
                                                    src_factor: wgpu::BlendFactor::SrcAlpha,
                                                    dst_factor: wgpu::BlendFactor::One,
                                                    operation: wgpu::BlendOperation::Add,
                                                },
                                                alpha: wgpu::BlendComponent {
                                                    src_factor: wgpu::BlendFactor::One,
                                                    dst_factor: wgpu::BlendFactor::Zero,
                                                    operation: wgpu::BlendOperation::Add,
                                                },
                                            },
                                            _ => unimplemented!("Unknown blend state"),
                                        }),
                                        write_mask: Default::default(),
                                    })
                                })
                                .collect::<Vec<_>>(),
                        }),
                        cache: None,
                        multiview_mask: None,
                        })
            }) else {
                continue;
            };

            // A terrain pass claims a draw-buffer slot of its own, and claimed here rather than at the
            // top of the loop because every `continue` above is a pipeline that will not draw: a
            // skipped pass that had already taken a slot would leave the pass after it without one.
            //
            // Running out is not a picture that is wrong, it is a pass that draws nothing - so it is
            // said out loud. `SECTION_DRAW_SLOTS` is three and the shipped graph has three terrain
            // pipelines; a test in `wgpu-mc-jni` counts them in `graph.yaml` so that a fourth is a
            // failing test rather than an invisible layer.
            let draw_slot = if matches!(
                &pipeline_config.geometry[..],
                "@geo_terrain" | "@geo_terrain_translucent"
            ) {
                let slot = next_draw_slot;
                next_draw_slot += 1;

                if slot >= crate::mc::SECTION_DRAW_SLOTS {
                    log::error!(
                        "wgpu-mc: the render graph's '{pipeline_name}' is terrain pipeline {} and this \
                         build has {} draw-buffer slot(s) for them; skipping it, because sharing a slot \
                         with a pass recorded in the same submission draws that pass's sections",
                        slot + 1,
                        crate::mc::SECTION_DRAW_SLOTS
                    );
                    continue;
                }

                Some(slot)
            } else {
                None
            };

            self.pipelines.insert(
                pipeline_name.clone(),
                BoundPipeline {
                    pipeline: render_pipeline,
                    bind_groups: wm_bind_groups,
                    // The sizes are the ones the layout was just built with, kept beside the offsets so
                    // a draw can check that the data it hands over is the data the slot holds. See
                    // `set_immediates`.
                    immediates: pipeline_config
                        .immediates
                        .iter()
                        .map(|(index, name)| (*index as u32, immediate_size_of(name)))
                        .collect(),
                    config: pipeline_config.clone(),
                    draw_slot,
                },
            );
        }
    }

    pub fn new(
        wm: &WmRenderer,
        config: ShaderPackConfig,
        mut resources: HashMap<String, ResourceBacking>,
        custom_bind_groups: Option<HashMap<String, &wgpu::BindGroupLayout>>,
        custom_geometry: Option<HashMap<String, Vec<wgpu::VertexBufferLayout>>>,
    ) -> Self {
        for (resource_id, shorthand) in &config.resources.resources {
            match shorthand {
                ShorthandResourceConfig::Int(_) => {}
                ShorthandResourceConfig::Float(_) => {}
                ShorthandResourceConfig::Mat3(_) => {}
                ShorthandResourceConfig::Mat4(_) => {}
                ShorthandResourceConfig::Longhand(LonghandResourceConfig { typed, .. }) => {
                    match typed {
                        TypeResourceConfig::Blob { .. } => {}
                        TypeResourceConfig::Texture3d { .. } => {}
                        TypeResourceConfig::Texture2d { src } => {
                            // A texture that cannot be read is left unregistered rather than
                            // unwrapped: this runs inside a `#[jni_fn]` frame, where a panic aborts the
                            // JVM, and a pipeline that names it is then skipped by `create_pipelines` -
                            // which is one pipeline fewer and a line in the log, not a game that ends
                            // the first time it draws a world.
                            let Some(bytes) = wm
                                .mc
                                .resource_provider
                                .get_bytes(&ResourcePath::from(&src[..]))
                            else {
                                log::warn!(
                                    "wgpu-mc: the render graph's {resource_id} names {src}, which \
                                     this build cannot read; skipping it"
                                );
                                continue;
                            };

                            let tav = match TextureAndView::from_image_file_bytes(
                                &wm.gpu,
                                &bytes,
                                resource_id,
                            ) {
                                Ok(tav) => tav,
                                Err(err) => {
                                    log::warn!(
                                        "wgpu-mc: the render graph's {resource_id} ({src}) could not \
                                         be decoded: {err}; skipping it"
                                    );
                                    continue;
                                }
                            };

                            resources.insert(
                                resource_id.clone(),
                                ResourceBacking::Texture2D(Arc::new(tav)),
                            );
                        }
                        TypeResourceConfig::TextureDepth => {}
                        TypeResourceConfig::F32 { .. } => {}
                        TypeResourceConfig::F64 { .. } => {}
                        TypeResourceConfig::I64 { .. } => {}
                        TypeResourceConfig::I32 { .. } => {}
                        TypeResourceConfig::Mat3(_) => {}
                        TypeResourceConfig::Mat4(_) => {}
                    }
                }
            }
        }

        let mut graph = Self {
            config,
            pipelines: LinkedHashMap::new(),
            resources,
            terrain_frame: std::cell::RefCell::new(TerrainFrame {
                // One entry per pass slot, so a pass can index its own draws by the slot it claimed.
                passes: (0..crate::mc::SECTION_DRAW_SLOTS)
                    .map(|_| PassDraws::default())
                    .collect(),
                ..TerrainFrame::default()
            }),
        };

        // The sampler is always available; the atlas only once a resource reload has baked one. The
        // two travel together - a pipeline that samples the atlas without it is skipped by
        // `create_pipelines`, which is a graph with one pipeline fewer rather than no renderer.
        graph.resources.insert(
            "@sampler".into(),
            ResourceBacking::Sampler(wm.mc.texture_manager.default_sampler.clone()),
        );

        // The game's own block atlas, for the faces whose sprite the game animates - fire, lava, the
        // campfire, a lantern - and the sampler those faces are drawn with.
        //
        // **The filters are not decided here.** Both block atlases go through
        // `crate::render::atlas::block_atlas_sampler`, so this one and `TextureManager`'s cannot drift
        // apart - and drifting apart is the failure this arrangement exists for. There was a revision
        // where only this sampler was bilinear, while the blocks beside every animated sprite were point
        // sampled, and a player handed that path said "these are all blurry, there is none of the
        // game's crisp pixels left" about the fire, the lava and every other animated sprite. One
        // function, two callers, one answer.
        //
        // The answer it currently gives is `Nearest`, on both filters, which is a **step back from the
        // game's own sampler** and is where this stands until the distant-lava flicker is settled:
        // changing the sampling rate while debugging a sample-frequency artifact moves the thing being
        // measured. The game's own is `CLAMP_TO_EDGE, FilterMode.LINEAR, FilterMode.LINEAR` plus the
        // video settings' anisotropy - `LevelRenderer` builds one and `ChunkSectionsToRender#renderGroup`
        // binds the block atlas with it for *both* groups - and the full note, including why the
        // texture-filtering option does not turn that off, is on `block_atlas_sampler`.
        //
        // The address mode is the one thing that is this site's own: the game asks for `ClampToEdge`.
        //
        // The lightmap below stays `LINEAR`, which is the game's own choice for it: its `Sampler2` is
        // bound `getClampToEdge(FilterMode.LINEAR)` on the line after the block atlas's. It is a 16x16
        // lookup table rather than a texture, so it is not part of the question above.
        //
        // Always registered, even before the JVM has handed the atlas over: a named resource that is
        // missing is a pipeline the graph *skips* (`create_pipelines`), and losing the whole terrain
        // pass because an animated texture is not ready yet is not a trade worth making. Until then
        // the binding is a one-texel white texture, which nothing samples - a face is only baked with
        // the game's coordinates once it has been handed over (`GAME_ATLAS_BOUND`), and the graph is
        // built again - with the real atlas in this slot - before any such face can be drawn.
        graph.resources.insert(
            "@sampler_mc_block_atlas".into(),
            ResourceBacking::Sampler(Arc::new(
                wm.gpu
                    .device
                    .create_sampler(&crate::render::atlas::game_atlas_sampler()),
            )),
        );

        // **The magnification pair, which is the only way to get the game's own crispness back.**
        //
        // The game magnifies with `GL_NEAREST` and minifies with `GL_LINEAR_MIPMAP_LINEAR`, and in GL
        // those are independent. wgpu is not: any `anisotropy_clamp` above 1 requires the min, mag *and*
        // mipmap filters to be linear, and it enforces that by returning an error from `create_sampler`.
        // So one sampler cannot be both crisp up close and anisotropic at distance, and a `Nearest` one
        // pays for its crispness with the fluid shimmer.
        //
        // Two samplers is the way out, and it is not a compromise: **anisotropic filtering only means
        // anything on the minification side**, where a surface is sampled at several points and the
        // derivative is a lie in one direction. A magnified surface needs none of it, so these two are
        // each used over exactly the range it is for. The shader picks between them per fragment on
        // whether the coordinates are being stretched or squeezed - see the terrain shaders.
        //
        // Registered here, beside the anisotropic one, so the three cannot drift apart, and always
        // registered for the same reason that one is: a named resource that is missing is a pipeline the
        // graph skips, which would cost the whole terrain pass.
        graph.resources.insert(
            "@sampler_mc_block_atlas_magnify".into(),
            ResourceBacking::Sampler(Arc::new(wm.gpu.device.create_sampler(
                &crate::render::atlas::atlas_magnify_sampler(wgpu::AddressMode::ClampToEdge),
            ))),
        );

        graph.resources.insert(
            "@sampler_block_atlas_magnify".into(),
            ResourceBacking::Sampler(Arc::new(wm.gpu.device.create_sampler(
                &crate::render::atlas::atlas_magnify_sampler(wgpu::AddressMode::Repeat),
            ))),
        );

        // **The third pair: a minified *animated* face.**
        //
        // Its magnification is `Linear` rather than `Nearest`, because nearest magnification of a coarse
        // level is what turns a moving sprite into one flat patch of its own running average - stepping as
        // the frames advance rather than fading - and it has no anisotropy, because a `Nearest` mip filter
        // beside one is a validation error rather than a picture. See
        // `atlas_animated_minified_sampler` for why neither existing pair could express this.
        graph.resources.insert(
            "@sampler_mc_block_atlas_animated".into(),
            ResourceBacking::Sampler(Arc::new(wm.gpu.device.create_sampler(
                &crate::render::atlas::atlas_animated_minified_sampler(
                    wgpu::AddressMode::ClampToEdge,
                ),
            ))),
        );

        graph.resources.insert(
            "@sampler_block_atlas_animated".into(),
            ResourceBacking::Sampler(Arc::new(wm.gpu.device.create_sampler(
                &crate::render::atlas::atlas_animated_minified_sampler(wgpu::AddressMode::Repeat),
            ))),
        );

        let game_atlas = game_block_atlas();

        match &game_atlas {
            Some(view) => {
                graph.resources.insert(
                    "@texture_mc_block_atlas".into(),
                    ResourceBacking::TextureView(view.clone()),
                );
            }
            None => {
                // A white texel, made here rather than left out: see the comment above.
                match TextureAndView::from_rgb_bytes(
                    &wm.gpu,
                    &[0xff, 0xff, 0xff, 0xff],
                    wgpu::Extent3d {
                        width: 1,
                        height: 1,
                        depth_or_array_layers: 1,
                    },
                    Some("wgpu-mc: no game block atlas yet"),
                    wgpu::TextureFormat::Rgba8Unorm,
                    1,
                ) {
                    Ok(placeholder) => {
                        graph.resources.insert(
                            "@texture_mc_block_atlas".into(),
                            ResourceBacking::Texture2D(Arc::new(placeholder)),
                        );
                    }
                    Err(err) => {
                        log::warn!(
                            "wgpu-mc: the placeholder for the game's block atlas could not be \
                             created ({err}), so the terrain pipeline cannot be built"
                        );
                    }
                }
            }
        }

        // The game's lightmap, and the sampler the game samples it with: `Sampler2` for the terrain is
        // `getClampToEdge(LINEAR)`, and the texture has one mip level, so the clamp is a formality.
        //
        // Always registered, like the block atlas above and for the same reason - a missing resource is
        // a *skipped pipeline* - but with a different fallback: the game may not have handed one over
        // yet on the first build, and a lightmap of one white texel would draw the world at full
        // brightness. The fallback is the 16x16 curve this shader used before it sampled the game's, so
        // an early frame or a failed handover is the picture this renderer has always drawn.
        graph.resources.insert(
            "@sampler_game_lightmap".into(),
            ResourceBacking::Sampler(Arc::new(wm.gpu.device.create_sampler(
                &wgpu::SamplerDescriptor {
                    label: Some("wgpu-mc: the game's lightmap"),
                    address_mode_u: wgpu::AddressMode::ClampToEdge,
                    address_mode_v: wgpu::AddressMode::ClampToEdge,
                    address_mode_w: wgpu::AddressMode::ClampToEdge,
                    mag_filter: wgpu::FilterMode::Linear,
                    min_filter: wgpu::FilterMode::Linear,
                    mipmap_filter: wgpu::MipmapFilterMode::Nearest,
                    lod_min_clamp: 0.0,
                    lod_max_clamp: 0.0,
                    compare: None,
                    anisotropy_clamp: 1,
                    border_color: None,
                },
            ))),
        );

        let game_lightmap = game_lightmap();

        if game_lightmap.is_none() {
            log::warn!(
                "wgpu-mc: the game's lightmap has not been handed over yet; the terrain is drawn with \
                 this renderer's own light curve until it is (see `GAME_LIGHTMAP`)"
            );
        }

        match &game_lightmap {
            Some(view) => {
                graph.resources.insert(
                    "@texture_game_lightmap".into(),
                    ResourceBacking::TextureView(view.clone()),
                );
            }
            None => {
                let fallback = fallback_lightmap();

                match TextureAndView::from_rgb_bytes(
                    &wm.gpu,
                    &fallback,
                    wgpu::Extent3d {
                        width: 16,
                        height: 16,
                        depth_or_array_layers: 1,
                    },
                    Some("wgpu-mc: no game lightmap yet"),
                    wgpu::TextureFormat::Rgba8Unorm,
                    1,
                ) {
                    Ok(placeholder) => {
                        graph.resources.insert(
                            "@texture_game_lightmap".into(),
                            ResourceBacking::Texture2D(Arc::new(placeholder)),
                        );
                    }
                    Err(err) => {
                        log::warn!(
                            "wgpu-mc: the fallback lightmap could not be created ({err}), so the \
                             terrain pipeline cannot be built"
                        );
                    }
                }
            }
        }

        match wm.mc.texture_manager.atlases.read().get(BLOCK_ATLAS) {
            Some(block_atlas) => {
                graph.resources.insert(
                    "@texture_block_atlas".into(),
                    ResourceBacking::Texture2D(block_atlas.texture.clone()),
                );
            }
            None => {
                log::warn!(
                    "wgpu-mc: no block atlas is registered yet, so the render graph's terrain \
                     pipeline cannot be built; a resource reload that bakes the atlas rebuilds it"
                );
            }
        }

        graph.create_pipelines(wm, custom_bind_groups, custom_geometry);

        graph
    }

    /// The layers each terrain pass draws, in the order it draws them - keyed by the geometry name a
    /// pipeline in `graph.yaml` asks for.
    ///
    /// **Two passes, because Minecraft has two.** Its own terrain is drawn as two *groups*
    /// (`ChunkSectionLayerGroup`): `OPAQUE` is the solid layer and then the cutout one inside a single
    /// render pass, and `TRANSLUCENT` is the translucent layer in a pass of its own - which is where
    /// water and every other blending block model ends up, because a face that blends cannot be drawn in
    /// the pass that writes opaque depth. So the two are split the same way here, and each name below
    /// stands in for one group.
    ///
    /// The two need different *pipeline state* and not just different ranges: the opaque two neither
    /// blend nor are drawn back to front and both write depth, while the translucent one blends, must not
    /// write depth, and is drawn far-to-near. That is `graph.yaml`'s job - the two pipelines there differ
    /// in `blending` and in `depth_write` - which is why this split is along the pipeline and not along
    /// the layer.
    ///
    /// Each entry is `(layer, the alpha cutoff that layer's own Minecraft pipeline declares)`. The
    /// cutoffs are Minecraft's own numbers: `SOLID_TERRAIN` declares no `ALPHA_CUTOUT` at all (which is a
    /// cutoff of zero, a test no alpha fails), `CUTOUT_TERRAIN` declares 0.5, and `TRANSLUCENT_TERRAIN`
    /// declares **0.01** - small, and not zero: a translucent texture with a *nearly* empty texel has to
    /// leave a hole, and a mip level of one has a small non-zero alpha everywhere, so a cutoff of zero
    /// would paint the holes in.
    ///
    /// **Keyed on the pipeline and not on the geometry**, because the opaque group is now two pipelines
    /// over one geometry: `terrain_solid` draws the solid layer and `terrain` draws the cutout one, and
    /// the split exists for one reason - only the cutout shader has a `discard` in it, and a `discard`
    /// costs the whole pipeline its early-Z. See `terrain_solid.wgsl` and the two pipelines in
    /// `graph.yaml`. Asking the *geometry* would hand both pipelines both layers and put the solid layer
    /// straight back through the shader with the test in it.
    fn terrain_layers(pipeline_name: &str) -> &'static [(RenderLayer, f32)] {
        match pipeline_name {
            // The cutout half of Minecraft's opaque group: `CUTOUT_TERRAIN`, cutoff 0.5.
            "terrain" => &[(RenderLayer::Cutout, 0.5)],
            // The solid half, whose shader declares no cutoff at all. The number here is written down
            // for the push constant to carry, and nothing in that shader reads it.
            "terrain_solid" => &[(RenderLayer::Solid, 0.0)],
            "@geo_terrain_translucent" | "translucent_terrain" => {
                &[(RenderLayer::Transparent, 0.01)]
            }
            // Every other pipeline draws no terrain layers at all, which is what the callers of this
            // that ask about `RenderLayer::Solid` are relying on: the opaque group is the one that
            // claims the frame's depth, and a pass that draws none must not take it.
            _ => &[],
        }
    }

    /// Whether a section is worth submitting, and if not, why not.
    ///
    /// Split out of the draw loop so the one distinction that matters can be tested: `None` is "the JVM
    /// has not said", which is a frustum question, and `Some` is the game's own answer, which is obeyed
    /// as it stands **including when it is empty**. An empty set meaning "not told" is the bug this
    /// variant exists to make impossible - it draws the whole arena for a frame in which the game
    /// deliberately culled everything, and it reads as "occlusion culling does nothing".
    ///
    /// **And a third answer, for a section the list has not had the chance to judge.** The list is a
    /// snapshot between refills - `LevelRenderer#applyFrustum` runs only when the camera has turned by
    /// more than two degrees or the occlusion graph reports a change - so an unnamed section that entered
    /// the arena *after* the last refill is unnamed because the answer is old, not because the game
    /// looked and said no. Asking the frustum about it is the difference between drawing it and leaving a
    /// 16x16x16 hole in a frame that neither renderer draws it in. Every other unnamed section is still
    /// `OutOfSight`, so the culling itself is untouched. See `Scene::sections_since_the_list`.
    fn section_visibility(
        visible: Option<&std::collections::HashSet<glam::IVec3>>,
        sections_since_the_list: &std::collections::HashSet<glam::IVec3>,
        pos: &glam::IVec3,
    ) -> SectionVisibility {
        match visible {
            None => SectionVisibility::AskTheFrustum,
            Some(visible) if visible.contains(pos) => SectionVisibility::Draw,
            Some(_) if sections_since_the_list.contains(pos) => {
                // Counted here rather than at the call site, because this is the only place the reason
                // for the answer is known. See [`SECTIONS_TOO_NEW`].
                SECTIONS_TOO_NEW.fetch_add(1, Ordering::Relaxed);

                SectionVisibility::AskTheFrustum
            }
            Some(_) => SectionVisibility::OutOfSight,
        }
    }

    /// Puts the gather's list in the order the layers being drawn need it in.
    ///
    /// **Far to near when one of them blends**, and untouched otherwise. The translucent layer is the
    /// only one that cares: it is drawn with `depth_write: false` and blending on, so each face mixes
    /// with whatever is already in the target and has to be drawn before the face behind it. The opaque
    /// two write depth and are then ordered by the depth test, so their order is free.
    ///
    /// `sort_unstable_by` because a `VisibleSection` is a position, six `u32`s and a `f32`: two entries
    /// with the same distance are two sections at the same distance, and which of them goes first is not
    /// a fact about either. `total_cmp` rather than `partial_cmp` because a `f32` that is NaN would make
    /// the ordering a partial one, and a sort that wants a total order given a partial one is a panic or
    /// a silent mess; `total_cmp` is a total order over every bit pattern a distance can hold.
    fn sort_for_drawing(list: &mut [VisibleSection], layers: &[(RenderLayer, f32)]) {
        if layers
            .iter()
            .any(|(layer, _)| *layer == RenderLayer::Transparent)
        {
            list.sort_unstable_by(|a, b| b.distance_squared.total_cmp(&a.distance_squared));
        }
    }

    /// Records the graph's passes into `encoder`, one pass per pipeline, in the order the config lists
    /// them.
    ///
    /// `depth_override` is the depth texture the passes that name `@texture_depth` attach instead of
    /// the scene's own: the caller is the one that knows what the frame before and after this pass
    /// wrote, and a terrain pass that does not share their depth buffer is geometry the rest of the
    /// frame cannot occlude. `None` uses the scene's texture, which is the standalone case.
    /// The same, with the culling frustum built from a view-projection matrix.
    ///
    /// A caller that has the camera's matrices and nothing else is the common case - the JNI side is
    /// handed one from Minecraft's camera once a frame - and the frustum's planes are derived from
    /// that same matrix, so building it here keeps the matrix convention in one place. The matrix is
    /// column-major, which is the order `glam`, `joml` and the uniform buffer all agree on.
    ///
    /// `model_translation` is the translation of the model matrix the pass draws with, and it is what
    /// makes the *culling* agree with the drawing: the frustum's planes are relative to the camera,
    /// while a section is named by absolute block coordinates, so a box built from the name alone sits
    /// `camera.y` blocks away from where the shader draws it - above the camera rather than below it,
    /// which culls the ground under the player and keeps the sky. See [`Self::render`].
    #[allow(clippy::too_many_arguments)]
    pub fn render_with_mvp(
        &self,
        wm: &WmRenderer,
        encoder: &mut wgpu::CommandEncoder,
        scene: &Scene,
        render_target: &wgpu::TextureView,
        depth_override: Option<&wgpu::TextureView>,
        clear_color: [f32; 3],
        view_projection: [[f32; 4]; 4],
        model_translation: [f32; 3],
    ) {
        self.render_with_mvp_only(
            wm,
            encoder,
            scene,
            render_target,
            depth_override,
            clear_color,
            view_projection,
            model_translation,
            None,
        );
    }

    /// How many direction changes this frame's walk may take, or `None` to draw from the game's list.
    ///
    /// `None` is the default - `adv_culling` at zero - and it is also the answer for a camera this side
    /// cannot seed a walk from. The walk starts in the camera's own section and is bounded by the world's
    /// own box, so a camera above the build limit or below the level has a box that refuses its own seed.
    /// Minecraft's own graph handles the same case by seeding a whole plane of sections at the level's
    /// edge; this walk has one seed, so the honest thing is to draw from the game's list this frame -
    /// which is what the frame does with the setting off - rather than to draw nothing at all. The
    /// cross-check says when that is happening, once a second.
    fn terrain_walk_budget(camera_section: IVec3) -> Option<u8> {
        let culling = ADV_CULLING.load(Ordering::Relaxed).min(16) as u8;

        if culling == 0 {
            return None;
        }

        // No box yet - the first frame of a run, or a caller that never pushes one - is not a reason to
        // refuse: the walk is then bounded by the frustum alone, as it was before the box existed.
        match crate::mc::world_extent::get() {
            None => Some(culling - 1),
            Some(extent) if !extent.holds_the_layer(camera_section.y) => None,
            Some(_) => Some(culling - 1),
        }
    }

    /// **The frame's one pass over the world**: the gather, every terrain pass's records, and the
    /// uploads, all built together so the frame's three passes do not each do it.
    ///
    /// Called from a terrain pass when the frame's key does not match: once a frame in practice, since
    /// [`begin_frame`] is what advances the key's frame, and once more if a *second* view is drawn
    /// inside one frame. That second case is why a pass that has already drawn keeps the records it has,
    /// which [`PassDraws::drawn`] is about.
    ///
    /// The gather is the reason this exists. It walks the arena's `HashMap` and tests every section
    /// against the frustum and against the game's occlusion list, and **none of that depends on which
    /// layer is being drawn** - so three passes over one frame produced the same list three times. The
    /// records do depend on the layer, which is why they are built per pass from the one list.
    fn rebuild_terrain_frame(
        &self,
        frame: &mut TerrainFrame,
        wm: &WmRenderer,
        scene: &Scene,
        frustum: &Frustum<f32>,
        key: TerrainFrameKey,
        honour_occlusion: bool,
    ) {
        let camera_section = key.camera_section;

        // How many direction changes this renderer's own walk may take, or `None` to draw from the game's
        // list. Read once a frame: it decides the walk below and nothing about a pipeline.
        let budget = Self::terrain_walk_budget(camera_section);

        // **The flood is run beside the game's answer once a second, and it decides nothing.** This is
        // the one place the frame's frustum, the camera's section and the arena are all in hand. See
        // `cross_check_the_flood`.
        //
        // **Gated on the diagnostic-log switch, because it is not free either.** The cross-check walks up
        // to four budgets a second, which measured 8 ms of frame thread a second at render distance 16 and
        // 33 ms at 32 - a hitch, once a second, in a feature that is off by default. The line it writes is
        // a diagnostic of the walk, so the switch that turns diagnostics on is where it belongs, and with
        // that switch off this costs nothing at all.
        //
        // **And it is outside the timing below**, which is the one thing about it worth saying here: it runs
        // inside this function, once a second, and it costs tens of milliseconds when it does - so a build
        // time that included it would be a measurement of the diagnostic in the second it fired rather than
        // of the frame. The numbers this function reports are the ones a session without the switch pays.
        if crate::mc::chunk::DIAGNOSTIC_LOGGING.load(Ordering::Relaxed) {
            cross_check_the_flood(scene, frustum, camera_section);
        }

        // The whole call from here is timed, and the gather and the record loop inside it are timed
        // separately: the first number is what a rebuilt frame costs on the render thread, and the other two
        // are the halves of it - the walk of the arena, and building and uploading one set of records per
        // pass. There is no early return in this function to miss, which is why one pair of reads is enough.
        // See [`TERRAIN_BUILD_NANOS`].
        let build_started = std::time::Instant::now();

        // The areas this frame's sections fall in, and their frustum tables: one entry per 128-block
        // region, which is a handful against the thousands of sections inside them.
        // ---- the gather, once ----

        let mut culled = 0u64;
        let mut out_of_sight = 0u64;

        // **Which plane culled the last section**, handed back to the frustum test as its cache index.
        // The crate returns the plane that answered "outside" (`coherent_test_against_frustum`'s second
        // value) and takes it back as a hint, so it is tried first: a run of sections on one side of the
        // view - which is what a frustum's edge is, section after section in one plane - is then culled by
        // one plane test instead of six. It is a hint and not a filter: every plane is still tested before
        // anything is culled, which is what makes this an ordering rather than a second cull.
        let mut culled_by: u8 = 0;
        let mut hint_hits = 0u64;
        let mut hint_misses = 0u64;
        frame.visible.clear();

        let gather_started = std::time::Instant::now();

        {
            // **Read locks, and they are held for the walk only.** The arena's is a read lock because
            // nothing under it writes - it was `write()`, which took the exclusive lock for the whole of
            // the gather once a frame while every bake thread's `allocate` needs that same lock.
            let sections = scene.section_storage.read();
            let visible = scene.visible_sections.read();

            // **And which sections are too new for the list to have judged.** Taken in the same order the
            // drain takes them in (the arena first), so the two can never deadlock: the drain inserts into
            // this set at the publish, under the arena's write lock. See `section_visibility`.
            let sections_since_the_list = scene.sections_since_the_list.read();

            // **The walk's own answer, when the setting asks for it.** One flood per frame, into a set
            // the loop below tests by position: the walk is a map of a few thousand sections while the
            // loop visits every section the arena holds, so building the set is the cheap side of that.
            //
            // The answers are snapshotted rather than read through the lock per poll - a walk polls
            // thousands of positions and the arena's guard is already held here.
            let flooded: Option<std::collections::HashSet<IVec3>> = budget.map(|budget| {
                let answers = crate::mc::visibility::snapshot();
                let world = FloodWorld {
                    answers: &answers,
                    sections: &sections,
                    frustum,
                    camera_section,
                };

                section_graph::flood(&world, camera_section, budget)
                    .visible
                    .into_iter()
                    .collect()
            });

            for (pos, section) in sections.iter() {
                if honour_occlusion {
                    // **Which list this frame obeys.** The walk's answer when the setting asks for one -
                    // and it is a *subset* of the game's, so what it drops is the whole of what the
                    // setting does - and the game's own list otherwise, which is the default.
                    //
                    // The walk's path does not go through `section_visibility`, so the stale-snapshot
                    // answer that function gives is not asked for here: the walk starts from the camera's
                    // own section every frame and never reads a list that can be a frame old.
                    let drawn = match flooded.as_ref() {
                        Some(drawn) => drawn.contains(pos),
                        None => {
                            Self::section_visibility(
                                visible.as_ref(),
                                &sections_since_the_list,
                                pos,
                            ) != SectionVisibility::OutOfSight
                        }
                    };

                    if !drawn {
                        out_of_sight += 1;
                        continue;
                    }
                }
                // The section's position *relative to the camera's section*: the view matrix carries the
                // camera's offset within its own section and nothing else, so a draw is placed by naming
                // where it is with the big part taken out of it. See [`TerrainFrameKey::camera_section`].
                let rel_pos = *pos - camera_section;

                // The box the section occupies *where the shader draws it*: the same
                // camera-section-relative position the immediate carries, times sixteen to the block
                // units the frustum is measured in. A box built from the absolute name instead would be
                // thousands of blocks from the geometry it stands for, and the sections around the camera
                // would be culled out of their own frame.
                let a: Vec3<f32> = [
                    rel_pos.x as f32 * 16.0,
                    rel_pos.y as f32 * 16.0,
                    rel_pos.z as f32 * 16.0,
                ]
                .into();
                let b: Vec3<f32> = a + Vec3::new(16.0, 16.0, 16.0);

                let bounds: AABB<f32> = AABB::new(a.into_array(), b.into_array());

                // Still tested, because it is nearly free and the two disagree in both directions: the
                // game's graph is a frame old and conservative about what a neighbour hides, and a
                // section it left out may be one the camera has since turned toward.
                // The second value is the plane that culled it - or the hint back, for one that was not
                // culled - and it is carried into the next section's test. See `culled_by`.
                let (in_the_frustum, plane) =
                    bounds.coherent_test_against_frustum(frustum, culled_by);

                if plane == culled_by {
                    hint_hits += 1;
                } else {
                    hint_misses += 1;
                }

                culled_by = plane;

                if !in_the_frustum {
                    culled += 1;
                    continue;
                }
                // Which layers the arena actually holds for this section. Both ranges travel, because a
                // draw needs both: the index range to draw and the vertex range to draw it *from*. A
                // section the arena has nothing in for a layer is counted where that layer's records are
                // built, because "the arena has no cutout here" is a fact about the layer.
                let mut ranges: [Option<crate::mc::chunk::DrawnLayer>; 3] = [None, None, None];
                let mut any = false;

                for (layer_index, layer) in section.layers.iter().enumerate() {
                    if let Some(layer) = layer {
                        // The arena the layer lives in travels with its ranges: the draw loop rebinds
                        // when it changes, and drawing a section against another arena is a section of
                        // somebody else's geometry.
                        ranges[layer_index] = Some((
                            layer.buffer,
                            layer.index_range.clone(),
                            layer.vertex_range.start..layer.vertex_range.start + 1,
                        ));
                        any = true;
                    }
                }

                if !any {
                    continue;
                }

                // The distance the translucent layer is sorted by, measured between section centres the
                // way the game measures it - in sections, and squared, because a square root would be
                // thrown away by the comparison it feeds.
                let dx = rel_pos.x as f32;
                let dy = rel_pos.y as f32;
                let dz = rel_pos.z as f32;

                frame.visible.push(VisibleSection {
                    relative_position: rel_pos,
                    ranges,
                    distance_squared: dx * dx + dy * dy + dz * dz,
                });
            }

            // The walk's own two sizes: what it visited and what it kept. Counted here rather than after the
            // block because the arena guard is what knows the first, and this is the one place a frame walks
            // it. See [`TERRAIN_GATHERS`].
            TERRAIN_GATHERS.fetch_add(1, Ordering::Relaxed);
            TERRAIN_WALKED.fetch_add(sections.len() as u64, Ordering::Relaxed);
            TERRAIN_VISIBLE.fetch_add(frame.visible.len() as u64, Ordering::Relaxed);
        }

        TERRAIN_GATHER_NANOS.fetch_add(
            gather_started.elapsed().as_nanos() as u64,
            Ordering::Relaxed,
        );

        frame.culled = culled;
        frame.out_of_sight = out_of_sight;

        // **Far to near, once, for the pass that blends** - and the storage order is left alone for the
        // passes that do not. See [`TerrainFrame::sorted`] for why there are two lists and not one.
        //
        // Always built rather than only when some pass blends: the copy is a few hundred entries and the
        // sort is one a frame, against two arena walks removed, and building it conditionally would make
        // a pass's correctness depend on a predicate in this function agreeing with the one in
        // `terrain_layers`.
        frame.sorted.clear();
        frame.sorted.extend_from_slice(&frame.visible);
        Self::sort_for_drawing(&mut frame.sorted, &[(RenderLayer::Transparent, 0.01)]);

        // The gather-level counts belong to the frame, so they are added here, once. See
        // [`TerrainFrame`].
        TERRAIN_CULLED.fetch_add(culled, Ordering::Relaxed);
        TERRAIN_OUT_OF_SIGHT.fetch_add(out_of_sight, Ordering::Relaxed);

        // **What the plane hint bought**, once a second: a section whose culling plane was the one the
        // section before it was culled by costs one plane test instead of six. See `culled_by`.
        {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_secs())
                .unwrap_or(0);

            if PLANE_HINT_REPORTED.swap(now, Ordering::Relaxed) != now {
                let total = hint_hits + hint_misses;

                log::info!(
                    "wgpu-mc: the frustum cull, last frame: {} section(s) tested, {} of them answered by the \
                     plane the section before was culled by ({:.1}% - one plane test rather than up to six)",
                    total,
                    hint_hits,
                    if total == 0 {
                        0.0
                    } else {
                        100.0 * hint_hits as f64 / total as f64
                    }
                );
            }
        }

        // ---- the records, one set per pass ----

        let chunk_buffers = scene.chunk_buffers.load_full();

        // **A new frame starts with nothing drawn from it**, which is what lets every pass below be
        // rebuilt. The flag means "this pass has already drawn from *this frame's* records" - so it has
        // to be cleared when the records are about to be replaced wholesale, and it has to *not* be
        // cleared when this is a second view inside the same frame, because that is the case it exists
        // for. Without this the flags would stand forever after the first frame and every later frame
        // would draw the first frame's list, one frame's world frozen in place.
        if frame.starts_a_new_frame(&key) {
            for draws in &mut frame.passes {
                draws.drawn = false;
            }
        }

        // Asked once for the frame rather than per pass, and kept on each set of draws: the draw loop has
        // to run the loop the arguments were written for, and this is a setting. See
        // [`PassDraws::batched`].
        let batchable = terrain_batches_draws();

        let mut empty = 0u64;
        let mut layer_empty = [0u64; 3];

        // The other half of the frame's build cost, timed from here to the end of the loop below: the record
        // sets (one per pass, each a walk of the sections the gather kept) and their uploads. See
        // [`TERRAIN_RECORDS_NANOS`].
        let records_started = std::time::Instant::now();

        for (name, pipeline) in self.pipelines.iter() {
            let Some(slot) = pipeline.draw_slot else {
                continue;
            };

            let draws = &mut frame.passes[slot];

            // A pass that has already drawn this frame keeps what it has. See [`PassDraws::drawn`].
            if draws.drawn {
                continue;
            }

            draws.records.clear();
            draws.calls.clear();
            draws.spans.clear();
            draws.empty = [0u64; 3];

            let mut truncated = 0u64;

            // **Which order this pass draws in**, which is the one thing about the lists that is per pass:
            // a pass that draws a blending layer needs far to near, and a pass that does not needs the
            // storage order that keeps its batched calls long. See [`TerrainFrame::sorted`].
            let layers = Self::terrain_layers(name);
            let blending = layers
                .iter()
                .any(|(layer, _)| *layer == RenderLayer::Transparent);
            let sections: &[VisibleSection] = if blending {
                &frame.sorted
            } else {
                &frame.visible
            };

            for (layer_index, _) in layers {
                let layer_index = *layer_index as usize;
                let start = draws.calls.len();

                for section in sections {
                    let Some((arena, index_range, vertex_range)) =
                        section.ranges[layer_index].clone()
                    else {
                        draws.empty[layer_index] += 1;
                        empty += 1;
                        layer_empty[layer_index] += 1;
                        continue;
                    };

                    // An arena the storage knows about and the renderer does not: the two lists are kept
                    // in step, so this is a bug rather than a state - but leaving one section out is
                    // better than panicking mid-pass, which ends the process.
                    if chunk_buffers.get(arena as usize).is_none() {
                        log::warn!(
                            "wgpu-mc: a section names arena {arena}, which the renderer has no buffer \
                             for; skipping it"
                        );
                        continue;
                    }

                    // A draw past the end of the record buffer is a draw with nowhere to describe
                    // itself, so it is left out and counted rather than wrapped or dropped in silence.
                    // See `SECTION_DRAW_CAPACITY` and `report_truncated_draws`.
                    if draws.records.len() >= crate::mc::SECTION_DRAW_CAPACITY {
                        truncated += 1;
                        continue;
                    }

                    let instance = draws.records.len() as u32;

                    draws.records.push(SectionDraw {
                        x: section.relative_position.x,
                        y: section.relative_position.y,
                        z: section.relative_position.z,
                        word_base: vertex_range.start,
                    });

                    draws.calls.push(DrawCall {
                        arena,
                        index_range,
                        instance,
                    });
                }

                draws.spans.push((layer_index, start, draws.calls.len()));
            }

            draws.batched = batchable && the_indirect_path_holds(draws.calls.len());

            // **How close the frame is to the cliff**, and whether it went over it. Only counted while the
            // indirect path is on at all: with `terrain_indirect` off there is no cap to be near, and a
            // peak recorded from a run that never batches would answer a question nobody asked.
            if batchable {
                let calls = draws.calls.len() as u64;
                TERRAIN_CALLS_PEAK.fetch_max(calls, Ordering::Relaxed);

                if !the_indirect_path_holds(draws.calls.len()) {
                    report_the_indirect_cap(calls);
                }
            }

            // ---- the upload, once per pass and before its first draw ----
            //
            // `Queue::write_buffer` is applied at the next `submit`, ahead of everything recorded in that
            // submission - so every write a frame makes lands before every draw in it, which is why each
            // pass has a slot of its own. See `SECTION_DRAW_SLOTS`.
            if !draws.records.is_empty() {
                wm.gpu.queue.write_buffer(
                    &scene.section_draws[slot].buffer,
                    0,
                    bytemuck::cast_slice(&draws.records),
                );

                TERRAIN_UPLOADS.fetch_add(1, Ordering::Relaxed);
                TERRAIN_UPLOAD_BYTES.fetch_add(
                    (draws.records.len() * std::mem::size_of::<SectionDraw>()) as u64,
                    Ordering::Relaxed,
                );
            }

            if draws.batched && !draws.calls.is_empty() {
                let args = draws
                    .calls
                    .iter()
                    .map(|call| wgpu::util::DrawIndexedIndirectArgs {
                        index_count: call.index_range.len() as u32,
                        instance_count: 1,
                        first_index: call.index_range.start,
                        // **Zero on both paths.** `base_vertex` is added to the *vertex* index, and the
                        // index buffer holds the section's own indices - so the arena slot is in the
                        // record instead. See `SectionDraw`.
                        base_vertex: 0,
                        // **Which record this draw is.** The vertex stage reads
                        // `section_draws[instance_index]`, and for an indirect draw this is the only
                        // channel there is for anything per draw.
                        first_instance: call.instance,
                    })
                    .collect::<Vec<_>>();

                wm.gpu.queue.write_buffer(
                    &scene.indirect_buffers[slot],
                    0,
                    bytemuck::cast_slice(&args),
                );

                TERRAIN_UPLOADS.fetch_add(1, Ordering::Relaxed);
                TERRAIN_UPLOAD_BYTES.fetch_add(
                    (args.len() * std::mem::size_of::<wgpu::util::DrawIndexedIndirectArgs>())
                        as u64,
                    Ordering::Relaxed,
                );
            }

            report_truncated_draws(truncated);
        }

        TERRAIN_RECORDS_NANOS.fetch_add(
            records_started.elapsed().as_nanos() as u64,
            Ordering::Relaxed,
        );

        TERRAIN_EMPTY.fetch_add(empty, Ordering::Relaxed);

        for layer in 0..3 {
            LAYER_EMPTY[layer].fetch_add(layer_empty[layer], Ordering::Relaxed);
            LAYER_EMPTY_TOTAL[layer].fetch_add(layer_empty[layer], Ordering::Relaxed);
        }

        TERRAIN_BUILD_NANOS.fetch_add(build_started.elapsed().as_nanos() as u64, Ordering::Relaxed);

        frame.key = Some(key);
    }

    /// The same, recording **the named pipelines and nothing else**.
    ///
    /// The two terrain groups are one graph but two moments in a frame, and the moment is not cosmetic.    /// Minecraft draws its opaque terrain, then its entities and features, then its translucent terrain
    /// (`LevelRenderer#addMainPass`: `renderGroup(OPAQUE)`, `submitEntities`,
    /// `renderTranslucentFeatures`, `renderGroup(TRANSLUCENT)`), and water that is blended into the
    /// frame **before** the entities is water the entities are then drawn on top of - which is exactly
    /// what "a mob in a lake looks like it never entered the water" is.
    ///
    /// So each group is recorded when the game reaches that group's own pass, and each one opens its own
    /// render pass over the frame's colour and depth. `only` is the names of the pipelines to record, or
    /// `None` for the whole graph - which is what a caller drawing a frame of its own wants. It is a
    /// **list** because one of the groups is more than one pipeline now: Minecraft's opaque group draws
    /// the solid layer and the cutout layer, and this side draws them with two pipelines
    /// (`terrain_solid` and `terrain`) so that only the cut-out one carries a `discard`. See
    /// [`RenderGraph::terrain_layers`].
    #[allow(clippy::too_many_arguments)]
    pub fn render_with_mvp_only(
        &self,
        wm: &WmRenderer,
        encoder: &mut wgpu::CommandEncoder,
        scene: &Scene,
        render_target: &wgpu::TextureView,
        depth_override: Option<&wgpu::TextureView>,
        clear_color: [f32; 3],
        view_projection: [[f32; 4]; 4],
        model_translation: [f32; 3],
        only: Option<&[&str]>,
    ) {
        let frustum = Frustum::from_modelview_projection(with_gl_depth_range(view_projection));
        let mut geometry = HashMap::new();

        self.render(
            wm,
            encoder,
            scene,
            render_target,
            depth_override,
            clear_color,
            &mut geometry,
            &frustum,
            model_translation,
            only,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &self,
        wm: &WmRenderer,
        encoder: &mut wgpu::CommandEncoder,
        scene: &Scene,
        render_target: &wgpu::TextureView,
        depth_override: Option<&wgpu::TextureView>,
        clear_color: [f32; 3],
        geometry: &mut HashMap<String, Box<dyn Geometry>>,
        frustum: &Frustum<f32>,
        model_translation: [f32; 3],
        only: Option<&[&str]>,
    ) {
        let arena = WmArena::new(4096);

        let mut should_clear_depth = true;

        for (pipeline_name, bound_pipeline) in &self.pipelines {
            // A name not in the list is skipped, and None means every pipeline is wanted.
            if only.is_some_and(|only| !only.contains(&pipeline_name.as_str())) {
                continue;
            }

            let pipeline_config = self.config.pipelines.pipelines.get(pipeline_name).unwrap();

            let mut render_pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: None,
                occlusion_query_set: None,
                timestamp_writes: None,
                color_attachments: &pipeline_config
                    .output
                    .iter()
                    .map(|texture_name| {
                        Some(RenderPassColorAttachment {
                            view: match &texture_name[..] {
                                "@framebuffer_texture" => render_target,
                                _ => unimplemented!(),
                            },
                            depth_slice: None,
                            resolve_target: None,
                            ops: Operations {
                                load: if !pipeline_config.clear {
                                    LoadOp::Load
                                } else {
                                    LoadOp::Clear(Color {
                                        r: clear_color[0] as f64,
                                        g: clear_color[1] as f64,
                                        b: clear_color[2] as f64,
                                        a: 1.0,
                                    })
                                },
                                store: StoreOp::Store,
                            },
                        })
                    })
                    .collect::<Vec<_>>(),
                depth_stencil_attachment: pipeline_config.depth.as_ref().map(|depth_texture| {
                    // The caller's texture keeps its own contents: it is already the frame's depth
                    // buffer, and clearing it here would erase whatever the passes before this one
                    // wrote into it.
                    let overridden = if depth_texture == "@texture_depth" {
                        depth_override
                    } else {
                        None
                    };

                    // Whether this pass clears the caller's depth buffer, which is a decision about the
                    // *frame* and not about the pass: the frame's depth belongs to whoever wrote it
                    // first, and every pass after that has to load it. `should_clear_depth` says one has
                    // already claimed it - and `only` is the other half, because the two terrain groups
                    // are recorded at two different moments in one frame and each call starts with
                    // `should_clear_depth` true again. Clearing there would erase the whole opaque
                    // world's depth under the water, and every face drawn after it would be tested
                    // against a bare buffer.
                    //
                    // Which pass claims it is asked of the **layers**, not of a name: the opaque group is
                    // the one that draws solid geometry, and the translucent one draws none.
                    let opaque_group = Self::terrain_layers(pipeline_name)
                        .iter()
                        .any(|(layer, _)| *layer == RenderLayer::Solid);

                    let will_clear_depth = should_clear_depth
                        && overridden.is_none()
                        && only.is_none_or(|_| opaque_group);
                    should_clear_depth = false;

                    let depth_view = match overridden {
                        Some(view) => view,
                        None if depth_texture == "@texture_depth" => {
                            arena.alloc(scene.depth_texture.read().create_view(
                                &wgpu::TextureViewDescriptor {
                                    label: None,
                                    format: Some(wgpu::TextureFormat::Depth32Float),
                                    dimension: Some(wgpu::TextureViewDimension::D2),
                                    usage: Some(wgpu::TextureUsages::RENDER_ATTACHMENT),
                                    aspect: Default::default(),
                                    base_mip_level: 0,
                                    mip_level_count: None,
                                    base_array_layer: 0,
                                    array_layer_count: None,
                                },
                            ))
                        }
                        None => match self.resources.get(depth_texture) {
                            Some(ResourceBacking::Texture2D(view)) => &view.view,
                            _ => unimplemented!("Unknown depth target {}", depth_texture),
                        },
                    };

                    RenderPassDepthStencilAttachment {
                        view: depth_view,
                        depth_ops: Some(Operations {
                            load: if will_clear_depth {
                                LoadOp::Clear(1.0)
                            } else {
                                LoadOp::Load
                            },
                            store: StoreOp::Store,
                        }),
                        stencil_ops: None,
                    }
                }),
                multiview_mask: None,
            });

            match &pipeline_config.geometry[..] {
                // The two terrain passes: the opaque one draws the solid and cutout layers, the
                // translucent one draws the transparent layer. See `terrain_layers` for which, and for
                // why there are two passes rather than one.
                "@geo_terrain" | "@geo_terrain_translucent" => {
                    render_pass.set_pipeline(&bound_pipeline.pipeline);

                    // The pair of draw buffers this pass owns. `None` cannot happen - a terrain pipeline
                    // that could not claim a slot is not in the graph, see `create_pipelines` - and a
                    // pass that drew nothing is a better answer to it than an index panic inside a
                    // `#[jni_fn]` frame, which ends the process.
                    let Some(draw_slot) = bound_pipeline.draw_slot else {
                        log::error!(
                            "wgpu-mc: the '{pipeline_name}' pipeline draws terrain and holds no \
                             draw-buffer slot; drawing nothing for it this frame"
                        );
                        continue;
                    };

                    // Everything the pipeline binds that is *not* the arena: the matrices, the two
                    // atlases and their samplers, the lightmap, the fog block. The arena's own group is
                    // left out here and bound per section, because which arena a draw reads is a property
                    // of the section rather than of the pass.
                    for (index, bind_group) in bound_pipeline.bind_groups.iter() {
                        match bind_group {
                            WmBindGroup::Resource(name) => match &name[..] {
                                // Bound per draw - see `bound_arena`.
                                "@bg_ssbo_chunks" => {}
                                // Bound once for the whole pass, which is the point of the buffer: a
                                // `multi_draw` cannot be told anything per draw, so the section's
                                // position and its arena slot travel in this buffer and the vertex
                                // stage reads them by `instance_index`. Written below, before the first
                                // draw of this pass and after the gather that decides what goes in it.
                                "@bg_section_draws" => {
                                    let Some(slot) = bound_pipeline.draw_slot else {
                                        // Unreachable: a pipeline draws terrain only if it claimed a
                                        // slot, and one that could not is not in the graph at all.
                                        continue;
                                    };

                                    render_pass.set_bind_group(
                                        *index,
                                        &scene.section_draws[slot].bind_group,
                                        &[],
                                    );
                                }
                                _ => unimplemented!(),
                            },
                            WmBindGroup::Custom(bind_group) => {
                                render_pass.set_bind_group(*index, bind_group, &[]);
                            }
                        }
                    }

                    // Arena 0 is bound before the first draw and rebound whenever a section names a
                    // different one: a bind group and an index buffer are per arena, and both have to
                    // move together. `None` rather than `Some(0)` so the first draw always binds -
                    // assuming the initial state is what would make a section of arena 0 draw from
                    // arena 1.
                    let mut bound_arena: Option<u32> = None;

                    // The draw-time counters only, and they are added once, at the end. The gather's own
                    // counts - what the frustum rejected and what the game's list did not name - belong to
                    // the frame and are added where the gather is. See `TerrainFrame`.
                    let mut drawn = 0u64;
                    let mut drawn_total = 0u64;
                    let mut layer_drawn = [0u64; 3];
                    let mut layer_drawn_total = [0u64; 3];

                    // The section the camera is in, which is the origin every draw is placed *relative
                    // to*. The alternative - the section's own absolute position - is a number of up to
                    // thirty million, and `f32` steps by four thousandths of a block out there. That is
                    // four orders of magnitude more than the depth buffer can forgive, and it is what the
                    // entity shadows were fighting with: a shadow's piece is a quad lying on the top face
                    // of a block (`EntityRenderer#extractShadowPiece`), so it is *coplanar* with the face
                    // this pass draws under it, and which of the two the depth test sees is decided by
                    // their last bits. The error was a function of the world position and not of the
                    // camera, which is why the stripes did not move when the camera did.
                    //
                    // Relative to the camera's *section* and not to the camera itself, because the
                    // fractional part of the camera's position is in the view matrix (the JVM builds it
                    // that way, see `TerrainPass`): `section * 16` stays an exact integer here, the
                    // subtraction is of two small integers, and the offset within the section is a
                    // number below sixteen wherever it is applied. Vanilla computes the same sum in its
                    // own vertex shader - `Position + (ChunkPosition - CameraBlockPos) + CameraOffset` -
                    // for the same reason.
                    let camera_section = *scene.camera_section_pos.read();

                    // The frustum the culler below is handed is built from this same view-projection
                    // matrix, so the boxes have to be in the space that matrix reads - which is
                    // camera-section-relative blocks, and that is what the model matrix being the
                    // identity means. Said once rather than once a frame, because a warning that fires
                    // on every pass is a log with nothing else in it.
                    static WARNED_ABOUT_MODEL: std::sync::atomic::AtomicBool =
                        std::sync::atomic::AtomicBool::new(false);

                    if !model_translation.iter().all(|value| *value == 0.0)
                        && !WARNED_ABOUT_MODEL.swap(true, Ordering::Relaxed)
                    {
                        log::warn!(
                            "wgpu-mc: the terrain pass was handed a model matrix with a translation in \
                             it ({model_translation:?}); the culling frustum does not account for one"
                        );
                    }

                    // **The frame's one pass over the world, and this pass is three passes deep.**
                    //
                    // Everything that decides whether a section is drawn at all - is it named by the
                    // game's occlusion graph, is it inside the frustum, does the arena hold the layer - is
                    // asked once per *frame* now, and what comes out is a list of
                    // `(position, ranges, distance)` plus, per pass, the records and draws built from it.
                    // The frame's three terrain passes walk that and nothing else: they do not touch the
                    // arena's `HashMap`, they do not rebuild a box, and they do not re-run the frustum
                    // test. None of that depends on which layer is being drawn, so the three passes used
                    // to produce the same list three times and now produce it once. See
                    // [`TerrainFrame`], which is where it is kept, and [`TerrainFrameKey`], which is what
                    // decides whether it has to be produced again.
                    let key = TerrainFrameKey {
                        frame: FRAME.load(Ordering::Relaxed),
                        // The culler's own planes, which is all it reads of the matrix.
                        frustum: frustum.planes.map(|plane| plane.into_array()),
                        camera_section,
                        visible: scene.visible_sections_revision(),
                        model_translation,
                    };

                    let mut frame = self.terrain_frame.borrow_mut();

                    if frame.key != Some(key) {
                        // **How much of this is the submission counter alone.** `frame` advances at every
                        // submission (`begin_frame`), and what the records must not outlive is the *arena's
                        // deferred free*, which happens once per presented frame in the drain. So a
                        // submission that is not a present - a second flush inside one frame, which
                        // `render stats` reports as more submissions than presented frames - changes
                        // nothing the gather reads, and this counts the rebuilds that were thrown away for
                        // it. Everything else about the key is compared with the frame number pinned.
                        TERRAIN_REBUILDS.fetch_add(1, Ordering::Relaxed);

                        let same_except_the_frame = frame.key.is_some_and(|previous| {
                            TerrainFrameKey {
                                frame: previous.frame,
                                ..key
                            } == previous
                        });

                        if same_except_the_frame {
                            TERRAIN_REBUILDS_FRAME_ONLY.fetch_add(1, Ordering::Relaxed);
                        }

                        self.rebuild_terrain_frame(
                            &mut frame,
                            wm,
                            scene,
                            frustum,
                            key,
                            TERRAIN_OCCLUSION.load(Ordering::Relaxed),
                        );
                    }

                    // **Marked before the draws, not after.** The draws read the record buffer at
                    // submission time rather than here, so a rebuild that happened between this pass's
                    // draws and the next pass's would leave the counts and offsets already recorded
                    // describing a list that is no longer in the buffer. A pass that is about to draw is
                    // therefore a pass a later rebuild leaves alone. See [`PassDraws::drawn`].
                    frame.passes[draw_slot].drawn = true;

                    let draws = &frame.passes[draw_slot];

                    // The arenas, loaded once for the draws and held for all of them: a pass that read
                    // the list at two different moments could bind one buffer and index another. Loaded
                    // *after* the frame's records were built, which is the order that fails safely - an
                    // arena added in between is one the build did not know about, so a section naming it
                    // was left out of the records; the other order is a section left in and indexed into a
                    // list with no such entry, which is a panic inside a `#[jni_fn]` frame.
                    let chunk_buffers = scene.chunk_buffers.load_full();

                    // Half a texel for this side's own atlas, which is a fixed size, and for the game's,
                    // which is not - see the write below for why the two cannot share a number.
                    //
                    // **The half-texel shift, and it is zero - the experiment is over and it lost.**
                    //
                    // It was tried because a face's coordinates run edge to edge, so a sample lands on the
                    // boundary between two texels rather than on one, and two texels at every point would be
                    // softer than one. **It does not make the picture any crisper** - a bilinear filter
                    // interpolates between texel centres wherever it is asked, and half a texel only moves
                    // which of the two a sample is nearer.
                    //
                    // It was left in the tree anyway while the search went on, and a player then reported the
                    // thing it *does* do: **a visible half-texel offset in the block textures**. Which it
                    // should be - half a texel is half a texel, and moving every sample by that much is a
                    // picture shifted by that much. So it goes back to zero, and the two fields stay because
                    // the shader still reads them and a constant of zero is the honest way to say "no shift".
                    //
                    // The plumbing test that proved these values reach the shader is worth keeping in mind:
                    // forty texels moved the terrain visibly, so a shift written here is a shift applied.
                    const HALF_TEXEL_TEXELS: f32 = 0.0;

                    let our_half_texel =
                        HALF_TEXEL_TEXELS / crate::render::atlas::ATLAS_DIMENSIONS as f32;

                    // Asked for once rather than per draw, because the answer is a lock and it changes
                    // only when the game re-stitches its atlas - and because the pair is what the shader
                    // needs: it picks between them with the same flag it picks the atlas with.
                    //
                    // The two are genuinely different numbers rather than one rounded value, and the runs
                    // this was written against are why: the game's stitcher produced 2048x2048 on three
                    // launches and 1024x1024 on the next two, while this side's atlas is always
                    // `ATLAS_DIMENSIONS`. A single "half texel" would be half on one of them and a quarter
                    // on the other.
                    let game_half_texel = game_atlas_size()
                        .map_or(0.0, |(width, _)| HALF_TEXEL_TEXELS / width as f32);

                    // **One texel of each atlas, which is what the magnification test compares the
                    // coordinates' screen-space derivative against.**
                    //
                    // Per atlas, for the reason everything else here is: this side's is
                    // `ATLAS_DIMENSIONS` and never moves, the game's is whatever its stitcher packed - and
                    // a test that called a surface magnified on a 2048 atlas and minified on a 1024 one
                    // would pick the `Nearest` sampler for one and the anisotropic one for the other, on
                    // the same picture.
                    //
                    // The game's falls back to this side's rather than to zero: zero would make the test
                    // "is the derivative below zero", which nothing is, so every face would take the
                    // anisotropic sampler - the picture before any of this, which is the direction to fail
                    // in.
                    let our_texel = 1.0 / crate::render::atlas::ATLAS_DIMENSIONS as f32;
                    let game_texel =
                        game_atlas_size().map_or(our_texel, |(width, _)| 1.0 / width as f32);

                    // **One record per draw, built with the frame rather than here.**
                    //
                    // `draws.records[i]` is what the vertex stage reads for draw `i` - the section's
                    // position and its arena slot - and `draws.calls[i]` is what the draw itself needs.
                    // Both paths read the record out of the buffer, because the section's position is not
                    // in the immediate: a `multi_draw` is one call, so an immediate can only hold what
                    // every draw in it shares. See [`RenderGraph::rebuild_terrain_frame`], which is where
                    // they are built and uploaded - once for the frame, not once per pass.
                    // The half-texel shifts, in the order the shader's `SectionPosition` declares them,
                    // and the shader picks the one belonging to the atlas the face samples. Not
                    // conditional here: the flag that says which atlas a face uses is per vertex, and a
                    // layer can hold faces of both kinds, so there is no answer at this level to write.
                    // The pass' immediate: the five numbers about the two atlases that every draw of a
                    // layer shares, and the layer's own alpha cutoff, which is filled in per layer below.
                    // See [`SectionPositionImmediate`] for the layout and for why the section's own
                    // position is not one of these.
                    //
                    // **The bias is read here, once per pass, rather than per draw.** It is a setting,
                    // so a value cached across frames would follow the options screen late - and a value
                    // read at the top of the pass is as current as the frame it is drawn in. That is the
                    // granularity the batching forced: a `multi_draw` cannot be handed an immediate per
                    // draw, so everything in here has to be true for all of them.
                    let mut immediate = SectionPositionImmediate {
                        alpha_cutout: 0.0,
                        lod_bias: crate::render::atlas::atlas_lod_bias(),
                        half_texel_ours: our_half_texel,
                        half_texel_game: game_half_texel,
                        texel_ours: our_texel,
                        texel_game: game_texel,
                        // **The two numbers the game's own terrain shader is handed for its sampling**:
                        // whether the `textureFiltering` option has RGSS on, and one texel of the atlas
                        // it reads. See the shader's `use_rgss` and `sampleRGSS`, which is a port of the
                        // game's and cannot be expressed with a sampler. `GameRenderer` writes exactly
                        // this comparison into its own `GlobalSettingsUniform`.
                        use_rgss: u32::from(
                            crate::render::atlas::texture_filtering()
                                == crate::render::atlas::TEXTURE_FILTERING_RGSS,
                        ),
                    };

                    // **Whether this pass batches its draws, and the uploads, are with the build.** Both
                    // are decisions about the records rather than about the draws - the batching is what
                    // the indirect arguments were written for, and the upload has to happen before the
                    // frame's first draw of that buffer - so they live in
                    // [`RenderGraph::rebuild_terrain_frame`] and this pass only obeys. See
                    // [`PassDraws::batched`].

                    for ((layer_index, start, end), (_, alpha_cutout)) in
                        draws.spans.iter().zip(Self::terrain_layers(pipeline_name))
                    {
                        // The immediate, **once for the layer**: the alpha cutoff is the one field of it
                        // a layer changes, and the rest is what every draw of the pass shares. Set
                        // before the layer's first draw and not per draw, because the batched path
                        // cannot set anything per draw at all.
                        immediate.alpha_cutout = *alpha_cutout;
                        set_immediates(
                            &bound_pipeline.immediates,
                            &mut render_pass,
                            &[bytemuck::bytes_of(&immediate)],
                        );

                        if draws.batched {
                            // **The longest runs of consecutive draws that share an arena.**
                            //
                            // `multi_draw_indexed_indirect` reads one index buffer and one arena bind
                            // group for every record in the call, so a call may only span draws of one
                            // arena. The runs are contiguous slices of the list and are issued in order,
                            // so the transparent layer's far-to-near order comes out exactly as it went
                            // in: a run boundary splits a call, it does not reorder anything.
                            //
                            // Sections are gathered in storage order and the allocator hands out of one
                            // arena until it is full, so this is a handful of calls per frame rather
                            // than one per section.
                            let mut index = *start;

                            while index < *end {
                                let arena = draws.calls[index].arena;
                                let mut stop = index + 1;

                                while stop < *end && draws.calls[stop].arena == arena {
                                    stop += 1;
                                }

                                let buffer = &chunk_buffers[arena as usize];

                                render_pass.set_bind_group(1, &buffer.bind_group, &[]);
                                render_pass.set_index_buffer(
                                    buffer.buffer.slice(..),
                                    wgpu::IndexFormat::Uint32,
                                );

                                render_pass.multi_draw_indexed_indirect(
                                    &scene.indirect_buffers[draw_slot],
                                    // In bytes, and every record is the same size - so the offset of
                                    // record `i` is `i` records in. The records were written in this
                                    // same order, which is what makes the two agree.
                                    index as u64 * INDIRECT_RECORD_SIZE,
                                    (stop - index) as u32,
                                );

                                let count = (stop - index) as u64;
                                drawn += count;
                                drawn_total += count;
                                layer_drawn[*layer_index] += count;
                                layer_drawn_total[*layer_index] += count;

                                index = stop;
                            }
                        } else {
                            // **One call per draw**, which is the path this renderer has always had -
                            // and now the fallback: for a device without
                            // `Features::INDIRECT_FIRST_INSTANCE` (a record's `first_instance` may not
                            // be non-zero there), for a pass with more draws than the indirect buffer
                            // holds, and for a run with the switch off.
                            //
                            // The picture is the same either way, and by construction rather than by
                            // care: the same records in the same order, the same shader, and the same
                            // `instance_index` - a record's own number - reaching it.
                            for call in &draws.calls[*start..*end] {
                                // **Rebind when the section lives in a different arena.** The bind group
                                // is the storage buffer the vertex stage reads `chunk_data` out of and
                                // the index buffer is where `draw_indexed` reads the indices, so both
                                // are per arena and both have to move together - binding one and not the
                                // other draws a section out of somebody else's geometry.
                                if bound_arena != Some(call.arena) {
                                    let buffer = &chunk_buffers[call.arena as usize];

                                    render_pass.set_bind_group(1, &buffer.bind_group, &[]);
                                    render_pass.set_index_buffer(
                                        buffer.buffer.slice(..),
                                        wgpu::IndexFormat::Uint32,
                                    );
                                    bound_arena = Some(call.arena);
                                }

                                // One instance, and its number is which record the vertex stage is to
                                // read - the number the batched path writes into `first_instance`.
                                render_pass.draw_indexed(
                                    call.index_range.clone(),
                                    0,
                                    call.instance..call.instance + 1,
                                );

                                drawn += 1;
                                drawn_total += 1;
                                layer_drawn[*layer_index] += 1;
                                layer_drawn_total[*layer_index] += 1;
                            }
                        }
                    }

                    // One `fetch_add` each, for the whole pass: the atomics are drained by a log line
                    // once a second and nothing on the draw path reads them. The gather's own counts and
                    // the per-layer "the arena has nothing here" counts are added where the frame is
                    // built, so what is left here is what this pass drew.
                    TERRAIN_DRAWN.fetch_add(drawn, Ordering::Relaxed);
                    TERRAIN_DRAWN_TOTAL.fetch_add(drawn_total, Ordering::Relaxed);

                    for layer in 0..3 {
                        LAYER_DRAWN[layer].fetch_add(layer_drawn[layer], Ordering::Relaxed);
                        LAYER_DRAWN_TOTAL[layer]
                            .fetch_add(layer_drawn_total[layer], Ordering::Relaxed);
                    }

                    report_terrain_pass();
                }
                "@geo_entities" => {
                    render_pass.set_pipeline(&bound_pipeline.pipeline);

                    let instances = { scene.entity_instances.lock().clone() };

                    for entity_instances in instances.values() {
                        for (index, bind_group) in bound_pipeline.bind_groups.iter() {
                            match bind_group {
                                WmBindGroup::Resource(name) => match &name[..] {
                                    "@bg_entity" => {
                                        render_pass.set_bind_group(
                                            *index,
                                            Some(&*entity_instances.uploaded.bind_group),
                                            &[],
                                        );
                                    }
                                    _ => unimplemented!(),
                                },
                                WmBindGroup::Custom(bind_group) => {
                                    render_pass.set_bind_group(*index, bind_group, &[]);
                                }
                            }
                        }

                        let mut pc: HashMap<String, (Vec<u8>, ShaderStages)> = HashMap::new();
                        pc.insert(
                            "@pc_parts_per_entity".to_string(),
                            (
                                bytemuck::cast_slice(&[entity_instances.entity.parts.len() as u32])
                                    .to_vec(),
                                ShaderStages::VERTEX,
                            ),
                        );
                        set_push_constants(pipeline_config, &mut render_pass, Some(pc));

                        render_pass.set_vertex_buffer(0, entity_instances.entity.mesh.slice(..));
                        render_pass
                            .set_vertex_buffer(1, entity_instances.uploaded.instance_vbo.slice(..));

                        render_pass.draw(
                            0..entity_instances.entity.vertex_count,
                            0..entity_instances.capacity,
                        );
                    }
                }
                "@geo_sun_moon" => {
                    for (index, bind_group) in bound_pipeline.bind_groups.iter() {
                        match bind_group {
                            WmBindGroup::Custom(bind_group) => {
                                render_pass.set_bind_group(*index, bind_group, &[]);
                            }
                            WmBindGroup::Resource(_) => {}
                        }
                    }
                    let sun_buffer = wm.gpu.device.create_buffer_init(&BufferInitDescriptor {
                        label: None,
                        contents: bytemuck::cast_slice(&SunMoonVertex::load_vertex_sun()),
                        usage: BufferUsages::VERTEX,
                    });
                    let moon_buffer = wm.gpu.device.create_buffer_init(&BufferInitDescriptor {
                        label: None,
                        contents: bytemuck::cast_slice(&SunMoonVertex::load_vertex_moon(
                            scene.sky_state.load().moon_phase,
                        )),
                        usage: BufferUsages::VERTEX,
                    });

                    render_pass.set_pipeline(&bound_pipeline.pipeline);
                    let pc = get_environmental_push_constants(scene);
                    set_push_constants(pipeline_config, &mut render_pass, Some(pc));

                    render_pass.set_vertex_buffer(0, sun_buffer.slice(..));
                    render_pass.draw(0..6, 0..1);

                    render_pass.set_vertex_buffer(0, moon_buffer.slice(..));
                    render_pass.draw(0..6, 0..1);
                }
                "@geo_sky_scatter" => {
                    for (index, bind_group) in bound_pipeline.bind_groups.iter() {
                        match bind_group {
                            WmBindGroup::Custom(bind_group) => {
                                render_pass.set_bind_group(*index, bind_group, &[]);
                            }
                            WmBindGroup::Resource(_) => {}
                        }
                    }

                    let (light_sky_vertices, light_sky_indices) =
                        SkyVertex::load_vertex_light_sky();
                    let light_sky_buffer = (
                        wm.gpu.device.create_buffer_init(&BufferInitDescriptor {
                            label: None,
                            contents: bytemuck::cast_slice(&light_sky_vertices),
                            usage: BufferUsages::VERTEX,
                        }),
                        wm.gpu.device.create_buffer_init(&BufferInitDescriptor {
                            label: None,
                            contents: bytemuck::cast_slice(&light_sky_indices),
                            usage: BufferUsages::INDEX,
                        }),
                    );

                    let (dark_sky_vertices, dark_sky_indices) = SkyVertex::load_vertex_dark_sky();
                    let dark_sky_buffer = (
                        wm.gpu.device.create_buffer_init(&BufferInitDescriptor {
                            label: None,
                            contents: bytemuck::cast_slice(&dark_sky_vertices),
                            usage: BufferUsages::VERTEX,
                        }),
                        wm.gpu.device.create_buffer_init(&BufferInitDescriptor {
                            label: None,
                            contents: bytemuck::cast_slice(&dark_sky_indices),
                            usage: BufferUsages::INDEX,
                        }),
                    );

                    render_pass.set_pipeline(&bound_pipeline.pipeline);
                    let pc = get_environmental_push_constants(scene);
                    set_push_constants(pipeline_config, &mut render_pass, Some(pc));

                    render_pass.set_vertex_buffer(0, light_sky_buffer.0.slice(..));
                    render_pass.set_index_buffer(light_sky_buffer.1.slice(..), IndexFormat::Uint32);
                    render_pass.draw_indexed(0..24, 0, 0..1);

                    render_pass.set_vertex_buffer(0, dark_sky_buffer.0.slice(..));
                    render_pass.set_index_buffer(dark_sky_buffer.1.slice(..), IndexFormat::Uint32);
                    render_pass.draw_indexed(0..24, 0, 0..1);
                }
                "@geo_sky_fog" => {
                    for (index, bind_group) in bound_pipeline.bind_groups.iter() {
                        match bind_group {
                            WmBindGroup::Custom(bind_group) => {
                                render_pass.set_bind_group(*index, bind_group, &[]);
                            }
                            WmBindGroup::Resource(_) => {}
                        }
                    }

                    let (fog_sphere_vertices, fog_sphere_indices) = SkyVertex::load_fog_sphere();
                    let fog_sphere = (
                        wm.gpu.device.create_buffer_init(&BufferInitDescriptor {
                            label: None,
                            contents: bytemuck::cast_slice(&fog_sphere_vertices),
                            usage: BufferUsages::VERTEX,
                        }),
                        wm.gpu.device.create_buffer_init(&BufferInitDescriptor {
                            label: None,
                            contents: bytemuck::cast_slice(&fog_sphere_indices),
                            usage: BufferUsages::INDEX,
                        }),
                    );

                    render_pass.set_pipeline(&bound_pipeline.pipeline);
                    let pc = get_environmental_push_constants(scene);
                    set_push_constants(pipeline_config, &mut render_pass, Some(pc));

                    render_pass.set_vertex_buffer(0, fog_sphere.0.slice(..));
                    render_pass.set_index_buffer(fog_sphere.1.slice(..), IndexFormat::Uint32);
                    render_pass.draw_indexed(0..51, 0, 0..1);
                }
                // "@geo_sky_stars" => {
                //     for (index, bind_group) in bound_pipeline.bind_groups.iter() {
                //         match bind_group {
                //             WmBindGroup::Custom(bind_group) => {
                //                 render_pass.set_bind_group(*index, bind_group, &[]);
                //             }
                //             WmBindGroup::Resource(_) => {}
                //         }
                //     }
                //     let stars_vertex_buffer = scene.stars_vertex_buffer.read();
                //     let stars_vertex = stars_vertex_buffer.as_ref().unwrap().slice(..);
                //
                //     let stars_index_buffer = scene.stars_index_buffer.read();
                //     let stars_index = stars_index_buffer.as_ref().unwrap().slice(..);
                //
                //     render_pass.set_pipeline(&bound_pipeline.pipeline);
                //     let pc = get_environmental_push_constants(scene);
                //     set_push_constants(pipeline_config, &mut render_pass, Some(pc));
                //
                //     render_pass.set_vertex_buffer(0, stars_vertex);
                //     render_pass.set_index_buffer(stars_index, IndexFormat::Uint32);
                //     render_pass.draw_indexed(0..*scene.stars_length.read(), 0, 0..1);
                // }
                _ => match geometry.get_mut(&pipeline_config.geometry) {
                    None => unimplemented!("Unknown geometry {}", &pipeline_config.geometry),
                    Some(geometry) => {
                        geometry.render(wm, self, bound_pipeline, &mut render_pass, &arena);
                    }
                },
            }
        }
    }
}

fn get_environmental_push_constants(scene: &Scene) -> HashMap<String, (Vec<u8>, ShaderStages)> {
    let sky = &scene.sky_state.load();
    let render_effects = &scene.render_effects.load();

    let mut pc: HashMap<String, (Vec<u8>, ShaderStages)> = HashMap::new();
    pc.insert(
        "@pc_environment_data".to_string(),
        (
            bytemuck::cast_slice(&[
                sky.angle,
                sky.brightness,
                sky.star_shimmer,
                render_effects.fog_start,
                render_effects.fog_end,
                render_effects.fog_shape,
                render_effects.fog_color[0],
                render_effects.fog_color[1],
                render_effects.fog_color[2],
                render_effects.fog_color[3],
                sky.color[0],
                sky.color[1],
                sky.color[2],
                render_effects.dimension_fog_color[0],
                render_effects.dimension_fog_color[1],
                render_effects.dimension_fog_color[2],
                render_effects.dimension_fog_color[3],
            ])
            .to_vec(),
            ShaderStages::VERTEX_FRAGMENT,
        ),
    );
    pc
}

pub fn set_push_constants(
    pipeline: &PipelineConfig,
    render_pass: &mut wgpu::RenderPass,
    push_constants: Option<HashMap<String, (Vec<u8>, wgpu::ShaderStages)>>,
) {
    pipeline.immediates.iter().for_each(|(offset, resource)| {
        match push_constants
            .as_ref()
            .and_then(|others| others.get(resource))
        {
            None => unimplemented!("Unknown push constant resource value"),
            Some((data, _stages)) => render_pass.set_immediates(*offset as u32, data),
        }
    });
}

/// How many bytes the shader reads out of each immediate, by the name the config gives it.
///
/// The sizes the shaders themselves declare: an immediate whose layout is smaller than the struct the
/// shader reads out of it is a draw that reads whatever follows in the buffer. One place, because the
/// layout is built from it and every draw checks against it.
fn immediate_size_of(name: &str) -> u32 {
    match name {
        "@pc_mat4_model" => 64,
        // **Seven four-byte members, and the three that are missing are the point.** `SectionPosition`
        // is the layer's alpha cutoff, the atlas level-of-detail bias, the two half-texel shifts, one
        // texel of each atlas, and whether the game's RGSS filtering is on. The section's x, y and z
        // were the first three members and are in `SectionDraw` now, because a `multi_draw` is one call
        // and an immediate holds what the whole of it shares. See the struct in `terrain.wgsl`.
        "@pc_section_position" => 28,
        "@pc_total_sections" => 4,
        "@pc_parts_per_entity" => 4,
        "@pc_electrum_color" => 16,
        "@pc_environment_data" => 68,
        _ => 0,
    }
}

/// Writes a draw's immediates, **without allocating**.
///
/// `slots` is the pipeline's [`BoundPipeline::immediates`] - every `(offset, size)` its layout
/// declares, in slot order - and `data` is the values, one per slot and in the same order. The caller
/// builds `data` in a fixed-size array on its own stack, which is the whole point of the shape: the
/// previous version took a `HashMap<String, (Vec<u8>, ShaderStages)>` and the terrain pass built one
/// **per section per layer**, so a pass over four hundred sections allocated four hundred `HashMap`s,
/// four hundred `String`s and four hundred `Vec`s to write sixteen bytes each.
///
/// The values are lengths, not a buffer, because the two callers have sixteen bytes and sixty-eight and
/// a slice of one is not a slice of the other. A slot with nothing to write is skipped rather than
/// zeroed: an immediate a draw does not set keeps whatever the last draw left there, which is the
/// behaviour the map version had too - a name that was not in the map was an `unimplemented!`.
pub fn set_immediates(slots: &[(u32, u32)], render_pass: &mut wgpu::RenderPass, data: &[&[u8]]) {
    for (index, (offset, size)) in slots.iter().enumerate() {
        let Some(bytes) = data.get(index) else {
            continue;
        };

        // A value shorter than the slot is a shader reading whatever follows it in the buffer, and a
        // longer one is a write past the end of the slot. Both are wrong, and the layout is where the
        // right answer is written down.
        assert_eq!(
            bytes.len() as u32,
            *size,
            "immediate {index} at offset {offset} is {size} byte(s) and was handed {}",
            bytes.len()
        );

        render_pass.set_immediates(*offset, bytes);
    }
}

/// Rewrites a clip-space matrix from wgpu's depth range into the one the culler extracts from.
///
/// wgpu's clip space is `0..1` in z, always - that is the WebGPU convention rather than a backend's -
/// while the Gribb-Hartmann extraction in `treeculler` takes the near plane to be the third row *plus*
/// the fourth, which is the `-1..1` convention. The planes that come out of the mismatch are not this
/// frustum's: a world drawn through that answer is a world with ground missing from it, and nothing
/// about the picture says which of the two ranges went in. Converting the matrix rather than the planes
/// is one row operation, `z' = 2z - 1`, and keeps the extraction in one place.
fn with_gl_depth_range(mvp: [[f32; 4]; 4]) -> [[f32; 4]; 4] {
    let mut converted = mvp;

    // `[column][row]`, so the z *row* of a column-major matrix is the third entry of every column -
    // which is the one index that is not the first here. Written as `[2][row]` this rewrites a column
    // instead, and the planes come out of a frustum that has nothing to do with the camera.
    for column in 0..4 {
        converted[column][2] = 2.0 * mvp[column][2] - mvp[column][3];
    }

    converted
}

/// Which layer each of the terrain pipelines draws. See [`RenderGraph::terrain_layers`].
#[cfg(test)]
mod terrain_layer_tests {
    use super::*;
    use wgpu_mc_modtree as modtree;

    /// The split, and the three cutoffs that go with it.
    ///
    /// All four numbers are Minecraft's own, and every one of them is a picture difference rather than
    /// an error if it is wrong: the wrong layer is water drawn opaque, and a cutoff of zero paints in the
    /// holes a translucent mip level leaves nearly empty.
    ///
    /// The values are `RenderPipelines.SOLID_TERRAIN` (no `ALPHA_CUTOUT` at all), `CUTOUT_TERRAIN`
    /// (`0.5`) and `TRANSLUCENT_TERRAIN` (`0.01`), and the layers are `ChunkSectionLayerGroup.OPAQUE`
    /// and `TRANSLUCENT`.
    ///
    /// **The opaque group is three pipelines now and the test asks all three**, because the split that
    /// matters is the one between `terrain_solid` and `terrain`: they share one geometry, so a lookup
    /// keyed on the geometry would give both of them both layers - which is the early-Z problem back
    /// again, in the one place a wrong answer costs performance rather than a picture.
    #[test]
    fn the_terrain_pipelines_draw_one_layer_each_and_none_draws_another_s() {
        let solid = RenderGraph::terrain_layers("terrain_solid");
        let cutout = RenderGraph::terrain_layers("terrain");
        let translucent = RenderGraph::terrain_layers("translucent_terrain");

        assert_eq!(
            solid,
            [(RenderLayer::Solid, 0.0)],
            "the solid pipeline draws the solid layer, and its shader declares no cutoff at all"
        );

        assert_eq!(
            cutout,
            [(RenderLayer::Cutout, 0.5)],
            "the cutout pipeline draws the cutout layer at Minecraft's CUTOUT_TERRAIN cutoff, and it \
             is the only one of the two with a discard in its shader"
        );

        assert_eq!(
            translucent,
            [(RenderLayer::Transparent, 0.01)],
            "the translucent pipeline is one layer, and its cutoff is Minecraft's 0.01 rather than zero"
        );

        // The three together are every layer there is, once each: a layer two drew would be drawn twice
        // and a layer none drew would be baked and invisible, which is the state water was in.
        let mut drawn: Vec<usize> = solid
            .iter()
            .chain(cutout)
            .chain(translucent)
            .map(|(layer, _)| *layer as usize)
            .collect();
        drawn.sort_unstable();

        assert_eq!(
            drawn,
            [0, 1, 2],
            "the three pipelines between them have to draw the solid, cutout and transparent layers \
             exactly once each"
        );

        // And the geometry-keyed lookup that would have handed the solid layer to the shader with the
        // test in it is gone: a name the graph does not know draws nothing.
        assert!(
            RenderGraph::terrain_layers("@geo_terrain").is_empty(),
            "the geometry is not the key any more - two pipelines share it"
        );
    }

    /// **Only the pipelines that are cut out carry a `discard`**, which is the whole reason there are
    /// two of them.
    ///
    /// Read off the shipped shader sources rather than from the graph config, because the graph cannot
    /// see this: `discard` is not a pipeline state, it is an instruction inside the fragment stage, and
    /// its effect on early-Z is a property of the compiled program. A pipeline that draws the solid
    /// layer and has one is the bug this pair of files exists to fix.
    ///
    /// The sources come out of the Neolectrum checkout, so a clone without the mod skips the check
    /// rather than failing it; see `wgpu_mc_modtree`.
    #[test]
    fn only_the_cut_out_terrain_shaders_discard() {
        // Whether there is a checkout to read from at all: `source` below reads whichever file it is
        // asked for, so the answer here is only about the Neolectrum checkout being there.
        if modtree::shader_dir().is_none() {
            modtree::skip("the discard check");
            return;
        }

        let source = |name: &str| {
            modtree::shader(name)
                .unwrap_or_else(|| panic!("the Neolectrum checkout has no {name}.wgsl to read"))
        };

        // The two that are allowed one: the cutout layer's, and the translucent layer's.
        for name in ["terrain", "translucent_terrain"] {
            let text = if name == "translucent_terrain" {
                source("terrain")
            } else {
                source(name)
            };

            assert!(
                text.contains("discard;"),
                "{name} draws a cut-out layer and has to discard"
            );
        }

        // And the one that must not, which is the entire point of its file.
        assert!(
            !source("terrain_solid").contains("discard;"),
            "the solid layer's shader must have no discard in it at all: one is enough to cost the \
             whole pipeline its early-Z, and the solid layer is most of the screen"
        );
    }

    /// **An empty visible set is not the same as no visible set.**
    ///
    /// The three cases, and the middle one is the bug worth a test: a section the game named is drawn, a
    /// section it did not name is not - and an empty set, which is what a frame with nothing on screen
    /// produces, must not fall back to the frustum. Read as "not told", it draws the entire arena for a
    /// frame the game deliberately emptied, and the feature looks like it does nothing at all.
    #[test]
    fn an_empty_visible_set_draws_nothing_rather_than_everything() {
        let here = glam::ivec3(3, 4, 5);
        let elsewhere = glam::ivec3(-1, 0, 2);
        let mut told = std::collections::HashSet::new();
        told.insert(here);

        // What the gather passes when no section has been taken over since the list arrived, which is
        // the state every assertion about culling below is made in.
        let nothing_taken = std::collections::HashSet::new();

        // Nothing has been sent: the frustum decides, so anything might still be drawn.
        assert_eq!(
            RenderGraph::section_visibility(None, &nothing_taken, &here),
            SectionVisibility::AskTheFrustum
        );
        assert_eq!(
            RenderGraph::section_visibility(None, &nothing_taken, &elsewhere),
            SectionVisibility::AskTheFrustum
        );

        // A set that names this section draws it and one that does not, does not.
        assert_eq!(
            RenderGraph::section_visibility(Some(&told), &nothing_taken, &here),
            SectionVisibility::Draw
        );
        assert_eq!(
            RenderGraph::section_visibility(Some(&told), &nothing_taken, &elsewhere),
            SectionVisibility::OutOfSight,
            "the steady state is what the culling is for: a section the list has had every chance to \
             name, and did not"
        );

        // And the empty set: the game looked and saw nothing.
        let empty = std::collections::HashSet::new();
        assert_eq!(
            RenderGraph::section_visibility(Some(&empty), &nothing_taken, &here),
            SectionVisibility::OutOfSight,
            "an empty list is the game saying it saw nothing, not the JVM saying nothing"
        );

        // **And the section the list was never given the chance to judge**, which is the hole this
        // third answer exists to close: it entered the arena after the list was last refilled, so "not
        // named" is a stale answer rather than the game saying no. The frustum decides instead.
        let mut since = std::collections::HashSet::new();
        since.insert(elsewhere);

        assert_eq!(
            RenderGraph::section_visibility(Some(&told), &since, &elsewhere),
            SectionVisibility::AskTheFrustum,
            "a section taken over since the list was sent is not culled by it"
        );
        assert_eq!(
            RenderGraph::section_visibility(Some(&empty), &since, &elsewhere),
            SectionVisibility::AskTheFrustum,
            "not even by an empty list: the game looked before this section was here"
        );

        // A named section is still drawn whatever the set says, and the set does not reach a section
        // the list *has* judged - both of which are the culling surviving the fix.
        assert_eq!(
            RenderGraph::section_visibility(Some(&told), &since, &here),
            SectionVisibility::Draw
        );
        assert_eq!(
            RenderGraph::section_visibility(Some(&empty), &since, &here),
            SectionVisibility::OutOfSight,
            "a section the list named is not, and this one it did not name and has judged"
        );
    }

    fn at(distance_squared: f32) -> VisibleSection {
        VisibleSection {
            relative_position: glam::IVec3::ZERO,
            ranges: [None, None, None],
            distance_squared,
        }
    }

    /// **The translucent layer is drawn far to near, and that is a picture difference.**
    ///
    /// It blends and does not write depth, so a face drawn late mixes with a face drawn early - which
    /// means the near face has to come last or the far one is painted over it. The pass used to walk the
    /// arena's `HashMap` in hash order, which is no order at all.
    ///
    /// The opaque layers are deliberately **not** sorted: they write depth, so the depth test orders
    /// them, and sorting a few hundred sections for a layer that does not care is work for nothing.
    #[test]
    fn a_blending_layer_is_sorted_far_to_near_and_an_opaque_one_is_left_alone() {
        let unsorted = || vec![at(4.0), at(400.0), at(64.0), at(1.0)];

        // The translucent layer, alone and beside an opaque one.
        for layers in [
            &[(RenderLayer::Transparent, 0.01)][..],
            &[(RenderLayer::Solid, 0.0), (RenderLayer::Transparent, 0.01)][..],
        ] {
            let mut list = unsorted();
            RenderGraph::sort_for_drawing(&mut list, layers);

            let distances: Vec<f32> = list.iter().map(|s| s.distance_squared).collect();
            assert_eq!(
                distances,
                [400.0, 64.0, 4.0, 1.0],
                "the furthest section has to be drawn first: {layers:?}"
            );
        }

        // The opaque pipelines, which have nothing to gain from an order.
        for layers in [
            &[(RenderLayer::Solid, 0.0)][..],
            &[(RenderLayer::Cutout, 0.5)][..],
            &[(RenderLayer::Solid, 0.0), (RenderLayer::Cutout, 0.5)][..],
        ] {
            let mut list = unsorted();
            RenderGraph::sort_for_drawing(&mut list, layers);

            let distances: Vec<f32> = list.iter().map(|s| s.distance_squared).collect();
            assert_eq!(
                distances,
                [4.0, 400.0, 64.0, 1.0],
                "an opaque layer is ordered by the depth test, not here: {layers:?}"
            );
        }
    }
}

/// **No `textureSample` under an `if`**, checked on the shader both terrain pipelines build from.
///
/// `textureSample` takes an implicit level of detail from the derivatives of its coordinates, and WGSL
/// only defines those in *uniform* control flow: a `textureSample` inside a branch is undefined
/// behaviour, whether or not the condition happens to hold for every fragment of a primitive. The
/// terrain shader picked which atlas to read with exactly that shape:
///
/// ```wgsl
/// if (in.game_atlas == 1u) { texel = textureSample(t_game_atlas, ...); }
/// else                     { texel = textureSample(t_texture, ...); }
/// ```
///
/// The argument for it was that `game_atlas` is `@interpolate(flat)`, so every fragment of one
/// primitive takes the same branch. That is an argument about the picture, not about the program: it is
/// true of the hardware it was tried on and it is not what the specification says. The samples are
/// hoisted out of the branch now and the choice is a `select` on the two values.
///
/// **Structural rather than a search for the text**, which is the only reason this is worth writing: the
/// offending code is quoted in the comment above the fix, and in `terrain_solid.wgsl` beside it, so a
/// test that looked for the string would fail on the explanation of why it must not be there. Instead the
/// shader is parsed and walked: every `ImageSample` in a fragment entry point whose level is `Auto`, and
/// which sits inside an `If` block - at any depth - is a failure.
#[cfg(test)]
mod texture_sample_uniformity_tests {
    use crate::wgpu::naga;
    use wgpu_mc_modtree as modtree;

    /// How many auto-level image fetches the module holds, at any depth. The sanity check beside the
    /// assertion: "found no sample in a branch" is also true of a shader with no samples at all.
    ///
    /// Counts `Auto` **and** `Bias`, because both ask the hardware for a level from the derivatives and
    /// both are therefore subject to the invariant. See [`is_auto_sample`], which is the same rule used
    /// for the branch walk - one predicate, so the count and the walk cannot disagree about what they
    /// are looking for.
    fn total_auto_samples(module: &naga::Module) -> usize {
        module
            .entry_points
            .iter()
            .map(|entry| {
                entry
                    .function
                    .expressions
                    .iter()
                    .filter(|(_, expression)| is_auto_sample(expression))
                    .count()
            })
            .sum()
    }

    /// Every auto-level `textureSample` in `source` that is inside a branch, as `(entry point, depth)`.
    ///
    /// **A finding is a depth above zero**, and the depth is carried for that reason: zero is a sample
    /// at the entry point's own top level, which is uniform control flow and is exactly where these
    /// belong. Reporting at zero would make the test fail on the fixed shader.
    pub(super) fn samples_inside_branches(source: &str) -> Vec<(String, usize)> {
        let module = naga::front::wgsl::parse_str(source).expect("the terrain shader parses");

        let mut found = Vec::new();

        for entry in &module.entry_points {
            if entry.stage != naga::ShaderStage::Fragment {
                // The vertex stage's lightmap fetch is a `textureSampleLevel` with an explicit level,
                // which is legal anywhere - and it has to be, because a vertex stage has no derivatives
                // to take a level from.
                continue;
            }

            walk(
                &entry.function,
                &entry.function.body,
                0,
                entry.name.clone(),
                &mut found,
            );
        }

        found
    }

    /// Walks a block, carrying how many branches deep it is.
    ///
    /// Only `Emit` ranges are inspected, and only their own expressions - see the note there on why
    /// nothing follows a handle. An `If` condition and a `Switch` selector are read directly, because
    /// they are single handles rather than ranges: a sample *there* is a sample outside the branch it
    /// guards, which is why they are reported at this depth and not one deeper.
    fn walk(
        function: &naga::Function,
        block: &naga::Block,
        branch_depth: usize,
        entry: String,
        found: &mut Vec<(String, usize)>,
    ) {
        for statement in block.iter() {
            match statement {
                naga::Statement::If {
                    condition,
                    accept,
                    reject,
                } => {
                    if let Ok(expression) = function.expressions.try_get(*condition)
                        && is_auto_sample(expression)
                        && branch_depth > 0
                    {
                        found.push((entry.clone(), branch_depth));
                    }

                    walk(function, accept, branch_depth + 1, entry.clone(), found);
                    walk(function, reject, branch_depth + 1, entry.clone(), found);
                }
                naga::Statement::Switch { selector, cases } => {
                    if let Ok(expression) = function.expressions.try_get(*selector)
                        && is_auto_sample(expression)
                        && branch_depth > 0
                    {
                        found.push((entry.clone(), branch_depth));
                    }

                    for case in cases {
                        walk(function, &case.body, branch_depth + 1, entry.clone(), found);
                    }
                }
                naga::Statement::Loop {
                    body, continuing, ..
                } => {
                    // A loop body is not a branch: every invocation runs it the same number of times in
                    // the shaders here, and what WGSL's uniformity analysis objects to is divergence.
                    walk(function, body, branch_depth, entry.clone(), found);
                    walk(function, continuing, branch_depth, entry.clone(), found);
                }
                naga::Statement::Block(inner) => {
                    walk(function, inner, branch_depth, entry.clone(), found)
                }
                naga::Statement::Emit(range) => {
                    // **A range of handles, and every expression in it is looked at on its own.** naga
                    // emits each expression an evaluation needs, in dependency order, into one slice, so
                    // the sample and the `select` that consumes it are both in this range and both are
                    // visited. That is why nothing here follows a handle: following one would reach an
                    // operand a second time, from its consumer as well as from the range, and report one
                    // sample twice - which is exactly what the first version of this did.
                    let indices = range.index_range();
                    let length = (indices.end - indices.start) as usize;

                    for (_, expression) in function
                        .expressions
                        .iter()
                        .skip(indices.start as usize)
                        .take(length)
                    {
                        if is_auto_sample(expression) && branch_depth > 0 {
                            found.push((entry.clone(), branch_depth));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Whether this expression is a texture fetch that asks the hardware for a level of detail.
    ///
    /// **`Auto` and `Bias` both do**, and both therefore need derivatives: `Auto` takes the level from
    /// them, `Bias` takes it from them and shifts it. A `Bias` of zero behaves exactly as an `Auto`, so
    /// the shipped shader's `textureSampleBias(.., ATLAS_LOD_BIAS)` is the same fetch as long as that
    /// constant is zero - and the invariant below has to apply to it either way, because a bias whose
    /// level is computed inside a branch is the same undefined behaviour a plain sample is.
    ///
    /// `Zero` and `Exact` name a level outright and are legal wherever they appear, which is why the
    /// vertex stage's lightmap fetch - a `textureSampleLevel(.., 0.0)` - is not a finding.
    fn is_auto_sample(expression: &naga::Expression) -> bool {
        matches!(
            expression,
            naga::Expression::ImageSample {
                level: naga::SampleLevel::Auto | naga::SampleLevel::Bias(_),
                ..
            }
        )
    }

    /// The invariant, on both shipped terrain shaders.
    #[test]
    fn the_terrain_shaders_never_sample_a_texture_under_a_branch() {
        for name in ["terrain", "terrain_solid"] {
            let Some(source) = modtree::shader(name) else {
                modtree::skip("the branch-walk check");
                return;
            };

            // The sanity check first, so a shader that lost its samples says *that* rather than passing
            // the assertion below by finding nothing.
            let module = naga::front::wgsl::parse_str(&source).expect("the terrain shader parses");
            let auto = total_auto_samples(&module);

            // **Zero, and the zero is the finding rather than a shader that lost its samples.**
            //
            // It was six: two atlases times three samplers, each fetched with `textureSampleBias` so the
            // level came from the hardware and from the bias together. **The level no longer comes from the
            // hardware at all** - the fragment stage ports the game's own `sampleNearest` and `sampleRGSS`,
            // which fetch with `textureSampleGrad` and `textureSampleLevel` - so there is no auto-level fetch
            // left to count, and the count is asserted at zero so that one *reappearing* is the failure.
            //
            // The invariant below is what actually matters and it is now stronger than it was: a
            // `textureSampleGrad` still computes its level from derivatives, and the derivatives of a
            // `sample_nearest` are of the *unshifted* coordinates, so the same rule applies. What the
            // shaders do now is **hoist every derivative, and every level, into one `sample_geometry`
            // called at the top level**, and branch over fetches that name their level outright - which
            // is why the shape below is allowed to be real `if`s again. See the sampling note in
            // `terrain.wgsl`.
            assert_eq!(
                auto, 0,
                "{name}.wgsl has {auto} auto-level texture fetch(es); the game's own sampling functions take \
                 the level explicitly, so every fetch here should be a `textureSampleLevel` - an `Auto` or \
                 `Bias` sample means one was written the old way, and one inside a branch is the undefined \
                 behaviour this test exists for"
            );

            let found = samples_inside_branches(&source);

            assert!(
                found.is_empty(),
                "{name}.wgsl samples a texture inside a branch at {found:?}; the level of detail comes \
                 from derivatives, which WGSL only defines in uniform control flow, so this is undefined \
                 behaviour however uniform the condition looks. Take the level explicitly - \
                 `textureSampleLevel` carries no uniformity requirement - or sample both and `select` \
                 between the results."
            );
        }
    }

    /// **The same invariant, on every shader in the directory rather than on the two terrain ones.**
    ///
    /// The test above names `terrain` and `terrain_solid`, and that scope is exactly why a real one
    /// survived: `sun_moon_cycle.wgsl` sampled its sun or its moon from inside an `if` on an interpolated
    /// varying - the same undefined behaviour, in a shader nothing was looking at. A file is covered the
    /// moment it is in the directory now, which is the only version of this that cannot be outgrown.
    ///
    /// What is *not* asserted here is a count of auto-level fetches: those are fine at the top level, and
    /// the terrain pair is where the explicit level was the point. This is only about where a sample sits.
    #[test]
    fn no_shader_this_crate_builds_samples_a_texture_under_a_branch() {
        let Some(directory) = modtree::shader_dir() else {
            modtree::skip("the branch-walk check");
            return;
        };

        let mut checked = 0;

        for entry in std::fs::read_dir(&directory)
            .unwrap_or_else(|err| panic!("{} is unreadable: {err}", directory.display()))
            .map(|entry| entry.expect("a readable directory entry"))
        {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("wgsl") {
                continue;
            }

            let name = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .expect("a UTF-8 file name")
                .to_string();

            let source = std::fs::read_to_string(&path)
                .unwrap_or_else(|err| panic!("{} is unreadable: {err}", path.display()));

            let found = samples_inside_branches(&source);

            assert!(
                found.is_empty(),
                "{name}.wgsl samples a texture inside a branch at {found:?}; the level of detail comes \
                 from derivatives, which WGSL only defines in uniform control flow, so this is undefined \
                 behaviour however uniform the condition looks. Sample both and `select` between the \
                 results, or take the level explicitly with `textureSampleLevel`."
            );

            checked += 1;
        }

        // A walk that found nothing would satisfy the assertion above.
        assert!(
            checked > 8,
            "only {checked} shader(s) were found in {}",
            directory.display()
        );
    }
}

#[cfg(test)]
mod binding_visibility_tests {
    use super::*;
    use crate::wgpu::naga;
    use naga::valid::{Capabilities, ValidationFlags, Validator};
    use wgpu_mc_modtree as modtree;

    /// Every global a function reaches, following the calls it makes.
    fn globals_used(
        module: &naga::Module,
        function: &naga::Function,
        seen: &mut Vec<usize>,
        out: &mut Vec<naga::Handle<naga::GlobalVariable>>,
    ) {
        fn calls_in(
            module: &naga::Module,
            block: &naga::Block,
            seen: &mut Vec<usize>,
            out: &mut Vec<naga::Handle<naga::GlobalVariable>>,
        ) {
            for statement in block.iter() {
                match statement {
                    naga::Statement::Call { function, .. } => into(module, *function, seen, out),
                    naga::Statement::Block(block) => calls_in(module, block, seen, out),
                    naga::Statement::If { accept, reject, .. } => {
                        calls_in(module, accept, seen, out);
                        calls_in(module, reject, seen, out);
                    }
                    naga::Statement::Switch { cases, .. } => {
                        for case in cases {
                            calls_in(module, &case.body, seen, out);
                        }
                    }
                    naga::Statement::Loop {
                        body, continuing, ..
                    } => {
                        calls_in(module, body, seen, out);
                        calls_in(module, continuing, seen, out);
                    }
                    _ => {}
                }
            }
        }

        fn into(
            module: &naga::Module,
            function: naga::Handle<naga::Function>,
            seen: &mut Vec<usize>,
            out: &mut Vec<naga::Handle<naga::GlobalVariable>>,
        ) {
            if seen.contains(&function.index()) {
                return;
            }

            seen.push(function.index());

            let function = &module.functions[function];

            for (_, expression) in function.expressions.iter() {
                match expression {
                    naga::Expression::GlobalVariable(handle) => out.push(*handle),
                    naga::Expression::CallResult(callee) => into(module, *callee, seen, out),
                    _ => {}
                }
            }

            calls_in(module, &function.body, seen, out);
        }

        for (_, expression) in function.expressions.iter() {
            if let naga::Expression::GlobalVariable(handle) = expression {
                out.push(*handle);
            }
        }

        calls_in(module, &function.body, seen, out);
    }

    /// **The solid shader parses and validates**, with naga rather than the device.
    ///
    /// It is a copy of `terrain.wgsl` with one branch taken out, and a copy is exactly the kind of file
    /// that drifts: a binding only one of them declares, a `var` left unused, a name one of them uses and
    /// the other does not. `create_pipelines` skips a pipeline whose shader will not build and says so in
    /// the log, but what that costs is the entire solid layer of the world not being drawn - so it is
    /// worth a test that fails here rather than a log line that is easy to miss.
    #[test]
    fn the_solid_terrain_shader_parses_and_validates() {
        let Some(terrain_solid) = modtree::shader("terrain_solid") else {
            modtree::skip("the solid-shader check");
            return;
        };

        let module =
            naga::front::wgsl::parse_str(&terrain_solid).expect("the solid terrain shader parses");

        Validator::new(ValidationFlags::all(), Capabilities::all())
            .validate(&module)
            .expect("the solid terrain shader validates");
    }

    /// **The size the shader says `SectionPosition` is, and the size this side declares for it, are the
    /// same number - and getting it wrong aborts the process rather than the draw.**
    ///
    /// wgpu computes the immediate's required size from the shader's struct and refuses to draw unless
    /// every byte of it was set:
    ///
    /// ```text
    /// wgpu error: Validation Error
    /// Not all immediate data required by the pipeline has been set via set_immediates
    /// (missing byte ranges: 48..52)
    /// thread caused non-unwinding panic. aborting.
    /// ```
    ///
    /// That is a client that dies on the first frame of the world, and the four missing bytes were a
    /// duplicate field this side was sending and the shader was not reading the way it was written - so the
    /// failure mode of a layout change here is not a wrong colour, it is a process that stops. `naga` gives
    /// the same number the device would, which is what makes this a test.
    ///
    /// Both terrain shaders, because a layout that agreed with one of them and not the other would abort
    /// on whichever layer drew first. They are read out of the Neolectrum checkout, so a clone without the
    /// mod skips this rather than failing it; see `wgpu_mc_modtree`.
    #[test]
    fn the_shaders_section_position_is_the_size_the_immediate_declares() {
        let Some(terrain) = modtree::shader("terrain") else {
            modtree::skip("the immediate-layout checks");
            return;
        };

        let Some(terrain_solid) = modtree::shader("terrain_solid") else {
            modtree::skip("the immediate-layout checks");
            return;
        };

        for (name, source) in [
            ("terrain", terrain.as_str()),
            ("terrain_solid", terrain_solid.as_str()),
        ] {
            let module = naga::front::wgsl::parse_str(source).expect("the terrain shader parses");

            let structure = module
                .types
                .iter()
                .find(|(_, ty)| ty.name.as_deref() == Some("SectionPosition"))
                .map(|(handle, _)| handle)
                .unwrap_or_else(|| panic!("{name}.wgsl declares no `SectionPosition`"));

            let size = module.types[structure].inner.size(module.to_ctx());

            assert_eq!(
                size,
                immediate_size_of("@pc_section_position"),
                "{name}.wgsl's `SectionPosition` is {size} bytes and the immediate is declared {} - wgpu \
                 aborts the process when the shader asks for more than was set",
                immediate_size_of("@pc_section_position")
            );
        }
    }

    /// **Every member of the immediate, by name and by offset, on both sides of it.**
    ///
    /// The size test above is not enough on its own, and this is the bug that says so. The immediate was
    /// a `[u8; 28]` with a hand-written offset table, and the change that took the section's position
    /// out of it left `constants[4..8]` - the atlas level-of-detail bias - with nothing writing it. Every
    /// member is four bytes, so the struct stayed twenty-eight and the test above stayed green; the
    /// picture was a `lod_bias` of zero, which is to say a player's `-4` silently stopped applying, and
    /// the field next to it was one slot from having the bias written into it instead.
    ///
    /// So the check is per member: naga is asked for the shipped struct's member list, and every name,
    /// offset and size is compared against the Rust struct the pass writes. A renamed member, a
    /// reordered pair of same-sized fields, a field added to one side and not the other - all three are
    /// this test, and none of them is anything else.
    ///
    /// `terrain_solid.wgsl` too, because the two share one immediate and a mismatch there aborts the
    /// layer that draws first. Both files come out of the Neolectrum checkout, so a clone without the
    /// mod skips the check rather than failing it.
    #[test]
    fn the_immediates_members_line_up_by_name_and_offset() {
        let Some(terrain) = modtree::shader("terrain") else {
            modtree::skip("the immediate-layout checks");
            return;
        };

        let Some(terrain_solid) = modtree::shader("terrain_solid") else {
            modtree::skip("the immediate-layout checks");
            return;
        };

        for (name, source) in [
            ("terrain", terrain.as_str()),
            ("terrain_solid", terrain_solid.as_str()),
        ] {
            let module = naga::front::wgsl::parse_str(source).expect("the terrain shader parses");

            let members = module
                .types
                .iter()
                .find(|(_, ty)| ty.name.as_deref() == Some("SectionPosition"))
                .and_then(|(_, ty)| match &ty.inner {
                    naga::TypeInner::Struct { members, .. } => Some(members.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{name}.wgsl declares no `SectionPosition` struct"));

            // The same seven names in the same order, taken from the other side. `offset_of!` rather
            // than a written-down number: a literal here would be a second hand-written table, which is
            // the thing this replaced.
            let written = [
                (
                    "alpha_cutout",
                    std::mem::offset_of!(SectionPositionImmediate, alpha_cutout),
                ),
                (
                    "lod_bias",
                    std::mem::offset_of!(SectionPositionImmediate, lod_bias),
                ),
                (
                    "half_texel_ours",
                    std::mem::offset_of!(SectionPositionImmediate, half_texel_ours),
                ),
                (
                    "half_texel_game",
                    std::mem::offset_of!(SectionPositionImmediate, half_texel_game),
                ),
                (
                    "texel_ours",
                    std::mem::offset_of!(SectionPositionImmediate, texel_ours),
                ),
                (
                    "texel_game",
                    std::mem::offset_of!(SectionPositionImmediate, texel_game),
                ),
                (
                    "use_rgss",
                    std::mem::offset_of!(SectionPositionImmediate, use_rgss),
                ),
            ];

            assert_eq!(
                members.len(),
                written.len(),
                "{name}.wgsl's `SectionPosition` has {} member(s) and the immediate this side writes \
                 has {} - a member on one side only is a field the shader reads at another's offset",
                members.len(),
                written.len()
            );

            for (member, (field, offset)) in members.iter().zip(written) {
                assert_eq!(
                    member.name.as_deref(),
                    Some(field),
                    "{name}.wgsl's `SectionPosition` declares `{:?}` where this side writes `{field}`, \
                     and the pass fills its fields by name",
                    member.name
                );
                assert_eq!(
                    member.offset as usize, offset,
                    "{name}.wgsl puts `{field}` at byte {} and this side writes it at byte {offset}",
                    member.offset
                );
                assert_eq!(
                    module.types[member.ty].inner.size(module.to_ctx()),
                    4,
                    "{name}.wgsl's `{field}` is not four bytes, so the Rust struct's stride and this \
                     one's have parted"
                );
            }

            assert_eq!(
                std::mem::size_of::<SectionPositionImmediate>(),
                immediate_size_of("@pc_section_position") as usize,
                "the immediate this side writes is {} bytes and the layout declares {}",
                std::mem::size_of::<SectionPositionImmediate>(),
                immediate_size_of("@pc_section_position")
            );
        }
    }

    /// **The size of one draw's record, on both sides of it.**
    ///
    /// The pass writes `SectionDraw` as a flat array of `#[repr(C)]` structs and the vertex stage reads
    /// `section_draws[instance_index]` out of the same bytes, so the two have to agree on the stride.
    /// wgpu would not refuse a disagreement: the buffer is bound as an untyped run of bytes and only the
    /// shader knows where a record starts, so a shader that read 32 bytes per record would take the
    /// next draw's `x` as this one's `word_base` - and what that looks like is sections drawn at other
    /// sections' positions and vertices read from other sections' slots, not an error anywhere.
    ///
    /// Every member of both is a four-byte scalar, so the struct's size is its stride; the assertion is
    /// written against the size because that is the number naga will give.
    ///
    /// The two shaders are read out of the Neolectrum checkout; without a checkout the check is skipped
    /// rather than failed.
    #[test]
    fn a_draw_record_is_the_size_the_shader_strides_over() {
        let Some(terrain) = modtree::shader("terrain") else {
            modtree::skip("the draw-record checks");
            return;
        };

        let Some(terrain_solid) = modtree::shader("terrain_solid") else {
            modtree::skip("the draw-record checks");
            return;
        };

        for (name, source) in [
            ("terrain", terrain.as_str()),
            ("terrain_solid", terrain_solid.as_str()),
        ] {
            let module = naga::front::wgsl::parse_str(source).expect("the terrain shader parses");

            let structure = module
                .types
                .iter()
                .find(|(_, ty)| ty.name.as_deref() == Some("SectionDraw"))
                .map(|(handle, _)| handle)
                .unwrap_or_else(|| panic!("{name}.wgsl declares no `SectionDraw`"));

            let size = module.types[structure].inner.size(module.to_ctx());

            assert_eq!(
                size as usize,
                std::mem::size_of::<SectionDraw>(),
                "{name}.wgsl's `SectionDraw` is {size} bytes and the one the pass writes is {} - the \
                 records are a flat array, so a different stride is every draw reading its neighbour's \
                 fields",
                std::mem::size_of::<SectionDraw>()
            );
        }
    }

    /// **`section_draws` is visible to the vertex stage alone, and this is the half of that a layout
    /// cannot check for itself.**
    ///
    /// The layout in `create_bind_group_layouts` declares `ShaderStages::VERTEX` for this one binding -
    /// the only binding in this renderer narrower than both stages - and that is legal exactly as long
    /// as no other stage names it. If one ever does, wgpu refuses to build the pipeline:
    ///
    /// ```text
    /// Shader global ResourceBinding { group: 2, binding: 0 } is not available in the pipeline layout
    /// ```
    ///
    /// and that refusal arrives as a panic inside a `#[jni_fn]` frame, which ends the game rather than
    /// the draw. Both terrain shaders, because one is a copy of the other and a change made to one of
    /// them is exactly the shape this is here to catch. They are read out of the Neolectrum checkout, so
    /// a clone of this repository without the mod skips the check rather than failing it.
    #[test]
    fn the_draw_records_are_read_by_the_vertex_stage_alone() {
        let Some(terrain) = modtree::shader("terrain") else {
            modtree::skip("the draw-record visibility checks");
            return;
        };

        let Some(terrain_solid) = modtree::shader("terrain_solid") else {
            modtree::skip("the draw-record visibility checks");
            return;
        };

        for (name, source) in [
            ("terrain", terrain.as_str()),
            ("terrain_solid", terrain_solid.as_str()),
        ] {
            let used = sampled(source);

            assert!(
                used.iter().any(|(stage, kind, group, binding)| {
                    *stage == naga::ShaderStage::Vertex
                        && *group == 2
                        && *binding == 0
                        && *kind == ResourceKind::Storage
                }),
                "{name}.wgsl's vertex stage has to read group 2 binding 0 - the record with the \
                 section's position and its arena slot in it - so if it no longer does, this test is \
                 about nothing and the layout can be narrowed or the batching removed: {used:?}"
            );

            assert!(
                !used.iter().any(|(stage, _, group, binding)| {
                    *stage == naga::ShaderStage::Fragment && *group == 2 && *binding == 0
                }),
                "{name}.wgsl's fragment stage names group 2 binding 0, and the layout declares that \
                 binding visible to the vertex stage only - which is a pipeline wgpu refuses to build, \
                 and a refusal on this path ends the process"
            );
        }
    }

    /// Which global bindings each stage of every entry point of the shader reaches for, and what the
    /// graph's layout would call each one. See [`ResourceKind::visibility`].
    fn sampled(source: &str) -> Vec<(naga::ShaderStage, ResourceKind, u32, u32)> {
        let module = naga::front::wgsl::parse_str(source).expect("the terrain shader parses");

        // The same front end and the same analysis wgpu runs over a shader before it matches it
        // against a pipeline layout, so this is the check that failed at runtime - with naga in place
        // of the device, which is what makes it a test rather than a crash.
        Validator::new(ValidationFlags::all(), Capabilities::all())
            .validate(&module)
            .expect("the terrain shader validates");

        let mut used = Vec::new();

        for entry in &module.entry_points {
            let mut handles = Vec::new();
            globals_used(&module, &entry.function, &mut Vec::new(), &mut handles);

            for handle in handles {
                let variable = &module.global_variables[handle];

                let Some(binding) = variable.binding else {
                    continue;
                };

                let kind = match module.types[variable.ty].inner {
                    naga::TypeInner::Image { .. } => ResourceKind::Texture,
                    naga::TypeInner::Sampler { .. } => ResourceKind::Sampler,
                    _ => match variable.space {
                        naga::AddressSpace::Storage { .. } => ResourceKind::Storage,
                        _ => ResourceKind::Buffer,
                    },
                };

                let usage = (entry.stage, kind, binding.group, binding.binding);

                if !used.contains(&usage) {
                    used.push(usage);
                }
            }
        }

        used
    }

    fn stage_flag(stage: naga::ShaderStage) -> ShaderStages {
        match stage {
            naga::ShaderStage::Vertex => ShaderStages::VERTEX,
            naga::ShaderStage::Fragment => ShaderStages::FRAGMENT,
            naga::ShaderStage::Compute => ShaderStages::COMPUTE,
            // Nothing else is an entry point of a shader this renderer builds a pipeline for, so a
            // stage here is a shader that would have to be looked at rather than mapped to a flag.
            other => panic!("{other:?} is not a stage this renderer builds a pipeline for"),
        }
    }

    /// **The bug this test is for.** The terrain vertex stage fetches the game's lightmap - one texel
    /// per vertex, because the light has to be interpolated as a colour - and the layout this graph
    /// builds said a texture is visible to the fragment stage only. wgpu refused the pipeline:
    ///
    /// ```text
    /// In Device::create_render_pipeline, label = 'terrain'
    ///   Error matching ShaderStages(VERTEX) shader requirements against the pipeline
    ///     Shader global ResourceBinding { group: 0, binding: 7 } is not available in the pipeline layout
    /// ```
    ///
    /// and because the device reports that by panicking inside a `#[jni_fn]` frame - which cannot
    /// unwind - the game ended while entering a world. The terrain was never drawn at all, so the
    /// picture was not "wrong", it was absent.
    ///
    /// The shader is read out of the Neolectrum checkout, so a clone of this repository that has never
    /// seen the mod skips the check rather than failing it; see `wgpu_mc_modtree`.
    #[test]
    fn a_binding_is_visible_to_the_stage_that_samples_it() {
        let Some(terrain) = modtree::shader("terrain") else {
            modtree::skip("the binding-visibility check");
            return;
        };

        let used = sampled(&terrain);

        assert!(
            used.iter().any(|(stage, kind, group, binding)| {
                *stage == naga::ShaderStage::Vertex
                    && *group == 0
                    && *binding == 7
                    && *kind == ResourceKind::Texture
            }),
            "the terrain vertex stage samples group 0 binding 7 - the game's lightmap. If it no longer \
             does, this test is about nothing and the layout can be narrowed again: {used:?}"
        );

        for (stage, kind, group, binding) in used {
            assert!(
                kind.visibility().contains(stage_flag(stage)),
                "group {group} binding {binding} is a {kind:?} that the {stage:?} stage uses, and a \
                 layout that hides it from that stage is a pipeline wgpu refuses to build"
            );
        }
    }
}

#[cfg(test)]
mod lightmap_tests {
    use super::*;

    /// The fallback lightmap is the curve the shader used before it sampled the game's, texel for
    /// texel.
    ///
    /// Worth a test because it is the *only* thing between a failed handover and a world drawn at full
    /// brightness: `max(block, sky) / 15 * 0.7 + 0.3`, per the comment on the function, at the texel
    /// `sample_lightmap` indexes for that pair.
    #[test]
    fn the_fallback_lightmap_is_the_curve_this_shader_used_to_apply() {
        let image = fallback_lightmap();

        let texel = |block: usize, sky: usize| {
            let at = (sky * 16 + block) * 4;
            [image[at], image[at + 1], image[at + 2], image[at + 3]]
        };

        assert_eq!(
            texel(0, 0),
            [77, 77, 77, 255],
            "no light at all: 0.3 of full"
        );
        assert_eq!(
            texel(15, 15),
            [255, 255, 255, 255],
            "full light both ways: 1.0"
        );
        assert_eq!(
            texel(15, 0),
            [255, 255, 255, 255],
            "the brighter of the two is what counts"
        );
        assert_eq!(
            texel(0, 15),
            [255, 255, 255, 255],
            "whichever of the two it is: the old curve took their maximum"
        );
        assert_eq!(
            texel(8, 8),
            [172, 172, 172, 255],
            "8/15 of the way up the curve"
        );
    }
}

#[cfg(test)]
mod culling_tests {
    use super::*;
    use glam::Mat4;

    /// A section in front of the camera is tested where it is drawn, not where its name points.
    ///
    /// The pass works in **camera-section-relative** blocks: the graph writes `(section -
    /// camera_section)` into the immediate, the view matrix carries the camera's offset inside its own
    /// section, and the frustum is built from that same pair. A box built from the section's absolute
    /// name is therefore thousands of blocks away from the geometry it stands for - the ground under
    /// the player becomes a box outside the frustum and is culled, which is terrain missing exactly
    /// where the player is looking, with the water and the sky behind it left in its place.
    #[test]
    fn a_section_below_the_camera_is_tested_where_it_is_drawn() {
        // The camera in its section, and the identity rotation, which is all this test needs: what is
        // being asked is which *space* the boxes and the frustum are in.
        let camera = glam::Vec3::new(0.0, 100.0, 0.0);
        let camera_section = glam::IVec3::new(0, 6, 0);
        let projection = Mat4::perspective_rh(70f32.to_radians(), 16.0 / 9.0, 0.05, 256.0);

        // The view the JVM builds: the rotation with the camera's offset inside its section, which is
        // `100.0 - 6 * 16 = 4.0` here.
        let offset = camera - camera_section.as_vec3() * 16.0;
        let view = Mat4::from_translation(-offset);
        let frustum = Frustum::from_modelview_projection(with_gl_depth_range(
            (projection * view).to_cols_array_2d(),
        ));

        let size = glam::Vec3::new(16.0, 16.0, 16.0);

        // A section one under the camera's own and forty blocks in front of it, in the space the graph
        // sends: `section - camera_section`.
        let section = glam::IVec3::new(0, 5, -3);
        let relative: glam::Vec3 = (section - camera_section).as_vec3() * 16.0;
        let bounds = AABB::new(relative.to_array(), (relative + size).to_array());
        assert!(
            bounds.coherent_test_against_frustum(&frustum, 0).0,
            "the section in front of the camera was culled where it is drawn"
        );

        // And the same section built from its absolute name, which is where a transform that forgot the
        // camera's section would put it: a hundred blocks up in the air, out of the frustum, gone.
        let named_at: glam::Vec3 = section.as_vec3() * 16.0;
        let named = AABB::new(named_at.to_array(), (named_at + size).to_array());
        assert!(
            !named.coherent_test_against_frustum(&frustum, 0).0,
            "a box built from the absolute name is in front of the camera, so the relative position is \
             not what the frustum is measured in"
        );
    }

    /// The reason the transform is camera-section-relative at all: the vertex that reaches the depth
    /// buffer keeps a millionth of a block of accuracy far from the origin, where the absolute form lost
    /// four thousandths of one.
    ///
    /// This is the entity-shadow stripes, as arithmetic. An entity's shadow is a quad lying on the top
    /// face of a block (`EntityRenderer#extractShadowPiece`), drawn afterwards with
    /// `LESS_THAN_OR_EQUAL` - so it is *coplanar* with the terrain face under it, and whether the depth
    /// test sees the shadow or the ground is decided by the last bits of two positions that are supposed
    /// to be the same number. The error that decided it was a function of the world position and not of
    /// the camera, which is why the stripes stayed where they were while the camera moved.
    ///
    /// Both arrangements are the same matrix pair applied to the same world point; what differs is where
    /// the big number is formed. Both are run here the way the shader runs them - the position is summed
    /// in `f32` and then multiplied by the matrix in `f32` - against an `f64` reference, so what the two
    /// errors measure is what actually reaches the depth buffer.
    #[test]
    fn a_vertex_keeps_its_accuracy_far_from_the_origin() {
        // A camera and a block on the ground, two hundred thousand blocks out: a world that has been
        // walked a long way, and not the worst case by any means.
        let camera = glam::DVec3::new(200_000.37, 71.62, -200_000.19);
        let section = glam::IVec3::new(12_500, 4, -12_501);
        let camera_section = glam::IVec3::new(12_500, 4, -12_501);

        // A vertex on a block's top face: the section's own corner, on the sixteenth grid.
        let local = glam::DVec3::new(13.0, 16.0, 5.0);

        // The whole chain runs in `f32` in the shader, which is the point: the sum is formed there and
        // the matrix multiplies it there.
        let turn = glam::Mat4::from_rotation_y(0.7) * glam::Mat4::from_rotation_x(0.3);
        let exact_turn = glam::DMat4::from_rotation_y(0.7) * glam::DMat4::from_rotation_x(0.3);

        let world = section.as_dvec3() * 16.0 + local;
        let exact = exact_turn * (world - camera).extend(1.0);

        // What the pass does now: the section relative to the camera's, and the camera's offset inside
        // its own section in the matrix.
        let offset = camera - camera_section.as_dvec3() * 16.0;
        let relative = (section - camera_section).as_dvec3() * 16.0 + local;
        let now = turn * (relative.as_vec3() - offset.as_vec3()).extend(1.0);

        // What it did before: the section's absolute position, and the camera's position in the matrix.
        let absolute = section.as_dvec3() * 16.0 + local;
        let before = turn * (absolute.as_vec3() - camera.as_vec3()).extend(1.0);

        let error = |value: glam::Vec4| (value.as_dvec4() - exact).truncate().length();

        let now_error = error(now);
        let before_error = error(before);

        assert!(
            now_error < 1e-4,
            "the camera-section-relative transform is {now_error} blocks out, which is more than the \
             depth buffer can absorb"
        );
        assert!(
            before_error > 100.0 * now_error,
            "the absolute transform should be the one that loses the precision: {before_error} against \
             {now_error} blocks"
        );
    }

    /// The conversion puts a `0..1` projection into the convention the culler reads, and the culler
    /// keeps what is in front of the camera and drops what is behind it.
    ///
    /// This is the whole of the culler's contract with the terrain pass: Minecraft's camera hands over a
    /// `joml` `perspective` built with the device's depth range, and every section the arena holds is
    /// asked about with it. The test is worth its lines because the failure mode is quiet from the
    /// outside - "0 section(s) drawn, N culled by the frustum" is one line in the log, and the picture
    /// is a world with holes in it.
    #[test]
    fn a_zero_to_one_projection_culls_like_the_gl_one() {
        // The pass works in camera-section-relative blocks, so these are 16-block boxes with the camera
        // at the origin looking down -z, which is the space a right-handed projection describes.
        let around_the_camera = AABB::new([-8.0f32, -8.0, -8.0], [8.0f32, 8.0, 8.0]);
        let in_front = AABB::new([-8.0f32, -8.0, -40.0], [8.0f32, 8.0, -24.0]);
        let behind = AABB::new([-8.0f32, -8.0, 24.0], [8.0f32, 8.0, 40.0]);

        // `glam`'s `perspective_rh` is the `0..1` range and `perspective_rh_gl` the `-1..1` one, which
        // is the pair of conventions Minecraft's `Projection` can produce.
        let zero_to_one = Mat4::perspective_rh(70f32.to_radians(), 16.0 / 9.0, 0.05, 256.0);
        let minus_one_to_one = Mat4::perspective_rh_gl(70f32.to_radians(), 16.0 / 9.0, 0.05, 256.0);

        let converted =
            Frustum::from_modelview_projection(with_gl_depth_range(zero_to_one.to_cols_array_2d()));
        let already_gl = Frustum::from_modelview_projection(minus_one_to_one.to_cols_array_2d());

        // The two matrices describe the same volume, so the converted frustum is the other one's
        // planes: this is what says the conversion is the depth range and nothing else.
        for (index, (from_zero_to_one, gl)) in converted
            .planes
            .iter()
            .zip(already_gl.planes.iter())
            .enumerate()
        {
            let difference = (from_zero_to_one.x - gl.x).abs()
                + (from_zero_to_one.y - gl.y).abs()
                + (from_zero_to_one.z - gl.z).abs()
                + (from_zero_to_one.w - gl.w).abs();
            // Relative rather than absolute: the far plane's `w` is the far distance itself (256 here),
            // and the two `glam` constructors reach it through different divisions, so their last bits
            // differ. A wrong depth range moves a plane by its whole length, which this still catches.
            let scale = 1.0 + gl.x.abs().max(gl.y.abs()).max(gl.z.abs()).max(gl.w.abs());
            assert!(
                difference < 1e-3 * scale,
                "plane {index} differs by {difference}: converted {from_zero_to_one:?}, native {gl:?}"
            );
        }

        for (name, frustum) in [
            ("converted 0..1", &converted),
            ("native -1..1", &already_gl),
        ] {
            for (label, bounds) in [
                ("the camera's own section", &around_the_camera),
                ("a section in front of it", &in_front),
            ] {
                assert!(
                    bounds.coherent_test_against_frustum(frustum, 0).0,
                    "{label} was culled with a {name} projection"
                );
            }

            assert!(
                !behind.coherent_test_against_frustum(frustum, 0).0,
                "a section behind the camera was kept with a {name} projection"
            );
        }
    }
}

/// **The detector fires.** A test that asserts "nothing was found" cannot be told from a test whose
/// detector never finds anything, and this one is a walk over a parsed module with a branch-depth
/// counter in it - exactly the shape that can silently look at the wrong arena or the wrong statement.
///
/// So the shader this test exists for is fed to it as a string: the branch over `game_atlas`, the two
/// `textureSample`s inside it, and the expectation that both are found and both are reported as being
/// in a branch. If the walk ever stops working, this fails before the real test can pass for the wrong
/// reason.
#[cfg(test)]
mod uniformity_detector_tests {
    use super::texture_sample_uniformity_tests as detector;

    /// The shape `terrain.wgsl` had, kept as a fixture rather than described in a comment.
    const BRANCHING_TERRAIN: &str = r#"
@group(0) @binding(0) var t_game_atlas: texture_2d<f32>;
@group(0) @binding(1) var t_game_sampler: sampler;
@group(0) @binding(2) var t_texture: texture_2d<f32>;
@group(0) @binding(3) var t_sampler: sampler;

struct V {
    @builtin(position) pos: vec4<f32>,
    @location(0) tex_coords: vec2<f32>,
    @interpolate(flat) @location(1) game_atlas: u32,
};

@fragment
fn frag(in: V) -> @location(0) vec4<f32> {
    var texel: vec4<f32>;
    if (in.game_atlas == 1u) {
        texel = textureSample(t_game_atlas, t_game_sampler, in.tex_coords);
    } else {
        texel = textureSample(t_texture, t_sampler, in.tex_coords);
    }

    return texel;
}
"#;

    #[test]
    fn the_branching_shape_this_was_written_for_is_reported() {
        let found = detector::samples_inside_branches(BRANCHING_TERRAIN);

        assert_eq!(
            found.len(),
            2,
            "both fetches are inside the branch and both have to be reported: {found:?}"
        );

        for (entry, depth) in found {
            assert_eq!(entry, "frag");
            assert_eq!(
                depth, 1,
                "each is one branch deep, which is what makes it a finding"
            );
        }
    }

    /// And the fixed shape is not, which is what the depth is carried for: a sample at the entry
    /// point's own top level is uniform control flow and is where these belong.
    #[test]
    fn the_hoisted_shape_is_not_reported() {
        let hoisted = BRANCHING_TERRAIN
            .replace(
                "    var texel: vec4<f32>;\n    if (in.game_atlas == 1u) {\n        texel = textureSample(t_game_atlas, t_game_sampler, in.tex_coords);\n    } else {\n        texel = textureSample(t_texture, t_sampler, in.tex_coords);\n    }\n",
                "    let from_game = textureSample(t_game_atlas, t_game_sampler, in.tex_coords);\n    let from_ours = textureSample(t_texture, t_sampler, in.tex_coords);\n    let texel = select(from_ours, from_game, in.game_atlas == 1u);\n",
            );

        assert_ne!(
            hoisted, BRANCHING_TERRAIN,
            "the fixture did not change, so this test is checking the wrong string"
        );

        assert!(
            detector::samples_inside_branches(&hoisted).is_empty(),
            "hoisting the samples out of the branch is the fix, and it has to read as one"
        );
    }
}

/// **A varying the fragment stage declares and never reads is a varying nobody filled in.**
///
/// This is the test for the class of bug that `tex_coords2` and `blend` were: the vertex stage wrote
/// `vec2(0.0, 0.0)` and `0.0` into them, the fragment stage never mentioned them, and the two facts
/// together look like a gap - "these are hardcoded, something should be filling them in" - when they are
/// in fact dead. The compiler removes a varying nothing reads, so the value never reached the GPU and no
/// picture ever depended on it, but the source reads as though one should.
///
/// **It is a detector and not a blanket assertion**, which is the part worth explaining. The terrain
/// shaders do have other unused varyings on purpose - `normal`, `section`, `ao`, `light_uv` - and they
/// are documented as unused where they are declared, because the *vertex format* carries them and
/// dropping them is a different change from dropping these two. A blanket "every declared varying is
/// read" would fail on those today and be deleted by whoever hit it.
///
/// What it asserts instead is that **no varying is silently dead**: every struct member with a location
/// that the fragment stage never reads must be named in the shader as deliberately unread. The check for
/// that is `intentionally_unread` below, and it is deliberately dumb - it looks for the member's own
/// name next to the word, in the text above the struct - so satisfying it means a person wrote down why,
/// which is the whole point.
#[cfg(test)]
mod shader_interface_tests {
    use crate::wgpu::naga;
    use wgpu_mc_modtree as modtree;

    /// Whether the source says, next to this member's declaration, that it is not read.
    ///
    /// **Line-based, anchored on the declaration.** Both halves matter:
    ///
    ///  - the anchor is the line that declares the member, `@location(N) name:`, and not the first
    ///    place the member's *name* appears - because a name also appears inside the comment that
    ///    explains some other member, and a search from there finds a window that happens to cover the
    ///    wrong declaration;
    ///  - the window is the comment block directly above that line, and not a fixed number of characters
    ///    back - because a fixed window runs past the comment it was looking for and picks up whatever
    ///    prose came before it, which is how the first version of this said "yes" to everything.
    ///
    /// It is still not parsing prose: the phrase has to be there, and the person who wrote the member
    /// has to have written it.
    fn intentionally_unread(source: &str, member: &str) -> bool {
        let lines: Vec<&str> = source.lines().collect();

        // The declaration's own line, found by the attribute that opens it.
        let Some(at) = lines.iter().position(|line| {
            let trimmed = line.trim_start();
            (trimmed.starts_with("@location(") || trimmed.starts_with("@builtin("))
                && trimmed.contains(&format!(") {member}:"))
        }) else {
            return false;
        };

        // And the contiguous comment block directly above it, which is where a reason would be written.
        let above = lines[..at]
            .iter()
            .rev()
            .take_while(|line| {
                let trimmed = line.trim_start();
                trimmed.starts_with("//")
            })
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
            .to_lowercase();

        [
            "unused",
            "never read",
            "nothing reads",
            "not read",
            "does not read",
        ]
        .iter()
        .any(|phrase| above.contains(phrase))
    }

    /// Every call a function makes, at any depth of its own control flow.
    ///
    /// **A call is a `Statement` in naga, not an expression** - the expression arena holds only the
    /// `CallResult` - so following calls out of an entry point means walking the body, and the body is a
    /// tree: a call inside an `if`, a loop or a `switch` is a call. The shader this is written for calls
    /// `shade` and `fragment_geometry` at the top level of each entry point, so a shallow walk would work
    /// today and stop working the first time one of them moves into a branch.
    pub(super) fn for_each_call(
        statements: &[naga::Statement],
        visit: &mut impl FnMut(naga::Handle<naga::Function>, &[naga::Handle<naga::Expression>]),
    ) {
        for statement in statements {
            match statement {
                naga::Statement::Call {
                    function,
                    arguments,
                    ..
                } => visit(*function, arguments),
                naga::Statement::Block(block) => for_each_call(block, visit),
                naga::Statement::If { accept, reject, .. } => {
                    for_each_call(accept, visit);
                    for_each_call(reject, visit);
                }
                naga::Statement::Loop {
                    body, continuing, ..
                } => {
                    for_each_call(body, visit);
                    for_each_call(continuing, visit);
                }
                naga::Statement::Switch { cases, .. } => {
                    for case in cases {
                        for_each_call(&case.body, visit);
                    }
                }
                _ => {}
            }
        }
    }

    /// The location-bearing struct members **no fragment entry point reads**.
    ///
    /// Per entry point, and then across them: a varying that one variant reads is not dead. The vertex
    /// stage is shared between `frag` and `frag_game_atlas`, so the single-atlas variant pays the write of
    /// `game_atlas` and never looks at it - deliberately, because a vertex entry point of its own would be
    /// a second copy of the hottest code in the renderer to save two selects per vertex. See
    /// `the_single_atlas_entry_point_samples_one_atlas` for what *is* asserted about that variant.
    ///
    /// **The reads are followed through calls**, and that is not a detail: the body that reads the
    /// varyings is `shade` and `fragment_geometry`, which both entry points call. The first version of this
    /// looked only at the entry point's own expression arena, which was right while the fragment stage was
    /// one function and sees a fragment stage that reads nothing at all now.
    ///
    /// A called function's argument is mapped back to the entry point's own: `shade(in, texel)` passes the
    /// entry point's `in` as `shade`'s first argument, so an `AccessIndex` off that handle is a read of the
    /// same varying. A worklist of `(function, argument)` pairs, starting from the entry point's `in`, is
    /// the whole of it. `None` is the entry point's own function, which is not in `module.functions`.
    fn unread_varyings(source: &str) -> Vec<String> {
        use std::collections::HashSet;

        let module = naga::front::wgsl::parse_str(source).expect("the shader parses");

        let named: Vec<(usize, &naga::Function)> = module
            .functions
            .iter()
            .map(|(handle, function)| (handle.index(), function))
            .collect();

        // Per entry point, the members it does not read; the answer is their intersection.
        let mut unread_by_entry: Vec<Vec<String>> = Vec::new();

        for entry in &module.entry_points {
            if entry.stage != naga::ShaderStage::Fragment {
                continue;
            }

            let mut unread: Vec<String> = Vec::new();

            for (which_arg, argument) in entry.function.arguments.iter().enumerate() {
                let naga::TypeInner::Struct { members, .. } = &module.types[argument.ty].inner
                else {
                    continue;
                };

                let mut touched: Vec<usize> = Vec::new();
                let mut seen: HashSet<(Option<usize>, u32)> = HashSet::new();
                let mut queue: Vec<(Option<usize>, u32)> = vec![(None, which_arg as u32)];

                while let Some((caller, arg)) = queue.pop() {
                    if !seen.insert((caller, arg)) {
                        continue;
                    }

                    let function = match caller {
                        None => &entry.function,
                        Some(index) => named
                            .iter()
                            .find(|(handle, _)| *handle == index)
                            .map(|(_, function)| *function)
                            .expect("a callee of a function in this module"),
                    };

                    for (_, expression) in function.expressions.iter() {
                        // Every field of this argument the function touches. A read of `in.ao3` is an
                        // `AccessIndex` off *this* argument handle - the probe this was written from
                        // shows `AccessIndex(base=FunctionArgument(0), idx=field)` for each one - so the
                        // set of indices reached that way is the set of fields that mean something.
                        if let naga::Expression::AccessIndex { base, index } = expression
                            && let Ok(naga::Expression::FunctionArgument(the_arg)) =
                                function.expressions.try_get(*base)
                            && *the_arg == arg
                        {
                            touched.push(*index as usize);
                        }
                    }

                    // And what it hands to whatever it calls, which is how a read inside `shade` is
                    // attributed to the entry point that called it.
                    for_each_call(&function.body, &mut |callee, arguments| {
                        for (position, passed) in arguments.iter().enumerate() {
                            if let Ok(naga::Expression::FunctionArgument(from)) =
                                function.expressions.try_get(*passed)
                                && *from == arg
                            {
                                queue.push((Some(callee.index()), position as u32));
                            }
                        }
                    });
                }

                for (field, member) in members.iter().enumerate() {
                    if !matches!(member.binding, Some(naga::Binding::Location { .. })) {
                        // A `@builtin(position)` is not a varying between stages in the same sense.
                        continue;
                    }

                    if !touched.contains(&field) {
                        unread.push(
                            member
                                .name
                                .clone()
                                .unwrap_or_else(|| format!("field {field}")),
                        );
                    }
                }
            }

            unread_by_entry.push(unread);
        }

        // A member is dead only when **every** variant ignores it.
        unread_by_entry
            .first()
            .map(|first| {
                first
                    .iter()
                    .filter(|member| unread_by_entry.iter().all(|entry| entry.contains(member)))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The two terrain shaders carry no varying their fragment stage never reads without saying so.
    ///
    /// They are read out of the Neolectrum checkout, so a clone of this repository that has never seen
    /// the mod skips the check rather than failing it; see `wgpu_mc_modtree`.
    #[test]
    fn the_terrain_shaders_do_not_carry_a_silently_dead_varying() {
        for name in ["terrain", "terrain_solid"] {
            let Some(source) = modtree::shader(name) else {
                modtree::skip("the dead-varying check");
                return;
            };

            let unexplained: Vec<String> = unread_varyings(&source)
                .into_iter()
                .filter(|member| !intentionally_unread(&source, member))
                .collect();

            assert!(
                unexplained.is_empty(),
                "{name}.wgsl declares {} varying(s) its fragment stage never reads and does not say so: \
                 {unexplained:?}.\n\n\
                 A varying that is written and never read is one the compiler removes, so nothing depends \
                 on its value - but the write reads as though something should be filling it in, and the \
                 next person to look has to search the whole vertex format to find out that nothing \
                 should. Either drop the varying, or say beside it that it is unread and why.",
                unexplained.len()
            );
        }
    }

    /// **The detector fires.** A test that asserts "nothing was found" cannot be told from one whose
    /// detector never finds anything, and this one walks an expression arena looking for accesses to a
    /// particular argument - exactly the shape that can silently look at the wrong handle.
    ///
    /// The fixture is the shape `terrain.wgsl` had: a varying the vertex stage writes and the fragment
    /// stage never mentions.
    #[test]
    fn a_varying_that_is_written_and_never_read_is_reported() {
        let dead = r#"
struct VertexResult {
    @builtin(position) pos: vec4<f32>,
    @location(0) tex_coords: vec2<f32>,
    @location(1) tex_coords2: vec2<f32>,
    @location(2) blend: f32,
};

@vertex
fn vert(@builtin(vertex_index) vi: u32) -> VertexResult {
    var out: VertexResult;
    out.pos = vec4<f32>(0.0, 0.0, 0.0, 1.0);
    out.tex_coords = vec2<f32>(1.0, 0.5);
    out.tex_coords2 = vec2<f32>(0.0, 0.0);
    out.blend = 0.0;
    return out;
}

@fragment
fn frag(in: VertexResult) -> @location(0) vec4<f32> {
    return vec4<f32>(in.tex_coords, 0.0, 1.0);
}
"#;

        let mut unread = unread_varyings(dead);
        unread.sort();

        assert_eq!(
            unread,
            ["blend", "tex_coords2"],
            "both are written by the vertex stage and never read by the fragment stage, and the one that \
             *is* read must not be reported"
        );

        // And the other half of the detector: naming the reason is what makes it acceptable, which is
        // what the escape hatch is for and therefore what has to work. The comment has to be *directly*
        // above the declaration - that is the window - and one member at a time, so `blend` is still
        // reported while `tex_coords2` is not.
        let explained = dead.replace(
            "    @location(1) tex_coords2: vec2<f32>,",
            "    // Unused: the format reserves the slot and nothing reads it.\n    @location(1) tex_coords2: vec2<f32>,",
        );

        assert_ne!(explained, dead, "the fixture did not change");

        let unexplained: Vec<String> = unread_varyings(&explained)
            .into_iter()
            .filter(|member| !intentionally_unread(&explained, member))
            .collect();

        assert_eq!(
            unexplained,
            ["blend"],
            "`tex_coords2` now says it is unused and `blend` still does not, so exactly one is left"
        );
    }
}

#[cfg(test)]
mod terrain_frame_tests {
    use super::*;

    /// A key that matches nothing on its own, so each field can be changed one at a time.
    fn base_key() -> TerrainFrameKey {
        TerrainFrameKey {
            frame: 7,
            frustum: [[1.0, 2.0, 3.0, 4.0]; 6],
            camera_section: glam::IVec3::new(3, 4, 5),
            visible: 11,
            model_translation: [0.0, 0.0, 0.0],
        }
    }

    /// **Every field of the frame key is load-bearing**, and this is what says so one at a time.
    ///
    /// A field that does not break the match is a field that does not invalidate the cache, and the
    /// failure that makes is not a slow frame: a list that is reused when it should not be is a frame
    /// drawn from sections that are no longer there, or from arena ranges that have been handed back.
    /// The two that are easiest to leave out are the ones that look like they are already in the frustum:
    /// the camera's *section* (the view matrix carries only the offset within it), and the frame (a
    /// camera that has not moved produces the same frustum every frame while a section's ranges stop
    /// being valid after the frames that may still draw them).
    #[test]
    fn every_part_of_the_frame_key_breaks_the_match_on_its_own() {
        let base = base_key();

        let moved = |key: TerrainFrameKey| {
            assert_ne!(
                key, base,
                "this field does not take part in the comparison, so changing it alone would serve the \
                 cached list"
            );
        };

        moved(TerrainFrameKey {
            frame: base.frame + 1,
            ..base
        });
        moved(TerrainFrameKey {
            frustum: [[9.0, 2.0, 3.0, 4.0]; 6],
            ..base
        });
        moved(TerrainFrameKey {
            camera_section: base.camera_section + glam::IVec3::X,
            ..base
        });
        moved(TerrainFrameKey {
            visible: base.visible + 1,
            ..base
        });
        moved(TerrainFrameKey {
            model_translation: [1.0, 0.0, 0.0],
            ..base
        });

        assert_eq!(base, base_key(), "the key is not comparable with itself");
    }

    /// **A new frame expires the scratch**, which is what [`begin_frame`] is for.
    ///
    /// The counter is read when the key is built, so a frame that is announced makes the next key differ
    /// from the last one whatever else has or has not changed - and that is the whole mechanism: a camera
    /// standing still produces the same frustum, the same camera section and (with the game's list
    /// unchanged) the same revision, but it may not produce the same *frame*.
    #[test]
    fn begin_frame_expires_the_scratch() {
        let before = FRAME.load(Ordering::Relaxed);

        begin_frame();

        assert_eq!(
            FRAME.load(Ordering::Relaxed),
            before + 1,
            "the frame counter did not move, so a scratch built before this call would still match"
        );
    }

    /// The scratch starts empty and with one draw set per pass slot, so a pass can index its own.
    #[test]
    fn the_scratch_has_one_draw_set_per_pass_slot() {
        // Built the way `RenderGraph::new` builds it, which is what the pass relies on: `passes[slot]`
        // is a lookup that must not be out of bounds for any slot a pipeline could claim.
        let passes: Vec<PassDraws> = (0..crate::mc::SECTION_DRAW_SLOTS)
            .map(|_| PassDraws::default())
            .collect();

        assert_eq!(passes.len(), crate::mc::SECTION_DRAW_SLOTS);
        assert_eq!(
            passes.len(),
            // The layers, in the order `RenderLayer` declares them. Written out rather than counted,
            // because nothing on `RenderLayer` iterates - a fourth variant has to be added here too,
            // which is the point: `SECTION_DRAW_SLOTS` and the `[_; 3]` arrays beside it are written as
            // three, and a new layer would need all of them.
            [
                RenderLayer::Solid,
                RenderLayer::Cutout,
                RenderLayer::Transparent
            ]
            .len(),
            "there is one section-draw buffer per pass slot and `SECTION_DRAW_SLOTS` is written as the \
             number of layers - if a layer were added, every slot index past the new one would be wrong"
        );
    }

    /// **The two answers `starts_a_new_frame` has to give**, and why both matter.
    ///
    /// Read as "new" when it is not, a pass that has already recorded its draws has the record buffer
    /// rewritten under it: the recorded counts and offsets then describe a list that is no longer there.
    /// Read as "not new" when it is, the drawn flags are never cleared and every frame after the first
    /// rebuilds nothing - the arena is walked once, for the whole session, and the world stops following
    /// the camera. The first version of this had the second bug, which is why the flags and the frame
    /// counter are one test rather than two lines in the middle of a two-hundred-line function.
    #[test]
    fn a_frame_is_new_only_when_its_number_is() {
        let mut frame = TerrainFrame::default();

        let first = base_key();
        assert!(
            frame.starts_a_new_frame(&first),
            "a scratch that has never been built must be built, or the first frame draws nothing"
        );

        // What the rebuild does at the end, which is what the next call is compared against.
        frame.key = Some(first);

        assert!(
            !frame.starts_a_new_frame(&first),
            "the same frame asked twice is a second view inside it, not a new frame"
        );

        // The same frame number with *everything else* different is still the same frame: a second view
        // in one frame is what the drawn flags exist for, and it must not clear them.
        let mut second_view = first;
        second_view.frustum = [[9.0, 2.0, 3.0, 4.0]; 6];
        second_view.camera_section = glam::IVec3::new(-1, -2, -3);
        second_view.visible = first.visible + 1;

        assert!(
            !frame.starts_a_new_frame(&second_view),
            "a different view inside the same frame cleared the drawn flags, so a pass that has already \
             drawn would have its records rewritten under it"
        );

        let next = TerrainFrameKey {
            frame: first.frame + 1,
            ..second_view
        };

        assert!(
            frame.starts_a_new_frame(&next),
            "a new frame did not clear the drawn flags, so nothing would ever be rebuilt again"
        );
    }
}

#[cfg(test)]
mod atlas_entry_point_tests {
    use super::shader_interface_tests::for_each_call;
    use crate::wgpu::naga;
    use std::collections::{BTreeSet, HashSet};
    use wgpu_mc_modtree as modtree;

    /// Every module-scope variable one entry point can reach, following the calls out of it.
    ///
    /// "Reachable" and not "mentioned in the entry point's own body": the fragment stage's sampling is
    /// three functions deep (`frag` calls `sample_at_level`, which names the texture and the sampler), so
    /// a walk that stopped at the entry point would find no textures at all and this test would pass on a
    /// shader that samples everything.
    fn globals_reached(source: &str, entry: &str) -> BTreeSet<String> {
        let module = naga::front::wgsl::parse_str(source).expect("the shader parses");

        let entry_point = module
            .entry_points
            .iter()
            .find(|candidate| candidate.name == entry)
            .unwrap_or_else(|| panic!("{entry} is not an entry point of this shader"));

        let named: Vec<(usize, &naga::Function)> = module
            .functions
            .iter()
            .map(|(handle, function)| (handle.index(), function))
            .collect();

        let mut seen: HashSet<Option<usize>> = HashSet::new();
        let mut queue: Vec<Option<usize>> = vec![None];
        let mut reached = BTreeSet::new();

        while let Some(index) = queue.pop() {
            if !seen.insert(index) {
                continue;
            }

            let function = match index {
                None => &entry_point.function,
                Some(handle) => named
                    .iter()
                    .find(|(candidate, _)| *candidate == handle)
                    .map(|(_, function)| *function)
                    .expect("a callee of a function in this module"),
            };

            for (_, expression) in function.expressions.iter() {
                if let naga::Expression::GlobalVariable(variable) = expression
                    && let Some(name) = &module.global_variables[*variable].name
                {
                    reached.insert(name.clone());
                }
            }

            for_each_call(&function.body, &mut |callee, _| {
                queue.push(Some(callee.index()));
            });
        }

        reached
    }

    /// **The single-atlas entry point reaches one atlas, and the two-atlas one reaches both.**
    ///
    /// This is the claim that justifies having a second entry point at all, and it is checkable in the
    /// only place that matters - the source the pipeline is built from. `frag_game_atlas` does not mention
    /// this side's texture or any of its three samplers *anywhere in its call graph*, so there is no
    /// branch for naga to keep and no folding for a driver to do; `frag` mentions both atlases, so the
    /// choice between the two entry points is a real choice and the fallback still exists.
    ///
    /// What it does **not** assert is anything about the machine code the driver produces, and it cannot:
    /// the point of removing the branch from the source is precisely that nothing downstream has to.
    ///
    /// The shader files come out of the Neolectrum checkout - the same files and the same directory the
    /// other shader tests read, so a shader that moves is missing from all of them at once - and a
    /// checkout that is not there skips the check rather than failing it; see `wgpu_mc_modtree`.
    #[test]
    fn the_single_atlas_entry_point_samples_one_atlas() {
        const THE_GAMES: [&str; 4] = [
            "t_game_atlas",
            "t_game_sampler",
            "t_game_sampler_magnify",
            "t_game_sampler_animated",
        ];
        const OURS: [&str; 4] = [
            "t_texture",
            "t_sampler",
            "t_sampler_magnify",
            "t_sampler_animated",
        ];

        let Some(terrain) = modtree::shader("terrain") else {
            modtree::skip("the single-atlas entry-point check");
            return;
        };

        let Some(terrain_solid) = modtree::shader("terrain_solid") else {
            modtree::skip("the single-atlas entry-point check");
            return;
        };

        for (name, source) in [
            ("terrain", terrain.as_str()),
            ("terrain_solid", terrain_solid.as_str()),
        ] {
            let single = globals_reached(source, "frag_game_atlas");

            for variable in THE_GAMES {
                assert!(
                    single.contains(variable),
                    "{name}.wgsl's `frag_game_atlas` does not reach {variable}, so it is not the \
                     game's atlas it samples: {single:?}"
                );
            }

            for variable in OURS {
                assert!(
                    !single.contains(variable),
                    "{name}.wgsl's `frag_game_atlas` reaches {variable}, so the second atlas is still in \
                     the shader a terrain pipeline is built from - which is the whole thing the entry \
                     point exists to remove: {single:?}"
                );
            }

            // And the other one still has both, or the choice between them would be no choice at all.
            let both = globals_reached(source, "frag");

            for variable in THE_GAMES.into_iter().chain(OURS) {
                assert!(
                    both.contains(variable),
                    "{name}.wgsl's `frag` does not reach {variable}, so it is not the two-atlas variant \
                     any more and the fallback has nowhere to go: {both:?}"
                );
            }
        }
    }
}

#[cfg(test)]
mod indirect_capacity_tests {
    use super::*;

    /// **The boundary, written down.** A pass with exactly `INDIRECT_DRAW_CAPACITY` calls is batched - the
    /// argument buffer is allocated at that size, so the last slot is a slot - and one call more is the cliff
    /// the warning line exists for. The decision and the warning share this predicate, which is the point of
    /// it being a function: a counter that disagreed with the branch it reports would be a number about
    /// nothing.
    #[test]
    fn the_capacity_is_inclusive_and_the_next_call_is_the_cliff() {
        assert!(the_indirect_path_holds(0));
        assert!(the_indirect_path_holds(crate::mc::INDIRECT_DRAW_CAPACITY));
        assert!(!the_indirect_path_holds(
            crate::mc::INDIRECT_DRAW_CAPACITY + 1
        ));
    }
}
