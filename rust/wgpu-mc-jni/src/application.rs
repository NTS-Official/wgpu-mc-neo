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
                "@fog_environment"
            ],
            "the shader's own binding numbers are the keys of this map: the two atlases with their \
             samplers, the game's lightmap - which is the whole of the terrain's lighting - and the fog \
             block the game's fog is written into"
        );

        assert!(
            matches!(terrain.bind_groups.get(&1), Some(BindGroupDef::Resource(name)) if name == "@bg_ssbo_chunks"),
            "group 1 is the arena the sections were baked into"
        );
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
    #[test]
    fn the_shipped_terrain_shader_compiles() {
        use wgpu_mc::wgpu::naga;

        let source = include_str!(
            "../../../neoforge/src/main/resources/assets/wgpu_mc/shaders/terrain.wgsl"
        );

        let module = match naga::front::wgsl::parse_str(source) {
            Ok(module) => module,
            Err(error) => panic!(
                "naga will not parse the terrain shader: {}",
                error.emit_to_string(source)
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
