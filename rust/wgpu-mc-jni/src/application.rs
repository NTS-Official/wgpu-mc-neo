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

pub fn load_shaders(wm: &WmRenderer) {
    let shader_pack: ShaderPackConfig =
        serde_yaml::from_str(include_str!("../graph.yaml")).unwrap();

    let mut render_resources = HashMap::new();

    let mat4_projection = create_matrix_buffer(wm);
    let mat4_view = create_matrix_buffer(wm);
    let mat4_model = create_matrix_buffer(wm);

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
                "@sampler"
            ],
            "the shader's own binding numbers are the keys of this map"
        );

        assert!(
            matches!(terrain.bind_groups.get(&1), Some(BindGroupDef::Resource(name)) if name == "@bg_ssbo_chunks"),
            "group 1 is the arena the sections were baked into"
        );
    }
}
