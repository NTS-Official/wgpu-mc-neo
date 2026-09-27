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
    /// The direction the model declared for this face's `cullface`, already turned by the variant's
    /// rotation - or `None`, which is the common case and means the face is **never** culled.
    ///
    /// Minecraft's rule is exactly that split: a face with a `cullface` is handed to
    /// `Block#shouldRenderFace` for that direction, and one without is added as an *unculled* face
    /// (`UnbakedCuboidGeometry`). A face that declares nothing is a face the model wants drawn
    /// whatever is next to it - a plant's cross, a pane's edge, the inside of a mushroom cap - and a
    /// baker that culls it anyway is a hole in the world that no log line explains.
    pub cull: Option<Direction>,
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

/// A blockstate variant's rotation of the model it names: `x` first, then `y`, each a multiple of 90°.
///
/// Both are part of the *variant*, not of the model - the same model file is baked once per variant
/// that names it, with its own rotation - which is why everything here is applied per
/// `ModelProperties` and before the faces of several properties are merged into one mesh.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelRotation {
    pub x: i32,
    pub y: i32,
}

impl ModelRotation {
    pub fn new(x: i32, y: i32) -> ModelRotation {
        ModelRotation { x, y }
    }

    /// Whether this rotation leaves everything where it was, in which case the uv-lock machinery has
    /// nothing to do and the faces are built exactly as the model writes them.
    pub fn is_identity(self) -> bool {
        self.x % 360 == 0 && self.y % 360 == 0
    }

    /// A position inside the block's unit cube, rotated: `x` first, then `y`.
    ///
    /// The constants are not a rotation matrix's - each quarter turn is about the *middle of the
    /// block*, so a rotation that maps a corner out of the cube is folded back in. This is the form
    /// Minecraft's blockstate rotations are written in, and the one the model baker has always used.
    ///
    /// `x` is a **negative** quarter turn about X. That is not a guess: `x: 90` is
    /// `OctahedralGroup.BLOCK_ROT_X_90 = ROT_90_X_NEG`, which is `diag(1, 1, -1) * P132`, and it takes
    /// the model's up direction to **north**. The clearest place it shows is `amethyst_cluster`, whose
    /// model points up and whose `facing=north` variant is `{"x": 90}` - and the same turn is what puts
    /// a huge mushroom's cap skin on the side the `up`/`down` property names. This used to be written
    /// the other way round, which is why a mushroom cap had no top: the `up` piece was drawn at the
    /// bottom of the cube facing down, and the block below it (being a mushroom block, and so
    /// occluding) then culled it.
    ///
    /// `y` is the opposite sign to `x` in the same sense - `y: 90` is `ROT_90_Y_NEG`, which takes the
    /// model's north direction to **east** - and that one was already right.
    ///
    /// Anything that is not a quarter turn - the `360` a model may spell out for "no rotation", and
    /// any angle the game would have refused at load time - leaves the position where it was. That is
    /// a deliberate change from the `panic!("invalid rotation")` this used to be: a panic here happens
    /// inside a section bake, and a block that is drawn in its unrotated orientation is a far better
    /// outcome than a section that never finishes baking.
    pub fn position(self, v: Vec3) -> Vec3 {
        let v = match self.x {
            0 => v,
            90 => vec3(v.x, v.z, 1.0 - v.y),
            180 => vec3(v.x, 1.0 - v.y, 1.0 - v.z),
            270 => vec3(v.x, 1.0 - v.z, v.y),
            _ => v,
        };

        match self.y {
            0 => v,
            90 => vec3(1.0 - v.z, v.y, v.x),
            180 => vec3(1.0 - v.x, v.y, 1.0 - v.z),
            270 => vec3(v.z, v.y, 1.0 - v.x),
            _ => v,
        }
    }

    /// A direction, rotated - the same rotation with its translations dropped.
    ///
    /// A normal is not a position: the `1.0 -` in [`Self::position`] is where the block's middle is,
    /// and a direction has no position to be folded about. A matrix built this way stays axis-aligned
    /// for quarter turns, which is what the vertex packing needs - it writes the normal into three
    /// bits and panics on anything else.
    ///
    /// The branches are [`Self::position`]'s with the translations dropped, and the two have to be
    /// changed together: `x: 90` takes up to north here and up to the bottom of the cube there.
    pub fn direction(self, n: Vec3) -> Vec3 {
        let n = match self.x {
            0 => n,
            90 => vec3(n.x, n.z, -n.y),
            180 => vec3(n.x, -n.y, -n.z),
            270 => vec3(n.x, -n.z, n.y),
            _ => n,
        };

        match self.y {
            0 => n,
            90 => vec3(-n.z, n.y, n.x),
            180 => vec3(-n.x, n.y, -n.z),
            270 => vec3(n.z, n.y, -n.x),
            _ => n,
        }
    }

    /// The rotation as a 3×3 matrix, which is what the uv-lock transform below is composed from.
    pub fn matrix(self) -> Mat3 {
        Mat3::from_cols(
            self.direction(Vec3::X),
            self.direction(Vec3::Y),
            self.direction(Vec3::Z),
        )
    }

    /// The direction a face's declared `cullface` names once the model is rotated.
    ///
    /// Minecraft does the same to it - `Direction.rotate(modelState.transformation().getMatrix(),
    /// face.cullForDirection())` - so a rotated model is culled against the neighbour it now touches
    /// rather than the one it was written against.
    pub fn rotate_direction(self, dir: Direction) -> Direction {
        nearest_direction(self.direction(dir.to_vec().as_vec3()))
    }
}

/// The face a direction vector points at, for vectors that are axis-aligned: the axis it runs
/// furthest along, and the way it points.
fn nearest_direction(v: Vec3) -> Direction {
    let abs = v.abs();

    if abs.x >= abs.y && abs.x >= abs.z {
        if v.x >= 0.0 { Direction::East } else { Direction::West }
    } else if abs.y >= abs.z {
        if v.y >= 0.0 { Direction::Up } else { Direction::Down }
    } else if v.z >= 0.0 {
        Direction::South
    } else {
        Direction::North
    }
}

/// The frame a face's UVs live in, as a rotation of the south face's frame - Minecraft's
/// `BlockMath.VANILLA_UV_TRANSFORM_LOCAL_TO_GLOBAL`.
fn uv_frame(dir: Direction) -> Mat3 {
    let quarter = std::f32::consts::FRAC_PI_2;

    match dir {
        // The anchor: the south face's `u` runs west to east and its `v` runs bottom to top, and the
        // other five faces are this frame turned to face where they face.
        Direction::South => Mat3::IDENTITY,
        Direction::East => Mat3::from_rotation_y(quarter),
        Direction::West => Mat3::from_rotation_y(-quarter),
        Direction::North => Mat3::from_rotation_y(std::f32::consts::PI),
        Direction::Up => Mat3::from_rotation_x(-quarter),
        Direction::Down => Mat3::from_rotation_x(quarter),
    }
}

/// The matrix a face's UVs are put through when the variant sets `uvlock`.
///
/// `uvlock` means "the texture does not turn with the model": the geometry is rotated either way, and
/// this is the transform that turns the texture *back*, so a locked face looks the same however its
/// variant is rotated. Minecraft's own is
/// `GLOBAL_TO_LOCAL[newSide] * rotation * LOCAL_TO_GLOBAL[declared]`, where `newSide` is the face the
/// rotated normal ends up pointing at and the UV is transformed as the point `(u - 0.5, v - 0.5, 0)`
/// in sprite-normalized coordinates.
///
/// Without `uvlock` this is the identity, and the UVs simply stay attached to the corners the rotation
/// moves - which is what a player sees as the texture turning with the block.
fn uv_lock_matrix(rotation: ModelRotation, declared: Direction) -> Mat3 {
    if rotation.is_identity() {
        return Mat3::IDENTITY;
    }

    // The face this one *becomes*: Minecraft takes the rotated normal of the declared face and asks
    // which direction that is nearest - `faceAction` is `rotation ∘ LOCAL_TO_GLOBAL[declared]`, and
    // the local `(0, 0, 1)` is the declared face's own normal in that frame, not the south one.
    let turned = rotation.rotate_direction(declared);
    let matrix = rotation.matrix();

    uv_frame(turned).transpose() * matrix * uv_frame(declared)
}

/// Textures a model named and the resource provider could not read, over the whole run.
///
/// The step before [`MISSING_SPRITES`]: a texture that cannot be read is a sprite that is never packed
/// into the atlas, so every face that samples it is dropped a moment later. Both are counted
/// separately because the two have different causes - a missing file in a resource pack against a
/// sprite the atlas never allocated - and the same symptom: a block with a hole, or with nothing in it
/// at all. See `blockBakeDiagnostics`, which reports both to the game's own log.
pub static UNREADABLE_TEXTURES: MissingSprites = MissingSprites::new();

/// Sprites a face asked the atlas for and did not find, over the whole run.
///
/// A face whose sprite is missing is **dropped**, and that is deliberate: a block with one bad texture
/// reference must not take the block registry down with it. What it costs is a hole in a block - or a
/// block drawn as nothing at all, when the face that was dropped was the only one the model had - and
/// *nothing in any log*, which is the part that is not deliberate. A state that baked to an empty mesh
/// is a state with a perfectly good key, so it is also a state that still occludes its neighbours: the
/// block is invisible and the block behind it loses the face between them, and the two together look
/// like a hole in the world rather than like a texture that could not be found.
///
/// That is what a mushroom cap did for a whole session of looking at the wrong thing. Counting the
/// drops and remembering the first few names is what makes the next one a line instead of a hunt; the
/// JVM side logs the answer after the block cache, where its own log lives. See `blockBakeDiagnostics`.
pub static MISSING_SPRITES: MissingSprites = MissingSprites::new();

/// How many sprites [`MISSING_SPRITES`] remembers by name, so a broken pack cannot grow it forever.
const MISSING_SPRITE_NAMES: usize = 24;

#[derive(Debug)]
pub struct MissingSprites {
    faces: std::sync::atomic::AtomicU64,
    names: parking_lot::Mutex<Vec<String>>,
}

impl MissingSprites {
    const fn new() -> Self {
        Self {
            faces: std::sync::atomic::AtomicU64::new(0),
            names: parking_lot::Mutex::new(Vec::new()),
        }
    }

    /// Notes one dropped face, and the sprite it wanted.
    pub fn note(&self, sprite: &ResourcePath) {
        self.faces
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let mut names = self.names.lock();

        if names.len() < MISSING_SPRITE_NAMES && !names.iter().any(|name| name == &sprite.0) {
            names.push(sprite.0.clone());
        }
    }

    /// How many faces have been dropped for a sprite the atlas does not have.
    pub fn faces(&self) -> u64 {
        self.faces.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The first few sprites that went missing, by name.
    pub fn names(&self) -> Vec<String> {
        self.names.lock().clone()
    }
}

/// One element face, as the baker needs it: the sprite's corners in the atlas, the animation offset,
/// the tint index, the layer the sprite puts it in, and the direction it declared for culling.
struct FaceData {
    uv: UV,
    animation_uv_offset: u32,
    tint_index: i32,
    layer: RenderLayer,
    cull: Option<Direction>,
}

/// Reads one element face: its atlas UVs, and everything that is not geometry.
///
/// The UVs are the one part of a face that can be turned twice - once by the face's own `rotation`
/// (in [`get_atlas_uv`]) and once by the variant's `uvlock` - and both are applied here, in that
/// order, because that is the order Minecraft applies them in. The `cullface` is turned too, by the
/// variant's rotation alone.
fn face_data(
    tex: &schemas::models::ElementFace,
    declared: Direction,
    atlas: &Atlas,
    rotation: ModelRotation,
    uv_lock: bool,
) -> Option<FaceData> {
    let Some(uv) = get_atlas_uv(tex, atlas) else {
        // The one silent way this baker can fail, and the reason `MISSING_SPRITES` exists: the face is
        // gone, and whether the block is drawn with a hole in it or not at all is not something any
        // other line in the log can tell you.
        MISSING_SPRITES.note(&(&tex.texture.0).into());

        return None;
    };

    let texture: ResourcePath = (&tex.texture.0).into();

    let uv = if uv_lock {
        // `get_atlas_uv` above already found this sprite in the same map under the same key, so this
        // cannot be what makes a locked face disappear - it is here so that the sprite's rectangle is
        // read from one place rather than threaded through.
        let Some(sprite) = atlas.uv_map.read().get(&texture).copied() else {
            return None;
        };

        lock_uv(uv, sprite, uv_lock_matrix(rotation, declared))
    } else {
        uv
    };

    Some(FaceData {
        uv,
        animation_uv_offset: *atlas
            .animated_texture_offsets
            .read()
            .get(&texture)
            .unwrap_or(&0),
        tint_index: tex.tint_index,
        layer: atlas.sprite_layer(&texture).unwrap_or(RenderLayer::Solid),
        // Rotated as Minecraft rotates it, so the direction that comes out is the neighbour this face
        // now touches rather than the one the model file was written against. The culling loop tests
        // this one against the world as baked.
        cull: tex.cull_face.map(|face| {
            rotation.rotate_direction(match face {
                schemas::models::BlockFace::Down => Direction::Down,
                schemas::models::BlockFace::Up => Direction::Up,
                schemas::models::BlockFace::North => Direction::North,
                schemas::models::BlockFace::South => Direction::South,
                schemas::models::BlockFace::West => Direction::West,
                schemas::models::BlockFace::East => Direction::East,
            })
        }),
    })
}

/// Turns a face's four atlas corners by a uv-lock transform.
///
/// The transform is Minecraft's, which works in *sprite-normalized* coordinates: `(0, 0)` is one
/// corner of the sprite and `(1, 1)` the other, whatever the sprite's pixel size. So the corners are
/// taken out of atlas space, turned about the middle of the sprite, and put back - which is why the
/// sprite's own rectangle is needed and not just the face's.
fn lock_uv(uv: UV, sprite: UV, matrix: Mat3) -> UV {
    let width = (sprite.1.0 as f32 - sprite.0.0 as f32).max(1.0);
    let height = (sprite.1.1 as f32 - sprite.0.1 as f32).max(1.0);

    let turn = |corner: (u16, u16)| {
        let u = (corner.0 as f32 - sprite.0.0 as f32) / width - 0.5;
        let v = (corner.1 as f32 - sprite.0.1 as f32) / height - 0.5;

        let turned = matrix * vec3(u, v, 0.0);

        (
            (sprite.0.0 as f32 + (turned.x + 0.5) * width).round() as u16,
            (sprite.0.1 as f32 + (turned.y + 0.5) * height).round() as u16,
        )
    };

    (turn(uv.0), turn(uv.1))
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
    /// Whether this mesh has no faces at all: a block that is baked, keyed, drawn - and invisible.
    ///
    /// Worth asking because it is the shape of a whole class of failure: the state has a mesh (so it
    /// culls its neighbours like any other block) and the mesh has nothing in it. A model whose cases
    /// did not match, a model whose every face was dropped for a sprite the atlas does not have - both
    /// end here, and both look like a hole in the world rather than like a missing texture.
    pub fn is_empty(&self) -> bool {
        self.north.is_empty()
            && self.south.is_empty()
            && self.west.is_empty()
            && self.east.is_empty()
            && self.up.is_empty()
            && self.down.is_empty()
            && self.any.is_empty()
    }
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

                // The variant's own rotation and its `uvlock`, which belong to *this* model property
                // and are applied to its faces before they are merged with the others'. A multipart
                // model is several properties, each with its own rotation - and the mesh of a state
                // is merged, so the turn has to happen here rather than on a shared unrotated model.
                let rotation = ModelRotation::new(model_properties.x, model_properties.y);
                let uv_lock = model_properties.uv_lock;

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
                                    UNREADABLE_TEXTURES.note(&texture_path);

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
                        // Each face is handed to `face_data`, which reads its UVs, its layer and the
                        // direction it declared for culling, and applies the face's own rotation
                        // followed by the variant's `uvlock` to the UVs.
                        let north = element
                            .faces
                            .get(&schemas::models::BlockFace::North)
                            .and_then(|tex| face_data(tex, Direction::North, block_atlas, rotation, uv_lock));

                        let east = element
                            .faces
                            .get(&schemas::models::BlockFace::East)
                            .and_then(|tex| face_data(tex, Direction::East, block_atlas, rotation, uv_lock));

                        let south = element
                            .faces
                            .get(&schemas::models::BlockFace::South)
                            .and_then(|tex| face_data(tex, Direction::South, block_atlas, rotation, uv_lock));

                        let west = element
                            .faces
                            .get(&schemas::models::BlockFace::West)
                            .and_then(|tex| face_data(tex, Direction::West, block_atlas, rotation, uv_lock));

                        let up = element
                            .faces
                            .get(&schemas::models::BlockFace::Up)
                            .and_then(|tex| face_data(tex, Direction::Up, block_atlas, rotation, uv_lock));

                        let down = element
                            .faces
                            .get(&schemas::models::BlockFace::Down)
                            .and_then(|tex| face_data(tex, Direction::Down, block_atlas, rotation, uv_lock));

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
                            // The element's own rotation first, then the variant's - which is the
                            // order Minecraft applies them in, and the reason the variant's rotation
                            // is per model property rather than per model: the same model file baked
                            // under two variants is two different meshes.
                            let v = matrix * (v - vec_origin) + vec_origin;

                            rotation.position(v)
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
                                    tex_coords: [south_face.uv.1.0, south_face.uv.1.1],
                                },
                                BlockMeshVertex {
                                    position: p111,
                                    tex_coords: [south_face.uv.1.0, south_face.uv.0.1],
                                },
                                BlockMeshVertex {
                                    position: p011,
                                    tex_coords: [south_face.uv.0.0, south_face.uv.0.1],
                                },
                                BlockMeshVertex {
                                    position: p001,
                                    tex_coords: [south_face.uv.0.0, south_face.uv.1.1],
                                },
                            ],
                            normal: rotation.direction(vec3(0.0, 0.0, 1.0)),
                            tint_index: south_face.tint_index,
                            animation_uv_offset: south_face.animation_uv_offset,
                            layer: model_layer.stronger(south_face.layer),
                            cull: south_face.cull,
                        }));
                        faces.extend(west.map(|west_face| BlockModelFace {
                            vertices: [
                                BlockMeshVertex {
                                    position: p001,
                                    tex_coords: [west_face.uv.1.0, west_face.uv.1.1],
                                },
                                BlockMeshVertex {
                                    position: p011,
                                    tex_coords: [west_face.uv.1.0, west_face.uv.0.1],
                                },
                                BlockMeshVertex {
                                    position: p010,
                                    tex_coords: [west_face.uv.0.0, west_face.uv.0.1],
                                },
                                BlockMeshVertex {
                                    position: p000,
                                    tex_coords: [west_face.uv.0.0, west_face.uv.1.1],
                                },
                            ],
                            normal: rotation.direction(vec3(-1.0, 0.0, 0.0)),
                            tint_index: west_face.tint_index,
                            animation_uv_offset: west_face.animation_uv_offset,
                            layer: model_layer.stronger(west_face.layer),
                            cull: west_face.cull,
                        }));
                        faces.extend(north.map(|north_face| BlockModelFace {
                            vertices: [
                                BlockMeshVertex {
                                    position: p000,
                                    tex_coords: [north_face.uv.1.0, north_face.uv.1.1],
                                },
                                BlockMeshVertex {
                                    position: p010,
                                    tex_coords: [north_face.uv.1.0, north_face.uv.0.1],
                                },
                                BlockMeshVertex {
                                    position: p110,
                                    tex_coords: [north_face.uv.0.0, north_face.uv.0.1],
                                },
                                BlockMeshVertex {
                                    position: p100,
                                    tex_coords: [north_face.uv.0.0, north_face.uv.1.1],
                                },
                            ],
                            normal: rotation.direction(vec3(0.0, 0.0, -1.0)),
                            tint_index: north_face.tint_index,
                            animation_uv_offset: north_face.animation_uv_offset,
                            layer: model_layer.stronger(north_face.layer),
                            cull: north_face.cull,
                        }));
                        faces.extend(east.map(|east_face| BlockModelFace {
                            vertices: [
                                BlockMeshVertex {
                                    position: p100,
                                    tex_coords: [east_face.uv.1.0, east_face.uv.1.1],
                                },
                                BlockMeshVertex {
                                    position: p110,
                                    tex_coords: [east_face.uv.1.0, east_face.uv.0.1],
                                },
                                BlockMeshVertex {
                                    position: p111,
                                    tex_coords: [east_face.uv.0.0, east_face.uv.0.1],
                                },
                                BlockMeshVertex {
                                    position: p101,
                                    tex_coords: [east_face.uv.0.0, east_face.uv.1.1],
                                },
                            ],
                            normal: rotation.direction(vec3(1.0, 0.0, 0.0)),
                            tint_index: east_face.tint_index,
                            animation_uv_offset: east_face.animation_uv_offset,
                            layer: model_layer.stronger(east_face.layer),
                            cull: east_face.cull,
                        }));
                        faces.extend(up.map(|up_face| BlockModelFace {
                            vertices: [
                                BlockMeshVertex {
                                    position: p010,
                                    tex_coords: [up_face.uv.1.0, up_face.uv.1.1],
                                },
                                BlockMeshVertex {
                                    position: p011,
                                    tex_coords: [up_face.uv.1.0, up_face.uv.0.1],
                                },
                                BlockMeshVertex {
                                    position: p111,
                                    tex_coords: [up_face.uv.0.0, up_face.uv.0.1],
                                },
                                BlockMeshVertex {
                                    position: p110,
                                    tex_coords: [up_face.uv.0.0, up_face.uv.1.1],
                                },
                            ],
                            normal: rotation.direction(vec3(0.0, 1.0, 0.0)),
                            tint_index: up_face.tint_index,
                            animation_uv_offset: up_face.animation_uv_offset,
                            layer: model_layer.stronger(up_face.layer),
                            cull: up_face.cull,
                        }));

                        faces.extend(down.map(|down_face| BlockModelFace {
                            vertices: [
                                BlockMeshVertex {
                                    position: p000,
                                    tex_coords: [down_face.uv.1.0, down_face.uv.1.1],
                                },
                                BlockMeshVertex {
                                    position: p100,
                                    tex_coords: [down_face.uv.1.0, down_face.uv.0.1],
                                },
                                BlockMeshVertex {
                                    position: p101,
                                    tex_coords: [down_face.uv.0.0, down_face.uv.0.1],
                                },
                                BlockMeshVertex {
                                    position: p001,
                                    tex_coords: [down_face.uv.0.0, down_face.uv.1.1],
                                },
                            ],
                            normal: rotation.direction(vec3(0.0, -1.0, 0.0)),
                            tint_index: down_face.tint_index,
                            animation_uv_offset: down_face.animation_uv_offset,
                            layer: model_layer.stronger(down_face.layer),
                            cull: down_face.cull,
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

/// The variant rotation, and the uv-lock that undoes it for the texture. See [`ModelRotation`] and
/// [`uv_lock_matrix`].
#[cfg(test)]
mod rotation_tests {
    use super::*;

    /// A quarter turn about Y takes the model's north face to face east - "90° clockwise seen from
    /// above", which is what the blockstate field means.
    #[test]
    fn a_quarter_turn_about_y_takes_north_to_east() {
        let turn = ModelRotation::new(0, 90);

        assert_eq!(turn.rotate_direction(Direction::North), Direction::East);
        assert_eq!(turn.rotate_direction(Direction::East), Direction::South);
        assert_eq!(turn.rotate_direction(Direction::Up), Direction::Up, "the axis it turns about");
    }

    /// A quarter turn about X tips the model's top towards -z: the up face ends up pointing **north**.
    ///
    /// The sign is the whole reason this test exists, and it was written the other way round once. The
    /// model that says which is right is `amethyst_cluster`: its model points up, and its
    /// `facing=north` variant is `{"x": 90}`.
    #[test]
    fn a_quarter_turn_about_x_takes_up_to_north() {
        let tip = ModelRotation::new(90, 0);

        assert_eq!(tip.rotate_direction(Direction::Up), Direction::North);
        assert_eq!(tip.rotate_direction(Direction::South), Direction::Up);
        assert_eq!(tip.rotate_direction(Direction::East), Direction::East, "the axis it turns about");

        // And the position form has to agree with it, both ways round: the quad a single-face model
        // draws on `up` ends at the north boundary, and the one it draws on `north` ends at the bottom.
        assert_eq!(
            tip.position(vec3(0.5, 1.0, 0.5)),
            vec3(0.5, 0.5, 0.0),
            "the up quad goes to the north boundary, which is where `north` is"
        );
        assert_eq!(
            tip.position(vec3(0.5, 0.5, 0.0)),
            vec3(0.5, 0.0, 0.5),
            "and the north quad goes to the bottom of the cube, which is where `down` is"
        );
    }

    /// The wall variant of a cluster, end to end: up, then turned about Y onto the wall it names.
    ///
    /// `facing=east` is `{"x": 90, "y": 90}` in vanilla, so the model has to come out pointing east -
    /// and it only does with both signs right.
    #[test]
    fn a_wall_cluster_points_at_the_wall_it_names() {
        for (facing, y) in [
            (Direction::East, 90),
            (Direction::South, 180),
            (Direction::West, 270),
            (Direction::North, 0),
        ] {
            let turn = ModelRotation::new(90, y);

            assert_eq!(
                turn.rotate_direction(Direction::Up),
                facing,
                "facing={facing:?} is {{\"x\": 90, \"y\": {y}}}"
            );
        }
    }

    /// A huge mushroom's cap, which is what found the `x` sign and is the shape it costs most.
    ///
    /// `brown_mushroom_block.json` is six copies of one model - a single quad on `north`, from
    /// `template_single_face` - each turned so that the quad lands on the side its `when` names, with
    /// `uvlock` because the skin must not turn with it. So for each of the six: the quad has to lie on
    /// that boundary, and the turned normal has to point out of it. When `x: 270` was read as the
    /// other quarter turn, the `up` piece ended up on the *bottom* of the cube facing down - and
    /// because the block below a cap is another mushroom block, which occludes, that piece was then
    /// culled: a mushroom cap with no top.
    #[test]
    fn a_mushroom_cap_skin_lands_on_the_side_it_names() {
        let quad = [
            vec3(0.0, 0.0, 0.0),
            vec3(1.0, 0.0, 0.0),
            vec3(1.0, 1.0, 0.0),
            vec3(0.0, 1.0, 0.0),
        ];

        for (named, x, y) in [
            (Direction::North, 0, 0),
            (Direction::East, 0, 90),
            (Direction::South, 0, 180),
            (Direction::West, 0, 270),
            (Direction::Up, 270, 0),
            (Direction::Down, 90, 0),
        ] {
            let rotation = ModelRotation::new(x, y);

            assert_eq!(
                nearest_direction(rotation.direction(vec3(0.0, 0.0, -1.0))),
                named,
                "{named:?} is {{\"x\": {x}, \"y\": {y}}}: the quad faces away from the side it names"
            );

            // How far the turned quad is off the boundary it has to sit on.
            let off = |v: Vec3| {
                let p = rotation.position(v);

                match named {
                    Direction::North => p.z,
                    Direction::South => p.z - 1.0,
                    Direction::West => p.x,
                    Direction::East => p.x - 1.0,
                    Direction::Down => p.y,
                    Direction::Up => p.y - 1.0,
                }
            };

            for corner in quad {
                let distance = off(corner);

                assert!(
                    distance.abs() < 1e-5,
                    "{named:?} is {{\"x\": {x}, \"y\": {y}}}: {corner} landed {distance} off the \
                     {named:?} boundary"
                );
            }
        }
    }

    /// A direction is turned by the rotation's matrix, not by its position form: the `1.0 -` in the
    /// position form is where the middle of the block is, and a normal has no position to be folded
    /// about. The two agree only by accident - here on a corner of the cube that both leave at a
    /// corner.
    #[test]
    fn a_direction_is_not_a_position() {
        let turn = ModelRotation::new(0, 90);

        assert_eq!(turn.position(vec3(0.0, 1.0, 0.0)), vec3(1.0, 1.0, 0.0));
        assert_eq!(
            turn.direction(vec3(0.0, 1.0, 0.0)),
            vec3(0.0, 1.0, 0.0),
            "the up direction is along the axis of the turn and cannot move"
        );
    }

    /// The uv-lock case worth writing down, hand-derived from Minecraft's own formula.
    ///
    /// A **north** face of a model turned 90° about X becomes the **up** face. The transform is
    /// `GLOBAL_TO_LOCAL[up] * rotX(90) * LOCAL_TO_GLOBAL[north]`, and with those frames -
    /// `rotX(-90)` and `rotY(180)` - it is `rotX(180) * rotY(180)`: the sprite is turned half way
    /// round, `(u, v)` becoming `(1 - u, 1 - v)`.
    #[test]
    fn a_locked_face_turns_its_texture_by_what_the_model_turns() {
        let matrix = uv_lock_matrix(ModelRotation::new(90, 0), Direction::North);

        let turned = |u: f32, v: f32| {
            let out = matrix * vec3(u - 0.5, v - 0.5, 0.0);
            (out.x + 0.5, out.y + 0.5)
        };

        for (u, v, want_u, want_v) in [
            (0.0, 0.0, 1.0, 1.0),
            (1.0, 0.0, 0.0, 1.0),
            (0.25, 1.0, 0.75, 0.0),
        ] {
            let (got_u, got_v) = turned(u, v);

            assert!(
                (got_u - want_u).abs() < 1e-5 && (got_v - want_v).abs() < 1e-5,
                "({u}, {v}) landed on ({got_u}, {got_v}), not ({want_u}, {want_v})"
            );
        }
    }

    /// And the case that is *not* a turn: a north face turned 90° about Y becomes the east face, and
    /// those two frames compose with the rotation to the identity - the texture keeps every corner it
    /// had. It is still locked: the face it is on has turned, and the numbers on it have not.
    ///
    /// Compared by what it does rather than by `==`: a product of three rotations is the identity in
    /// exact arithmetic and not in `f32`.
    #[test]
    fn a_lock_that_comes_out_as_no_change_at_all() {
        let matrix = uv_lock_matrix(ModelRotation::new(0, 90), Direction::North);

        for corner in [
            vec3(-0.5, -0.5, 0.0),
            vec3(0.5, -0.5, 0.0),
            vec3(0.5, 0.5, 0.0),
            vec3(-0.5, 0.5, 0.0),
        ] {
            let out = matrix * corner;

            assert!(
                (out - corner).length() < 1e-5,
                "{corner} landed on {out}"
            );
        }
    }

    /// No rotation, no transform: an unrotated model's UVs are the ones the model writes, whether
    /// `uvlock` is set or not. The flag is about what a rotation does to them.
    #[test]
    fn a_model_that_is_not_turned_has_its_uvs_left_alone() {
        for declared in [
            Direction::North,
            Direction::South,
            Direction::East,
            Direction::West,
            Direction::Up,
            Direction::Down,
        ] {
            assert_eq!(
                uv_lock_matrix(ModelRotation::new(0, 0), declared),
                Mat3::IDENTITY,
                "{declared:?} with no rotation"
            );
            assert_eq!(
                uv_lock_matrix(ModelRotation::new(360, 0), declared),
                Mat3::IDENTITY,
                "{declared:?} with a full turn, which is no turn"
            );
        }
    }

    /// The transform stays in the sprite's plane and maps its unit square onto itself, with no
    /// scaling - which is what keeps a locked texture the same size as an unlocked one.
    #[test]
    fn a_lock_transform_is_a_rotation_of_the_sprite() {
        for x in [0, 90, 180, 270] {
            for y in [0, 90, 180, 270] {
                for declared in [Direction::North, Direction::East, Direction::Up, Direction::Down] {
                    let matrix = uv_lock_matrix(ModelRotation::new(x, y), declared);

                    for corner in [
                        vec3(-0.5, -0.5, 0.0),
                        vec3(0.5, -0.5, 0.0),
                        vec3(0.5, 0.5, 0.0),
                        vec3(-0.5, 0.5, 0.0),
                    ] {
                        let out = matrix * corner;

                        assert!(
                            out.z.abs() < 1e-5,
                            "x={x} y={y} {declared:?}: the transform left the sprite's plane"
                        );
                        assert!(
                            out.x.abs() <= 0.5001 && out.y.abs() <= 0.5001,
                            "x={x} y={y} {declared:?}: {out} is outside the sprite"
                        );
                    }
                }
            }
        }
    }

    /// Which sprite corners the lock transform lands on, for every rotation the blockstate format can
    /// write: the corners have to stay corners, however the sprite is turned.
    #[test]
    fn the_corners_stay_the_corners() {
        for x in [0, 90, 180, 270] {
            for y in [0, 90, 180, 270] {
                for declared in [Direction::North, Direction::South, Direction::Up, Direction::Down] {
                    let matrix = uv_lock_matrix(ModelRotation::new(x, y), declared);
                    let mut landed: Vec<(i32, i32)> =
                        [(-0.5, -0.5), (0.5, -0.5), (0.5, 0.5), (-0.5, 0.5)]
                            .iter()
                            .map(|(u, v)| {
                                let out = matrix * vec3(*u, *v, 0.0);
                                (out.x.round() as i32, out.y.round() as i32)
                            })
                            .collect();

                    landed.sort();
                    assert_eq!(
                        landed,
                        vec![(-1, -1), (-1, 1), (1, -1), (1, 1)],
                        "x={x} y={y} {declared:?}: the transform is not a turn of the sprite"
                    );
                }
            }
        }
    }
}
