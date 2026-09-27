use crate::mc::chunk::RenderLayer;
use crate::mc::direction::java_mask_has;
use glam::{Mat3, Vec3, vec3};
use itertools::Itertools;
use minecraft_assets::api::ModelResolver;
use minecraft_assets::schemas;
use minecraft_assets::schemas::blockstates::ModelProperties;
use serde_derive::{Deserialize, Serialize};

use crate::mc::direction::Direction;
use crate::mc::resource::{ResourcePath, ResourceProvider};
use crate::render::atlas::Atlas;
use crate::texture::UV;

/// A block position: x, y, z
pub type BlockPos = (i32, u16, i32);

#[derive(Hash, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub struct BlockstateKey {
    /// An index into [BlockManager]
    pub block: u16,
    /// Used to quickly figure out which [ModelMesh] a [Block] should return without having to hash strings
    pub augment: u16,
}

impl BlockstateKey {
    pub fn pack(&self) -> u32 {
        ((self.block as u32) << 16) | (self.augment as u32)
    }
}

impl From<(u16, u16)> for BlockstateKey {
    fn from(tuple: (u16, u16)) -> Self {
        Self {
            block: tuple.0,
            augment: tuple.1,
        }
    }
}

impl From<u32> for BlockstateKey {
    fn from(int: u32) -> Self {
        Self::from(((int >> 16) as u16, (int & 0xffff) as u16))
    }
}

///The state of one block, describing which variant
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChunkBlockState {
    Air,
    State(BlockstateKey),
}

impl ChunkBlockState {
    pub fn is_air(&self) -> bool {
        matches!(self, Self::Air)
    }
}

///Represents a vertex in a block mesh, including an additional UV offset index for animated textures.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct BlockMeshVertex {
    pub position: Vec3,
    pub tex_coords: [u16; 2],
}
#[derive(Debug, Clone, Copy)]
pub struct BlockModelFace {
    pub vertices: [BlockMeshVertex; 4],
    pub normal: Vec3,
    pub tint_index: i32,
    pub animation_uv_offset: u32,
    /// The layer this face is baked into, from two answers the game also asks separately: what the
    /// model says (`force_translucent`, `render_type` - `declared_layer`) and what the *sprite* it
    /// samples says (`Atlas::sprite_layer`, which is where ice, leaves and every plant are decided -
    /// none of them declares anything in its model).
    ///
    /// Per face rather than per model because that is what the game does, and because a model is not
    /// one thing: a grass block is an opaque cube with a cutout overlay on its sides, and classifying
    /// it as a whole would hand the whole block to Minecraft's cutout pass. Per face keeps the cube in
    /// this renderer's solid pass and leaves the overlay to the pass that owns cutout.
    pub layer: RenderLayer,
}

/// What a block *state* says about the faces around it, as Minecraft itself answers it.
///
/// Two masks, one bit per direction, both computed on the JVM side when the block is registered
/// (`RegistryMixin`) and keyed by the state the section palette carries:
///
/// - `occlusion`: `state.getFaceOcclusionShape(dir) == Shapes.block()`. The state's own shape covers
///   that whole face, so a neighbour's face against it is not drawn. Note that this is *not* the
///   model: glass, ice, leaves and every plant have a full-cube model and none of them occlude
///   anything, which is exactly the difference this type exists for.
/// - `self_hide`: `state.skipRendering(state, dir)`. The block leaves out the face between itself
///   and a neighbour of its own kind - glass against glass, ice against ice, the bars of a pane, a
///   fluid against itself.
///
/// The bits are in Java's `Direction.ordinal()` order, because that is the side that computes them;
/// read them with [`crate::mc::direction::java_mask_has`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FaceFlags {
    pub occlusion: u8,
    pub self_hide: u8,
}

impl FaceFlags {
    /// The flags two states that wear the same model agree on.
    ///
    /// A packed key can be worn by more than one state - a property the blockstate file does not
    /// vary its model on, `waterlogged` for one - and what these bits decide is whether a *face* is
    /// left out of the mesh, where being wrong means a hole in the world. So a bit survives only
    /// where every state with this model has it, and a disagreement costs an invisible face between
    /// two blocks rather than a missing one.
    pub fn and(self, other: FaceFlags) -> FaceFlags {
        FaceFlags {
            occlusion: self.occlusion & other.occlusion,
            self_hide: self.self_hide & other.self_hide,
        }
    }

    /// Whether the state's shape covers the whole face in this direction.
    pub fn occludes(self, dir: Direction) -> bool {
        java_mask_has(self.occlusion, dir)
    }

    /// Whether the state hides its own face toward a neighbour of the same kind in this direction.
    pub fn hides_same_state(self, dir: Direction) -> bool {
        java_mask_has(self.self_hide, dir)
    }
}

/// Parses a model file, allowing for the object form of a texture that 26.1 introduced.
///
/// A model texture may be an object rather than a string:
///
/// ```json
/// "textures": { "all": { "sprite": "minecraft:block/black_stained_glass", "force_translucent": true } }
/// ```
///
/// The `minecraft_assets` schema this crate parses models with - a git dependency pinned to a
/// revision older than that - has `Textures` as a map of `String`, so one of those fails the whole
/// model with `invalid type: map, expected a string` and the block is drawn as bedrock. That is 163
/// entries in vanilla alone: every stained glass and stained glass pane, and redstone dust.
///
/// The retry rewrites those objects into the sprite string they carry, which is everything this
/// renderer reads from a texture entry; `force_translucent` only picks the render layer, and
/// Minecraft's own mesh is what decides that for the blocks drawn today. A model that fails for any
/// other reason fails again, and the error reported is the first one - the one that names the field
/// that was actually wrong.
fn parse_model(json: &str) -> Result<schemas::Model, serde_json::Error> {
    let error = match serde_json::from_str(json) {
        Ok(model) => return Ok(model),
        Err(error) => error,
    };

    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(json) else {
        return Err(error);
    };

    if !flatten_textures(&mut value) {
        return Err(error);
    }

    serde_json::from_value(value).map_err(|_| error)
}

/// Replaces every object-valued entry of `textures` with the sprite it names.
///
/// Returns whether anything was rewritten, so the caller can tell "this model does not use the
/// object form" from "it does, and it still does not parse".
fn flatten_textures(value: &mut serde_json::Value) -> bool {
    let Some(textures) = value
        .get_mut("textures")
        .and_then(|textures| textures.as_object_mut())
    else {
        return false;
    };

    let mut changed = false;

    for texture in textures.values_mut() {
        if let Some(sprite) = texture.get("sprite").and_then(|sprite| sprite.as_str()) {
            *texture = serde_json::Value::String(sprite.to_string());
            changed = true;
        }
    }

    changed
}

/// The render layer a model json asks for, if it asks for one.
///
/// Two spellings, because the game has used both. 26.1 spells "this model draws with blending" as
/// `force_translucent` on a texture entry - `glass.json` is the object form
/// `{"sprite": …, "force_translucent": true}`, which is also the form [`flatten_textures`] has to
/// rewrite before the model schema can read it, so this reads the field out of the raw json rather
/// than out of the parsed model. 1.21 spelled it as `render_type` on the model
/// (`minecraft:translucent`, `minecraft:cutout`, `minecraft:solid`), and a pack or a mod written
/// for that spelling still lands in the layer it asked for.
///
/// This is the model's half of the answer and it is the *floor* under a face's layer: the sprite a
/// face samples can put it in a stronger one, and does for everything a model says nothing about -
/// ice, leaves, every plant. See [`BlockModelFace::layer`].
fn declared_layer(json: &str) -> Option<RenderLayer> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;

    let mut layer = match value.get("render_type").and_then(|kind| kind.as_str()) {
        Some("minecraft:translucent" | "translucent") => Some(RenderLayer::Transparent),
        Some("minecraft:cutout" | "cutout" | "minecraft:cutout_mipped" | "cutout_mipped") => {
            Some(RenderLayer::Cutout)
        }
        Some("minecraft:solid" | "solid") => Some(RenderLayer::Solid),
        _ => None,
    };

    // A model that asks for translucency in either spelling gets it, whichever its parent said.
    let forced = value
        .get("textures")
        .and_then(|textures| textures.as_object())
        .is_some_and(|textures| {
            textures.values().any(|texture| {
                texture
                    .get("force_translucent")
                    .and_then(|forced| forced.as_bool())
                    .unwrap_or(false)
            })
        });

    if forced {
        layer = Some(layer.map_or(RenderLayer::Transparent, |layer| {
            layer.stronger(RenderLayer::Transparent)
        }));
    }

    layer
}

fn recurse_model_parents(
    model: &schemas::Model,
    resource_provider: &dyn ResourceProvider,
    models: &mut Vec<ResourcePath>,
) -> Result<(), MeshBakeError> {
    if let Some(parent_path_string) = &model.parent {
        let parent_path: ResourcePath = ResourcePath::from(parent_path_string)
            .prepend("models/")
            .append(".json");

        // A model whose parent is not in the pack - a mod that ships the child without the vanilla
        // parent it extends, a pack that removes one - used to be an `expect` naming the path, and a
        // panic here is a block the game cannot draw *and* a block registry that never finishes
        // baking. The error carries the path instead, and the caller drops the model.
        let parent_json = resource_provider
            .get_string(&parent_path)
            .ok_or_else(|| MeshBakeError::UnresolvedResourcePath(parent_path.clone()))?;

        // Named here as well as in the caller, which only knows the block: a parse failure inside a
        // model has to say *which* file, and the error itself carries serde's line and column.
        let parent: schemas::Model = parse_model(&parent_json).map_err(|err| {
            log::warn!("wgpu-mc: the parent model {parent_path} could not be read: {err}");
            MeshBakeError::JsonError(err)
        })?;

        recurse_model_parents(&parent, resource_provider, models)?;
        models.push(parent_path);
    }

    Ok(())
}

/// The model chain of one model, merged into what its faces actually sample.
///
/// Every model goes through [`ModelResolver::resolve_model`], including one with no parent at all.
/// That is not a formality: the resolver is what rewrites the `#name` a *face* carries into the
/// sprite the model's `textures` map points at, and a model that skipped it reached the baker with
/// `#all` still in its faces - which `get_atlas_uv` then looked up in the atlas as a sprite called
/// `#all`, found nothing, and dropped the face. Silently: a parentless model was a block drawn with
/// no faces at all, and nothing in the log said why.
///
/// The layer a model asks for comes back with it: the strongest spelling in the chain wins, because
/// `force_translucent` sits on the *textures* a child inherits from its parents.
fn resolve_model(
    model: schemas::Model,
    declared: Option<RenderLayer>,
    resource_provider: &dyn ResourceProvider,
) -> Result<(schemas::Model, Option<RenderLayer>), MeshBakeError> {
    let mut parent_paths = Vec::new();
    recurse_model_parents(&model, resource_provider, &mut parent_paths)?;

    // The layer the model chain asks for. The strongest spelling wins rather than the child's:
    // `force_translucent` sits on the *textures*, which a child that does not override them inherits
    // from its parent, so a chain that asks for blending anywhere in it is a chain that blends.
    let mut layer = declared;

    let parents: Vec<schemas::Model> = parent_paths
        .iter()
        .map(|parent_path| {
            let json = resource_provider
                .get_string(parent_path)
                .ok_or_else(|| MeshBakeError::UnresolvedResourcePath(parent_path.clone()))?;

            if let Some(declared) = declared_layer(&json) {
                layer = Some(layer.map_or(declared, |layer| layer.stronger(declared)));
            }

            parse_model(&json).map_err(MeshBakeError::JsonError)
        })
        .collect::<Result<_, _>>()?;

    let mut schema = ModelResolver::resolve_model([&model].into_iter().chain(parents.iter()));

    if let Some(textures) = &mut schema.textures {
        let copy = textures.clone();

        // `resolve` is `None` for a reference that does not name anything in the model or its
        // parents - `#all` with no `all` - so it is reported rather than unwrapped.
        let mut unresolved = None;

        textures.iter_mut().for_each(|(_key, texture)| {
            if texture.reference().is_some() {
                match texture.resolve(&copy) {
                    Some(resolved) => texture.0 = resolved.to_string(),
                    None => {
                        unresolved.get_or_insert_with(|| {
                            MeshBakeError::UnresolvedTextureReference(texture.0.clone())
                        });
                    }
                }
            }
        });

        if let Some(err) = unresolved {
            return Err(err);
        }
    }

    Ok((schema, layer))
}

/// The first texture reference left in a resolved model, described, or `None` when every texture it
/// names is a concrete sprite.
///
/// Both places a model names one, because both reach the same lookup: the `textures` map, whose keys
/// the faces resolve through, and the faces themselves, which are what `get_atlas_uv` is handed. A
/// reference that got this far is a sprite the atlas will not have, and the face that samples it is
/// dropped without a word - so the description is what the caller puts in the error it reports.
fn unresolved_texture(model: &schemas::Model) -> Option<String> {
    if let Some(textures) = &model.textures {
        if let Some((key, value)) = textures
            .iter()
            .find(|(_key, value)| value.reference().is_some())
        {
            return Some(format!("key: {key} value: {value:?}"));
        }
    }

    model
        .elements
        .iter()
        .flatten()
        .flat_map(|element| element.faces.iter())
        .find(|(_direction, face)| face.texture.reference().is_some())
        .map(|(direction, face)| format!("face {direction:?} samples {}", face.texture.0))
}

fn get_atlas_uv(face: &schemas::models::ElementFace, block_atlas: &Atlas) -> Option<UV> {
    let uv = face.uv.unwrap_or([0.0, 0.0, 16.0, 16.0]).map(|x| x as u16);
    let atlas_map = block_atlas.uv_map.read();
    atlas_map
        .get(&(&face.texture.0).into())
        .copied()
        .map(|tex| {
            let tw = (tex.1.0 - tex.0.0, tex.1.1 - tex.0.1);
            let uvs = match face.rotation {
                0 => ((uv[0], uv[1]), (uv[2], uv[3])),
                90 => ((tw.1 - uv[1], uv[0]), (tw.1 - uv[3], uv[2])),
                180 => ((tw.0 - uv[0], tw.1 - uv[1]), (tw.0 - uv[2], tw.1 - uv[3])),
                270 => ((uv[1], tw.0 - uv[0]), (uv[3], tw.0 - uv[2])),
                _ => unreachable!(),
            };
            (
                (tex.0.0 + uvs.0.0, tex.0.1 + uvs.0.1),
                (tex.0.0 + uvs.1.0, tex.0.1 + uvs.1.1),
            )
        })
}

pub struct RenderSettings {
    pub opaque: bool,
}

/// TODO: Use actual error handling library
#[derive(Debug)]
pub enum MeshBakeError {
    UnresolvedTextureReference(String),
    UnresolvedResourcePath(ResourcePath),
    JsonError(serde_json::Error),
}

/// A block model which has been baked into a mesh and is ready for rendering
#[derive(Debug)]
pub struct ModelMesh {
    pub north: Vec<BlockModelFace>,
    pub south: Vec<BlockModelFace>,
    pub west: Vec<BlockModelFace>,
    pub east: Vec<BlockModelFace>,
    pub up: Vec<BlockModelFace>,
    pub down: Vec<BlockModelFace>,
    pub any: Vec<BlockModelFace>,
    pub cull: u8,
}

impl ModelMesh {
    pub fn bake<'a>(
        model_properties: impl IntoIterator<Item = &'a ModelProperties>,
        resource_provider: &dyn ResourceProvider,
        block_atlas: &Atlas,
    ) -> Result<Self, MeshBakeError> {
        let mesh = model_properties
            .into_iter()
            .map(|model_properties: &ModelProperties| {
                let model_resource_path = ResourcePath::from(&model_properties.model)
                    .prepend("models/")
                    .append(".json");

                let model_json = resource_provider
                    .get_string(&model_resource_path)
                    .ok_or_else(|| MeshBakeError::UnresolvedResourcePath(model_resource_path.clone()))?;

                //Recursively resolve the model using it's parents if it has any
                let (model, layer): (schemas::Model, Option<RenderLayer>) = resolve_model(
                    //Parse the JSON into the model schema
                    parse_model(&model_json).map_err(|err| {
                        log::warn!(
                            "wgpu-mc: the model {model_resource_path} could not be read: {err}"
                        );
                        MeshBakeError::JsonError(err)
                    })?,
                    declared_layer(&model_json),
                    resource_provider,
                )?;

                // What the model chain says every one of its faces is at least: `force_translucent`
                // on a 26.1 texture entry, or the older `render_type`. The sprite a face samples can
                // put it in a stronger layer than this, and does for everything the model says
                // nothing about.
                let model_layer = layer.unwrap_or(RenderLayer::Solid);

                // A `#name` that survived resolution is a face about to be looked up in the atlas as
                // a sprite called `#name` - which is not there, so the face is dropped and the block
                // is drawn with a hole in it and nothing in the log. Both places a model can still be
                // holding one are checked *because* the difference is invisible: the `textures` map,
                // which names the sprite under a key, and the faces, which sample it. See
                // `resolve_model` for how a model gets here resolved at all.
                if let Some(reference) = unresolved_texture(&model) {
                    return Err(MeshBakeError::UnresolvedTextureReference(reference));
                }

                if let Some(textures) = model.textures {
                    let uv_map = block_atlas.uv_map.read();

                    let unallocated_textures: Vec<ResourcePath> = textures
                        .iter()
                        .filter_map(|(_, texture)| {
                            let texture_id: ResourcePath = (&texture.0).into();
                            if !uv_map.contains_key(&texture_id) {
                                //Block UV atlas doesn't contain a texture, so we add it
                                Some(texture_id)
                            } else {
                                None
                            }
                        })
                        .collect();

                    drop(uv_map);

                    // A model can name a texture this provider cannot read - a pack that ships the
                    // model without its image, a reference that only resolves under another
                    // namespace, a block that NeoForge offers but never registers a sprite for.
                    // That used to be an `unwrap`, and it took the whole block registry down with
                    // it: nothing is cached, every block is baked, and the panic lands on whichever
                    // thread was asked to bake. Skipping the texture instead leaves the faces that
                    // use it untextured - `get_atlas_uv` below drops them - which is the same thing
                    // the game shows for a missing texture, and the warning says which path to look
                    // at.
                    let unallocated_textures: Vec<(&ResourcePath, Vec<u8>)> = unallocated_textures
                        .iter()
                        .filter_map(|path| {
                            let texture_path = path.prepend("textures/").append(".png");
                            match resource_provider.get_bytes(&texture_path) {
                                Some(data) => Some((path, data)),
                                None => {
                                    log::warn!(
                                        "wgpu-mc: {texture_path} is named by a model but cannot \
                                        be read; the faces using it are left untextured"
                                    );
                                    None
                                }
                            }
                        })
                        .collect();

                    if !unallocated_textures.is_empty() {
                        block_atlas.allocate(
                            unallocated_textures
                                .iter()
                                .map(|(path, data)| (*path, data)),
                            resource_provider,
                        );
                    }
                };

                Ok(model
                    .elements
                    .iter()
                    .flatten()
                    .flat_map(|element| {
                        //Face textures
                        let north = element
                            .faces
                            .get(&schemas::models::BlockFace::North)
                            .as_ref()
                            .and_then(|tex| {
                                get_atlas_uv(tex, block_atlas).map(|uv| {
                                    (
                                        //The default UV for this texture
                                        uv,
                                        //If this texture has an animation, get the offset, otherwise default to 0
                                        *block_atlas
                                            .animated_texture_offsets
                                            .read()
                                            .get(&(&tex.texture.0).into())
                                            .unwrap_or(&0),
                                        tex.tint_index,
                                        //What the sprite's own pixels say about the face that samples
                                        //them: this is where ice, leaves and every plant are decided,
                                        //because none of them declares anything in its model.
                                        block_atlas
                                            .sprite_layer(&(&tex.texture.0).into())
                                            .unwrap_or(RenderLayer::Solid),
                                    )
                                })
                            });

                        let east = element
                            .faces
                            .get(&schemas::models::BlockFace::East)
                            .as_ref()
                            .and_then(|tex| {
                                get_atlas_uv(tex, block_atlas).map(|uv| {
                                    (
                                        //The default UV for this texture
                                        uv,
                                        //If this texture has an animation, get the offset, otherwise default to 0
                                        *block_atlas
                                            .animated_texture_offsets
                                            .read()
                                            .get(&(&tex.texture.0).into())
                                            .unwrap_or(&0),
                                        tex.tint_index,
                                        //What the sprite's own pixels say about the face that samples
                                        //them: this is where ice, leaves and every plant are decided,
                                        //because none of them declares anything in its model.
                                        block_atlas
                                            .sprite_layer(&(&tex.texture.0).into())
                                            .unwrap_or(RenderLayer::Solid),
                                    )
                                })
                            });

                        let south = element
                            .faces
                            .get(&schemas::models::BlockFace::South)
                            .as_ref()
                            .and_then(|tex| {
                                get_atlas_uv(tex, block_atlas).map(|uv| {
                                    (
                                        //The default UV for this texture
                                        uv,
                                        //If this texture has an animation, get the offset, otherwise default to 0
                                        *block_atlas
                                            .animated_texture_offsets
                                            .read()
                                            .get(&(&tex.texture.0).into())
                                            .unwrap_or(&0),
                                        tex.tint_index,
                                        //What the sprite's own pixels say about the face that samples
                                        //them: this is where ice, leaves and every plant are decided,
                                        //because none of them declares anything in its model.
                                        block_atlas
                                            .sprite_layer(&(&tex.texture.0).into())
                                            .unwrap_or(RenderLayer::Solid),
                                    )
                                })
                            });

                        let west = element
                            .faces
                            .get(&schemas::models::BlockFace::West)
                            .as_ref()
                            .and_then(|tex| {
                                get_atlas_uv(tex, block_atlas).map(|uv| {
                                    (
                                        //The default UV for this texture
                                        uv,
                                        //If this texture has an animation, get the offset, otherwise default to 0
                                        *block_atlas
                                            .animated_texture_offsets
                                            .read()
                                            .get(&(&tex.texture.0).into())
                                            .unwrap_or(&0),
                                        tex.tint_index,
                                        //What the sprite's own pixels say about the face that samples
                                        //them: this is where ice, leaves and every plant are decided,
                                        //because none of them declares anything in its model.
                                        block_atlas
                                            .sprite_layer(&(&tex.texture.0).into())
                                            .unwrap_or(RenderLayer::Solid),
                                    )
                                })
                            });

                        let up = element
                            .faces
                            .get(&schemas::models::BlockFace::Up)
                            .as_ref()
                            .and_then(|tex| {
                                get_atlas_uv(tex, block_atlas).map(|uv| {
                                    (
                                        //The default UV for this texture
                                        uv,
                                        //If this texture has an animation, get the offset, otherwise default to 0
                                        *block_atlas
                                            .animated_texture_offsets
                                            .read()
                                            .get(&(&tex.texture.0).into())
                                            .unwrap_or(&0),
                                        tex.tint_index,
                                        //What the sprite's own pixels say about the face that samples
                                        //them: this is where ice, leaves and every plant are decided,
                                        //because none of them declares anything in its model.
                                        block_atlas
                                            .sprite_layer(&(&tex.texture.0).into())
                                            .unwrap_or(RenderLayer::Solid),
                                    )
                                })
                            });

                        let down = element
                            .faces
                            .get(&schemas::models::BlockFace::Down)
                            .as_ref()
                            .and_then(|tex| {
                                get_atlas_uv(tex, block_atlas).map(|uv| {
                                    (
                                        //The default UV for this texture
                                        uv,
                                        //If this texture has an animation, get the offset, otherwise default to 0
                                        *block_atlas
                                            .animated_texture_offsets
                                            .read()
                                            .get(&(&tex.texture.0).into())
                                            .unwrap_or(&0),
                                        tex.tint_index,
                                        //What the sprite's own pixels say about the face that samples
                                        //them: this is where ice, leaves and every plant are decided,
                                        //because none of them declares anything in its model.
                                        block_atlas
                                            .sprite_layer(&(&tex.texture.0).into())
                                            .unwrap_or(RenderLayer::Solid),
                                    )
                                })
                            });
                        let rot = &element.rotation;
                        let matrix = match rot.axis {
                            schemas::models::Axis::X => {
                                Mat3::from_rotation_x(rot.angle.to_radians())
                            }
                            schemas::models::Axis::Y => {
                                Mat3::from_rotation_y(rot.angle.to_radians())
                            }
                            schemas::models::Axis::Z => {
                                Mat3::from_rotation_z(rot.angle.to_radians())
                            }
                        };
                        let vec_origin = Vec3::from_array(rot.origin) / 16.0;

                        let vertex_transform = |v: Vec3| {
                            let v = match model_properties.x {
                                0 => v,
                                90 => vec3(v.x, 1.0 - v.z, v.y),
                                180 => vec3(v.x, 1.0 - v.y, 1.0 - v.z),
                                270 => vec3(v.x, v.z, 1.0 - v.y),
                                _ => panic!("invalid rotation"),
                            };
                            let v = matrix * (v - vec_origin) + vec_origin;

                            match model_properties.y {
                                0 => v,
                                90 => vec3(1.0 - v.z, v.y, v.x),
                                180 => vec3(1.0 - v.x, v.y, 1.0 - v.z),
                                270 => vec3(v.z, v.y, 1.0 - v.x),
                                _ => panic!("invalid rotation"),
                            }
                        };

                        let p000 = vertex_transform(vec3(
                            element.from[0] / 16.0,
                            element.from[1] / 16.0,
                            element.from[2] / 16.0,
                        ));
                        let p001 = vertex_transform(vec3(
                            element.from[0] / 16.0,
                            element.from[1] / 16.0,
                            element.to[2] / 16.0,
                        ));
                        let p010 = vertex_transform(vec3(
                            element.from[0] / 16.0,
                            element.to[1] / 16.0,
                            element.from[2] / 16.0,
                        ));
                        let p011 = vertex_transform(vec3(
                            element.from[0] / 16.0,
                            element.to[1] / 16.0,
                            element.to[2] / 16.0,
                        ));
                        let p100 = vertex_transform(vec3(
                            element.to[0] / 16.0,
                            element.from[1] / 16.0,
                            element.from[2] / 16.0,
                        ));
                        let p101 = vertex_transform(vec3(
                            element.to[0] / 16.0,
                            element.from[1] / 16.0,
                            element.to[2] / 16.0,
                        ));
                        let p110 = vertex_transform(vec3(
                            element.to[0] / 16.0,
                            element.to[1] / 16.0,
                            element.from[2] / 16.0,
                        ));
                        let p111 = vertex_transform(vec3(
                            element.to[0] / 16.0,
                            element.to[1] / 16.0,
                            element.to[2] / 16.0,
                        ));

                        let mut faces = vec![];
                        faces.extend(south.map(|south_face| BlockModelFace {
                            vertices: [
                                BlockMeshVertex {
                                    position: p101,
                                    tex_coords: [south_face.0.1.0, south_face.0.1.1],
                                },
                                BlockMeshVertex {
                                    position: p111,
                                    tex_coords: [south_face.0.1.0, south_face.0.0.1],
                                },
                                BlockMeshVertex {
                                    position: p011,
                                    tex_coords: [south_face.0.0.0, south_face.0.0.1],
                                },
                                BlockMeshVertex {
                                    position: p001,
                                    tex_coords: [south_face.0.0.0, south_face.0.1.1],
                                },
                            ],
                            normal: vec3(0.0, 0.0, 1.0),
                            tint_index: south_face.2,
                            animation_uv_offset: south_face.1,
                            layer: model_layer.stronger(south_face.3),
                        }));
                        faces.extend(west.map(|west_face| BlockModelFace {
                            vertices: [
                                BlockMeshVertex {
                                    position: p001,
                                    tex_coords: [west_face.0.1.0, west_face.0.1.1],
                                },
                                BlockMeshVertex {
                                    position: p011,
                                    tex_coords: [west_face.0.1.0, west_face.0.0.1],
                                },
                                BlockMeshVertex {
                                    position: p010,
                                    tex_coords: [west_face.0.0.0, west_face.0.0.1],
                                },
                                BlockMeshVertex {
                                    position: p000,
                                    tex_coords: [west_face.0.0.0, west_face.0.1.1],
                                },
                            ],
                            normal: vec3(-1.0, 0.0, 0.0),
                            tint_index: west_face.2,
                            animation_uv_offset: west_face.1,
                            layer: model_layer.stronger(west_face.3),
                        }));
                        faces.extend(north.map(|north_face| BlockModelFace {
                            vertices: [
                                BlockMeshVertex {
                                    position: p000,
                                    tex_coords: [north_face.0.1.0, north_face.0.1.1],
                                },
                                BlockMeshVertex {
                                    position: p010,
                                    tex_coords: [north_face.0.1.0, north_face.0.0.1],
                                },
                                BlockMeshVertex {
                                    position: p110,
                                    tex_coords: [north_face.0.0.0, north_face.0.0.1],
                                },
                                BlockMeshVertex {
                                    position: p100,
                                    tex_coords: [north_face.0.0.0, north_face.0.1.1],
                                },
                            ],
                            normal: vec3(0.0, 0.0, -1.0),
                            tint_index: north_face.2,
                            animation_uv_offset: north_face.1,
                            layer: model_layer.stronger(north_face.3),
                        }));
                        faces.extend(east.map(|east_face| BlockModelFace {
                            vertices: [
                                BlockMeshVertex {
                                    position: p100,
                                    tex_coords: [east_face.0.1.0, east_face.0.1.1],
                                },
                                BlockMeshVertex {
                                    position: p110,
                                    tex_coords: [east_face.0.1.0, east_face.0.0.1],
                                },
                                BlockMeshVertex {
                                    position: p111,
                                    tex_coords: [east_face.0.0.0, east_face.0.0.1],
                                },
                                BlockMeshVertex {
                                    position: p101,
                                    tex_coords: [east_face.0.0.0, east_face.0.1.1],
                                },
                            ],
                            normal: vec3(1.0, 0.0, 0.0),
                            tint_index: east_face.2,
                            animation_uv_offset: east_face.1,
                            layer: model_layer.stronger(east_face.3),
                        }));
                        faces.extend(up.map(|up_face| BlockModelFace {
                            vertices: [
                                BlockMeshVertex {
                                    position: p010,
                                    tex_coords: [up_face.0.1.0, up_face.0.1.1],
                                },
                                BlockMeshVertex {
                                    position: p011,
                                    tex_coords: [up_face.0.1.0, up_face.0.0.1],
                                },
                                BlockMeshVertex {
                                    position: p111,
                                    tex_coords: [up_face.0.0.0, up_face.0.0.1],
                                },
                                BlockMeshVertex {
                                    position: p110,
                                    tex_coords: [up_face.0.0.0, up_face.0.1.1],
                                },
                            ],
                            normal: vec3(0.0, 1.0, 0.0),
                            tint_index: up_face.2,
                            animation_uv_offset: up_face.1,
                            layer: model_layer.stronger(up_face.3),
                        }));

                        faces.extend(down.map(|down_face| BlockModelFace {
                            vertices: [
                                BlockMeshVertex {
                                    position: p000,
                                    tex_coords: [down_face.0.1.0, down_face.0.1.1],
                                },
                                BlockMeshVertex {
                                    position: p100,
                                    tex_coords: [down_face.0.1.0, down_face.0.0.1],
                                },
                                BlockMeshVertex {
                                    position: p101,
                                    tex_coords: [down_face.0.0.0, down_face.0.0.1],
                                },
                                BlockMeshVertex {
                                    position: p001,
                                    tex_coords: [down_face.0.0.0, down_face.0.1.1],
                                },
                            ],
                            normal: vec3(0.0, -1.0, 0.0),
                            tint_index: down_face.2,
                            animation_uv_offset: down_face.1,
                            layer: model_layer.stronger(down_face.3),
                        }));
                        faces
                    })
                    .collect::<Vec<BlockModelFace>>())
            })
            .flatten_ok()
            .collect::<Result<Vec<BlockModelFace>, MeshBakeError>>()?;

        let mut result = Self {
            north: vec![],
            south: vec![],
            west: vec![],
            east: vec![],
            up: vec![],
            down: vec![],
            any: vec![],
            cull: 0,
        };
        mesh.iter().for_each(|face| {
            let full_face = (face.vertices[0].position.fract() == vec3(0.0, 0.0, 0.0)
                && face.vertices[1].position.fract() == vec3(0.0, 0.0, 0.0)
                && face.vertices[2].position.fract() == vec3(0.0, 0.0, 0.0)
                && face.vertices[3].position.fract() == vec3(0.0, 0.0, 0.0))
                as u8;
            if face.vertices[0].position.x == 0.0
                && face.vertices[1].position.x == 0.0
                && face.vertices[2].position.x == 0.0
            {
                result.west.push(*face);
                result.cull |= full_face << Direction::West as u8;
            } else if face.vertices[0].position.x == 1.0
                && face.vertices[1].position.x == 1.0
                && face.vertices[2].position.x == 1.0
            {
                result.east.push(*face);
                result.cull |= full_face << Direction::East as u8;
            } else if face.vertices[0].position.y == 0.0
                && face.vertices[1].position.y == 0.0
                && face.vertices[2].position.y == 0.0
            {
                result.down.push(*face);
                result.cull |= full_face << Direction::Down as u8;
            } else if face.vertices[0].position.y == 1.0
                && face.vertices[1].position.y == 1.0
                && face.vertices[2].position.y == 1.0
            {
                result.up.push(*face);
                result.cull |= full_face << Direction::Up as u8;
            } else if face.vertices[0].position.z == 0.0
                && face.vertices[1].position.z == 0.0
                && face.vertices[2].position.z == 0.0
            {
                result.north.push(*face);
                result.cull |= full_face << Direction::North as u8;
            } else if face.vertices[0].position.z == 1.0
                && face.vertices[1].position.z == 1.0
                && face.vertices[2].position.z == 1.0
            {
                result.south.push(*face);
                result.cull |= full_face << Direction::South as u8;
            } else {
                result.any.push(*face);
            }
        });
        Ok(result)
    }
}

/// Resolving a model, and the check that catches what resolution could not do. See [`resolve_model`]
/// and [`unresolved_texture`].
#[cfg(test)]
mod texture_resolution_tests {
    use super::*;
    use std::collections::HashMap;

    /// A resource provider over a fixed set of files, which is all `resolve_model` asks for.
    struct Files(HashMap<String, String>);

    impl ResourceProvider for Files {
        fn get_bytes(&self, id: &ResourcePath) -> Option<Vec<u8>> {
            self.0.get(&id.0).map(|body| body.clone().into_bytes())
        }
    }

    fn files(entries: &[(&str, &str)]) -> Files {
        Files(
            entries
                .iter()
                .map(|(path, body)| (path.to_string(), body.to_string()))
                .collect(),
        )
    }

    /// A model that is nothing but one element facing north, with the texture it samples named by the
    /// `textures` map.
    fn one_face_model(texture: &str) -> &'static str {
        // The strings are leaked rather than threaded through a lifetime: a test that builds a model
        // out of JSON has nothing to borrow from.
        Box::leak(
            format!(
                r##"{{
                    "textures": {{ "all": "{texture}" }},
                    "elements": [
                        {{
                            "from": [0, 0, 0],
                            "to": [16, 16, 16],
                            "faces": {{ "north": {{ "texture": "#all" }} }}
                        }}
                    ]
                }}"##
            )
            .into_boxed_str(),
        )
    }

    fn north_face(model: &schemas::Model) -> &schemas::models::ElementFace {
        model.elements.as_ref().expect("elements")[0]
            .faces
            .get(&schemas::models::BlockFace::North)
            .expect("the face the fixture wrote")
    }

    /// A model with **no parent** still goes through the resolver, which is what rewrites the `#all`
    /// in its faces. Skipping that - which is what the early return did - left the face sampling a
    /// sprite called `#all`: not in the atlas, so the face was dropped and the block was drawn with
    /// no faces at all, silently.
    #[test]
    fn a_parentless_model_has_the_textures_in_its_faces_resolved() {
        let provider = files(&[(
            "minecraft:models/block/solo.json",
            one_face_model("minecraft:block/stone"),
        )]);

        let json = provider
            .get_string(&ResourcePath::from("minecraft:models/block/solo.json"))
            .expect("the fixture's model");
        let model = parse_model(&json).expect("a model this build can read");
        assert!(model.parent.is_none(), "the fixture has no parent");

        let (resolved, _) = resolve_model(model, None, &provider).expect("it resolves");

        assert_eq!(
            north_face(&resolved).texture.0, "minecraft:block/stone",
            "the face samples the sprite its `textures` map names, not the reference"
        );
        assert!(unresolved_texture(&resolved).is_none());
    }

    /// And what the resolver cannot do is reported rather than left to the atlas lookup, which would
    /// drop the face without a word.
    #[test]
    fn a_reference_nothing_defines_is_reported() {
        let provider = files(&[(
            "minecraft:models/block/broken.json",
            one_face_model("#nothing_defines_this"),
        )]);

        let json = provider
            .get_string(&ResourcePath::from("minecraft:models/block/broken.json"))
            .expect("the fixture's model");
        let model = parse_model(&json).expect("a model this build can read");

        let described = unresolved_texture(&model).expect("the reference is still there");
        assert!(
            described.contains("nothing_defines_this"),
            "the description has to name it: {described}"
        );
    }
}
