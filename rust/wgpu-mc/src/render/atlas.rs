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

use crate::mc::chunk::RenderLayer;
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
    /// The sprites the game animates, keyed by the sprite they belong to, with the `.mcmeta` block they
    /// declared it in.
    ///
    /// Keyed rather than a plain list, because the one question this side asks about it is "does the
    /// game animate *this* sprite": a face whose texture the game animates has to be baked with the
    /// game's own atlas coordinates and sent to the game's atlas, since that is the atlas the animation
    /// passes render into (see [`Atlas::sprite_rects`]). The block itself is kept for the day the frame
    /// timing is needed here as well.
    pub animated_textures: RwLock<HashMap<ResourcePath, schemas::texture::TextureAnimation>>,
    /// The layer each sprite's own pixels put it in, filled as it is allocated.
    ///
    /// Minecraft decides a face's render layer from the sprite it samples - `force_translucent` on the
    /// model is the other half, and the only one that can be read from JSON - so this is the table the
    /// block model baker asks when it fills in a face's layer. See [`Atlas::sprite_layer`].
    pub sprite_layers: RwLock<HashMap<ResourcePath, RenderLayer>>,

    /// Where each sprite sits in the **game's own** atlas, in the game's coordinates: `[u0, v0, u1,
    /// v1]`, the four numbers `TextureAtlasSprite#getU0` and its neighbours answer with.
    ///
    /// These are for the faces whose texture the game animates. The game animates an atlas by
    /// *rendering* the due frame into its own atlas texture (`TextureAtlas#cycleAnimationFrames`), so a
    /// face that samples this side's copy of that sprite shows whatever frame was copied and never
    /// changes - the fire that burns still. A face whose sprite is animated is therefore baked with
    /// *these* coordinates and a flag that sends it to the game's atlas instead, and it animates for no
    /// cost at all: nothing is copied, nothing is re-uploaded, the game does what it already does.
    ///
    /// Filled by `WgpuNative.registerSprite`, one call per sprite, before the block states are cached.
    pub sprite_rects: RwLock<HashMap<ResourcePath, [f32; 4]>>,
    /// How many sprites have been added to the image since the texture on the GPU was last written.
    ///
    /// The one thing that makes [`Atlas::allocate`] and [`Atlas::upload`] two calls rather than one:
    /// the atlas is composed on the CPU by whoever bakes a model, and copied to the GPU by whoever
    /// knows the frame's structure. A sprite that is allocated and never uploaded is a block that is
    /// invisible - see [`Atlas::allocate`] - so the count is what [`Atlas::upload_if_dirty`] reads.
    sprites_since_upload: std::sync::atomic::AtomicU64,
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
            animated_textures: RwLock::new(HashMap::new()),
            sprite_layers: Default::default(),
            sprite_rects: Default::default(),
            sprites_since_upload: std::sync::atomic::AtomicU64::new(0),
            size: ATLAS_DIMENSIONS,
        }
    }

    /// The layer a sprite's own pixels put it in, or `None` for a sprite this atlas does not hold.
    ///
    /// The game asks the sprite the same question, one face at a time - `Material.Baked#sprite`
    /// through `SpriteContents#computeTransparency(u0, v0, u1, v1)` over the face's own UV rectangle,
    /// and `force_translucent` on the model overrules it. What is stored here is the answer for the
    /// *whole* sprite, which is the same answer for every face of a sprite that is one kind of thing
    /// throughout - ice, leaves, glass, a plant - and the conservative one for a sprite that is not:
    /// a face that only samples the opaque part of a partly-cutout sprite is put in the cutout layer
    /// too, where Minecraft's own pass draws it correctly. What that costs is this renderer drawing
    /// less of the world, which is the trade to make towards a picture that is right.
    /// One sprite's place in the game's atlas, and the layer the game files it under.
    ///
    /// Both arrive in the same call because both are answers only the game has: the rectangle is the
    /// game's own layout (this side packs its sprites somewhere else entirely), and the layer is the
    /// game's reading of the sprite's transparency, which this side otherwise guesses from the pixels -
    /// and a guess is why a sprite that is part opaque and part cutout is classified as one of them.
    pub fn register_sprite(&self, path: &ResourcePath, rect: [f32; 4], layer: RenderLayer) {
        self.sprite_rects.write().insert(path.clone(), rect);
        self.sprite_layers.write().insert(path.clone(), layer);
    }

    /// Whether the game animates this sprite. See [`Atlas::animated_textures`].
    pub fn sprite_is_animated(&self, path: &ResourcePath) -> bool {
        self.animated_textures.read().contains_key(path)
    }

    /// Where this sprite sits in the game's own atlas. See [`Atlas::sprite_rects`].
    pub fn sprite_rect(&self, path: &ResourcePath) -> Option<[f32; 4]> {
        self.sprite_rects.read().get(path).copied()
    }

    /// Where an **animated** sprite sits in the game's atlas, or `None` for every other sprite - and
    /// for every sprite at all while the animated-texture switch is off or the game's atlas has not
    /// been handed over to the pass that draws the terrain.
    ///
    /// One question rather than four because the answers are one decision, and it is
    /// [`crate::mc::block::face_uses_game_atlas`] that makes it: this only supplies the two the atlas
    /// owns. A face that gets this wrong keeps its own coordinates rather than being drawn from the
    /// wrong place, which is the direction to fail in - a "no" is the frozen fire the renderer has
    /// always had, while a "yes" that should have been a "no" is a face with the wrong texture on it.
    /// See `Vertex::uv_flags`' `UV_GAME_ATLAS` for the whole of it.
    pub fn game_atlas_rect(&self, path: &ResourcePath) -> Option<[f32; 4]> {
        crate::mc::block::face_uses_game_atlas(
            self.sprite_is_animated(path),
            self.sprite_rects.read().get(path).copied(),
        )
    }

    pub fn sprite_layer(&self, path: &ResourcePath) -> Option<RenderLayer> {
        self.sprite_layers.read().get(path).copied()
    }

    /// Add multiple textures to the atlas. This automatically handles .mcmeta files when dealing with block textures
    ///
    /// This writes the **CPU image** and the maps, and nothing else: the texture the pass samples is
    /// whatever the last [`Atlas::upload`] put there. A sprite allocated after that upload is a sprite
    /// whose rectangle is in `uv_map` - so `get_atlas_uv` finds it, the faces that name it are baked -
    /// and whose texels are not on the GPU at all, where wgpu's zero-initialization makes them
    /// `(0, 0, 0, 0)`. A face that samples that is discarded by the shader's alpha test, so the block
    /// is **baked, keyed, culled against its neighbours, and invisible**, with nothing in any log.
    ///
    /// That is not hypothetical: the block models are baked in two passes, and the second one - the
    /// multipart models, which are generated lazily as each state is mapped - runs *after*
    /// `bake_blocks` uploads the atlas. Every sprite that only a multipart model names was therefore
    /// never uploaded, and the mushroom blocks are exactly that: their two textures belong to
    /// `template_single_face` models that nothing else in the game names. See
    /// [`Atlas::upload_if_dirty`], which is what closes it.
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
        let mut sprite_layers = self.sprite_layers.write();

        let before = map.len();

        images.into_iter().for_each(|(name, slice)| {
            self.allocate_one(
                &mut image_buffer,
                &mut map,
                &mut allocator,
                &mut animated_textures,
                &mut sprite_layers,
                name,
                slice.as_ref(),
                resource_provider,
            );
        });

        // Only the sprites that actually landed: a texture that could not be decoded, or that does not
        // fit the atlas, is skipped above and is not a reason to copy the texture again.
        let added = map.len() - before;

        if added != 0 {
            self.sprites_since_upload
                .fetch_add(added as u64, std::sync::atomic::Ordering::Relaxed);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn allocate_one(
        &self,
        image_buffer: &mut ImageBuffer<Rgba<u8>, Vec<u8>>,
        map: &mut HashMap<ResourcePath, UV>,
        allocator: &mut AtlasAllocator,
        animated_textures: &mut HashMap<ResourcePath, schemas::texture::TextureAnimation>,
        sprite_layers: &mut HashMap<ResourcePath, RenderLayer>,
        path: &ResourcePath,
        image_bytes: &[u8],
        resource_provider: &dyn ResourceProvider,
    ) {
        // A sprite the atlas already holds is not allocated a second time: the same path is the same
        // image, and the second rectangle is atlas space nothing can give back - while the map, which
        // is what the baker reads, goes on pointing at whichever of the two was written last. A
        // resource reload is the one thing that changes the pixels behind a path, and it clears this
        // map first (see [`Atlas::clear`]), so a reloaded sprite is allocated again rather than
        // skipped here.
        if map.contains_key(path) {
            return;
        }

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
        let Some(allocation) =
            allocator.allocate(Size2D::new(image.width() as i32, image.height() as i32))
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

        let mcmeta_path = sprite_metadata_path(path);

        let mcmeta = resource_provider
            .get_string(&mcmeta_path)
            .and_then(|string| serde_json::from_str::<schemas::texture::Texture>(&string).ok());

        if let Some(animation) = mcmeta.and_then(|texture| texture.animation) {
            animated_textures.insert(path.clone(), animation);
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

        // What this sprite's own pixels say about the faces that sample it, read here because this is
        // where the decoded image is: the atlas keeps the composed one, not the sprites.
        sprite_layers.insert(path.clone(), layer_of_pixels(&image));
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

        // Everything the image holds is on the GPU now, whenever it was allocated. Clearing the count
        // here rather than in the caller is what makes it a fact about the texture instead of a promise
        // the caller has to keep.
        self.sprites_since_upload
            .store(0, std::sync::atomic::Ordering::Relaxed);

        false
    }

    /// Uploads the atlas if sprites have been added since the last upload, and answers whether it did.
    ///
    /// The gap this closes is the one between the two passes that bake block models. `bake_blocks`
    /// bakes every `variants` model, allocates the sprites they name, and uploads - and then the JVM
    /// asks for a mesh per block *state*, which is what bakes the `multipart` models, one state at a
    /// time. A sprite that only a multipart model names is allocated into the image by that second pass
    /// and, without this call, copied to the GPU never: its faces are baked and sample `(0, 0, 0, 0)`,
    /// and the shader's alpha test discards them.
    ///
    /// What that looked like was a mushroom block that was completely invisible while every number in
    /// the renderer said it was there - it had a key, it had a mesh, it drew a thousand faces, it
    /// culled the faces of the blocks around it - and the two textures that went missing were exactly
    /// the two nothing else in the game names. See [`Atlas::allocate`].
    ///
    /// The whole texture is copied, because a partial upload would have to track which rectangles
    /// changed and the mip chain makes that a per-level problem; it is a few milliseconds, once per
    /// batch of late sprites, against a block that is not drawn at all.
    pub fn upload_if_dirty(&self, wm: &WmRenderer) -> bool {
        if self
            .sprites_since_upload
            .load(std::sync::atomic::Ordering::Relaxed)
            == 0
        {
            return false;
        }

        self.upload(wm);

        true
    }

    /// How many sprites are waiting for an upload. See [`Atlas::upload_if_dirty`].
    pub fn sprites_since_upload(&self) -> u64 {
        self.sprites_since_upload
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Empties the atlas: every sprite is unallocated again, and the next [`Atlas::allocate`] packs it
    /// from scratch.
    ///
    /// This is what a **resource reload** does to it, and the whole thing has to go rather than the
    /// pixels being written over. A reload can change a sprite's size, so the rectangles packed from
    /// one pack are only valid for that pack - and every face already baked holds those rectangles in
    /// its vertices, which is why a reload also re-bakes the block models and has every section meshed
    /// again.
    ///
    /// `uv_map` going with the rest is the part that is easy to leave out and silent when it is.
    /// [`Atlas::allocate`] skips a path the map already has, so an atlas cleared of everything *but*
    /// the map re-packs nothing at all: the old pixels stay, at the old rectangles, and the result is
    /// indistinguishable from a reload that did not happen. The map is also what
    /// [`Atlas::sprite_is_animated`] and the lock in `face_data` are read from, so it is one table for
    /// "where is this sprite", not a cache.
    ///
    /// The image is wiped and the texture is marked as needing an upload, so a reload that allocates
    /// nothing ends with a blank atlas rather than with the last pack's pixels.
    pub fn clear(&self) {
        self.allocator.write().clear();
        self.uv_map.write().clear();
        self.animated_textures.write().clear();
        self.sprite_layers.write().clear();
        self.sprite_rects.write().clear();
        *self.image.write() = ImageBuffer::new(self.size, self.size);

        // One, not zero: the count is what `upload_if_dirty` reads, and what has changed is the atlas
        // itself rather than a sprite added to it.
        self.sprites_since_upload
            .store(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Where a sprite's `.mcmeta` lives, from the name the sprite is packed under.
///
/// This is the one place that turns a sprite **name** - `minecraft:block/fire_0`, which is what a
/// model's `textures` map names and what `uv_map` is keyed by - into the **file** beside it, and it is
/// the same conversion the callers of [`Atlas::allocate`] do to read the image at all
/// (`prepend("textures/").append(".png")`, in `get_model_by_key` and in the fluid lookup). The two
/// have to agree: they are the same sprite.
///
/// It used to be `.mcmeta` appended to the sprite *name*, which is a path no resource pack ships -
/// `minecraft:block/fire_0.mcmeta` is one directory away from the real
/// `minecraft:textures/block/fire_0.png.mcmeta`. That read failed silently (a missing metadata file
/// is the normal case, since most sprites are not animated), so **every** animated sprite this side
/// packed was recorded as a still one, and `Atlas::animated_textures` stayed empty while looking
/// perfectly well maintained.
fn sprite_metadata_path(sprite: &ResourcePath) -> ResourcePath {
    sprite.prepend("textures/").append(".png.mcmeta")
}

/// The layer a sprite's pixels put it in: opaque, cutout or translucent.
///
/// Read the same way round as the game's own answer: a texel that is *half* there makes the sprite
/// translucent (it blends, so it has to be drawn after everything opaque), a texel that is not there
/// at all makes it cutout (it is drawn with the alpha test), and a sprite of fully opaque texels is
/// solid. Partial alpha wins over a hole because the hole can be tested and the blend cannot be
/// avoided: a sprite with both is a sprite that blends, and the transparent parts of a blended sprite
/// are transparent for free.
fn layer_of_pixels(image: &image::DynamicImage) -> RenderLayer {
    use image::GenericImageView;

    let mut layer = RenderLayer::Solid;

    for (_x, _y, pixel) in image.pixels() {
        match pixel.0[3] {
            0 => layer = layer.stronger(RenderLayer::Cutout),
            alpha if alpha < 255 => return RenderLayer::Transparent,
            _ => {}
        }
    }

    layer
}

#[cfg(test)]
mod sprite_layer_tests {
    use super::*;
    use image::{DynamicImage, ImageBuffer};

    /// A sprite of the given texels, in row order.
    fn sprite(width: u32, height: u32, texels: &[[u8; 4]]) -> DynamicImage {
        DynamicImage::ImageRgba8(ImageBuffer::from_fn(width, height, |x, y| {
            Rgba(texels[(y * width + x) as usize])
        }))
    }

    /// Every texel opaque is the solid layer, which is what most of a world is made of.
    #[test]
    fn an_opaque_sprite_is_solid() {
        let opaque = sprite(2, 1, &[[10, 20, 30, 255], [40, 50, 60, 255]]);

        assert_eq!(layer_of_pixels(&opaque), RenderLayer::Solid);
    }

    /// A hole makes it cutout - what the terrain pass already draws with its `discard`, and what
    /// Minecraft's own cutout pass owns once the layer is read from the sprite.
    #[test]
    fn a_sprite_with_a_hole_is_cutout() {
        let leaves = sprite(2, 1, &[[10, 20, 30, 255], [0, 0, 0, 0]]);

        assert_eq!(layer_of_pixels(&leaves), RenderLayer::Cutout);
    }

    /// A texel that is half there makes it translucent, whatever else is in the sprite: a blended
    /// sprite cannot be drawn with the alpha test alone, so the layer has to be the one that blends -
    /// and this is what ice is, which is why it was being drawn twice.
    #[test]
    fn a_sprite_that_blends_is_translucent() {
        let blended = sprite(2, 1, &[[10, 20, 30, 255], [40, 50, 60, 128]]);

        assert_eq!(layer_of_pixels(&blended), RenderLayer::Transparent);
    }
}

#[cfg(test)]
mod sprite_metadata_path_tests {
    use super::*;

    /// A sprite's metadata lives beside the sprite's **file**, not beside the name a model calls it
    /// by. Getting this wrong is silent - a `.mcmeta` that is not there is the ordinary case for a
    /// sprite that is not animated - and it left `animated_textures` empty for the whole life of the
    /// table, which is what made the fire a still image.
    #[test]
    fn a_sprites_metadata_is_looked_for_beside_its_file() {
        let sprite = ResourcePath::from("minecraft:block/fire_0");

        assert_eq!(
            sprite_metadata_path(&sprite).0,
            "minecraft:textures/block/fire_0.png.mcmeta",
            "which is the file a pack ships, and the one the image was read from plus `.mcmeta`"
        );
    }

    /// And the namespace comes along, because a pack's animated sprite is under the pack's namespace.
    #[test]
    fn the_namespace_survives_the_conversion() {
        let sprite = ResourcePath::from("somepack:block/thing");

        assert_eq!(
            sprite_metadata_path(&sprite).0,
            "somepack:textures/block/thing.png.mcmeta"
        );
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
                let source =
                    image.get_pixel((x * 2 + dx).min(width - 1), (y * 2 + dy).min(height - 1));
                let alpha = source[3] as u32;

                for channel in 0..3 {
                    weighted[channel] += source[channel] as u32 * alpha / 255;
                }

                alpha_sum += alpha;
            }

            let alpha = alpha_sum / 4;

            let colour = |channel: u32| -> u8 {
                // `alpha` is the average of the four samples and only ever zero when all four are, in
                // which case there is no colour to un-premultiply and the answer is a transparent
                // black; the division is spelled as a checked one so that it stays an answer.
                (channel / 4 * 255)
                    .checked_div(alpha)
                    .map_or(0, |value| value.min(255) as u8)
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
