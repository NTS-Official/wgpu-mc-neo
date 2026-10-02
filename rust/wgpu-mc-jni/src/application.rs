use std::sync::Arc;

use once_cell::sync::Lazy;
use once_cell::sync::OnceCell;
use parking_lot::lock_api::Mutex;
use wgpu_mc::{
    WmRenderer,
    wgpu::{
        self, BufferAddress, BufferBindingType,
        util::{BufferInitDescriptor, DeviceExt},
    },
};

use crate::{RENDER_GRAPH, gl::ElectrumVertex};
use std::collections::HashMap;
use wgpu_mc::render::{
    graph::{RenderGraph, ResourceBacking},
    shaderpack::ShaderPackConfig,
};

pub static SHOULD_STOP: OnceCell<()> = OnceCell::new();

/// The three matrices the graph's terrain pipeline reads, as the buffers it binds.
///
/// They are kept here rather than inside the graph because the two have different lifetimes: the graph
/// is rebuilt on every shader reload, and the values keep arriving on the frame's schedule - the JVM
/// sends them through `set_matrix`, and `upload_terrain_matrices` is what puts them in the buffers the
/// pass reads.
pub static TERRAIN_MATRICES: Lazy<parking_lot::Mutex<Option<TerrainMatrices>>> =
    Lazy::new(|| parking_lot::Mutex::new(None));

/// The three matrix buffers, cloned out of [TERRAIN_MATRICES] for each upload.
#[derive(Clone)]
pub struct TerrainMatrices {
    pub model: Arc<wgpu::Buffer>,
    pub view: Arc<wgpu::Buffer>,
    pub projection: Arc<wgpu::Buffer>,
}

/// The fog buffer the terrain shader reads, alongside the matrices and for the same reason: the graph
/// is rebuilt on every shader reload and the values keep arriving per frame.
pub static TERRAIN_FOG: Lazy<parking_lot::Mutex<Option<Arc<wgpu::Buffer>>>> =
    Lazy::new(|| parking_lot::Mutex::new(None));

/// How many bytes the shader's `FogEnvironment` struct takes: a `vec4` colour, four floats, and a `vec3`
/// with the padding uniform layout gives it.
const FOG_BYTES: u64 = 48;

pub fn load_shaders(wm: &WmRenderer) {
    let shader_pack: ShaderPackConfig =
        serde_yaml::from_str(include_str!("../graph.yaml")).unwrap();

    let mut render_resources = HashMap::new();

    let mat4_projection = create_matrix_buffer(wm);
    let mat4_view = create_matrix_buffer(wm);
    let mat4_model = create_matrix_buffer(wm);

    // Zeroed, and written for this frame before the pass reads it: a fog colour of zero is a fog that
    // blends nothing, so the frames before the first upload are the unfogged picture rather than a black
    // one. See `renderer::FOG`.
    let fog = Arc::new(wm.gpu.device.create_buffer_init(&BufferInitDescriptor {
        label: Some("wgpu-mc: terrain fog"),
        contents: &[0u8; FOG_BYTES as usize],
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::UNIFORM,
    }));

    *TERRAIN_FOG.lock() = Some(fog.clone());

    render_resources.insert(
        "@fog_environment".into(),
        ResourceBacking::Buffer(fog, BufferBindingType::Uniform),
    );

    *TERRAIN_MATRICES.lock() = Some(TerrainMatrices {
        model: mat4_model.clone(),
        view: mat4_view.clone(),
        projection: mat4_projection.clone(),
    });

    render_resources.insert(
        "@mat4_view".into(),
        ResourceBacking::Buffer(mat4_view.clone(), BufferBindingType::Uniform),
    );

    render_resources.insert(
        "@mat4_perspective".into(),
        ResourceBacking::Buffer(mat4_projection.clone(), BufferBindingType::Uniform),
    );

    render_resources.insert(
        "@mat4_model".into(),
        ResourceBacking::Buffer(mat4_model.clone(), BufferBindingType::Uniform),
    );

    let mut custom_bind_groups = HashMap::new();
    custom_bind_groups.insert(
        "@texture_electrum_gui".into(),
        wm.bind_group_layouts.get("texture").unwrap(),
    );
    custom_bind_groups.insert(
        "@mat4_electrum_gui".into(),
        wm.bind_group_layouts.get("matrix").unwrap(),
    );

    let mut custom_geometry = HashMap::new();
    custom_geometry.insert(
        "@geo_electrum_gui".into(),
        vec![wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<ElectrumVertex>() as BufferAddress,
            step_mode: Default::default(),
            attributes: &ElectrumVertex::VAO,
        }],
    );

    let render_graph = RenderGraph::new(
        wm,
        shader_pack,
        render_resources,
        Some(custom_bind_groups),
        Some(custom_geometry),
    );

    match RENDER_GRAPH.get() {
        None => {
            RENDER_GRAPH.set(Mutex::new(render_graph)).unwrap();
        }
        Some(mutex) => {
            *mutex.lock() = render_graph;
        }
    }

    // Diagnostics: which pipelines the graph came out with. A pipeline whose resources are not
    // registered is skipped rather than unwrapped (see `create_pipelines`), so "the terrain pass is
    // not drawing" is answered here instead of at the draw.
    if crate::debug::logging() {
        let graph = RENDER_GRAPH.get().unwrap().lock();
        log::info!(
            "wgpu-mc: the render graph has {} pipeline(s): {}",
            graph.pipelines.len(),
            graph
                .pipelines
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

/// Writes the matrices the JVM last sent into the buffers the graph's terrain pipeline binds.
///
/// Three buffers, three matrices, and one of them is the one the culler reads too - see
/// `render_terrain_pass`. Nothing here checks whether they have ever been sent: a zeroed matrix draws
/// nothing, which is the honest picture of a frame whose camera nobody has described yet.
pub fn upload_terrain_matrices(wm: &WmRenderer) {
    let matrices = {
        let matrices = crate::renderer::MATRICES.lock();
        (
            matrices.terrain_transformation,
            matrices.view,
            matrices.projection,
        )
    };

    let buffers = TERRAIN_MATRICES.lock().clone();
    let Some(buffers) = buffers else {
        return;
    };

    wm.gpu
        .queue
        .write_buffer(&buffers.model, 0, bytemuck::cast_slice(&matrices.0));
    wm.gpu
        .queue
        .write_buffer(&buffers.view, 0, bytemuck::cast_slice(&matrices.1));
    wm.gpu
        .queue
        .write_buffer(&buffers.projection, 0, bytemuck::cast_slice(&matrices.2));

    // And the fog, which the JVM read out of the same frame's camera render state as those matrices.
    let fog = *crate::renderer::FOG.lock();

    if let Some(buffer) = TERRAIN_FOG.lock().as_ref() {
        wm.gpu
            .queue
            .write_buffer(buffer, 0, bytemuck::cast_slice(&fog));
    }
}

fn create_matrix_buffer(wm: &WmRenderer) -> Arc<wgpu::Buffer> {
    Arc::new(wm.gpu.device.create_buffer_init(&BufferInitDescriptor {
        label: None,
        contents: &[0; 64],
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::UNIFORM,
    }))
}

#[cfg(test)]
mod tests {
    use wgpu_mc::render::shaderpack::{BindGroupDef, ShaderPackConfig};

    /// The graph the mod ships parses, and it carries the terrain pipeline.
    ///
    /// The yaml is `include_str!`d when the graph is built, so a field this crate's schema no longer
    /// has - `push_constants:` where `immediates:` is what is read, say - is dropped in silence rather
    /// than refused, and the graph comes up with a pipeline that draws nothing. Every field the pass
    /// needs is named here, because "the terrain is missing" is otherwise a picture with no line in the
    /// log to go with it.
    #[test]
    fn the_shipped_graph_has_the_terrain_pipeline() {
        let config: ShaderPackConfig =
            serde_yaml::from_str(include_str!("../graph.yaml")).expect("graph.yaml parses");

        let terrain = config
            .pipelines
            .pipelines
            .get("terrain")
            .expect("the graph has the terrain pipeline");

        assert_eq!(terrain.geometry, "@geo_terrain");
        assert_eq!(terrain.depth.as_deref(), Some("@texture_depth"));
        assert_eq!(terrain.output, ["@framebuffer_texture"]);
        assert_eq!(
            terrain.output_format, "rgba8unorm",
            "the pass draws into Minecraft's own target, and wgpu checks the format when the pipeline \
             is set in it"
        );
        assert_eq!(
            terrain.immediates.values().next().map(String::as_str),
            Some("@pc_section_position"),
            "the section position is what tells the shader which section it is drawing"
        );

        let group0 = match terrain.bind_groups.get(&0) {
            Some(BindGroupDef::Entries(entries)) => entries.clone(),
            other => panic!("bind group 0 is the entries the shader declares, not {other:?}"),
        };

        assert_eq!(
            group0.values().cloned().collect::<Vec<_>>(),
            [
                "@mat4_model",
                "@mat4_view",
                "@mat4_perspective",
                "@texture_block_atlas",
                "@sampler",
                "@texture_mc_block_atlas",
                "@sampler_mc_block_atlas",
                "@texture_game_lightmap",
                "@sampler_game_lightmap",
                "@fog_environment",
                // **Two more samplers over the same two textures**, and they are the magnification half of
                // the pair: the shader picks between a `Nearest` magnification and an anisotropic
                // minification per fragment, because wgpu will not put both behaviours in one sampler. See
                // `atlas_magnify_sampler` in `wgpu-mc`.
                "@sampler_mc_block_atlas_magnify",
                "@sampler_block_atlas_magnify",
                // And the third pair: a minified *animated* face, which wants neither of the others - see
                // `atlas_animated_minified_sampler`. The list is exact and in order because the keys are the
                // shader's own binding slots, so a new sampler has to be added here deliberately.
                "@sampler_mc_block_atlas_animated",
                "@sampler_block_atlas_animated"
            ],
            "the shader's own binding numbers are the keys of this map: the two atlases with their \
             samplers, the same two with the magnifying samplers, the same two again with the sampler a \
             minified animated face needs, the game's lightmap - which is the whole of the terrain's \
             lighting - and the fog block the game's fog is written into"
        );

        assert!(
            matches!(terrain.bind_groups.get(&1), Some(BindGroupDef::Resource(name)) if name == "@bg_ssbo_chunks"),
            "group 1 is the arena the sections were baked into"
        );
    }

    /// **Every terrain pipeline in the shipped graph binds the draw records at group 2, and there are
    /// enough draw-buffer slots for all of them.**
    ///
    /// Two contracts, and both are silent when broken. A terrain pipeline without group 2 is a pipeline
    /// whose vertex stage reads a binding the layout does not declare, which wgpu refuses - and a
    /// refusal on this path ends the process rather than the pass. A terrain pipeline that cannot claim
    /// one of `SECTION_DRAW_SLOTS`' slots is skipped by `create_pipelines` with a line in the log, and
    /// what that costs is a whole layer of the world: the count is in this yaml and the capacity is in
    /// `wgpu_mc`, so this is the only place both are visible at once.
    ///
    /// The number of slots is also what the scene allocates its draw buffers from, so the assertion is
    /// an upper bound rather than an equality: one fewer terrain pipeline than slots is wasted video
    /// memory, and one more is a layer that does not draw.
    #[test]
    fn every_terrain_pipeline_has_a_draw_buffer_slot() {
        let config: ShaderPackConfig =
            serde_yaml::from_str(include_str!("../graph.yaml")).expect("graph.yaml parses");

        let terrain = config
            .pipelines
            .pipelines
            .iter()
            .filter(|(_, pipeline)| {
                matches!(
                    pipeline.geometry.as_str(),
                    "@geo_terrain" | "@geo_terrain_translucent"
                )
            })
            .collect::<Vec<_>>();

        assert!(
            !terrain.is_empty(),
            "the graph names no terrain pipeline at all, so this test is checking nothing"
        );

        assert!(
            terrain.len() <= wgpu_mc::mc::SECTION_DRAW_SLOTS,
            "the graph names {} terrain pipeline(s) and the scene has {} draw-buffer slot(s); the pass \
             that cannot claim one draws nothing",
            terrain.len(),
            wgpu_mc::mc::SECTION_DRAW_SLOTS
        );

        for (name, pipeline) in terrain {
            assert!(
                matches!(
                    pipeline.bind_groups.get(&2),
                    Some(BindGroupDef::Resource(resource)) if resource == "@bg_section_draws"
                ),
                "the '{name}' pipeline has to bind the draw records at group 2: the section's position \
                 travels in that buffer now, because a `multi_draw` cannot be handed an immediate per \
                 draw"
            );
        }
    }

    /// The graph the mod ships also carries the translucent terrain pipeline, and it is the *only*
    /// pipeline that blends without writing depth.
    ///
    /// This is the water. It is checked here rather than left to the run because every way of getting it
    /// wrong is silent: a pipeline whose geometry name does not match what `graph.rs` matches on is a
    /// pass that opens and draws nothing, one with `depth_write` left at its default turns every pane of
    /// glass into a curtain - the nearest one writes depth and hides the rest - and one with `replace`
    /// instead of `alpha_blending` draws water opaque, which looks almost right until the first pane of
    /// glass behind it disappears.
    #[test]
    fn the_shipped_graph_has_the_translucent_terrain_pipeline() {
        let config: ShaderPackConfig =
            serde_yaml::from_str(include_str!("../graph.yaml")).expect("graph.yaml parses");

        let opaque = config
            .pipelines
            .pipelines
            .get("terrain")
            .expect("the graph has the terrain pipeline");

        let translucent = config
            .pipelines
            .pipelines
            .get("translucent_terrain")
            .expect("the graph has the translucent terrain pipeline");

        assert_eq!(translucent.geometry, "@geo_terrain_translucent");
        assert_eq!(translucent.blending, "alpha_blending");
        assert!(
            !translucent.depth_write,
            "a translucent face is tested against the depth behind it and must not become what the \
             next one is tested against"
        );
        assert!(
            opaque.depth_write,
            "and the opaque terrain still writes the depth the translucent pass tests against"
        );

        // Everything else the two passes need is the same, because they draw the same arena out of the
        // same bindings - a field copied wrong between them is a pass that draws into the wrong texture
        // or samples the wrong atlas, and neither fails until the frame is on screen.
        assert_eq!(translucent.depth, opaque.depth);
        assert_eq!(translucent.output, opaque.output);
        assert_eq!(translucent.output_format, opaque.output_format);
        assert_eq!(
            translucent.immediates, opaque.immediates,
            "the section position is what tells the shader which section it is drawing"
        );
        assert_eq!(
            translucent.bind_groups, opaque.bind_groups,
            "the same arena, the same two atlases and the same lightmap"
        );
    }

    /// **Every pipeline in the shipped graph has a shader file behind it.**
    ///
    /// This is the check for a failure that is completely silent: the graph resolves a pipeline's shader
    /// as `wgpu_mc:shaders/<name>.wgsl`, where `<name>` is the pipeline's own name *unless it names
    /// another*, and a pipeline whose shader is not found is **skipped** - on purpose, because a shader
    /// can legitimately be missing while the atlas is still being stitched, and a panic there would end
    /// the process. What that costs when the name is simply wrong is a pass that never opens and a layer
    /// nothing draws: the second terrain pass - the one that draws water - was written with
    /// `translucent_terrain:` and no `shader:`, so it looked for a `translucent_terrain.wgsl` that does
    /// not exist and was skipped every time. The frame looked fine, the exit code was zero, and the only
    /// sign was a transparent layer that was not on screen.
    ///
    /// The files are checked by name against this crate's own list, because the shaders live in the
    /// mod's resources rather than beside the Rust: `SHADERS` below is every `.wgsl` there is.
    #[test]
    fn every_pipeline_in_the_shipped_graph_has_its_shader() {
        let config: ShaderPackConfig =
            serde_yaml::from_str(include_str!("../graph.yaml")).expect("graph.yaml parses");

        /// Every shader the mod ships, by the name a pipeline would resolve.
        ///
        /// Transcribed from the mod's `src/main/resources/assets/wgpu_mc/shaders/` rather than globbed:
        /// a build script that read that directory would be the only way to glob it, and this list is
        /// the thing that has to be *kept* in step with it - a shader added there without being added
        /// here fails this test, which is the right direction for the warning to point.
        const SHADERS: &[&str] = &[
            "clear",
            "debug_lines",
            "electrum_gui",
            "entity",
            "grass",
            "sky_fog",
            "sky_scatter",
            "stars",
            "sun_moon_cycle",
            "terrain",
            "terrain_solid",
            "transparent",
        ];

        for (name, pipeline) in &config.pipelines.pipelines {
            let shader = pipeline.shader.as_deref().unwrap_or(name);

            assert!(
                SHADERS.contains(&shader),
                "the '{name}' pipeline draws with '{shader}.wgsl', which the mod does not ship; the \
                 graph would skip the pipeline in silence and the layer it draws would never appear. \
                 Shipping shaders: {SHADERS:?}"
            );
        }
    }

    /// The shader the shipped graph draws the terrain with compiles.
    ///
    /// The graph is built at startup and the shader is read out of the mod's own resources - this is
    /// the file `wgpu_mc:shaders/terrain.wgsl` resolves to - and a WGSL mistake in it is not a shader
    /// that draws something wrong: the module fails to validate while the pipeline is being created,
    /// wgpu answers with a validation error, this crate's error handler panics on it, and the process
    /// ends. Nothing between writing the file and running the game says so otherwise, and the file is
    /// edited by hand.
    ///
    /// Validation is naga's, which is the same front end wgpu compiles WGSL with, so what passes here
    /// is what the pipeline creation will accept. `IMMEDIATES` is part of `Capabilities::all()`, which
    /// is what the pass needs for its `var<immediate>` section position.
    ///
    /// The file is read out of the Neolectrum checkout, so this skips on a machine that has the engine
    /// without the mod; see `wgpu_mc_modtree`.
    #[test]
    fn the_shipped_terrain_shader_compiles() {
        use wgpu_mc::wgpu::naga;

        let Some(source) = wgpu_mc_modtree::shader("terrain") else {
            wgpu_mc_modtree::skip("the terrain shader check");
            return;
        };

        let module = match naga::front::wgsl::parse_str(&source) {
            Ok(module) => module,
            Err(error) => panic!(
                "naga will not parse the terrain shader: {}",
                error.emit_to_string(&source)
            ),
        };

        let mut validator = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        );

        if let Err(error) = validator.validate(&module) {
            panic!("the terrain shader does not validate: {error:?}");
        }

        // The two textures the fragment stage chooses between and their samplers, by the name each is
        // declared under: a shader that lost one of them would still compile, still validate, and
        // silently sample whatever the graph binds in the slot it kept.
        let declared = module
            .global_variables
            .iter()
            .filter_map(|(_, global)| global.name.clone())
            .collect::<Vec<String>>();

        for wanted in ["t_texture", "t_game_atlas", "t_sampler", "t_game_sampler"] {
            assert!(
                declared.iter().any(|name| name == wanted),
                "the terrain shader no longer declares {wanted}; it declares {declared:?}"
            );
        }
    }
}
