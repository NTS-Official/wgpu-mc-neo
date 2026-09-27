use std::vec::Vec;

use bytemuck::{Pod, Zeroable};

use wgpu_mc::wgpu;
use wgpu_mc::wgpu::vertex_attr_array;

#[derive(Debug, Pod, Zeroable, Copy, Clone)]
#[repr(C)]
pub struct ElectrumVertex {
    pub pos: [f32; 4],
    pub uv: [f32; 2],
    pub color: [f32; 4],
    pub use_uv: u32,
}

impl ElectrumVertex {
    pub const VAO: [wgpu::VertexAttribute; 4] = vertex_attr_array![
        0 => Float32x4,
        1 => Float32x2,
        2 => Float32x4,
        3 => Uint32
    ];
}

// The vertex mappers are the ABI's own vertex layouts - six floats of position and colour, five of
// position and UV, and so on - and the calls that used them went with the paths that fed them. They
// are kept rather than deleted because they are the written-down form of what `ElectrumVertex` means
// for each layout, and a new GL call site needs one of them.
#[allow(dead_code)]
impl ElectrumVertex {
    pub fn map_pos_col_float3(verts: &[[f32; 6]]) -> Vec<ElectrumVertex> {
        verts
            .iter()
            .map(|vert| {
                let mut vertex = ElectrumVertex::zeroed();

                vertex.pos[0..3].copy_from_slice(&vert[0..3]);
                vertex.pos[3] = 1.0;
                vertex.color[0..3].copy_from_slice(&vert[3..6]);
                vertex.color[3] = 1.0;

                vertex
            })
            .collect()
    }

    pub fn map_pos_uv(verts: &[[f32; 5]]) -> Vec<ElectrumVertex> {
        verts
            .iter()
            .map(|vert| {
                let mut vertex = ElectrumVertex::zeroed();

                vertex.pos[0..3].copy_from_slice(&vert[0..3]);
                vertex.pos[3] = 1.0;
                vertex.uv.copy_from_slice(&vert[3..5]);
                vertex.color = [1.0; 4];
                vertex.use_uv = 1;

                vertex
            })
            .collect()
    }

    pub fn map_pos_uv_color(verts: &[[f32; 6]]) -> Vec<ElectrumVertex> {
        verts
            .iter()
            .map(|vert| {
                let mut vertex = ElectrumVertex::zeroed();

                vertex.pos[0..3].copy_from_slice(&vert[0..3]);
                vertex.pos[3] = 1.0;
                vertex.uv.copy_from_slice(&vert[3..5]);

                let color: u32 = bytemuck::cast(vert[5]);
                let r = (color & 0xff) as f32 / 255.0;
                let g = ((color >> 8) & 0xff) as f32 / 255.0;
                let b = ((color >> 16) & 0xff) as f32 / 255.0;
                let a = ((color >> 24) & 0xff) as f32 / 255.0;

                vertex.color = [r, g, b, a];
                vertex.use_uv = 1;

                vertex
            })
            .collect()
    }

    pub fn map_pos_color_uint(verts: &[[f32; 4]]) -> Vec<ElectrumVertex> {
        verts
            .iter()
            .map(|vert| {
                let mut vertex = ElectrumVertex::zeroed();

                vertex.pos[0..3].copy_from_slice(&vert[0..3]);
                vertex.pos[3] = 1.0;

                let color: u32 = bytemuck::cast(vert[3]);
                let r = (color & 0xff) as f32 / 255.0;
                let g = ((color >> 8) & 0xff) as f32 / 255.0;
                let b = ((color >> 16) & 0xff) as f32 / 255.0;
                let a = ((color >> 24) & 0xff) as f32 / 255.0;

                vertex.color = [r, g, b, a];
                vertex.use_uv = 0;

                vertex
            })
            .collect()
    }

    pub fn map_pos_color_uv_light(verts: &[[u8; 28]]) -> Vec<ElectrumVertex> {
        verts
            .iter()
            .map(|vert| {
                let mut vertex = ElectrumVertex::zeroed();

                //Because of alignment issues we can't use bytemuck here
                vertex.pos[0] = f32::from_ne_bytes(vert[0..4].try_into().unwrap());
                vertex.pos[1] = f32::from_ne_bytes(vert[4..8].try_into().unwrap());
                vertex.pos[2] = f32::from_ne_bytes(vert[8..12].try_into().unwrap());
                vertex.pos[3] = 1.0;

                let color: u32 = u32::from_ne_bytes(vert[12..16].try_into().unwrap());
                let r = (color & 0xff) as f32 / 255.0;
                let g = ((color >> 8) & 0xff) as f32 / 255.0;
                let b = ((color >> 16) & 0xff) as f32 / 255.0;
                let a = ((color >> 24) & 0xff) as f32 / 255.0;

                vertex.color = [r, g, b, a];
                vertex.use_uv = 1;

                vertex.uv[0] = f32::from_ne_bytes(vert[16..20].try_into().unwrap());
                vertex.uv[1] = f32::from_ne_bytes(vert[20..24].try_into().unwrap());

                vertex
            })
            .collect()
    }
}
