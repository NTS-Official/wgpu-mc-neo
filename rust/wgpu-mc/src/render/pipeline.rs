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

/// The bit in [`Vertex::uv_flags`] that says **the game animates this face's sprite** - it is a frame of a
/// moving texture, not a still one.
///
/// It exists for the level-of-detail bias, and for the one thing that separates an animated sprite from
/// every other: **a coarse mip level of a moving sprite is a moving average.** A still sprite's levels are
/// a consistent pyramid and do not change from frame to frame; a scrolling, frame-interpolated one's
/// coarsest levels are a running mean of the animation, so their value swings as the frames advance. That
/// is the fluid shimmer - temporal, on a stationary camera, and the fluids' alone.
///
/// So the bias that cancels it belongs on these faces and nowhere else. It was applied to every minified
/// surface for a round, which removed the shimmer and sharpened every distant *static* surface past what
/// the mip chain is for - a moiré on far terrain, reported as soon as it shipped. The two are the same
/// mechanism pointed at different sprites, so which sprites is the whole of the fix.
///
/// Read from the sprite's `.mcmeta` (see `Atlas::sprite_is_animated`), which is the game's own answer to
/// "does this move" rather than a list this side keeps. **Every sprite is on the game's atlas** - see
/// [`UV_GAME_ATLAS`] - so this cannot be inferred from that flag, and it is written beside it instead.
pub const UV_ANIMATED: u32 = 1 << 1;

/// Where in [`Vertex::uv_flags`] the **level-of-detail floor** is packed, and how wide it is.
///
/// A moving sprite's coarse mip levels are a running average of its animation rather than a smaller copy of
/// it, and past some level there is no animation left to see - only that average, which moves. That is the
/// fluid shimmer, and **no offset can express the fix**: an offset is a count of levels, so one number lands
/// a 16-texel sprite and a 32-texel one at different points of their own detail. `-4` was measured against
/// the water and left the lava; `-5` left it too. What the two need is a **floor**, and a floor is a
/// property of the sprite - so it is computed at bake time, where the sprite's size is known, and carried
/// in the vertex rather than guessed at in a shader.
///
/// The value is **how many levels to take off**, and the shader subtracts it in place of the constant that
/// used to stand there. `0` means no floor, which is every static sprite and every face drawn from this
/// side's own atlas.
///
/// **Four bits at bit 2, because only the low ten bits of this field reach the vertex at all**:
/// `compressed` writes bits 0..8 into `array[10]` and bits 8..10 into the bottom of `array[11]`, so a flag
/// above bit 9 is dropped in silence. Bits 2..6 are the field; bit 6 upward is free.
/// The bit in [`Vertex::uv_flags`] that says this face belongs to Minecraft's **`cutout_mipped`**
/// pipeline rather than its `cutout` one.
///
/// The game's two cutout pipelines are not the same test: `RenderPipelines` declares
/// `pipeline/cutout_terrain` with `ALPHA_CUTOUT` **0.5** and the mipped cutout family with **0.1**. A
/// sprite drawn mipped therefore keeps texels this renderer used to discard - and a mip of a leaf or a
/// grass tuft is mostly exactly those texels, which is why the two renderers' frames differed most on
/// the cutout sprites and least on the solid ones. This side has three chunk layers where the game has
/// four, so the difference travels as one bit per face and the shader picks the cutoff with it; see
/// `UV_CUTOUT_MIPPED_CUTOFF` in `terrain.wgsl`.
///
/// Bits 2..5 hold the level-of-detail floor, so this is the first free one. See [`UV_LOD_FLOOR_SHIFT`].
pub const UV_CUTOUT_MIPPED: u32 = 1 << 6;

pub const UV_LOD_FLOOR_SHIFT: u32 = 2;

/// The width of the [`UV_LOD_FLOOR_SHIFT`] field, as a mask. See there.
pub const UV_LOD_FLOOR_MASK: u32 = 0xF;

/// Packs a level-of-detail floor into the bits [`UV_LOD_FLOOR_SHIFT`] describes, saturating rather than
/// wrapping.
///
/// Saturating because the field is four bits and a floor is not: a sprite deep enough to need sixteen
/// levels is a sprite whose whole chain is within the animation, and clamping it to fifteen costs a fraction
/// of one level of detail while wrapping it would put the floor back at *no* floor - which is the shimmer,
/// at the far end of the range where it is worst.
pub fn lod_floor_bits(levels: u32) -> u32 {
    levels.min(UV_LOD_FLOOR_MASK) << UV_LOD_FLOOR_SHIFT
}

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

    /// One axis of a baked position, in the **1/2048-block** units this vertex format holds.
    ///
    /// **Sixteen bits an axis, at VulkanMod's scale.** VulkanMod's compressed terrain vertex carries its
    /// position as an `ivec4` - three fixed-point values at `1.0 / 2048.0` of a block, and the fourth spent
    /// on the lightmap - and this is that scale, with the fourth component left where this format already
    /// had it. What it replaces was eight bits plus a "this coordinate is exactly 16" flag: a grid of
    /// sixteenths of a block and nothing between the lines, and a model is free to put a face between them.
    /// That face used to be
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
    /// the wall it was burning on. A face strictly inside the block is therefore never put on 0 or 16.
    ///
    /// **At 1/2048 the nudge is nearly always unnecessary, and that is what the extra bits bought.** A
    /// grid of 2048 steps to the block resolves the fire's `0.01/16` - 0.000625 of a block, the value this
    /// whole note is about - to within a fifth of a step, so it lands off the boundary by rounding alone.
    /// The nudge is left for the coordinates that still round onto a multiple of 2048 without being one,
    /// and geometry on the grid is not moved at all.
    ///
    /// **Where the bits came from.** The sixteen bytes are now full: the position takes eight of them, and
    /// the five bits the per-vertex occlusion count was not using - a count of four needs three - are part
    /// of the position now. This note used to end by saying that "the honest fix for that is more bits
    /// rather than a cleverer rounding. `0.01/16` needs about 1/1024 of a block to survive". These are
    /// those bits, and one more. See [`Vertex::VERTEX_LENGTH`] for the size, [`Vertex::compressed`] for
    /// the layout, and the shader's decode in `shaders/terrain.wgsl`.
    #[inline]
    fn axis_to_2048ths(v: f32) -> u16 {
        // Clamped rather than wrapped: sixteen bits name 0..65535, which is 32 blocks - twice the section
        // and twice the reach this format had - and the clamp keeps a model that reaches outside its block
        // where the eight-bit format put it rather than moving it somewhere new.
        let steps = (v * 2048.0).clamp(0.0, 32768.0);
        let nearest = steps.round();

        // A face that is strictly *between* two block boundaries is never put on one of them, whichever
        // side it came from. Those lines are where the neighbouring block's own faces are, and geometry
        // that lands on one is coplanar with them: the fire's side quads are at 0.01/16 of a block, and
        // the variant for the opposite wall is the same quad turned by `y: 180`, so it arrives at
        // 15.99/16 - one step below the wall it is burning against. On-grid geometry is not moved at all,
        // because rounding it lands on itself.
        let on_a_block_boundary = nearest % 2048.0 == 0.0;
        let moved = if on_a_block_boundary && nearest != steps {
            if steps < nearest {
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
        // X, Y, Z, U, V: sixteen bits each, in that order, from the start - so x is the low half of the
        //   first word and y its high half, z the low half of the second with u above it, and v the low
        //   half of the third.
        // Normal: 3 bits, and the per-vertex occlusion count: 3 bits (a count of four)
        // Animated UV index: 10 bits ([`UV_GAME_ATLAS`] is the one that is used)
        // Color: 3 bytes
        // Block light nibble: 1 byte (4 bits for block, 4 bits for sky)

        // Total: **all 128 bits.** The position's eight bits an axis are what filled it, and they were
        // bought partly from the occlusion count, which was a byte and is a count of four.
        let mut array = [0; Self::VERTEX_LENGTH];

        let x = Self::axis_to_2048ths(self.position[0]);
        let y = Self::axis_to_2048ths(self.position[1]);
        let z = Self::axis_to_2048ths(self.position[2]);

        //position: a word each for x and y, and z sharing the second word with u
        array[0..2].copy_from_slice(&x.to_le_bytes());
        array[2..4].copy_from_slice(&y.to_le_bytes());
        array[4..6].copy_from_slice(&z.to_le_bytes());

        //U
        array[6..8].copy_from_slice(&self.uv[0].to_le_bytes());

        //V
        array[8..10].copy_from_slice(&self.uv[1].to_le_bytes());

        let normal_bits: u8 = match self.normal {
            [-1.0, 0.0, 0.0] => 0b00000100,
            [1.0, 0.0, 0.0] => 0b00000000,
            [0.0, 1.0, 0.0] => 0b00000001,
            [0.0, -1.0, 0.0] => 0b00000101,
            [0.0, 0.0, 1.0] => 0b00000010,
            [0.0, 0.0, -1.0] => 0b00000110,
            _ => unreachable!("Invalid vertex normal"),
        };

        //UV index, normal and the occlusion count. **The flags keep the place they had** -
        //`UV_GAME_ATLAS` is bit 16 of this word, which is what the shader tests and what the test below
        //pins - and the three bits above the normal are the occlusion count now rather than the
        //position's "this coordinate is exactly 16" flags, which do not exist any more.
        array[10] = self.uv_flags as u8;
        array[11] =
            (((self.uv_flags >> 8) as u8) & 0b11) | (normal_bits << 2) | ((self.ao & 0b111) << 5);

        //color
        array[12] = (self.color & 0xff) as u8;
        array[13] = ((self.color >> 8) & 0xff) as u8;
        array[14] = ((self.color >> 16) & 0xff) as u8;

        //and the light in the last byte
        array[15] = self.lightmap_coords;

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
        (
            "section_draws".into(),
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("Section Draw Bind Group Layout"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    // **The vertex stage alone**, which is the one thing about this layout that can be
                    // wrong in a way wgpu refuses rather than in a way a picture shows: the fragment stage
                    // never reads a draw's record, and a binding declared visible to one stage and named by
                    // the other is a pipeline wgpu will not build - and a refusal on this path ends the
                    // process. The test that reads both terrain shaders for exactly that is
                    // `the_draw_records_are_read_by_the_vertex_stage_alone` in `graph.rs`.
                    //
                    // Read-only, because the pass only reads it: the records are written with
                    // `Queue::write_buffer`, which is not a shader store.
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
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
    /// y that lands in another byte, or an axis read from the wrong half of a word, is terrain that is
    /// drawn somewhere it is not - mirrored, stretched, or off the ground entirely - with nothing in the
    /// log to say which.
    fn shader_decode(bytes: &[u8; 16]) -> [f32; 3] {
        let word = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());

        let v1 = word(0);
        let v2 = word(4);

        // Sixteen bits an axis at 1/2048 of a block, and no "exactly 16" flag to test: sixteen bits reach
        // 32 blocks, so 16.0 is 32768 like any other coordinate.
        [
            (v1 & 0xffff) as f32 / 2048.0,
            (v1 >> 16) as f32 / 2048.0,
            (v2 & 0xffff) as f32 / 2048.0,
        ]
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

    /// **A face's flags survive the packing, at the bit positions the shader reads them from.**
    ///
    /// The failure this is here for is silent in the worst way: a flag written into the wrong bit, or one
    /// that never reaches `compressed()` at all, produces a shader that reads a constant zero. For
    /// [`UV_GAME_ATLAS`] that is every face drawn from this side's frozen copy of its sprite - the still
    /// fire - and for [`UV_ANIMATED`] it is the level-of-detail offset silently not applying, which is the
    /// fluid shimmer coming back with nothing in any log to say why.
    ///
    /// The bits are checked **in `v3`** rather than through `uv_flags`, because `v3` is the word the shader
    /// is actually handed and the packing between the two is the part that can be wrong. `array[10] =
    /// uv_flags as u8` and `array[11] = (uv_flags >> 8) & 0b11 | ...` put the low ten bits at `v3 >> 16`,
    /// which is where both of these live.
    #[test]
    fn a_faces_flags_survive_the_packing() {
        let v3_of = |flags: u32| {
            let bytes = Vertex {
                uv_flags: flags,
                ..vertex_at([0.0, 0.0, 0.0])
            }
            .compressed();

            u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]])
        };

        for (flags, game_atlas, animated) in [
            (0u32, 0u32, 0u32),
            (UV_GAME_ATLAS, 1, 0),
            (UV_ANIMATED, 0, 1),
            (UV_GAME_ATLAS | UV_ANIMATED, 1, 1),
        ] {
            let v3 = v3_of(flags);

            assert_eq!(
                (v3 >> 16) & 1,
                game_atlas,
                "`UV_GAME_ATLAS` (flags {flags:#b}) is bit 16 of v3, which is what the shader tests"
            );
            assert_eq!(
                (v3 >> 17) & 1,
                animated,
                "`UV_ANIMATED` (flags {flags:#b}) is bit 17 of v3, which is what the shader tests - and a \
                 flag that does not arrive is a shader reading zero, not an error"
            );
            // And the floor field must not be disturbed by them, or the two features would share bits and
            // whichever was set last would win.
            assert_eq!(
                (v3 >> 18) & 0xf,
                0,
                "the floor field has to stay clear for flags that carry no floor"
            );
        }
    }

    /// **The level-of-detail floor survives the packing at the bits the shader reads it from.**
    ///
    /// The same silent-failure argument as the flags above, and worse: a floor that arrives as zero is not
    /// a wrong picture, it is *the fluid shimmer coming back* - the exact symptom this whole mechanism
    /// exists to remove - with nothing in any log to say the field was dropped. Only the low ten bits of
    /// `uv_flags` reach the vertex at all, so a floor packed above bit 9 would vanish without a warning from
    /// anything.
    ///
    /// The shader's side is spelled out here as a literal rather than through the constants, because the
    /// point is to catch the constants and the shader drifting apart: `terrain.wgsl` reads
    /// `(v3 >> 18u) & 0xfu`, and that expression is what this asserts against.
    #[test]
    fn a_level_floor_survives_the_packing() {
        let v3_of = |flags: u32| {
            let bytes = Vertex {
                uv_flags: flags,
                ..vertex_at([0.0, 0.0, 0.0])
            }
            .compressed();

            u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]])
        };

        // The whole field, at its ends and one past each, so a shift that is off by one shows up as a
        // factor of two rather than as a rounded value that happens to match.
        for levels in [0u32, 1, 2, 3, 4, 5, 7, 15] {
            let v3 = v3_of(lod_floor_bits(levels));

            assert_eq!(
                (v3 >> 18) & 0xf,
                levels,
                "a floor of {levels} level(s) came back as {} from the bits the shader reads",
                (v3 >> 18) & 0xf
            );
        }

        // And a floor wider than the field saturates rather than wrapping. Wrapping would answer *no* floor
        // at the top of the range, which is the shimmer at its worst rather than a half level of detail.
        assert_eq!(
            (v3_of(lod_floor_bits(16)) >> 18) & 0xf,
            15,
            "sixteen levels is more than four bits; it has to clamp, not wrap to zero"
        );
        assert_eq!(
            (v3_of(lod_floor_bits(999)) >> 18) & 0xf,
            15,
            "and so has anything above it"
        );
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

    /// A face that is not on the block grid is drawn where it was baked, and the nudge that used to keep
    /// it off the boundary is left for the few coordinates that still round onto one.
    ///
    /// This is the leaf litter: `template_leaf_litter_*` is one quad at 0.25/16 of a block, with an
    /// `up` and a `down` face, and the block below it draws its own top face at the boundary. At 1/16 that
    /// quad did not fit the grid at all and was *nudged* a whole sixteenth off the boundary to stop it
    /// flickering against the ground; at 1/2048 it simply fits, and that is the change in one assertion.
    /// It is also the fire, whose side quads are at 0.01/16 and whose `y: 180` variant lands at 15.99/16:
    /// those are 1.28 and 0.36 of a step away from the nearest line, so they round to one rather than onto
    /// a boundary, and the nudge does not have to move them.
    #[test]
    fn a_face_between_the_lines_is_drawn_off_them() {
        let decoded = shader_decode(&vertex_at([8.0, 0.25 / 16.0, 8.0]).compressed());

        assert_eq!(
            decoded[1],
            0.25 / 16.0,
            "the litter was moved off the coordinate it was baked at - the whole point of the finer grid \
             is that it does not have to be"
        );

        // And it is monotone: a coordinate never moves *down* past one it was above, and on-grid geometry
        // does not move at all - including both ends of the section, which used to need a flag bit each
        // because 16 did not fit in a byte.
        for (baked, drawn) in [
            (0.0, 0.0),
            // A value that still rounds onto a block boundary without being one: it is nudged a step off,
            // which is 1/2048 of a block now rather than 1/16.
            (0.0002, 1.0 / 2048.0),
            // The fire's own coordinate, resolved rather than moved.
            (0.01 / 16.0, 1.0 / 2048.0),
            (0.25 / 16.0, 0.25 / 16.0),
            (0.999 / 16.0, 128.0 / 2048.0),
            (1.0 / 16.0, 1.0 / 16.0),
            (15.5 / 16.0, 15.5 / 16.0),
            (15.99 / 16.0, 2047.0 / 2048.0),
            (16.0 / 16.0, 16.0 / 16.0),
            (16.01 / 16.0, 2049.0 / 2048.0),
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

/// The flag bits share one word, so a new one has to land somewhere free. See
/// [`UV_CUTOUT_MIPPED`].
#[cfg(test)]
mod uv_flag_bit_tests {
    use super::*;

    #[test]
    fn the_flag_bits_do_not_overlap() {
        let used = UV_GAME_ATLAS | UV_ANIMATED | (UV_LOD_FLOOR_MASK << UV_LOD_FLOOR_SHIFT);

        assert_eq!(
            UV_CUTOUT_MIPPED & used,
            0,
            "UV_CUTOUT_MIPPED overlaps a bit that already means something"
        );
        assert_eq!(UV_CUTOUT_MIPPED.count_ones(), 1);
    }
}
