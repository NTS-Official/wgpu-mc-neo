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
use crate::mc::chunk::RenderLayer;
use crate::mc::entity::InstanceVertex;
use crate::mc::resource::ResourcePath;
use crate::render::entity::EntityVertex;
use crate::render::pipeline::{BLOCK_ATLAS, QuadVertex};
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
static TERRAIN_DRAWN: AtomicU64 = AtomicU64::new(0);
static TERRAIN_CULLED: AtomicU64 = AtomicU64::new(0);
static TERRAIN_EMPTY: AtomicU64 = AtomicU64::new(0);
static TERRAIN_REPORTED: AtomicU64 = AtomicU64::new(0);

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

/// Reports what the terrain pass drew, once a second and only while the section diagnostics are on.
///
/// The counters are always kept - they are relaxed increments on the render thread, one per section -
/// because the line they feed is the only place a run says whether the Rust terrain reached the
/// screen at all.
fn report_terrain_pass() {
    let drawn = TERRAIN_DRAWN.swap(0, Ordering::Relaxed);
    let culled = TERRAIN_CULLED.swap(0, Ordering::Relaxed);
    let empty = TERRAIN_EMPTY.swap(0, Ordering::Relaxed);

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

    log::info!(
        "wgpu-mc: terrain pass: {drawn} section draw(s) - the solid and cutout layers of one pass - \
         {culled} culled by the frustum, {empty} with neither layer"
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

/// Hands over the game's block atlas. See [`GAME_BLOCK_ATLAS`] and [`GAME_ATLAS_BOUND`]. Called from
/// the JVM.
pub fn set_game_block_atlas(view: wgpu::TextureView) {
    *GAME_BLOCK_ATLAS.write() = Some(Arc::new(view));
    GAME_ATLAS_BOUND.store(true, Ordering::Relaxed);
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
}

#[derive(Debug)]
pub struct RenderGraph {
    pub config: ShaderPackConfig,
    pub pipelines: LinkedHashMap<String, BoundPipeline>,
    pub resources: HashMap<String, ResourceBacking>,
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

impl RenderGraph {
    fn create_pipelines(
        &mut self,
        wm: &WmRenderer,
        custom_bind_groups: Option<HashMap<String, &wgpu::BindGroupLayout>>,
        geometry_vertex_layouts: Option<HashMap<String, Vec<wgpu::VertexBufferLayout>>>,
    ) {
        self.pipelines.clear();

        let arena = WmArena::new(1024);

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
                .map(|(index, name)| match &name[..] {
                    "@pc_mat4_model" => 64,
                    "@pc_section_position" => 16,
                    "@pc_total_sections" => 4,
                    "@pc_parts_per_entity" => 4,
                    "@pc_electrum_color" => 16,
                    "@pc_environment_data" => 68,
                    _ => unimplemented!("immediate {index} ({name}) has no size"),
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
            let shader_resource = ResourcePath(format!("wgpu_mc:shaders/{}.wgsl", pipeline_name));
            let Some(shader) = WgslShader::init(
                &shader_resource,
                &*wm.mc.resource_provider,
                &wm.gpu.device,
                "frag".into(),
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
                "@geo_terrain" => vec![],
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
                                depth_write_enabled: Some(true),
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

            self.pipelines.insert(
                pipeline_name.clone(),
                BoundPipeline {
                    pipeline: render_pipeline,
                    bind_groups: wm_bind_groups,
                    config: pipeline_config.clone(),
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
        // **It is deliberately the same sampler as this side's own atlas**: nearest within a mip level,
        // a blend between the two it lands between, both ways, no anisotropy, level 0 to the top of the
        // chain. Which is *not* what the game samples its own terrain with - `LevelRenderer` builds
        // `CLAMP_TO_EDGE, LINEAR, LINEAR` plus the video settings' anisotropy - and that difference is
        // the whole point of writing it down here.
        //
        // Two atlases are in play in one frame: a face whose sprite the game animates is baked with the
        // game's coordinates and samples this one, and every face beside it - grass, stone, the leaves
        // above the fire - samples this side's copy through `TextureManager`'s `NEAREST`. A bilinear
        // sampler on one of the two is not a subtle difference at the magnification a block texture is
        // seen at: a sixteen-texel texture filling two hundred pixels is either sixteen squares or a
        // smear, and a player who had just been handed this path said exactly that - "these are all
        // blurry, there is none of the game's crisp pixels left" - about the fire, the lava and every
        // other sprite the game animates, while the blocks around them were clean.
        //
        // The filter that decides that is `mag_filter`, and it is the one thing here that is not the
        // game's own choice: the renderer's two atlases are the same textures at the same size, so the
        // picture has to be filtered the same way whichever of them a face was baked for.
        //
        // Always registered, even before the JVM has handed the atlas over: a named resource that is
        // missing is a pipeline the graph *skips* (`create_pipelines`), and losing the whole terrain
        // pass because an animated texture is not ready yet is not a trade worth making. Until then
        // the binding is a one-texel white texture, which nothing samples - a face is only baked with
        // the game's coordinates once it has been handed over (`GAME_ATLAS_BOUND`), and the graph is
        // built again - with the real atlas in this slot - before any such face can be drawn.
        graph.resources.insert(
            "@sampler_mc_block_atlas".into(),
            ResourceBacking::Sampler(Arc::new(wm.gpu.device.create_sampler(
                &wgpu::SamplerDescriptor {
                    label: Some("wgpu-mc: the game's block atlas"),
                    // `ClampToEdge` rather than `Repeat`, and everything else - the two filters, the
                    // mipmap filter, the level range, the anisotropy - left at its default, which is
                    // what `TextureManager::new` builds for this side's own atlas. One texture, one
                    // filter, whichever atlas a face was baked for.
                    address_mode_u: wgpu::AddressMode::ClampToEdge,
                    address_mode_v: wgpu::AddressMode::ClampToEdge,
                    address_mode_w: wgpu::AddressMode::ClampToEdge,
                    mag_filter: wgpu::FilterMode::Nearest,
                    min_filter: wgpu::FilterMode::Nearest,
                    mipmap_filter: wgpu::MipmapFilterMode::Linear,
                    ..Default::default()
                },
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
    ) {
        let arena = WmArena::new(4096);

        let mut should_clear_depth = true;

        for (pipeline_name, bound_pipeline) in &self.pipelines {
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

                    let will_clear_depth = should_clear_depth && overridden.is_none();
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
                "@geo_terrain" => {
                    render_pass.set_pipeline(&bound_pipeline.pipeline);

                    // The arena's buffer, loaded once for this pass and held for all of it: it is
                    // replaced when the arena is resized (a render distance report on joining a
                    // world), and a pass that read the two halves of it at different moments could
                    // bind the new buffer and index the old.
                    let chunk_buffer = scene.chunk_buffer.load_full();

                    for (index, bind_group) in bound_pipeline.bind_groups.iter() {
                        match bind_group {
                            WmBindGroup::Resource(name) => match &name[..] {
                                "@bg_ssbo_chunks" => {
                                    render_pass.set_bind_group(
                                        *index,
                                        &chunk_buffer.bind_group,
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

                    render_pass
                        .set_index_buffer(chunk_buffer.buffer.slice(..), wgpu::IndexFormat::Uint32);

                    let sections = scene.section_storage.write();

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

                    // The layers this pass draws, in the order the pass it stands in for draws them.
                    //
                    // That pass is the game's OPAQUE group, and it is *one* render pass with two
                    // pipelines inside it - `ChunkSectionsToRender#renderGroup` walks the group's
                    // layers, calling `setPipeline` for each, and opens nothing in between. A group
                    // whose first pipeline is the solid layer is therefore taken over whole: drawing
                    // only the solid layer here dropped every cutout face in the world from the frame,
                    // and there is no other pass for them - every leaf, plant and grass overlay simply
                    // disappeared, because Minecraft's own cutout draws would have happened further
                    // down the pass this one replaced.
                    //
                    // The two need no different pipeline state - neither blends, both write depth, and
                    // the shader discards the texels a cutout texture leaves empty - so what separates
                    // them here is only which range of the arena is drawn.
                    for layer_index in [RenderLayer::Solid as usize, RenderLayer::Cutout as usize] {
                        // The alpha test the layer's own pipeline asks for: Minecraft's cutout terrain
                        // pipeline defines `ALPHA_CUTOUT` as 0.5, and its solid one defines nothing -
                        // which is a cutoff of zero here, a test no alpha fails, and the reason a
                        // solid texture is never erased by its own alpha.
                        let alpha_cutout: f32 = if layer_index == RenderLayer::Cutout as usize {
                            0.5
                        } else {
                            0.0
                        };

                        for (pos, section) in sections.iter() {
                            // The section's position *relative to the camera's section*: the view matrix
                            // carries the camera's offset within its own section and nothing else, so a
                            // draw is placed by naming where it is with the big part taken out of it.
                            // See `camera_section` above for the whole of it.
                            let rel_pos = *pos - camera_section;

                            // The box the section occupies *where the shader draws it*: the same
                            // camera-section-relative position the immediate carries, times sixteen to
                            // the block units the frustum is measured in. A box built from the absolute
                            // name instead would be thousands of blocks from the geometry it stands for,
                            // and the sections around the camera would be culled out of their own frame.
                            let a: Vec3<f32> = [
                                rel_pos.x as f32 * 16.0,
                                rel_pos.y as f32 * 16.0,
                                rel_pos.z as f32 * 16.0,
                            ]
                            .into();
                            let b: Vec3<f32> = a + Vec3::new(16.0, 16.0, 16.0);

                            let bounds: AABB<f32> = AABB::new(a.into_array(), b.into_array());

                            if !bounds.coherent_test_against_frustum(frustum, 0).0 {
                                TERRAIN_CULLED.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }

                            let Some(layer) = &section.layers[layer_index] else {
                                TERRAIN_EMPTY.fetch_add(1, Ordering::Relaxed);
                                continue;
                            };

                            let mut pc: HashMap<String, (Vec<u8>, ShaderStages)> = HashMap::new();
                            // Sixteen bytes in the layout the shader's `SectionPosition` spells out:
                            // the three integers, then the layer's alpha cutoff.
                            let mut constants = [0u8; 16];
                            constants[..12]
                                .copy_from_slice(bytemuck::cast_slice(&rel_pos.to_array()));
                            constants[12..].copy_from_slice(&alpha_cutout.to_ne_bytes());

                            pc.insert(
                                "@pc_section_position".to_string(),
                                (
                                    constants.to_vec(),
                                    ShaderStages::VERTEX | ShaderStages::FRAGMENT,
                                ),
                            );
                            set_push_constants(pipeline_config, &mut render_pass, Some(pc));
                            render_pass.draw_indexed(
                                layer.index_range.clone(),
                                0,
                                layer.vertex_range.start..layer.vertex_range.start + 1,
                            );

                            TERRAIN_DRAWN.fetch_add(1, Ordering::Relaxed);
                            TERRAIN_DRAWN_TOTAL.fetch_add(1, Ordering::Relaxed);
                        }
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

#[cfg(test)]
mod binding_visibility_tests {
    use super::*;
    use crate::wgpu::naga;
    use naga::valid::{Capabilities, ValidationFlags, Validator};

    /// The shipped terrain shader, which is the one that samples a texture in its **vertex** stage.
    ///
    /// The same file the mod ships and `graph.yaml` names, read from this crate's own source tree: two
    /// levels up from `rust/wgpu-mc` is the repository root. A path that moves has to move this with it,
    /// which is what a compile error here means.
    const TERRAIN: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../neoforge/src/main/resources/assets/wgpu_mc/shaders/terrain.wgsl"
    ));

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
    #[test]
    fn a_binding_is_visible_to_the_stage_that_samples_it() {
        let used = sampled(TERRAIN);

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
