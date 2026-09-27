use wgpu::{BindGroupLayout, SamplerBindingType};

use std::collections::HashMap;

pub const BLOCK_ATLAS: &str = "wgpu_mc:atlases/block";
pub const ENTITY_ATLAS: &str = "wgpu_mc:atlases/entity";

#[derive(Copy, Clone, Debug)]
pub struct Vertex {
    pub position: [f32; 3],
    pub uv: [u16; 2],
    pub normal: [f32; 3],
    pub color: u32,
    /// The ten bits the vertex format reserves for an animated texture, per vertex. See
    /// [`UV_GAME_ATLAS`] for the one of them that carries anything.
    pub uv_flags: u32,
    pub lightmap_coords: u8,
    /// How many of the four blocks around this vertex's corner **fill their whole block**, which is what
    /// darkens it.
    ///
    /// Minecraft's `getShadeBrightness` is `0.2` for such a block and `1.0` for everything else - a
    /// torch, a plant, a slab, glass, a fluid - and a corner's brightness is the average of four of those
    /// samples, so it is `1 - 0.2 * count`: one of five steps from `1.0` down to `0.2`. Which four, and
    /// why the count rather than the brightness is what travels, is in `chunk.rs`'s `shades_corners`; the
    /// curve is the fragment shader's.
    pub ao: u8,
}

/// The bit in [`Vertex::uv_flags`] that says a vertex's UVs are in **Minecraft's** block atlas rather
/// than in the one this renderer builds.
///
/// Minecraft animates a texture by rendering the due frame into its own atlas
/// (`TextureAtlas#cycleAnimationFrames`, one `Animate <atlas>` pass per level), so the copy this side
/// packed at startup is a single frame that never changes: the fire burns still, and so does every
/// other animated sprite. A face whose sprite the game animates is therefore baked with the *game's*
/// own coordinates - `Atlas::sprite_rects`, which [`WgpuNative.registerSprite`] fills from the game's
/// stitcher - and marked with this bit, and the terrain shader sends it to the game's atlas instead.
///
/// It animates for no cost at all: nothing is copied, nothing is re-uploaded per frame, and the
/// frames are the ones the game is already drawing.
///
/// [`WgpuNative.registerSprite`]: ../../../../wgpu_mc_jni/index.html
pub const UV_GAME_ATLAS: u32 = 1 << 0;

/// What a game-atlas UV is multiplied by on the way into the sixteen bits the vertex has for it.
///
/// The coordinates are the game's own - `0..1` over the game's atlas - so this is what fills the
/// sixteen bits exactly, and a different scale from the one this side's own atlas goes through
/// (`1/2048`, one step a texel of the 2048-wide atlas `ATLAS_DIMENSIONS` builds). Two atlases of two
/// different sizes, and the shader picks the scale by the same flag [`UV_GAME_ATLAS`] that the bake
/// did. Sixty-four steps a texel on a 1024-wide atlas, which is the precision a sprite's own texel
/// boundaries need.
pub const UV_GAME_SCALE: f32 = 65535.0;

impl Vertex {
    pub const VERTEX_LENGTH: usize = 16;

    /// One axis of a baked position, in the 1/16-block units this vertex format holds.
    ///
    /// Eight bits to an axis, plus one for "it is exactly 256", is a grid of sixteenths of a block and
    /// nothing between the lines - and a model is free to put a face between them. That face used to be
    /// **truncated**, which lands it on the line *below* it, and for anything smaller than a block that
    /// line is usually the block boundary. The block boundary is where the neighbouring block's own
    /// face is, so the two are exactly coplanar, and which of them is seen is then decided per pixel by
    /// the last bits of two projected z values that are only equal to within rounding: the ground
    /// showing through a leaf litter in patches that change as the camera turns, and not at all as it
    /// moves, because moving both surfaces together leaves their difference where it was.
    ///
    /// Rounding to the **nearest** line keeps such a face off the boundary in both directions, which
    /// rounding up does not: the fire's side quads sit at `0.01/16` of a block - a hundredth of a
    /// texel, there for exactly this reason - and the variant for the opposite wall is that same quad
    /// turned by `y: 180`, so it lands at `15.99/16`. Rounding up moved the first to 1/16, which is
    /// harmless, and the second to 16/16, which is the wall's own face, and the fire flickered against
    /// the wall it was burning on. A face strictly inside the block is therefore never put on 0 or 256;
    /// everything else goes to the nearest line, so geometry on the grid is not moved at all. Geometry
    /// thinner than half a line still collapses onto a line, and this format cannot tell those apart:
    /// the honest fix for that is more bits rather than a cleverer rounding. `0.01/16` needs about
    /// 1/1024 of a block to survive, which is four more bits an axis than this vertex has. See
    /// [`Vertex::VERTEX_LENGTH`] and the shader's decode in `shaders/terrain.wgsl`.
    #[inline]
    fn axis_to_sixteenths(v: f32) -> u16 {
        // Clamped rather than wrapped: a byte and one flag bit cannot name a coordinate past 256, and
        // a model that reaches outside its block is one this format was never able to hold.
        let sixteenths = (v * 16.0).clamp(0.0, 256.0);
        let nearest = sixteenths.round();

        // A face that is strictly *between* two block boundaries is never put on one of them, whichever
        // side it came from. Those lines are where the neighbouring block's own faces are, and geometry
        // that lands on one is coplanar with them: the fire's side quads are at 0.01/16 of a block, and
        // the variant for the opposite wall is the same quad turned by `y: 180`, so it arrives at
        // 15.99/16 - one step below the wall it is burning against. On-grid geometry is not moved at
        // all, because rounding it lands on itself.
        let on_a_block_boundary = nearest % 16.0 == 0.0;
        let moved = if on_a_block_boundary && nearest != sixteenths {
            if sixteenths < nearest {
                nearest - 1.0
            } else {
                nearest + 1.0
            }
        } else {
            nearest
        };

        // The nudged value cannot leave the range: both ends of it are block boundaries, and a value
        // that is exactly one of them is never moved.
        moved as u16
    }

    pub fn compressed(self) -> [u8; Self::VERTEX_LENGTH] {
        // XYZ: 4 bytes (1 for each axis)
        // Normal: 3 bits
        // Color: 3 bytes
        // UV: 4 bytes
        // Animated UV index: 10 bits ([`UV_GAME_ATLAS`] is the one that is used)
        // XYZ add one flag: 3 bits
        // Block light nibble: 1 byte (4 bits for block, 4 bits for sky)

        // Total: 101 bits (13 bytes)
        let mut array = [0; Self::VERTEX_LENGTH];

        let x = Self::axis_to_sixteenths(self.position[0]);
        let y = Self::axis_to_sixteenths(self.position[1]);
        let z = Self::axis_to_sixteenths(self.position[2]);

        let x_byte = x as u8;
        let y_byte = y as u8;
        let z_byte = z as u8;

        let flag_byte = ((x == 256) as u8) | (((y == 256) as u8) << 1) | (((z == 256) as u8) << 2);

        //position
        array[0] = x_byte;
        array[1] = y_byte;
        array[2] = z_byte;

        //color
        array[3] = (self.color & 0xff) as u8;
        array[4] = ((self.color >> 8) & 0xff) as u8;
        array[5] = ((self.color >> 16) & 0xff) as u8;

        //U
        array[6] = self.uv[0].to_le_bytes()[0];
        array[7] = self.uv[0].to_le_bytes()[1];
        //V
        array[8] = self.uv[1].to_le_bytes()[0];
        array[9] = self.uv[1].to_le_bytes()[1];

        let normal_bits: u8 = match self.normal {
            [-1.0, 0.0, 0.0] => 0b00000100,
            [1.0, 0.0, 0.0] => 0b00000000,
            [0.0, 1.0, 0.0] => 0b00000001,
            [0.0, -1.0, 0.0] => 0b00000101,
            [0.0, 0.0, 1.0] => 0b00000010,
            [0.0, 0.0, -1.0] => 0b00000110,
            _ => unreachable!("Invalid vertex normal"),
        };

        //UV index and normal
        array[10] = self.uv_flags as u8;
        array[11] = (((self.uv_flags >> 8) as u8) & 0b11) | (normal_bits << 2) | (flag_byte << 5);
        array[12] = self.lightmap_coords;
        array[13] = self.ao;

        array
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct QuadVertex {
    pub position: [f32; 2],
}

impl QuadVertex {
    const VAA: [wgpu::VertexAttribute; 1] = wgpu::vertex_attr_array![
        0 => Float32x2,
    ];

    #[must_use]
    pub fn desc<'a>() -> wgpu::VertexBufferLayout<'a> {
        use std::mem;
        wgpu::VertexBufferLayout {
            array_stride: mem::size_of::<QuadVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::VAA,
        }
    }
}

pub fn create_bind_group_layouts(device: &wgpu::Device) -> HashMap<String, BindGroupLayout> {
    [
        (
            "camera".into(),
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("Camera Bind Group Layout"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            }),
        ),
        (
            "texture_and_sampler".into(),
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: None,
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            }),
        ),
        (
            "texture_depth".into(),
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("Depth Texture Descriptor"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Depth,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                }],
            }),
        ),
        (
            "texture".into(),
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("Texture Bind Group Layout Descriptor"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT | wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                }],
            }),
        ),
        (
            "texture_sampler".into(),
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("Texture Sampler Bind Group Layout Descriptor"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                        count: None,
                    },
                ],
            }),
        ),
        (
            "cubemap".into(),
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("Cubemap Bind Group Layout Descriptor"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::Cube,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            }),
        ),
        (
            "ssbo".into(),
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: None,
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX
                        | wgpu::ShaderStages::FRAGMENT
                        | wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            }),
        ),
        (
            "ssbo_mut".into(),
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: None,
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: false },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            }),
        ),
        (
            "matrix".into(),
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("Matrix Bind Group Layout"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            }),
        ),
        (
            "entity".into(),
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("Entity Bind Group Layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::VERTEX,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Storage { read_only: true },
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: false },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                ],
            }),
        ),
    ]
    .into_iter()
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shader's own decode of one vertex, transliterated from `shaders/terrain.wgsl`.
    ///
    /// This is the half of the terrain path that is not checked by either compiler: the baker writes
    /// sixteen bytes and the shader reads four words out of them, and the two only agree by hand. A
    /// y that lands in another byte, or a "this coordinate is 16" flag on the wrong axis, is terrain
    /// that is drawn somewhere it is not - mirrored, stretched, or off the ground entirely - with
    /// nothing in the log to say which.
    fn shader_decode(bytes: &[u8; 16]) -> [f32; 3] {
        let word = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());

        let v1 = word(0);
        let v3 = word(8);

        let mut x = (v1 & 0xff) as f32 * 0.0625;
        let mut y = ((v1 >> 8) & 0xff) as f32 * 0.0625;
        let mut z = ((v1 >> 16) & 0xff) as f32 * 0.0625;

        // The "one past the section edge" flags, which is how a coordinate of exactly 16 is stored.
        if (v3 >> 29) & 1 == 1 {
            x = 16.0;
        }
        if (v3 >> 30) & 1 == 1 {
            y = 16.0;
        }
        if (v3 >> 31) == 1 {
            z = 16.0;
        }

        [x, y, z]
    }

    fn vertex_at(position: [f32; 3]) -> Vertex {
        Vertex {
            position,
            uv: [0, 0],
            normal: [0.0, 1.0, 0.0],
            color: 0,
            uv_flags: 0,
            lightmap_coords: 0,
            ao: 0,
        }
    }

    /// A baked vertex decodes to the position it was baked at, on every axis and at both edges.
    #[test]
    fn a_baked_vertex_decodes_to_the_position_it_was_baked_at() {
        let positions = [
            [0.0, 0.0, 0.0],
            [3.0, 7.0, 11.0],
            [15.0, 15.0, 15.0],
            [16.0, 16.0, 16.0],
            [15.0, 1.0, 16.0],
            [16.0, 0.0, 8.0],
        ];

        for position in positions {
            assert_eq!(
                shader_decode(&vertex_at(position).compressed()),
                position,
                "a vertex baked at {position:?} did not come back at that position"
            );
        }
    }

    /// A face that is not on the 1/16 grid is drawn *off* the line it is near, not on it - on both
    /// sides of the block.
    ///
    /// This is the leaf litter: `template_leaf_litter_*` is one quad at 0.25/16 of a block, with an
    /// `up` and a `down` face, and the block below it draws its own top face at the boundary. Truncated,
    /// both are at y = 0 and the depth buffer picks between them per pixel - litter that flickers
    /// against the ground as the camera turns. It is also the fire, whose side quads are at 0.01/16:
    /// rounding *up* fixed the litter and broke the fire, because the variant for the opposite wall is
    /// the same quad turned by `y: 180` and so lands at 15.99/16, one step below the wall's own face.
    #[test]
    fn a_face_between_the_lines_is_drawn_off_them() {
        let decoded = shader_decode(&vertex_at([8.0, 0.25 / 16.0, 8.0]).compressed());

        assert_eq!(
            decoded[1],
            1.0 / 16.0,
            "the quad landed on the block boundary - the plane the ground's own top face is on"
        );

        // And it is monotone: a coordinate never moves *down* past one it was above, and on-grid
        // geometry does not move at all - including both ends of the section, one of which is the
        // coordinate the flag bit exists for.
        for (baked, drawn) in [
            (0.0, 0.0),
            (0.01 / 16.0, 1.0 / 16.0),
            (0.25 / 16.0, 1.0 / 16.0),
            (0.999 / 16.0, 1.0 / 16.0),
            (1.0 / 16.0, 1.0 / 16.0),
            (15.5 / 16.0, 15.0 / 16.0),
            (15.99 / 16.0, 15.0 / 16.0),
            (16.0 / 16.0, 16.0 / 16.0),
            (16.01 / 16.0, 17.0 / 16.0),
            (15.0, 15.0),
            (16.0, 16.0),
        ] {
            let decoded = shader_decode(&vertex_at([baked, 0.0, 0.0]).compressed());

            assert_eq!(
                decoded[0], drawn,
                "{baked} (blocks) was drawn at {}",
                decoded[0]
            );
        }
    }
}
