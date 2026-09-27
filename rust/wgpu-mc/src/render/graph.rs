use glam::ivec3;
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
    Sampler(Arc<wgpu::Sampler>),
}

impl ResourceBacking {
    pub fn get_bind_group_layout_entry(&self, binding: u32) -> wgpu::BindGroupLayoutEntry {
        match self {
            ResourceBacking::Buffer(_, buffer_ty) => wgpu::BindGroupLayoutEntry {
                binding,
                //TODO
                visibility: ShaderStages::all(),
                ty: wgpu::BindingType::Buffer {
                    ty: *buffer_ty,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            ResourceBacking::BufferArray(_buffers) => wgpu::BindGroupLayoutEntry {
                binding,
                visibility: ShaderStages::all(),
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            ResourceBacking::Texture2D(_) => wgpu::BindGroupLayoutEntry {
                binding,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    // Filterable, because the shaders this graph builds sample with `textureSample`
                    // and the atlas is `Rgba8Unorm`: a layout that says otherwise is not a pipeline
                    // that draws differently, it is one wgpu refuses to create at all.
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            ResourceBacking::Sampler(_) => wgpu::BindGroupLayoutEntry {
                binding,
                visibility: wgpu::ShaderStages::FRAGMENT,
                // Filtering for the same reason: `textureSample` needs one, and the default sampler
                // this graph binds is a filtering sampler with nearest filtering - non-filtering is
                // a different binding type, not a different filter mode.
                ty: wgpu::BindingType::Sampler(SamplerBindingType::Filtering),
                count: None,
            },
        }
    }

    pub fn get_bind_group_entries(&self, index: u32) -> Vec<wgpu::BindGroupEntry> {
        match self {
            ResourceBacking::Buffer(buffer, _buffer_ty) => vec![wgpu::BindGroupEntry {
                binding: index,
                resource: wgpu::BindingResource::Buffer(buffer.as_entire_buffer_binding()),
            }],
            ResourceBacking::Texture2D(texture) => vec![wgpu::BindGroupEntry {
                binding: index,
                resource: wgpu::BindingResource::TextureView(&texture.view),
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

            let immediate_size: u32 = pipeline_config
                .immediates
                .iter()
                .map(|(index, name)| match &name[..] {
                    "@pc_mat4_model" => 64,
                    "@pc_section_position" => 12,
                    "@pc_total_sections" => 4,
                    "@pc_parts_per_entity" => 4,
                    "@pc_electrum_color" => 16,
                    "@pc_environment_data" => 68,
                    _ => unimplemented!(),
                })
                .sum();

            // A pipeline that passes immediates needs the device feature for them, and wgpu answers a
            // layout without it with a validation error rather than a `None` - which, on this path, is
            // the process ending. There is no way to draw such a pipeline differently, so it is
            // skipped with the reason.
            if immediate_size > 0 && !wm.gpu.device.features().contains(wgpu::Features::IMMEDIATES) {
                log::error!(
                    "wgpu-mc: the render graph's '{pipeline_name}' pipeline passes {immediate_size} \
                     byte(s) of immediates per draw, and this device was created without the \
                     `immediates` feature; skipping it"
                );
                continue;
            }

            let layout = wm
                .gpu
                .device
                .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: None,
                    bind_group_layouts: &bind_group_layouts,
                    immediate_size,
                });

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

            let render_pipeline =
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
                                    wgpu::CompareFunction::Less
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
                    });

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
                            let Some(bytes) =
                                wm.mc.resource_provider.get_bytes(&ResourcePath::from(&src[..]))
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

                    for (index, bind_group) in bound_pipeline.bind_groups.iter() {
                        match bind_group {
                            WmBindGroup::Resource(name) => match &name[..] {
                                "@bg_ssbo_chunks" => {
                                    render_pass.set_bind_group(
                                        *index,
                                        &scene.chunk_buffer.bind_group,
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

                    render_pass.set_index_buffer(
                        scene.chunk_buffer.buffer.slice(..),
                        wgpu::IndexFormat::Uint32,
                    );

                    let sections = scene.section_storage.write();
                    let translation = Vec3::new(
                        model_translation[0],
                        model_translation[1],
                        model_translation[2],
                    );

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
                        for (pos, section) in sections.iter() {
                            // The section's *own* position: the view matrix carries the camera's
                            // translation, so a draw is placed by naming where it is, not by naming
                            // where it is relative to something the draw was told about separately.
                            // One transform, one truth - a second camera is a second thing that can be
                            // out of step with the first.
                            let rel_pos = *pos;

                            // The box the section occupies *where the shader draws it*: the section's
                            // name is absolute in y and section-relative in x and z, and the model
                            // matrix then translates all three - so a box built from the name alone
                            // would be compared against a frustum that is measured from the camera,
                            // and every section below the player would test as if it were above them.
                            // That is the ground culled out from under the camera, and the sky left in
                            // its place.
                            let a: Vec3<f32> = [
                                rel_pos.x as f32 * 16.0,
                                rel_pos.y as f32 * 16.0,
                                rel_pos.z as f32 * 16.0,
                            ]
                            .into();
                            let a = a + translation;
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
                            pc.insert(
                                "@pc_section_position".to_string(),
                                (
                                    bytemuck::cast_slice(&rel_pos.to_array()).to_vec(),
                                    ShaderStages::VERTEX,
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
            Some((data, stages)) => render_pass.set_immediates(*offset as u32, data),
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
mod culling_tests {
    use super::*;
    use glam::Mat4;

    /// A section in front of the camera is tested where it is drawn, not where its name points.
    ///
    /// A section's name is absolute in y and section-relative in x and z, and the model matrix then
    /// translates all three; the frustum, on the other hand, is measured from the camera. A box built
    /// from the name alone is therefore `camera.y` blocks away from the geometry it stands for - the
    /// ground under the player becomes a box in the sky, outside the frustum, and is culled. That is
    /// terrain missing exactly where the player is looking, with the water and the sky behind it left
    /// in its place, which reads as "the terrain is not drawn at all".
    #[test]
    fn a_section_below_the_camera_is_tested_where_it_is_drawn() {
        let camera = glam::Vec3::new(0.0, 100.0, 0.0);
        let _ = camera;
        let projection = Mat4::perspective_rh(70f32.to_radians(), 16.0 / 9.0, 0.05, 256.0);
        let frustum =
            Frustum::from_modelview_projection(with_gl_depth_range(projection.to_cols_array_2d()));

        // The camera is at y 100, so the pass's model matrix translates by -100 in y.
        let translation = Vec3::new(0.0, -100.0, 0.0);
        let size = Vec3::new(16.0, 16.0, 16.0);

        // A section at absolute y 80 - one section under the camera - forty blocks in front of it.
        let drawn_at = Vec3::new(0.0, 80.0, -40.0) + translation;
        let bounds = AABB::new(drawn_at.into_array(), (drawn_at + size).into_array());
        assert!(
            bounds.coherent_test_against_frustum(&frustum, 0).0,
            "the section in front of the camera was culled where it is drawn"
        );

        // And the same section built from its name alone, which is where that bug put it: eighty
        // blocks above the camera, out of the frustum, gone.
        let named_at = Vec3::new(0.0, 80.0, -40.0);
        let named = AABB::new(named_at.into_array(), (named_at + size).into_array());
        assert!(
            !named.coherent_test_against_frustum(&frustum, 0).0,
            "a box built from the name alone is in front of the camera, so the translation is not needed"
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
            let scale = 1.0
                + gl.x.abs().max(gl.y.abs()).max(gl.z.abs()).max(gl.w.abs());
            assert!(
                difference < 1e-3 * scale,
                "plane {index} differs by {difference}: converted {from_zero_to_one:?}, native {gl:?}"
            );
        }

        for (name, frustum) in [("converted 0..1", &converted), ("native -1..1", &already_gl)] {
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
