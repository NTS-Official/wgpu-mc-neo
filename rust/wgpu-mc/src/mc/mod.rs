//! Rust implementations of minecraft concepts that are important to us.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use chunk::SectionStorage;
use glam::{IVec2, ivec2};
use indexmap::map::IndexMap;
use minecraft_assets::schemas;
use minecraft_assets::schemas::blockstates::multipart::StateValue;
use parking_lot::{Mutex, RwLock};

use crate::mc::entity::{BundledEntityInstances, Entity};
use crate::mc::resource::ResourceProvider;
use crate::render::atlas::{Atlas, TextureManager};
use crate::render::pipeline::BLOCK_ATLAS;
use crate::util::BindableBuffer;
use crate::{Gpu, WmRenderer};

use self::block::ModelMesh;
use self::resource::ResourcePath;

pub mod block;
pub mod chunk;
pub mod direction;
pub mod entity;
pub mod resource;
/// Take in a block name (not a [ResourcePath]!) and optionally a variant state key, e.g. "facing=north" and format it some way
/// for example, `minecraft:anvil[facing=north]` or `Block{minecraft:anvil}[facing=north]`
pub type BlockVariantFormatter = dyn Fn(&str, Option<&str>) -> String;

pub struct BlockManager {
    /// This maps block state keys to either a [VariantMesh] or a [Multipart] struct. How the keys are formatted
    /// is defined by the user of wgpu-mc. For example `Block{minecraft:anvil}[facing=west]` or `minecraft:anvil#facing=west`
    pub blocks: IndexMap<String, Block>,
    /// What every block *state* the JVM described says about the faces around it, keyed by the packed
    /// [`BlockstateKey`](block::BlockstateKey) - which is the only thing a baked section carries about
    /// a block, so it is the only key this table can use.
    ///
    /// It is filled when the block registry is built (`cacheBlockStates`, on the JNI side) from the
    /// masks `RegistryMixin` computes for each state, and it is what the terrain baker's face test
    /// reads: a neighbour's *state* decides whether a face is drawn, not its model - see
    /// `chunk::face_is_hidden`.
    pub face_flags: HashMap<u32, block::FaceFlags>,
}

impl BlockManager {
    pub fn new() -> Self {
        Self {
            blocks: IndexMap::new(),
            face_flags: HashMap::new(),
        }
    }
}

impl Default for BlockManager {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub enum Block {
    Multipart(Multipart),
    Variants(IndexMap<Vec<(String, StateValue)>, Vec<Arc<ModelMesh>>>),
}

impl Block {
    pub fn get_model(&self, key: u16, _seed: u8) -> Option<Arc<ModelMesh>> {
        Some(match &self {
            Block::Multipart(multipart) => multipart.keys.read().get_index(key as usize)?.1.clone(),
            //TODO, random variant selection through weight and seed
            Block::Variants(variants) => variants.get_index(key as usize)?.1[0].clone(),
        })
    }

    pub fn get_model_by_key<'a>(
        &self,
        key: impl IntoIterator<Item = (&'a str, &'a StateValue)> + Clone,
        resource_provider: &dyn ResourceProvider,
        block_atlas: &Atlas,
        //TODO use this
        _seed: u8,
    ) -> Option<(Arc<ModelMesh>, u16)> {
        let key_map: HashMap<&str, &StateValue> = key.clone().into_iter().collect();

        let key_string = key
            .clone()
            .into_iter()
            .map(|(key, value)| {
                format!(
                    "{}={}",
                    key,
                    match value {
                        StateValue::Bool(bool) =>
                            if *bool {
                                "true"
                            } else {
                                "false"
                            },
                        StateValue::String(string) => string,
                    }
                )
            })
            .collect::<Vec<String>>()
            .join(",");

        match &self {
            Block::Multipart(multipart) => {
                {
                    if let Some(full) = multipart.keys.read().get_full(&key_string) {
                        return Some((full.2.clone(), full.0 as u16));
                    }
                }

                let mesh = multipart.generate_mesh(key, resource_provider, block_atlas)?;

                let mut multipart_write = multipart.keys.write();
                multipart_write.insert(key_string, mesh.clone());

                Some((mesh, multipart_write.len() as u16 - 1))
            }
            Block::Variants(variants) => {
                // A variant whose models all failed to bake is skipped rather than indexed into:
                // `variants` can hold an empty mesh list now that a bad model is dropped instead of
                // taking the registry with it (see `bake_blocks`).
                let full =
                    variants
                        .iter()
                        .enumerate()
                        .find(|(_, (variant_key, model_meshes))| {
                            !model_meshes.is_empty()
                                && variant_key.iter().all(
                                    |(variant_property_key, variant_property_value)| {
                                        key_map
                                            .get(&variant_property_key[..])
                                            .map_or(false, |v| v == &variant_property_value)
                                    },
                                )
                        })?;

                Some((full.1.1.first()?.clone(), full.0 as u16))
            }
        }
    }
}

#[derive(Debug)]
pub struct Multipart {
    pub cases: Vec<schemas::blockstates::multipart::Case>,
    pub keys: RwLock<IndexMap<String, Arc<ModelMesh>>>,
}

impl Multipart {
    /// The mesh a multipart block's cases add up to, or `None` if one of them cannot be baked.
    ///
    /// A multipart mesh is generated on demand - once per state a player actually looks at - so a
    /// model that fails here fails in the middle of the game rather than during the block cache, and
    /// `None` lets the caller fall back to the block it uses for a state with no model.
    pub fn generate_mesh<'a>(
        &self,
        key: impl IntoIterator<Item = (&'a str, &'a schemas::blockstates::multipart::StateValue)>
        + Clone,
        resource_provider: &dyn ResourceProvider,
        block_atlas: &Atlas,
    ) -> Option<Arc<ModelMesh>> {
        let apply_variants = self.cases.iter().filter_map(|case| {
            if case.applies(key.clone()) {
                Some(case.apply.models())
            } else {
                None
            }
        });

        match ModelMesh::bake(
            apply_variants.into_iter().flatten(),
            resource_provider,
            block_atlas,
        ) {
            Ok(mesh) => Some(Arc::new(mesh)),
            Err(err) => {
                log::warn!(
                    "wgpu-mc: a multipart model could not be baked ({err:?}); the state is drawn as \
                     bedrock, the same as one with no model at all"
                );
                None
            }
        }
    }
}

pub enum MultipartOrMesh {
    Multipart(Arc<Multipart>),
    Mesh(Arc<ModelMesh>),
}

/// Multipart models are generated dynamically as they can be too complex
pub struct BlockInstance {
    pub render_settings: block::RenderSettings,
    pub block: MultipartOrMesh,
}

#[derive(Default, Clone)]
pub struct SkyState {
    pub color: [f32; 3],
    pub angle: f32,
    pub brightness: f32,
    pub star_shimmer: f32,
    pub moon_phase: i32,
}

#[derive(Default, Clone)]
pub struct RenderEffectsData {
    pub fog_start: f32,
    pub fog_end: f32,
    pub fog_shape: f32,
    pub fog_color: [f32; 4],
    pub color_modulator: [f32; 4],
    pub dimension_fog_color: [f32; 4],
}

/// What the arena's buffer is used for: written by the section feed, read as vertices and indices by
/// the graph's draws, and bound as a storage buffer by the shader that fetches a section's quads.
///
/// One constant rather than the same four flags at both creation sites - the pool and the buffer are
/// resized together by [`Scene::set_arena_slots`], and the second of those sites is where a missing
/// flag would be a validation error at the first draw.
const ARENA_USAGE: wgpu::BufferUsages = wgpu::BufferUsages::COPY_DST
    .union(wgpu::BufferUsages::VERTEX)
    .union(wgpu::BufferUsages::STORAGE)
    .union(wgpu::BufferUsages::INDEX);

pub struct Scene {
    pub section_storage: RwLock<SectionStorage>,
    pub camera_section_pos: RwLock<IVec2>,
    /// The camera section the arena was last trimmed against.
    ///
    /// Trimming walks every section the arena holds, so it happens when the camera crosses into
    /// another section rather than once per frame: a frame where the camera stayed put has nothing to
    /// free, and the walk would cost more than the frames it ran on.
    pub trimmed_section_pos: RwLock<IVec2>,
    /// The arena's buffer, and the bind group that reaches it.
    ///
    /// Swapped rather than fixed, because the arena is sized from the render distance the game
    /// reports - and that report arrives when a world is joined, long after the scene was created. An
    /// `ArcSwap` so the render thread reads it per frame without a lock, and so a frame that is still
    /// drawing from the old buffer keeps it alive: the recorded pass holds its own reference to it.
    /// See [`Scene::set_arena_slots`], which is the only thing that replaces it.
    pub chunk_buffer: ArcSwap<BindableBuffer>,

    pub indirect_buffer: Arc<wgpu::Buffer>,

    pub entity_instances: Mutex<HashMap<String, BundledEntityInstances>>,
    pub sky_state: ArcSwap<SkyState>,

    pub render_effects: ArcSwap<RenderEffectsData>,

    pub depth_texture: RwLock<wgpu::Texture>,
}

impl Scene {
    pub fn new(wm: &WmRenderer, framebuffer_size: wgpu::Extent3d) -> Self {
        let indirect_buffer = wm.gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: 4 * 5 * 10000,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::INDIRECT,
            mapped_at_creation: false,
        });
        // Sized for a large render distance up front, so that a session which never reports one still
        // works, and resized to the one the game reports with [`Scene::set_arena_slots`] - which
        // happens when a world is joined, before anything has been baked. A buffer that is too large
        // is video memory; one that is too small is sections that cannot be baked.
        Self {
            section_storage: RwLock::new(SectionStorage::new(crate::mc::chunk::ARENA_SLOTS)),
            camera_section_pos: RwLock::new(ivec2(0, 0)),
            trimmed_section_pos: RwLock::new(ivec2(i32::MAX, i32::MAX)),
            chunk_buffer: ArcSwap::from_pointee(BindableBuffer::new_deferred(
                wm,
                crate::mc::chunk::ARENA_SLOTS as u64 * 4,
                ARENA_USAGE,
                "ssbo",
            )),
            indirect_buffer: Arc::new(indirect_buffer),

            entity_instances: Default::default(),
            sky_state: Default::default(),
            render_effects: Default::default(),
            depth_texture: wm
                .gpu
                .device
                .create_texture(&wgpu::TextureDescriptor {
                    label: None,
                    size: framebuffer_size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Depth32Float,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    view_formats: &[],
                })
                .into(),
        }
    }

    /// Sizes the arena to a pool of `slots` u32 slots, and answers whether it took.
    ///
    /// The pool and the buffer are one thing and are therefore set in one place: the pool is what the
    /// allocator hands ranges out of, the buffer is where those ranges live, and a pool larger than
    /// the buffer is a write past the end of it. Sizing them apart - which is what this replaced - is
    /// how the buffer came to be fixed at the largest render distance while the pool followed the
    /// game's: the memory was spent either way, and only the *capacity* moved.
    ///
    /// It only takes while the arena is empty, for the reason `SectionStorage::set_pool` gives: a
    /// range allocator cannot be resized under live allocations, and the bytes in the buffer are the
    /// only copy of the geometry that is on screen. `false` is a report that arrived after the world
    /// had been meshed - the arena keeps the size it was built with, and the caller says so.
    pub fn set_arena_slots(&self, wm: &WmRenderer, slots: u32) -> bool {
        if !self.section_storage.write().set_pool(slots) {
            return false;
        }

        // The old buffer goes when the last frame that recorded a draw from it is done with it: the
        // pass holds its own reference, and this drops ours.
        self.chunk_buffer.store(Arc::new(BindableBuffer::new_deferred(
            wm,
            slots as u64 * 4,
            ARENA_USAGE,
            "ssbo",
        )));

        true
    }

    pub fn resize_depth_texture(&self, wm: &WmRenderer, width: u32, height: u32) {
        self.depth_texture.read().destroy();
        *self.depth_texture.write() = wm.gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth32Float,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        })
    }
}

/// Minecraft-specific state and data structures go in here
pub struct MinecraftState {
    pub block_manager: RwLock<BlockManager>,

    pub entity_models: RwLock<HashMap<String, Arc<Entity>>>,

    pub resource_provider: Arc<dyn ResourceProvider>,
    pub texture_manager: TextureManager,

    pub animated_block_buffer: ArcSwap<Option<wgpu::Buffer>>,
    pub animated_block_bind_group: ArcSwap<Option<wgpu::BindGroup>>,
}

impl MinecraftState {
    #[must_use]
    pub fn new(wgpu_state: &Gpu, resource_provider: Arc<dyn ResourceProvider>) -> Self {
        MinecraftState {
            entity_models: RwLock::new(HashMap::new()),

            texture_manager: TextureManager::new(wgpu_state),

            block_manager: RwLock::new(BlockManager::new()),
            resource_provider,

            animated_block_buffer: ArcSwap::new(Arc::new(None)),
            animated_block_bind_group: ArcSwap::new(Arc::new(None)),
        }
    }

    /// Bake blocks from their blockstates
    ///
    /// # Example
    ///
    ///```ignore
    /// # use wgpu_mc::mc::MinecraftState;
    /// # use wgpu_mc::mc::resource::ResourcePath;
    /// # use wgpu_mc::WmRenderer;
    ///
    /// # let minecraft_state: MinecraftState;
    /// # let wm: WmRenderer;
    ///
    /// minecraft_state.bake_blocks(
    ///     &wm,
    ///     [("minecraft:anvil", &ResourcePath("minecraft:blockstates/anvil.json".into()))]
    /// );
    /// ```
    pub fn bake_blocks<'a>(
        &self,
        wm: &WmRenderer,
        block_states: impl IntoIterator<Item = (impl AsRef<str>, &'a ResourcePath)>,
    ) {
        let mut block_manager = self.block_manager.write();
        let atlases = self.texture_manager.atlases.read();
        // Not a panic: nothing registers a block atlas in this build yet - the Fabric module's atlas
        // loader was never ported - and a `#[jni_fn]` frame cannot unwind, so a missing atlas used to
        // be the JVM aborting on the block cache thread. Say what is missing and leave the registry
        // empty; everything that needs block models (the Rust terrain baker) will find it empty and
        // do nothing.
        let Some(block_atlas) = atlases.get(BLOCK_ATLAS) else {
            log::error!(
                "wgpu-mc: no block atlas is registered, so block models cannot be baked - the Rust \
                 terrain path needs one (see `bake_blocks`)"
            );
            return;
        };

        //Figure out which block models there are
        block_states
            .into_iter()
            .for_each(|(block_name, block_state)| {
                // One missing or malformed blockstate file must not take the game down: this runs on
                // a background thread whose panics abort the JVM.
                let Some(json) = self.resource_provider.get_string(block_state) else {
                    log::warn!("wgpu-mc: {} has no blockstate file; skipping it", block_state.0);
                    return;
                };

                let blockstates: schemas::BlockStates = match serde_json::from_str(&json) {
                    Ok(blockstates) => blockstates,
                    Err(err) => {
                        log::warn!("wgpu-mc: {} could not be read: {err}", block_state.0);
                        return;
                    }
                };

                let block = match &blockstates {
                    schemas::BlockStates::Variants { variants } => {
                        let meshes: IndexMap<Vec<(String, StateValue)>, Vec<Arc<ModelMesh>>> =
                            variants
                                .iter()
                                .filter_map(|(variant_id, variant)| {
                                    let key_iter = if !variant_id.is_empty() {
                                        variant_id
                                            .split(',')
                                            .filter_map(|kv_pair| {
                                                let mut split = kv_pair.split('=');
                                                if kv_pair.is_empty() {
                                                    return None;
                                                }

                                                Some((
                                                    split.next().unwrap().to_string(),
                                                    match split.next().unwrap() {
                                                        "true" => StateValue::Bool(true),
                                                        "false" => StateValue::Bool(false),
                                                        other => StateValue::String(other.into()),
                                                    },
                                                ))
                                            })
                                            .collect::<Vec<_>>()
                                    } else {
                                        vec![]
                                    };

                                    // A variant whose model cannot be baked is dropped rather than
                                    // unwrapped: one bad model in one blockstate file used to abort
                                    // the whole registry, which is the difference between a block
                                    // that does not draw and no terrain at all. The block itself
                                    // stays registered, with the variants that did bake.
                                    let mut meshes = Vec::with_capacity(variant.models().len());
                                    for variation in variant.models() {
                                        match ModelMesh::bake(
                                            std::slice::from_ref(variation),
                                            &*self.resource_provider,
                                            block_atlas,
                                        ) {
                                            Ok(mesh) => meshes.push(Arc::new(mesh)),
                                            Err(err) => {
                                                log::warn!(
                                                    "wgpu-mc: {} variant {variant_id} could not be \
                                                     baked ({err:?}); skipping it",
                                                    block_name.as_ref()
                                                );
                                                return None;
                                            }
                                        }
                                    }

                                    Some((key_iter, meshes))
                                })
                                .collect();

                        Block::Variants(meshes)
                    }
                    schemas::BlockStates::Multipart { cases } => Block::Multipart(Multipart {
                        cases: cases.clone(),
                        keys: RwLock::new(IndexMap::new()),
                    }),
                };

                block_manager
                    .blocks
                    .insert(String::from(block_name.as_ref()), block);
            });

        // The fluids' own textures. Lava and water have no block model to name them, so this is the
        // only place they can enter the atlas - and the fluid mesher draws them, so without this the
        // fluids would be baked untextured. Allocated before the upload, which is what puts them in
        // the texture the pass samples.
        for name in chunk::FLUID_TEXTURES {
            let path = ResourcePath(format!("minecraft:block/{name}"));

            if block_atlas.uv_map.read().contains_key(&path) {
                continue;
            }

            let Some(bytes) = self.resource_provider.get_bytes(&path) else {
                log::warn!("wgpu-mc: {path} cannot be read, so that fluid is not drawn");
                continue;
            };

            block_atlas.allocate([(&path, &bytes)], &*self.resource_provider);
        }

        block_atlas.upload(wm);
    }
}
