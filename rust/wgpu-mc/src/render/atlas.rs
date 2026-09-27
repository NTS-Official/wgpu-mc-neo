use std::collections::HashMap;
use std::fmt::{Debug, Formatter};
use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use guillotiere::AtlasAllocator;
use guillotiere::euclid::Size2D;
use image::imageops::overlay;
use image::{ImageBuffer, Rgba};
use minecraft_assets::schemas;
use parking_lot::RwLock;
use wgpu::Extent3d;

use crate::mc::resource::{ResourcePath, ResourceProvider};
use crate::texture::{TextureAndView, UV};
use crate::{Gpu, WmRenderer};

/// The width and height of an [atlas](Atlas];
pub const ATLAS_DIMENSIONS: u32 = 2048;

/// How many mip levels the block atlas is built with: the full image and three halves.
///
/// The game gives its block atlas four levels, and the reason it needs them is what a terrain drawn
/// from a 2048-wide atlas looks like without them: past a few chunks every surface is minified far
/// below one texel per pixel, and sampling the full-resolution image with nearest filtering turns that
/// into the moiré that crawls over distant terrain as the view moves. Four levels reach 256 texels,
/// which is where the visible crawling stops.
pub const ATLAS_MIP_LEVELS: u32 = 4;

/// A texture atlas. This is used in many places, most notably terrain and entity rendering.
/// Combines multiple small textures into a single big one, which can help improve performance.
///
/// # Example
///
///```ignore
/// # use wgpu_mc::mc::resource::{ResourcePath, ResourceProvider};
/// # use wgpu_mc::render::atlas::Atlas;
/// # use wgpu_mc::{Display, WmRenderer};
/// # use wgpu_mc::render::pipeline::RenderPipelineManager;
///
/// # let wgpu_state: Display;
/// # let wm_renderer: WmRenderer;
/// # let pipelines: RenderPipelineManager;
/// # let resource_provider: Box<dyn ResourceProvider>;
///
/// let atlas = Atlas::new(&wgpu_state, &pipelines, false);
///
/// let cobble = ResourcePath("minecraft:textures/block/cobblestone.json".into());
/// let dirt = ResourcePath("minecraft:textures/block/dirt.json".into());
///
/// atlas.allocate(
///     [
///         (
///             &cobble,
///             &resource_provider.get_bytes(&cobble).unwrap()
///         ),
///         (
///             &dirt,
///             &resource_provider.get_bytes(&dirt).unwrap()
///         )
///     ], &*resource_provider
/// );
///
/// atlas.upload(&wm_renderer);
/// ```
pub struct Atlas {
    /// The image allocator which decides where images should go in the atlas texture
    pub allocator: RwLock<AtlasAllocator>,
    /// The atlas image buffer itself. This is what gets uploaded to the GPU
    pub image: RwLock<ImageBuffer<Rgba<u8>, Vec<u8>>>,
    /// The mapping of image [ResourcePath]s to UV coordinates
    pub uv_map: RwLock<HashMap<ResourcePath, UV>>,
    /// The representation of the [Atlas]'s image buffer on the GPU, which can be bound to a draw call
    pub texture: Arc<TextureAndView>,
    /// Not every [Atlas] is used for block textures, but the ones that are store the information for each animated texture here
    pub animated_textures: RwLock<Vec<schemas::texture::TextureAnimation>>,
    pub animated_texture_offsets: RwLock<HashMap<ResourcePath, u32>>,
    size: u32,
}

impl Debug for Atlas {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Atlas {{ uv_map: {:?} }}", self.uv_map.read())
    }
}

impl Atlas {
    pub fn new(display: &Gpu, _resizes: bool) -> Self {
        let tv = TextureAndView::from_rgb_bytes(
            display,
            &vec![0u8; (ATLAS_DIMENSIONS * ATLAS_DIMENSIONS) as usize * 4],
            Extent3d {
                width: ATLAS_DIMENSIONS,
                height: ATLAS_DIMENSIONS,
                depth_or_array_layers: 1,
            },
            None,
            wgpu::TextureFormat::Rgba8Unorm,
            ATLAS_MIP_LEVELS,
        )
        .unwrap();

        Self {
            allocator: RwLock::new(AtlasAllocator::new(Size2D::new(
                ATLAS_DIMENSIONS as i32,
                ATLAS_DIMENSIONS as i32,
            ))),
            image: RwLock::new(ImageBuffer::new(ATLAS_DIMENSIONS, ATLAS_DIMENSIONS)),
            uv_map: Default::default(),
            texture: Arc::new(tv),
            animated_textures: RwLock::new(Vec::new()),
            animated_texture_offsets: Default::default(),
            size: ATLAS_DIMENSIONS,
        }
    }

    /// Add multiple textures to the atlas. This automatically handles .mcmeta files when dealing with block textures
    pub fn allocate<'a, T>(
        &self,
        images: impl IntoIterator<Item = (&'a ResourcePath, &'a T)>,
        resource_provider: &dyn ResourceProvider,
    ) where
        T: AsRef<[u8]> + 'a,
    {
        let mut allocator = self.allocator.write();
        let mut image_buffer = self.image.write();
        let mut map = self.uv_map.write();

        let mut animated_textures = self.animated_textures.write();
        // let mut animated_texture_offsets = self.animated_texture_offsets.write();

        images.into_iter().for_each(|(name, slice)| {
            self.allocate_one(
                &mut image_buffer,
                &mut map,
                &mut allocator,
                &mut animated_textures,
                name,
                slice.as_ref(),
                resource_provider,
            );
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn allocate_one(
        &self,
        image_buffer: &mut ImageBuffer<Rgba<u8>, Vec<u8>>,
        map: &mut HashMap<ResourcePath, UV>,
        allocator: &mut AtlasAllocator,
        animated_textures: &mut Vec<schemas::texture::TextureAnimation>,
        path: &ResourcePath,
        image_bytes: &[u8],
        resource_provider: &dyn ResourceProvider,
    ) {
        // A texture the `image` crate cannot decode - a pack with a mislabelled or truncated file -
        // is skipped rather than unwrapped, and the faces that name it come out untextured, which is
        // what `get_atlas_uv` already does for a texture that is not in the map.
        let Ok(image) = image::load_from_memory(image_bytes) else {
            log::warn!("wgpu-mc: {path} could not be decoded as an image; skipping it");
            return;
        };

        // The atlas does not resize (`Atlas::new` ignores its `resizes` flag), so a pack with more
        // or larger textures than 2048x2048 holds has nowhere to put the ones that do not fit.
        // Skipping them is the same trade as above: a texture that is not in the map is a face that
        // is not drawn, rather than a panic on the block cache thread.
        let Some(allocation) = allocator
            .allocate(Size2D::new(image.width() as i32, image.height() as i32))
        else {
            log::warn!(
                "wgpu-mc: {}x{} {path} does not fit in the {}x{} atlas; skipping it",
                image.width(),
                image.height(),
                ATLAS_DIMENSIONS,
                ATLAS_DIMENSIONS
            );
            return;
        };

        overlay(
            image_buffer,
            &image,
            allocation.rectangle.min.x as i64,
            allocation.rectangle.min.y as i64,
        );

        let mcmeta_path = path.append(".mcmeta");

        let mcmeta = resource_provider
            .get_string(&mcmeta_path)
            .and_then(|string| serde_json::from_str::<schemas::texture::Texture>(&string).ok());

        if let Some(texture) = mcmeta {
            if let Some(animation) = texture.animation {
                animated_textures.push(animation)
            }
        }

        map.insert(
            path.clone(),
            (
                (
                    allocation.rectangle.min.x as u16,
                    allocation.rectangle.min.y as u16,
                ),
                (
                    allocation.rectangle.max.x as u16,
                    allocation.rectangle.max.y as u16,
                ),
            ),
        );
    }

    /// Upload the atlas texture to the GPU. If the Atlas has to resize the texture on the GPU, then the bindable_texture that this struct provides may
    /// become obsolete if you .load() the BindableTexture before calling upload(), so you should get the BindableTexture after calling this function and not before-hand.
    /// Returns true if the atlas was resized.
    pub fn upload(&self, wm: &WmRenderer) -> bool {
        // The whole chain, not just the base level: see [ATLAS_MIP_LEVELS]. Each level is halved from
        // the one above on the CPU, which is a few milliseconds once at startup and one less pass to
        // get wrong - a mip chain generated by the GPU would want a blit pass per level, inside the
        // frame, ordered against the frame's own work.
        let mut level_image = self.image.read().clone();

        for level in 0..ATLAS_MIP_LEVELS {
            let size = (self.size >> level).max(1);

            wm.gpu.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.texture.texture,
                    mip_level: level,
                    origin: Default::default(),
                    aspect: Default::default(),
                },
                level_image.as_raw(),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    // `size` is in pixels and the atlas is RGBA8, so a row is four bytes a pixel: 8192
                    // for the 2048-wide atlas, not 2048. Handing wgpu the pixel count is a validation
                    // error - "Number of bytes per row is less than the number of bytes in a complete
                    // row" - and a validation error inside a `#[jni_fn]` frame ends the game, which is
                    // how it behaved the first time this upload was reached at all.
                    bytes_per_row: Some(size * 4),
                    rows_per_image: Some(size),
                },
                Extent3d {
                    width: size,
                    height: size,
                    depth_or_array_layers: 1,
                },
            );

            if level + 1 < ATLAS_MIP_LEVELS {
                level_image = halve(&level_image);
            }
        }

        false
    }

    pub fn clear(&self) {
        self.allocator.write().clear();
        self.animated_texture_offsets.write().clear();
        self.animated_textures.write().clear();
        *self.image.write() = ImageBuffer::new(self.size, self.size);
    }
}

/// Halves an image: one mip level of the atlas, from the one above it.
///
/// Averaged in *premultiplied* alpha, which is the whole of the care this needs: a block atlas is full
/// of cutout sprites, and a plain average of the four channels would mix the colour of a fully
/// transparent texel - black, in every texture that has one - into the opaque texels beside it, so
/// every leaf and pane of glass would come out of the coarse levels with a dark fringe. With the
/// colour weighted by its own alpha, a transparent texel contributes nothing to the colour and only to
/// the alpha, which is what "this texel is not there" means.
fn halve(image: &ImageBuffer<Rgba<u8>, Vec<u8>>) -> ImageBuffer<Rgba<u8>, Vec<u8>> {
    let width = image.width().max(1);
    let height = image.height().max(1);

    let mut out = ImageBuffer::new((width / 2).max(1), (height / 2).max(1));

    for y in 0..out.height() {
        for x in 0..out.width() {
            let mut weighted = [0u32; 3];
            let mut alpha_sum = 0u32;

            for (dx, dy) in [(0u32, 0u32), (1, 0), (0, 1), (1, 1)] {
                let source = image.get_pixel((x * 2 + dx).min(width - 1), (y * 2 + dy).min(height - 1));
                let alpha = source[3] as u32;

                for channel in 0..3 {
                    weighted[channel] += source[channel] as u32 * alpha / 255;
                }

                alpha_sum += alpha;
            }

            let alpha = alpha_sum / 4;

            let colour = |channel: u32| -> u8 {
                if alpha == 0 {
                    0
                } else {
                    (channel / 4 * 255 / alpha).min(255) as u8
                }
            };

            out.put_pixel(
                x,
                y,
                Rgba([
                    colour(weighted[0]),
                    colour(weighted[1]),
                    colour(weighted[2]),
                    alpha as u8,
                ]),
            );
        }
    }

    out
}

/// Stores uploaded textures which will be automatically updated whenever necessary
#[derive(Debug)]
pub struct TextureManager {
    pub default_sampler: Arc<wgpu::Sampler>,

    pub atlases: RwLock<HashMap<String, Atlas>>,
}

impl TextureManager {
    #[must_use]
    pub fn new(wgpu_state: &Gpu) -> Self {
        let sampler = wgpu_state.device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            // The atlas has a mip chain (see [ATLAS_MIP_LEVELS]) and this is what picks between its
            // levels: nearest within the level it lands on - the blocky look the game has - and a blend
            // between the two levels it lands between, so a surface crossing a level boundary does not
            // snap. That is the game's own `GL_NEAREST_MIPMAP_LINEAR`, and without it the chain would
            // never be read at all.
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });

        Self {
            default_sampler: Arc::new(sampler),
            atlases: RwLock::new(HashMap::new()),
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Zeroable, Pod)]
#[allow(unused)]
struct AnimatedUV {
    pub uv_1: [f32; 2],
    pub uv_2: [f32; 2],
    pub blend: f32,
    pub padding: f32,
}

// impl AnimatedTexture {
//     pub fn new(width: u32, height: u32, real_width: f32, animation: AnimationData) -> Self {
//         Self {
//             width,
//             height,
//             frame_size: width,
//             real_width,
//             real_frame_size: real_width,
//             animation,
//             frame_count: height / width,
//             subframe: 0,
//         }
//     }

//     pub fn get_frame_size(&self) -> u32 {
//         self.frame_size
//     }

//     pub fn update(&self, subframe: u32) -> [f32; 5] {

//         //Due to padding in the buffer, some of these elements are always left as 0.0
//         let mut out = [0.0; 5];
//         let mut current_frame = (subframe / self.animation.frame_time) % self.frame_count;

//         if self.animation.frames.is_some() { //if custom frame order is present translate to that
//             current_frame = self.animation.frames.as_ref().unwrap()[current_frame as usize];
//         }

//         out[1] = self.real_frame_size * (current_frame as f32);

//         if self.animation.interpolate {
//             let mut next_frame = ((subframe / self.animation.frame_time) + 1) % self.frame_count;

//             if self.animation.frames.is_some() { //if custom frame order is present translate to that
//                 next_frame = self.animation.frames.as_ref().unwrap()[next_frame as usize];
//             }

//             out[3] = self.real_frame_size * (next_frame as f32);
//             out[4] = ((subframe % self.animation.frame_time) as f32) / (self.animation.frame_time as f32);
//         }

//         out
//     }
// }
