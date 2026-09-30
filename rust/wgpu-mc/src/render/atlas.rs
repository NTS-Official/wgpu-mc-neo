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
/// One sprite as the game's atlas holds it: where it is, whether it moves, and how deep its chain goes.
///
/// A struct rather than a tuple because the three are read together and mean different things, and because
/// a fourth would otherwise be added to every `Some((rect, animated))` in the tree - see
/// [`Atlas::game_atlas_rect`] for what each is for.
#[derive(Copy, Clone, Debug)]
pub struct SpriteInAtlas {
    /// Where the sprite sits in the game's atlas, `0..1` over it.
    pub rect: [f32; 4],
    /// Whether the game animates it, which is what scopes the shader's level-of-detail offset. See
    /// `Vertex::uv_flags`' `UV_ANIMATED`.
    pub animated: bool,
    /// The coarsest level of the game's chain this sprite should be sampled from, or `None` when nothing
    /// said. See [`Atlas::sprite_level_caps`].
    pub level_cap: Option<u32>,
}

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
    /// **How deep each sprite's mip chain goes, as the coarsest level a face should sample.**
    ///
    /// Sent with the rectangle because it is the other thing only the registration knows: a sprite whose
    /// chain is built for the *atlas* has levels far past the point where its own detail is gone, and what
    /// is left there is not the sprite at a smaller size but a running average of it. For a scrolling,
    /// frame-interpolated sprite - the fluids - that average *moves*, which is the fluid shimmer: the level
    /// is not too coarse, it is past the end of the animation.
    ///
    /// **An offset cannot express this and that is why it is here.** A bias is a count of levels, so the
    /// same number lands a 16-texel sprite and a 32-texel one at different points of their own detail;
    /// `-4` was measured against the water and left the lava, and `-5` left it too. What the two need is a
    /// *floor*, and a floor is a property of the sprite - so it is registered beside the rectangle rather
    /// than guessed at in a shader.
    ///
    /// `None` for a sprite nothing was registered for, which is also the answer for every sprite this side
    /// draws from its own atlas.
    pub sprite_level_caps: RwLock<HashMap<ResourcePath, u32>>,
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
            sprite_level_caps: Default::default(),
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
    pub fn register_sprite(
        &self,
        path: &ResourcePath,
        rect: [f32; 4],
        layer: RenderLayer,
        level_cap: Option<u32>,
    ) {
        self.sprite_rects.write().insert(path.clone(), rect);
        self.sprite_layers.write().insert(path.clone(), layer);

        if let Some(cap) = level_cap {
            self.sprite_level_caps.write().insert(path.clone(), cap);
        }
    }

    /// Whether the game animates this sprite. See [`Atlas::animated_textures`].
    pub fn sprite_is_animated(&self, path: &ResourcePath) -> bool {
        self.animated_textures.read().contains_key(path)
    }

    /// How many sprites the game animates, which is **how many sprites carry `UV_ANIMATED`** and so how
    /// many get the level-of-detail offset that cancels the fluid shimmer.
    ///
    /// Reported because that offset compensates for a level chosen too coarse, and the set it applies to is
    /// the whole of how narrow the compensation is. It *should* be small - the fluids, the fire, a handful
    /// of others - and if it ever is not, the effect is a bias nobody asked for on sprites that were already
    /// right, which is the exact shape of the moiré this scoping was written to remove. A number in the log
    /// is what turns "should be small" into something that was checked.
    pub fn animated_sprite_count(&self) -> usize {
        self.animated_textures.read().len()
    }

    /// Where this sprite sits in the game's own atlas. See [`Atlas::sprite_rects`].
    pub fn sprite_rect(&self, path: &ResourcePath) -> Option<[f32; 4]> {
        self.sprite_rects.read().get(path).copied()
    }

    /// Where a sprite sits in the game's atlas, **whether the game animates it, and how deep its mip chain
    /// goes** - or `None` for every sprite at all while the animated-texture switch is off or the game's
    /// atlas has not been handed over to the pass that draws the terrain.
    ///
    /// One question rather than four because the answers are one decision, and it is
    /// [`crate::mc::block::face_uses_game_atlas`] that makes it: this only supplies the answers the atlas
    /// owns. A face that gets this wrong keeps its own coordinates rather than being drawn from the wrong
    /// place, which is the direction to fail in - a "no" is the frozen fire the renderer has always had,
    /// while a "yes" that should have been a "no" is a face with the wrong texture on it.
    pub fn game_atlas_rect(&self, path: &ResourcePath) -> Option<SpriteInAtlas> {
        let animated = self.sprite_is_animated(path);

        crate::mc::block::face_uses_game_atlas(
            animated,
            self.sprite_rects.read().get(path).copied(),
        )
        .map(|rect| SpriteInAtlas {
            rect,
            animated,
            level_cap: self.sprite_level_caps.read().get(path).copied(),
        })
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
        self.sprite_level_caps.write().clear();
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
mod block_atlas_sampler_tests {
    use super::*;

    /// **Both block atlases are filtered the same way**, which is the property this function exists for.
    ///
    /// A descriptor is built here rather than a sampler, because building one needs no device - which
    /// is the only reason this can be a test at all: the two sites that create the samplers are deep in
    /// `RenderGraph::new` and `TextureManager::new`, both of which want a `Gpu`.
    ///
    /// What is asserted is the **agreement**, not the filter. The filter itself has been moved three times
    /// and is the game's for now - all three linear, anisotropy 16, which is what `LevelRenderer` builds -
    /// so a test that pinned it would have had to be rewritten at each of those moves. The failure this is
    /// here to catch is not "it is linear" but "the two atlases disagree", which is what produced "these
    /// are all blurry, there is none of the game's crisp pixels left" about the fire, the lava and every
    /// other animated sprite while the blocks around them were clean.
    ///
    /// The address mode is the thing that *does* differ, so both are checked: this side's atlas is
    /// `Repeat`, because its coordinates come from this side's own packing, and the game's is
    /// `ClampToEdge`, which is what the game asks for.
    #[test]
    fn both_block_atlases_are_filtered_the_same_way() {
        let repeat = block_atlas_sampler(wgpu::AddressMode::Repeat);
        let clamp = block_atlas_sampler(wgpu::AddressMode::ClampToEdge);

        assert_eq!(
            repeat.mag_filter, clamp.mag_filter,
            "one atlas magnified one way and the other another is two filters in one frame"
        );
        assert_eq!(
            repeat.min_filter, clamp.min_filter,
            "and the same at minification, which is what a distant surface is"
        );
        assert_eq!(
            repeat.mipmap_filter, clamp.mipmap_filter,
            "and the same between mip levels"
        );
        assert_eq!(
            repeat.anisotropy_clamp, clamp.anisotropy_clamp,
            "and the same anisotropy, which is the filter that decides how a grazing angle is sampled"
        );

        // Whichever filter the two agree on, the mip chain has to be read, or the levels above zero are
        // never sampled at all.
        assert_eq!(
            repeat.mipmap_filter,
            wgpu::MipmapFilterMode::Linear,
            "the atlas has a mip chain and a blend between its levels is what reads it"
        );

        // And the one thing that is meant to differ.
        assert_eq!(repeat.address_mode_u, wgpu::AddressMode::Repeat);
        assert_eq!(repeat.address_mode_v, wgpu::AddressMode::Repeat);
        assert_eq!(repeat.address_mode_w, wgpu::AddressMode::Repeat);
        assert_eq!(clamp.address_mode_u, wgpu::AddressMode::ClampToEdge);
        assert_eq!(clamp.address_mode_v, wgpu::AddressMode::ClampToEdge);
        assert_eq!(clamp.address_mode_w, wgpu::AddressMode::ClampToEdge);
    }

    /// **Anisotropy above 1 needs the min, mag *and* mipmap filters linear, and getting it wrong is fatal
    /// rather than ugly.**
    ///
    /// A regression test for a crash that reached a running client **twice**, the second time because this
    /// test had been "corrected" into agreeing with the mistake.
    ///
    /// The first time the anisotropy was raised to 16 while magnification was `Nearest`; the second, a
    /// player turned `game_atlas_blend_mips` off, which makes the mip filter `Nearest` while the anisotropy
    /// stayed 16. Both ended in `create_sampler` returning `Err`, which on this path runs the panic hook and
    /// stops the process - the first on the first frame, the second two seconds after the option applied.
    ///
    /// **The rule is asserted rather than the values, and the rule is `wgpu-core`'s three separate checks**
    /// (`device/resource.rs`): `InvalidFilterModeWithAnisotropy` for the min filter, the same for the mag
    /// filter, and `InvalidMipmapFilterModeWithAnisotropy` for the mip filter. Reading two of those three
    /// and concluding "a `Nearest` mip filter is legal with anisotropy" is how the second crash was written
    /// down as a fix, and why the mip filter is asserted here beside the other two.
    ///
    /// Checked on the descriptor, because that is what a future edit is most likely to break: the anisotropy
    /// lives in `block_atlas_sampler` and the mip filter in `game_atlas_sampler`, so setting one without
    /// thinking about the other is an easy mistake to make - and now one that has been made twice.
    #[test]
    fn the_game_atlas_never_asks_for_anisotropy_without_linear_filters() {
        let assert_legal = |descriptor: &wgpu::SamplerDescriptor<'_>, blend: bool| {
            if descriptor.anisotropy_clamp > 1 {
                assert_eq!(
                    descriptor.mag_filter,
                    wgpu::FilterMode::Linear,
                    "anisotropy {} with a non-linear magnification filter is \
                     `InvalidFilterModeWithAnisotropy` (blend switch {blend})",
                    descriptor.anisotropy_clamp
                );
                assert_eq!(
                    descriptor.min_filter,
                    wgpu::FilterMode::Linear,
                    "and the same for minification (blend switch {blend})"
                );
                assert_eq!(
                    descriptor.mipmap_filter,
                    wgpu::MipmapFilterMode::Linear,
                    "and the mip filter is a third check, not an exemption: a `Nearest` one beside \
                     anisotropy is `InvalidMipmapFilterModeWithAnisotropy` (blend switch {blend})"
                );
            }
        };

        set_game_atlas_blend_mips(true);

        // **The option is on, because the anisotropy is the game's own answer now and its default answer is
        // 1.** See `terrain_anisotropy`: the clamp is `maxAnisotropyValue` under `ANISOTROPIC` and 1 under
        // the other two, exactly as `LevelRenderer` builds its terrain sampler - so a test that never sets
        // it is testing "Fast", whatever it says about blending.
        set_texture_filtering(TEXTURE_FILTERING_ANISOTROPIC);

        let blended = game_atlas_sampler();

        assert_legal(&blended, true);
        assert_eq!(
            blended.mipmap_filter,
            wgpu::MipmapFilterMode::Linear,
            "blending levels is what makes the mip filter linear"
        );
        assert_eq!(
            blended.anisotropy_clamp, GAME_ANISOTROPY,
            "and it is then what lets the game's own anisotropy through - which is `1 << maxAnisotropyBit`, \
             4 by default, rather than the 16 this renderer used to hardcode"
        );

        set_game_atlas_blend_mips(false);

        let stepped = game_atlas_sampler();

        assert_legal(&stepped, false);
        assert_eq!(
            stepped.mipmap_filter,
            wgpu::MipmapFilterMode::Nearest,
            "the switch is still what picks the filter"
        );
        assert_eq!(
            stepped.anisotropy_clamp, 1,
            "and the anisotropy has to follow it down. A player who only asked for the stepped mip filter \
             has not asked to lose anisotropic filtering, and it is a shame that this is the trade - but a \
             `Nearest` mip filter beside an anisotropy above 1 is `InvalidMipmapFilterModeWithAnisotropy`, \
             which ends the process rather than the filtering"
        );

        // **And the other two answers give no anisotropy at all**, which is the half of the option that has
        // nothing to do with the mip switch. `NONE` is the game's "Fast" and `RGSS` its "Fancy": the second
        // gets its filtering from the shader's four taps instead, which is the whole reason it does not want
        // a sampler that samples along the compressed axis.
        for method in [0, TEXTURE_FILTERING_RGSS] {
            set_texture_filtering(method);

            assert_legal(&game_atlas_sampler(), true);
            assert_eq!(
                game_atlas_sampler().anisotropy_clamp,
                1,
                "texture filtering {method} asks the sampler for nothing; RGSS is the shader's job"
            );
        }

        set_texture_filtering(TEXTURE_FILTERING_ANISOTROPIC);

        // Left on: it is the default, and a test that changed a global and left it changed would decide
        // the filters of whatever ran next.
        set_game_atlas_blend_mips(true);
    }
}

#[cfg(test)]
mod atlas_magnify_sampler_tests {
    use super::*;

    /// **The magnifying sampler is `Nearest` magnification, and the anisotropy is what it gives up.**
    ///
    /// This is the pair the shader picks between, so the thing worth pinning is that they really are two
    /// different samplers rather than two names for one: a `magnify` sampler that slowly acquired the
    /// anisotropic one's filters would leave the fragment stage selecting between identical fetches, which
    /// is the softness this exists to remove - and it would look exactly like the change not working.
    #[test]
    fn the_magnifying_sampler_is_nearest_and_the_minifying_one_is_not() {
        let magnify = atlas_magnify_sampler(wgpu::AddressMode::ClampToEdge);
        let minify = block_atlas_sampler(wgpu::AddressMode::ClampToEdge);

        assert_eq!(
            magnify.mag_filter,
            wgpu::FilterMode::Nearest,
            "`Nearest` magnification is the whole point: it is how the game magnifies, and a bilinear \
             filter at the magnification a block texture sees is what makes the near blocks soft"
        );
        assert_ne!(
            magnify.mag_filter, minify.mag_filter,
            "the two samplers are the two behaviours, so they cannot agree on magnification"
        );

        // **The constraint that forced two samplers to exist**, asserted on the descriptor rather than
        // left to `create_sampler`: wgpu refuses anisotropy above 1 without linear magnification and
        // minification, and on this path that refusal ends the process on the first frame.
        assert_eq!(
            magnify.anisotropy_clamp, 1,
            "anisotropy above 1 beside `Nearest` magnification is `InvalidFilterModeWithAnisotropy`"
        );

        // What it does *not* give up, because a magnified surface can still be squeezed by perspective and
        // the shader may hand this sampler either: the levels of the chain are still blended and the
        // coordinates are still clamped the way the game clamps them.
        assert_eq!(
            magnify.mipmap_filter,
            wgpu::MipmapFilterMode::Linear,
            "the mip chain is not this sampler's business to change"
        );
        assert_eq!(magnify.min_filter, wgpu::FilterMode::Linear);
        assert_eq!(magnify.address_mode_u, wgpu::AddressMode::ClampToEdge);
        assert_eq!(magnify.address_mode_v, wgpu::AddressMode::ClampToEdge);

        // And the address mode is the caller's, so the two atlases keep their own: this side's is packed
        // by this side and needs `Repeat`, the game's asks for `ClampToEdge`.
        let repeat = atlas_magnify_sampler(wgpu::AddressMode::Repeat);
        assert_eq!(repeat.address_mode_u, wgpu::AddressMode::Repeat);
        assert_eq!(repeat.mag_filter, wgpu::FilterMode::Nearest);
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

/// **The sampler both block atlases are drawn with**, and the only place either is built.
///
/// Two atlases are in play in one frame: every sprite is baked with the game's own coordinates and samples
/// the game's `blocks.png` (see `face_uses_game_atlas`), and this side's own packed copy is the fallback
/// for a sprite the game never told us about. Both go through here, so both take the same answer.
///
/// **`Linear` on all three filters plus anisotropic filtering, which is the game's own answer.**
/// `LevelRenderer` builds one sampler for its terrain - `CLAMP_TO_EDGE, FilterMode.LINEAR,
/// FilterMode.LINEAR, maxAnisotropy, OptionalDouble.empty()` - and hands it to both chunk groups. The
/// texture-filtering option only ever moves the anisotropy: `TextureFilteringMethod` is
/// `NONE`/`RGSS`/`ANISOTROPIC`, `FilterMode` has no off switch, and all three resolve to
/// `maxAnisotropy = 1` at the least.
///
/// **This took two round trips to arrive at, and both were caused by a fault elsewhere.**
///
/// The first: this side packed its own atlas and mipped the **whole packed sheet**, so a sprite's
/// transparent padding was averaged with its neighbours' and a cutout sprite had no fully opaque texel
/// above the second level. `Nearest` hid that - it never interpolates, so it never reads the damaged part -
/// and a bilinear filter over that chain is a blurred, half-transparent block, which is what one revision
/// of this shipped and what a player reported. With every sprite drawn from the game's atlas the chain
/// underneath is the game's - built per sprite, padded by `1 << mipLevel`, alpha coverage held across
/// levels - and the reason for `Nearest` went with it.
///
/// The second was tried next, and **the explanation that went with it was wrong.** With all three
/// filters linear and anisotropy on, the softness up close came back and so did nothing else that was
/// wanted - and the reason this function's note blamed for the shimmer turned out not to be it. The test
/// that settled it is the bias: a `-4` level-of-detail bias removes the shimmer and a `0` has it, and
/// **anisotropy does not move the chosen level at all** (it takes more samples *within* whichever level
/// the derivative names). A symptom that answers to the level cannot have anisotropy as its cause.
///
/// **wgpu will not have both.** `wgpu-core` refuses any `anisotropy_clamp` above 1 unless the min, mag
/// *and* mipmap filters are all `Linear` (`InvalidFilterModeWithAnisotropy`), and a validation error on
/// this path ends the process. So "crisp up close" and "anisotropic" really are a choice here - this is
/// the one trade in this sampler that is a property of the API rather than of the picture.
///
/// **What the shimmer is remains open**, and the honest state of it is this: the level is being chosen
/// too coarse for at least some surfaces, a bias cancels it, and a bias is the wrong shape of fix because
/// it moves every surface including the ones that are already right. That is why the filters are now the
/// game's and the bias is back at its neutral zero - so that the next measurement is of the level rather
/// than of a calibration standing in for it.
///
/// `address_mode` is `Repeat` for this side's atlas, whose coordinates come from its own packing, and
/// `ClampToEdge` for the game's, which is what the game asks for.
///
/// **One function, called twice, with `address_mode` the only thing that differs - and it is called from
/// `graph.rs` rather than copied there.** The two samplers used to be built in two places, and while they
/// happened to agree, nothing made them. The `atlas_base_mip_only` switch is what found it: it was read at
/// one call site only, so it clamped the game's atlas and left this renderer's at every level, which is the
/// opposite of what its name and its own comment say. A sampler constructed twice can disagree; one
/// constructed once cannot.
/// **The sampler a *minified* animated face is drawn with: `Linear` magnification, no anisotropy.**
///
/// This is the third of the three, and it exists for the one combination the other two cannot express.
///
///  * the anisotropic sampler magnifies `Linear`, which is right for a minified surface and is why it is
///    the obvious candidate - but `anisotropy_clamp` above 1 requires a `Linear` **mipmap** filter, which
///    is the switch `game_atlas_blend_mips`, and a player who turns that off would get an
///    `InvalidMipmapFilterModeWithAnisotropy` instead of a picture;
///  * and the magnifying sampler is `Nearest` magnification, which is right for a *magnified* surface and
///    actively wrong here: nearest magnification of a coarse level blows a single texel up over the whole
///    face, and a coarse level of an animated sprite is the running average of its animation - so the face
///    becomes one flat patch of that average, and the patch **steps** rather than fades as the frames
///    advance. A player described exactly that: "the bright spots look magnified, so a distant stretch of
///    lava reads as one big bright tile".
///
/// Linear magnification is what the game uses and what smooths those steps; giving up the anisotropy is
/// what this pays, and it is the cheaper half here - a magnifying filter is about how a texel is stretched,
/// and anisotropic filtering is about how several samples are taken along a *compressed* direction.
///
/// `mipmap_filter` stays `Linear` so that the mip switch does not have to be consulted: with an anisotropy
/// of 1 any mip filter is legal, which is what keeps this sampler out of the coupling that has already cost
/// two crashes.
pub fn atlas_animated_minified_sampler(
    address_mode: wgpu::AddressMode,
) -> wgpu::SamplerDescriptor<'static> {
    wgpu::SamplerDescriptor {
        anisotropy_clamp: 1,
        label: Some("wgpu-mc: a minified animated block atlas"),
        ..block_atlas_sampler(address_mode)
    }
}

/// **The sampler a magnified surface is drawn with: `Nearest`, with no anisotropic filtering.**
///
/// This exists because one sampler cannot be both things the terrain needs, and the reason is wgpu's
/// rather than the picture's: any `anisotropy_clamp` above 1 requires the min, mag *and* mipmap filters
/// to be linear (`wgpu-core`, `InvalidFilterModeWithAnisotropy`), so a sampler that filters
/// anisotropically cannot magnify the way the game does. The game has no such rule - its magnification is
/// `GL_NEAREST` and its minification is `GL_LINEAR_MIPMAP_LINEAR`, chosen independently - which is why
/// vanilla's blocks are crisp up close *and* stable at a grazing angle.
///
/// **Two samplers over two ranges is not a compromise, because the ranges do not overlap.** Anisotropic
/// filtering is a minification technique: it takes several samples along the direction a surface is
/// compressed and uses the derivative of the *other* direction to pick the level. A magnified surface has
/// nothing to compress - it is at level 0 by definition - so it needs none of that, and the anisotropy it
/// would be given is thrown away. Conversely `Nearest` magnification says nothing about a surface being
/// minified.
///
/// The shader decides between the two per fragment, on whether the coordinates' screen-space derivative is
/// larger than a texel. That test is exact rather than a heuristic: a derivative below one texel per pixel
/// *is* magnification, and that is the definition of it.
///
/// `min_filter` and `mipmap_filter` stay linear so that a surface right at the boundary has a sane answer
/// either way; `anisotropy_clamp` is 1 because wgpu would refuse anything else beside `Nearest`.
pub fn atlas_magnify_sampler(address_mode: wgpu::AddressMode) -> wgpu::SamplerDescriptor<'static> {
    wgpu::SamplerDescriptor {
        mag_filter: wgpu::FilterMode::Nearest,
        // **Set here and not inherited, which is not a tidiness.** `..block_atlas_sampler(..)` below copies
        // every field this does not name, and the base carries `anisotropy_clamp: 16` - so a version of
        // this function that named only `mag_filter` asked wgpu for `Nearest` magnification *with* an
        // anisotropy of 16, which is `InvalidFilterModeWithAnisotropy`, which on this path ends the
        // process on the first frame. That is the same trap `game_atlas_sampler` fell into one round
        // earlier by overriding a mip filter and not the anisotropy beside it, and the test below is what
        // caught it both times.
        anisotropy_clamp: 1,
        label: Some("wgpu-mc: a magnified block atlas"),
        ..block_atlas_sampler(address_mode)
    }
}

pub fn block_atlas_sampler(address_mode: wgpu::AddressMode) -> wgpu::SamplerDescriptor<'static> {
    wgpu::SamplerDescriptor {
        address_mode_u: address_mode,
        address_mode_v: address_mode,
        address_mode_w: address_mode,
        // **All three filters linear, plus anisotropic filtering: the game's own sampler.**
        //
        // `LevelRenderer` builds `CLAMP_TO_EDGE, FilterMode.LINEAR, FilterMode.LINEAR, maxAnisotropy` and
        // hands it to both chunk groups, and the texture-filtering option only ever moves the anisotropy.
        // So this is what the game's terrain is drawn with, and matching it is the default position: every
        // deviation has to be paid for somewhere, and the two that were tried here both were.
        //
        //   * `Nearest` magnification was tried twice. The first time it was hiding a broken mip chain of
        //     this side's own making; the second time it was kept deliberately, because magnifying a
        //     16-texel block texture by a hundred is what `Linear` makes soft - and it *is* soft, which is
        //     the trade rather than a fault. What it costs on the other side is that anisotropic filtering
        //     becomes unreachable (see `anisotropy_clamp`);
        //   * a `Nearest` mip filter was tried as a fix for the fluid shimmer and did not fix it, which is
        //     how the shimmer was traced to the level of detail rather than to the blend between levels.
        //
        // **What the shimmer's real cause turned out to be is still open**, and it is worth saying here
        // because it is the reason this sampler keeps being revisited: a `-4` level-of-detail bias removes
        // it, and a bias cannot be the answer because the atlas is not a fixed size - the game's stitcher
        // packs `blocks.png` at 2048x2048 with 5 mip levels in one run and 1024x1024 with 3 in the next, so
        // one constant cannot mean the same thing in both. Until that is understood the filters are the
        // game's and the bias is the thing under suspicion.
        // **`Linear` magnification: the game's own answer, and the softness is its cost rather than a
        // fault.**
        //
        // What it produces is a bilinear ramp between neighbouring texels, and at the magnification a block
        // texture actually sees - a 16-texel sprite across a hundred or more screen pixels, so each texel is
        // twelve to twenty-five of them - that ramp is most of the picture. There is no filter setting that
        // makes it crisp while it stays bilinear, so this stays soft on purpose for now: the fix is not
        // known, and `Nearest` was measured to be the wrong shape of answer.
        //
        // **The hypothesis that it was a texel-alignment fault was tested and is false.** The reasoning was
        // that a face's coordinates run edge to edge, so a sample lands on the boundary between two texels
        // rather than on one, and two texels at every point would be softer than one. So the coordinates
        // were shifted by half a texel, per atlas, from the renderer - and the picture did not change. Which
        // is what the arithmetic says it should do: half a texel moves *where* inside a texel the sample
        // lands, and magnification interpolates between texel centres either way. A bilinear filter is soft
        // because it interpolates, not because of where it is asked.
        //
        // `Nearest` was tried here twice before this. Each time it was crisp and each time the thing it
        // costs came back: wgpu refuses anisotropy unless all three filters are linear (see below), so a
        // `Nearest` magnification means no anisotropic filtering, and the fluid shimmer lives on the
        // minification side that anisotropy is what fixes.
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        // A blend between the two levels it lands between, so a surface crossing a level boundary does
        // not snap. The game's own choice: `OptionalDouble.empty()` for the maximum level is "every
        // level the texture has". This is the minification side and stays linear with it.
        mipmap_filter: wgpu::MipmapFilterMode::Linear,
        // **The switch belongs here, on both samplers, because it is a statement about both atlases.**
        // It was applied by hand to the game's sampler alone, in `graph.rs`; see the note above for what
        // that cost. Switching it on throws the chain away for whichever atlas is bound - one run with
        // it, one without, and the difference is the chain rather than one atlas.
        lod_min_clamp: 0.0,
        // The base-level-only switch, which is the blunt form of the same statement [`game_atlas_sampler`]
        // makes precisely: it clamps the chosen level outright, so *no* face reads the chain. It stays as
        // the comparison a player can flip - see `ATLAS_BASE_MIP_ONLY` - and it is honoured by both
        // samplers because it is a statement about the chain rather than about one atlas.
        lod_max_clamp: if crate::render::graph::atlas_base_mip_only() {
            0.0
        } else {
            u32::MAX as f32
        },
        // **16, the game's own value, which all three linear filters make legal.** Any `anisotropy_clamp`
        // above 1 requires exactly that - `wgpu-core` returns `InvalidFilterModeWithAnisotropy` otherwise,
        // and a validation error on this path ends the process rather than warning - so the anisotropic
        // filtering the game asks for and a `Nearest` magnification are a choice in wgpu rather than a
        // combination. Magnification is `Linear`, so there is nothing to trade and this is what stops a
        // floor seen at a grazing angle from being sampled at the coarse level its worst derivative names.
        //
        // **It is the thing `game_atlas_sampler` has to keep in step**, because that function overrides the
        // mip filter: anisotropy above 1 with a `Nearest` mip filter is the same fatal error, and it reached
        // a running client once for exactly that reason. See there.
        //
        // The ceiling: `wgpu-hal`'s `MAX_ANISOTROPY` is 16 and wgpu-core clamps to `[1, 16]` before the
        // backend sees the value, so a larger number would be silently reduced. There is no query for what
        // the driver really supports, because `Limits` has no anisotropy field; the device this was measured
        // on does report `DownlevelFlags::ANISOTROPIC_FILTERING`, and the game asks for the same number.
        // **`1` for this run: the anisotropy is the one thing that only touches the sides**, and a player
        // traced the overexposure to the round the dual filter arrived in - which is also the round this
        // became 16. A grazing-angle surface is the only place anisotropic filtering does anything, and the
        // sides are exactly that, so it is the first candidate rather than a guess.
        anisotropy_clamp: 1,
        compare: None,
        ..Default::default()
    }
}

/// **The level-of-detail bias the terrain shaders give their two block-atlas fetches**, in mip levels.
///
/// Zero is the honest default: `textureSampleBias(.., 0.0)` is the same fetch as `textureSample`, so the
/// renderer's normal state is the one vanilla has. A positive value samples a coarser level and a
/// negative one a finer level, which is the whole of what this is for - it is a **diagnostic** for the
/// question "is the level of detail chosen correctly", and it is a value rather than a constant because
/// **a constant cannot answer that question**.
///
/// That is not a hypothetical: this was `const ATLAS_LOD_BIAS: f32 = 0.0` in the two terrain shaders for
/// a round, and with it "the bias does nothing" and "the bias never reached the GPU" produce the same
/// picture. Moving it meant editing a shader, having it copied into the build directory, and restarting
/// the client - because **nothing watches the shader files** (`mark_pipelines_stale` is called on atlas
/// and lightmap handover only) - so two rounds of "I moved it and nothing changed" could not tell the
/// two apart. As an immediate the draw hands the value over, so a setting moves it and the next frame
/// uses it.
///
/// Held as the `f32`'s bits in an `AtomicU32`, because there is no atomic float - and read with
/// `from_bits` at the point of use.
static ATLAS_LOD_BIAS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// See [`ATLAS_LOD_BIAS`].
pub fn atlas_lod_bias() -> f32 {
    f32::from_bits(ATLAS_LOD_BIAS.load(std::sync::atomic::Ordering::Relaxed))
}

/// Sets [`ATLAS_LOD_BIAS`]. Takes effect on the next draw, which is the point of it being an immediate.
pub fn set_atlas_lod_bias(bias: f32) {
    ATLAS_LOD_BIAS.store(bias.to_bits(), std::sync::atomic::Ordering::Relaxed);
}

/// **Which of the game's three texture-filtering methods is in force**, as `TextureFilteringMethod`'s own id:
/// `0` is `NONE` (the game's "Fast"), `1` is `RGSS` ("Fancy") and `2` is `ANISOTROPIC` ("Fabulous").
///
/// The game's own enum, and its own numbering, rather than a switch of this side's: the value is read from
/// the option every frame and written straight into an immediate, so a translation layer between the two
/// would be one more thing that can disagree with the game about what a number means.
///
/// **This is the one input that decides how a surface is sampled**, and the three answers are genuinely three
/// different algorithms rather than one with a knob:
///
///  * `ANISOTROPIC` gives the sampler an `anisotropy_clamp` of the *option's* value
///    (`Options#maxAnisotropyValue`, `1 << maxAnisotropyBit`, so 4 by default) and leaves the ordinary
///    graded fetch to the hardware;
///  * `RGSS` leaves the sampler isotropic and puts the game's rotated-grid supersampling in the shader -
///    four taps on a rotated grid at an explicitly computed level, which no sampler can be asked for;
///  * `NONE` does neither, and is a plain fetch at whatever level the hardware picks.
///
/// See `wgpu_mc:shaders/terrain.wgsl`, whose `sampleNearest` and `sampleRGSS` are ports of the game's own
/// `terrain.fsh`, and `LevelRenderer`'s `chunkLayerSampler` for the sampler half.
static TEXTURE_FILTERING: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// See [`TEXTURE_FILTERING`].
pub fn texture_filtering() -> u32 {
    TEXTURE_FILTERING.load(std::sync::atomic::Ordering::Relaxed)
}

/// Sets [`TEXTURE_FILTERING`]. Pushed by the JVM, once per frame, from the game's own option.
pub fn set_texture_filtering(method: u32) {
    TEXTURE_FILTERING.store(method, std::sync::atomic::Ordering::Relaxed);
}

/// **The game's `TextureFilteringMethod.RGSS` id**, which is what the shader's `use_rgss` is compared
/// against. Named so the shader's immediate is not a bare `1` in this file.
pub const TEXTURE_FILTERING_RGSS: u32 = 1;

/// **The game's `TextureFilteringMethod.ANISOTROPIC` id**, which is the one that raises the sampler's
/// anisotropy. See [`TEXTURE_FILTERING`].
pub const TEXTURE_FILTERING_ANISOTROPIC: u32 = 2;

/// **The `anisotropy_clamp` the game''s own terrain sampler would be built with**, which is the whole of
/// what the `textureFiltering` option means to a sampler.
///
/// `LevelRenderer` builds it as
///
/// ```java
/// int maxAnisotropy = this.optionsRenderState.textureFiltering == TextureFilteringMethod.ANISOTROPIC
///     ? this.optionsRenderState.maxAnisotropyValue
///     : 1;
/// ```
///
/// and `Options#maxAnisotropyValue` is `1 << maxAnisotropyBit` clamped to what the device reports - **4 by
/// default**, from a bit of 2. This renderer used a flat 16, which is the hardware's usual maximum and four
/// times what the game asks for on its own highest setting.
///
/// **4 and not 16 has a consequence worth naming**: `anisotropy_clamp` above 1 requires the min, mag *and*
/// mipmap filters to be `Linear` (`wgpu-core`'s three separate validations, see
/// `the_game_atlas_never_asks_for_anisotropy_without_linear_filters`), and `game_atlas_blend_mips` is a
/// player's switch that moves the mip filter. So the value is read per sampler creation rather than baked
/// in, and the coupling is spelled out at each of the two places it is used.
fn terrain_anisotropy() -> u16 {
    if texture_filtering() == TEXTURE_FILTERING_ANISOTROPIC {
        // The game's default bit is 2, so its own answer is 4. `wgpu` clamps to what the adapter supports.
        GAME_ANISOTROPY
    } else {
        1
    }
}

/// The game's own anisotropy, `Options#maxAnisotropyValue` at its default bit of 2: `1 << 2`.
pub const GAME_ANISOTROPY: u16 = 4;
/// **The sampler for the game's own block atlas**, which is the one every animation is drawn from.
///
/// `block_atlas_sampler` with one field decided by a switch: whether mip levels are **blended**. On - the
/// default, and what anisotropic filtering requires - this is the game's own sampler. Off writes the
/// chosen level directly, which was an attempt at the fluid shimmer and is now the second half of the
/// comparison rather than the answer: the shimmer turned out to come from the *absence of anisotropic
/// filtering*, because the level is chosen from the larger of two screen-space derivatives and a grazing
/// surface's two are wildly unequal. See `block_atlas_sampler` for the whole of that.
///
/// ## Why that one field is different, and what was measured
///
/// With the `atlas_base_mip_only` switch on - which clamps `lod_max_clamp` to `0.0`, so the sampler can
/// only ever return level 0 - **the fluid flicker goes away**. With it off, it comes back. That is the
/// measurement this is built on, and it says the flicker involves the levels above the base one.
///
/// There are two things a level above the base can contribute, and they need separating because only one
/// of them is fixable here:
///
///  * **the level's own content**, which is current. `TextureAtlas#uploadAnimationFrames` walks every
///    level and draws the due frame into each through that level's own view and its own UBO
///    (`animationState.getDrawUbo(level)` is `spriteUbosByMip[level]`, one per entry of `byMipLevel`),
///    so no level is staler than any other. This was checked before anything was changed, and it is why
///    "the higher levels hold no live frame" is not the answer;
///  * **the blend between two of them**, which is what `MipmapFilterMode::Linear` does - and **the two
///    levels are not two resolutions of one image.** An animated sprite is a *scrolling* pattern:
///    `water_flow` and `lava_flow` move their sample point within the frame, so level *n* and level
///    *n+1* hold the same frame at the same instant sampled at two different rates, and blending them
///    mixes two phases of a moving pattern. A static sprite's levels are a consistent pyramid and blend
///    cleanly; a moving one's do not.
///
/// So the blend is what goes, not the chain. **This is a hypothesis with a measurement behind it rather
/// than a proof** - the measurement says "level 0 is stable, something above it is not", and this removes
/// the one mechanism that mixes levels without discarding them. If the flicker survives it, the
/// remaining reading is that a single coarse level of a scrolling sprite is itself unstable at this
/// scale, and the answer would be a level clamp per sprite rather than a filter.
///
/// What `Nearest` between levels costs, for the run that selects it: a static sprite crossing a level
/// boundary steps rather than fades, because there is no blend. That is why it is not the default.
///
/// ## The chain is not clamped, and that is not an oversight
///
/// Clamping `lod_max_clamp` to `0.0` - "sample the level the game is animating" - is the obvious thing to
/// try and it is wrong for a reason worth keeping written down: **the game does not animate one level.**
///
/// ```java
/// // TextureAtlas#uploadAnimationFrames
/// for (int level = 0; level <= this.maxMipLevel; level++) {
///     try (RenderPass pass = ...createRenderPass(() -> "Animate " + this.location,
///                                                this.mipViews[level], OptionalInt.empty())) {
///         ...
///         animationState.drawToAtlas(pass, animationState.getDrawUbo(level));
/// ```
///
/// `getDrawUbo(level)` is `spriteUbosByMip[level]`, and there is one UBO per entry of `byMipLevel` - so
/// every level receives the frame in the same call and no level is staler than any other. A clamp would
/// therefore clamp to *every* level, which is no clamp at all; what it really does is throw the chain
/// away, and a distant lump of lava would then sample one texel of a 16x16 sprite, which is the aliasing
/// the chain exists to prevent.
///
/// It remains reachable as the `atlas_base_mip_only` switch, honoured inside [`block_atlas_sampler`] for
/// both atlases, because the case for it is a picture rather than an argument.
pub fn game_atlas_sampler() -> wgpu::SamplerDescriptor<'static> {
    // **The mip filter and the anisotropy travel together, and that is not tidiness - it is the difference
    // between a sampler and a dead client.**
    //
    // `wgpu-core` checks the min, mag **and** mipmap filters separately, three checks rather than one, and
    // returns `InvalidMipmapFilterModeWithAnisotropy` for a `Nearest` mip filter beside an anisotropy above
    // 1. `create_sampler` answering `Err` on this path runs the panic hook and ends the process - and that is
    // exactly what a player saw the moment they turned `game_atlas_blend_mips` off: the option applied, and
    // two seconds later the client died inside `create_sampler`.
    //
    // They were decoupled for a round on the reasoning that magnification was `Linear` by then and "a
    // `Nearest` *mip* filter is legal with anisotropy" - which is false, and false in the direction that
    // crashes. The test below pins **all three** filters now, because reading two of the three checks and
    // concluding from them is precisely how this was got wrong twice.
    let blend = game_atlas_blend_mips();

    wgpu::SamplerDescriptor {
        mipmap_filter: if blend {
            wgpu::MipmapFilterMode::Linear
        } else {
            wgpu::MipmapFilterMode::Nearest
        },
        // **And the anisotropy has to follow it down, which is not tidiness but the difference between a
        // sampler and a crash.** `wgpu-core` checks the min, mag *and* mipmap filters separately - three
        // checks, not one - and returns `InvalidMipmapFilterModeWithAnisotropy` for a `Nearest` mip filter
        // beside an anisotropy above 1. `create_sampler` returning `Err` on this path runs the panic hook
        // and ends the process, which is exactly what a player saw the moment they turned this switch off:
        // the option applied, and the client died two seconds later in `create_sampler`.
        //
        // The pair was decoupled for a round on the reasoning that magnification was `Linear` by then and
        // "a `Nearest` *mip* filter is legal with anisotropy" - which is false, and false in the direction
        // that crashes. The test below pins all three filters now, because reading two of the three checks
        // and concluding from them is how this was got wrong.
        anisotropy_clamp: if blend { terrain_anisotropy() } else { 1 },
        label: Some("wgpu-mc: the game's block atlas"),
        ..block_atlas_sampler(wgpu::AddressMode::ClampToEdge)
    }
}

/// **Whether [`game_atlas_sampler`] blends between mip levels**, which is the `game_atlas_blend_mips`
/// setting.
///
/// A static rather than a parameter because the sampler is built when the render graph is, from a place
/// that has the settings but not this decision - and because the two callers that rebuild the graph are
/// both already settings-driven.
static GAME_ATLAS_BLEND_MIPS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

/// See [`GAME_ATLAS_BLEND_MIPS`].
pub fn game_atlas_blend_mips() -> bool {
    GAME_ATLAS_BLEND_MIPS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Sets [`GAME_ATLAS_BLEND_MIPS`]. The caller rebuilds the graph; see `debug::apply`.
pub fn set_game_atlas_blend_mips(blend: bool) {
    GAME_ATLAS_BLEND_MIPS.store(blend, std::sync::atomic::Ordering::Relaxed);
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
        let sampler = wgpu_state
            .device
            .create_sampler(&block_atlas_sampler(wgpu::AddressMode::Repeat));

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
