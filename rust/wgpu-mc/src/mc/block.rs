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
use crate::render::pipeline::{UV_GAME_ATLAS, UV_GAME_SCALE};
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
    /// The ten bits the vertex format reserves per vertex for an animated texture. See
    /// [`crate::render::pipeline::UV_GAME_ATLAS`]: the one bit that carries anything says this face's
    /// UVs are in the game's own atlas rather than in this side's copy of its sprite.
    pub uv_flags: u32,
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
/// Two masks, one bit per direction, and one flag, all computed on the JVM side when the block's key is
/// handed out (`Wgpu#helperSetBlockStateIndex`) and keyed by the state the section palette carries:
///
/// - `occlusion`: `state.getFaceOcclusionShape(dir) == Shapes.block()`. The state's own shape covers
///   that whole face, so a neighbour's face against it is not drawn. Note that this is *not* the
///   model: glass, ice, leaves and every plant have a full-cube model and none of them occlude
///   anything, which is exactly the difference this type exists for.
/// - `self_hide`: `state.skipRendering(state, dir)`. The block leaves out the face between itself
///   and a neighbour of its own kind - glass against glass, ice against ice, the bars of a pane, a
///   fluid against itself.
/// - `shades`: `state.getShadeBrightness(level, pos) < 1`, which is what the ambient-occlusion corner
///   test reads: a block that answers `0.2` darkens the corners it touches, and one that answers `1.0`
///   does not. **Not derivable from the two masks above**, which is why it is sent: `IceBlock` and
///   `TransparentBlock` are both full cubes with an empty occlusion shape - glass and ice look identical
///   from up here - and only glass overrides `getShadeBrightness` to `1.0`. Ice darkens corners in
///   vanilla and glass does not, and nothing but the block's own answer tells them apart.
/// - `blocks_motion`: `state.blocksMotion()`, the game's own "would a fluid flow past this, or into
///   it". One caller: `FlowingFluid#getFlow` looks *below* a neighbour that has no fluid of its own and
///   does not block motion, which is how a stream's surface learns to point at the drop it is about to
///   fall down. The three things this side has that look similar are all wrong for it - a plant does not
///   occlude, a slab is not a full cube, and neither of those is "blocks motion" - so it is read.
/// - `offset_max_y`: the **maximum vertical offset** of the block's own random placement, or zero either
///   for a block that stands where it was placed or for one whose offset is horizontal only. See
///   [`FaceFlags::block_offset`] for what it is used for and why the horizontal limit does not travel.
/// - `offset_xz`: whether the block is offset **at all**. Not the same question as the limit above, and
///   the difference is a whole class of blocks: a flower's offset type is `XZ`, whose vertical limit is
///   exactly zero, so a block that only carried the limit would be indistinguishable from a block with
///   no offset and every flower would stand dead centre - which is the bug this pair exists to avoid.
///
/// The bits are in Java's `Direction.ordinal()` order, because that is the side that computes them;
/// read them with [`crate::mc::direction::java_mask_has`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FaceFlags {
    pub occlusion: u8,
    pub self_hide: u8,
    pub shades: bool,
    pub blocks_motion: bool,
    pub offset_max_y: f32,
    pub offset_xz: bool,
}

/// `Mth.getSeed`, the hash every block's random offset is derived from.
///
/// ```java
/// public static long getSeed(int x, int y, int z) {
///     long seed = x * 3129871 ^ z * 116129781L ^ y;
///     seed = seed * seed * 42317861L + seed * 11L;
///     return seed >> 16;
/// }
/// ```
///
/// Wrapping arithmetic throughout, because that is what Java's `long` does and the whole point is to
/// agree with it bit for bit: the square in the middle overflows for any coordinate worth placing.
pub fn block_seed(x: i32, y: i32, z: i32) -> i64 {
    let seed = (x as i64).wrapping_mul(3_129_871) ^ (z as i64).wrapping_mul(116_129_781) ^ y as i64;

    seed.wrapping_mul(seed)
        .wrapping_mul(42_317_861)
        .wrapping_add(seed.wrapping_mul(11))
        >> 16
}

impl FaceFlags {
    /// The flags two states that wear the same model agree on.
    ///
    /// A packed key can be worn by more than one state - a property the blockstate file does not
    /// vary its model on, `waterlogged` for one - and what these bits decide is whether a *face* is
    /// left out of the mesh, where being wrong means a hole in the world. So a bit survives only
    /// where every state with this model has it, and a disagreement costs an invisible face between
    /// two blocks rather than a missing one.
    ///
    /// `shades` follows the same rule for a different reason: a corner that two states of one key
    /// disagree about is a corner that is *not* darkened, which is the direction that draws less
    /// shadow than vanilla rather than shadow where vanilla has none. `blocks_motion` is the same
    /// trade again: a neighbour that two states disagree about is one a fluid does not look past,
    /// which is the flow it would have had without the flag at all.
    ///
    /// `offset_max_y` is the odd one out and follows the strictest rule: it survives only where the two
    /// agree *exactly*, and a disagreement clears it rather than picking one. An offset placed wrong is
    /// a plant standing inside the block beside it, so a key whose states cannot agree on how far up
    /// they float is a key whose plants do not float - the smaller error. `offset_xz` is the same trade
    /// and for the same reason, so a key is offset only when every state wearing it is.
    pub fn and(self, other: FaceFlags) -> FaceFlags {
        FaceFlags {
            occlusion: self.occlusion & other.occlusion,
            self_hide: self.self_hide & other.self_hide,
            shades: self.shades && other.shades,
            blocks_motion: self.blocks_motion && other.blocks_motion,
            offset_max_y: if self.offset_max_y == other.offset_max_y {
                self.offset_max_y
            } else {
                0.0
            },
            offset_xz: self.offset_xz && other.offset_xz,
        }
    }

    /// Whether this state stands somewhere other than where it was placed.
    pub fn has_offset(self) -> bool {
        self.offset_xz
    }

    /// Whether the state's shape covers the whole face in this direction.
    pub fn occludes(self, dir: Direction) -> bool {
        java_mask_has(self.occlusion, dir)
    }

    /// Whether the state hides its own face toward a neighbour of the same kind in this direction.
    pub fn hides_same_state(self, dir: Direction) -> bool {
        java_mask_has(self.self_hide, dir)
    }

    /// Where this block actually stands, which for a plant is **not** where it was placed.
    ///
    /// Minecraft gives `short_grass`, `fern`, `bush`, `sugar_cane` and every flower a random offset
    /// derived from the block's own coordinates, so that a field of them is a field and not a grid
    /// (`BlockBehaviour.Properties#offsetType`). This is that function, verbatim, for the `XZ` and `XYZ`
    /// types the game uses - `NONE` is the zero this returns when [`FaceFlags::has_offset`] is false:
    ///
    /// ```java
    /// long seed = Mth.getSeed(pos.getX(), 0, pos.getZ());
    /// double y = ((float)(seed >>  4 & 15L) / 15.0F - 1.0) * block.getMaxVerticalOffset();
    /// double x = Mth.clamp(((float)(seed       & 15L) / 15.0F - 0.5) * 0.5, -maxH, maxH);
    /// double z = Mth.clamp(((float)(seed >>  8 & 15L) / 15.0F - 0.5) * 0.5, -maxH, maxH);
    /// ```
    ///
    /// Three things about it are worth knowing because each is a way to get it almost right:
    ///
    ///  - the **hash takes `y = 0`**, not the block's own `y`. A plant at the top of a hill and one at
    ///    the bottom of the same column get the same offset, which is part of what makes a field look
    ///    planted rather than shaken;
    ///  - `x` and `z` are **clamped** but `y` is not, and the vertical term is `bits / 15 - 1`, so it
    ///    runs `-maxY .. 0` - a plant may sink into the ground and may not float above it;
    ///  - that clamp's argument is `(bits/15 - 0.5) * 0.5`, which spans `-0.25 .. 0.25` **exactly** - the
    ///    two ends are reached at `bits = 0` and `bits = 15`, which do occur - so the game's default
    ///    limit of `0.25` is reproduced here and is enough for every block the game offsets except
    ///    pointed dripstone, which raises it and is not a plant. That is why only `maxY` crosses the
    ///    bridge and why the horizontal limit is a constant here rather than another field.
    pub fn block_offset(self, x: i32, z: i32) -> Vec3 {
        if !self.has_offset() {
            return Vec3::ZERO;
        }

        let seed = block_seed(x, 0, z);

        // `getMaxHorizontalOffset` is `0.25` for everything the game offsets except pointed dripstone,
        // which is not a plant and which this side does not bake.
        const MAX_HORIZONTAL: f32 = 0.25;

        let horizontal = |shift: u32| {
            (((seed >> shift & 15) as f32 / 15.0 - 0.5) * 0.5)
                .clamp(-MAX_HORIZONTAL, MAX_HORIZONTAL)
        };
        let vertical = ((seed >> 4 & 15) as f32 / 15.0 - 1.0) * self.offset_max_y;

        vec3(horizontal(0), vertical, horizontal(8))
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
        if v.x >= 0.0 {
            Direction::East
        } else {
            Direction::West
        }
    } else if abs.y >= abs.z {
        if v.y >= 0.0 {
            Direction::Up
        } else {
            Direction::Down
        }
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
    /// The paths a warning line has already been written for, so one missing texture is one line
    /// rather than one per model that names it.
    ///
    /// Here rather than in a `static` beside the warning, because a resource reload has to be able to
    /// forget it: a pack that fixes a texture is a texture whose *absence* has to be reportable again
    /// if the next pack removes it. A `Vec` rather than a set because it has to be constructible in a
    /// `static`, and because the list only ever holds the handful of paths a pack could not produce.
    warned: parking_lot::Mutex<Vec<String>>,
}

impl MissingSprites {
    const fn new() -> Self {
        Self {
            faces: std::sync::atomic::AtomicU64::new(0),
            names: parking_lot::Mutex::new(Vec::new()),
            warned: parking_lot::Mutex::new(Vec::new()),
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

    /// Whether this path has not been warned about yet, and remembers that it has been.
    pub fn warn_once(&self, path: &ResourcePath) -> bool {
        let mut warned = self.warned.lock();

        if warned.iter().any(|known| known == &path.0) {
            return false;
        }

        warned.push(path.0.clone());

        true
    }

    /// Forgets everything counted and everything warned about, for the next pack.
    ///
    /// Called when a resource reload starts: the counters are what the reload's own diagnostics
    /// report, and a count that carries the previous pack's failures into the new one is a number that
    /// describes neither.
    pub fn reset(&self) {
        self.faces.store(0, std::sync::atomic::Ordering::Relaxed);
        self.names.lock().clear();
        self.warned.lock().clear();
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

/// One element face, as the baker needs it: the sprite's corners in whichever atlas the face belongs
/// to, the vertex flags that say which one that is, the tint index, the layer the sprite puts it in,
/// and the direction it declared for culling.
struct FaceData {
    uv: UV,
    uv_flags: u32,
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
///
/// A face that writes no `uv` is handed the one its element's box implies, in the units a model file
/// writes them in. That happens here rather than at the six call sites because it is the same field
/// either way, and because `drawXFaces`-style callers do not exist: every face is read by this
/// function, so every face gets the default - see [`default_face_uv`].
///
/// Which atlas those UVs are *in* is the other half, and it is decided by
/// [`Atlas::game_atlas_rect`]: a face whose sprite the game animates and whose atlas the pass can
/// sample is baked with the game's coordinates and flagged for it, and every other face keeps the
/// coordinates this side packed. The two are not interchangeable - the scales differ, and so do the
/// atlases - which is why the flag and the coordinates are written together, here, and never apart.
fn face_data(
    tex: &schemas::models::ElementFace,
    bounds: ElementBounds,
    declared: Direction,
    atlas: &Atlas,
    rotation: ModelRotation,
    uv_lock: bool,
) -> Option<FaceData> {
    let texture: ResourcePath = (&tex.texture.0).into();

    // `#all` and every other reference is resolved by `resolve_model` before this runs, so the sprite
    // named here is a concrete one - or one the atlas does not have, which `get_atlas_uv` reports.
    let uv = tex.uv.unwrap_or_else(|| default_face_uv(bounds, declared));

    // `Some` only for a sprite the game animates in an atlas the built pass samples: see
    // `Atlas::game_atlas_rect`, which is the whole of the decision.
    let game_rect = atlas.game_atlas_rect(&texture);

    let (uv, uv_flags) = match game_rect {
        Some(rect) => (get_game_atlas_uv(uv, tex.rotation, rect), UV_GAME_ATLAS),
        None => {
            let Some(uv) = get_atlas_uv(uv, tex.rotation, atlas, &texture) else {
                // The one silent way this baker can fail, and the reason `MISSING_SPRITES` exists: the
                // face is gone, and whether the block is drawn with a hole in it or not at all is not
                // something any other line in the log can tell you.
                MISSING_SPRITES.note(&texture);

                return None;
            };

            (uv, 0)
        }
    };

    let uv = if uv_lock {
        // `lock_uv` turns the corners about the middle of the sprite's rectangle, so the rectangle has
        // to be in the same units as the corners it is given: this side's atlas pixels, or the game's
        // own coordinates at the same scale the corners went through (`GAME_UV_SCALE`).
        let sprite = match game_rect {
            Some(rect) => game_rect_to_bits(rect),
            // `get_atlas_uv` above already found this sprite in the same map under the same key, so
            // this cannot be what makes a locked face disappear - it is here so that the sprite's
            // rectangle is read from one place rather than threaded through.
            None => atlas.uv_map.read().get(&texture).copied()?,
        };

        lock_uv(uv, sprite, uv_lock_matrix(rotation, declared))
    } else {
        uv
    };

    Some(FaceData {
        uv,
        uv_flags,
        tint_index: tex.tint_index,
        layer: atlas.sprite_layer(&texture).unwrap_or(RenderLayer::Solid),
        // Rotated as Minecraft rotates it, so the direction that comes out is the neighbour this face
        // now touches rather than the one the model file was written against. The culling loop tests
        // this one against the world as baked.
        cull: tex
            .cull_face
            .map(|face| rotation.rotate_direction(direction_of(face))),
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

        // The direct parent *first*, then its own ancestors: the resolver takes each answer from the
        // first model in the chain that has one, and that has to be the nearest ancestor rather than
        // the root. It was the root's, because this pushed after recursing - so a model whose chain
        // overrides its elements halfway up got the *grandparent's* geometry. `glazed_terracotta` is
        // the one that showed it: `template_glazed_terracotta` overrides `cube`'s elements to sample
        // one `#pattern`, the flattened chain put `cube` first, and the faces came out sampling
        // `#up`/`#down`/... - keys the template never defines - so every one of the sixteen colours
        // failed to bake in all four facings and was drawn as bedrock. See
        // `ModelResolver::resolve_elements`, whose own docs say "increasing level of parenthood".
        models.push(parent_path);
        recurse_model_parents(&parent, resource_provider, models)?;
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
    if let Some((key, value)) = model.textures.as_ref().and_then(|textures| {
        textures
            .iter()
            .find(|(_key, value)| value.reference().is_some())
    }) {
        return Some(format!("key: {key} value: {value:?}"));
    }

    model
        .elements
        .iter()
        .flatten()
        .flat_map(|element| element.faces.iter())
        .find(|(_direction, face)| face.texture.reference().is_some())
        .map(|(direction, face)| format!("face {direction:?} samples {}", face.texture.0))
}

/// The direction a model file's face key names.
///
/// Three places need this and they need the same answer: the atlas UVs a face gets, the direction its
/// `cullface` is turned by, and the normal its quad is given. Written once because a face key that
/// mapped to two different directions would bake a face lit and culled on one side and drawn on
/// another - which is a shape of bug that looks like a texture problem.
const fn direction_of(face: schemas::models::BlockFace) -> Direction {
    match face {
        schemas::models::BlockFace::Down => Direction::Down,
        schemas::models::BlockFace::Up => Direction::Up,
        schemas::models::BlockFace::North => Direction::North,
        schemas::models::BlockFace::South => Direction::South,
        schemas::models::BlockFace::West => Direction::West,
        schemas::models::BlockFace::East => Direction::East,
    }
}

/// Every face a model may write, each paired with the direction it names.
///
/// This is the walk an element's faces are baked in. Six entries and not `element.faces.keys()`,
/// because a `HashMap`'s order is not an order: two loads of the same model would emit the same faces
/// in different orders, and while the mesh is merged per direction afterwards, a difference that only
/// shows up on a reload is a difference nobody can debug.
const MODEL_FACES: [(schemas::models::BlockFace, Direction); 6] = [
    (schemas::models::BlockFace::Down, Direction::Down),
    (schemas::models::BlockFace::Up, Direction::Up),
    (schemas::models::BlockFace::North, Direction::North),
    (schemas::models::BlockFace::South, Direction::South),
    (schemas::models::BlockFace::West, Direction::West),
    (schemas::models::BlockFace::East, Direction::East),
];

/// The `from` and `to` of one element, in the sixteen units a model file writes them in.
///
/// Kept in those units rather than in blocks, because the one thing that reads them is
/// [`default_face_uv`], which is written in them too: an element's `uv` is sixteen units to the sprite,
/// so a face's default rectangle is the element's *own* rectangle, in the space the model file used.
#[derive(Clone, Copy)]
struct ElementBounds {
    from: [f32; 3],
    to: [f32; 3],
}

/// The `uv` a face samples when the model file does not write one: the sprite the element *covers*.
///
/// A face with no `uv` does not sample the whole sprite - it samples the part of it the element's own
/// box is, which is why a bottom slab's top face is the top half of the sprite and its sides are cut
/// down to the slab's height. It is not an optimization the game could have skipped: a slab's side is
/// half as tall as the block and its sprite is not, so stretching the sprite over it draws the texture
/// at half scale, and a pane drawn that way is a full block of glass in the middle of an empty one.
///
/// Minecraft's version is `FaceBakery#defaultFaceUV`, six lines that pick two of the box's axes for `u`
/// and two for `v`, one line per facing - and the facing is the key the model file wrote the face
/// under, *not* where the face ends up pointing after a variant rotation. That is why this takes the
/// direction the caller looked the face up by:
///
/// ```java
/// case DOWN  -> new UVs(from.x(), 16.0F - to.z(), to.x(), 16.0F - from.z());
/// case UP    -> new UVs(from.x(), from.z(), to.x(), to.z());
/// case NORTH -> new UVs(16.0F - to.x(), 16.0F - to.y(), 16.0F - from.x(), 16.0F - from.y());
/// case SOUTH -> new UVs(from.x(), 16.0F - to.y(), to.x(), 16.0F - from.y());
/// case WEST  -> new UVs(from.z(), 16.0F - to.y(), to.z(), 16.0F - from.y());
/// case EAST  -> new UVs(16.0F - to.z(), 16.0F - to.y(), 16.0F - from.z(), 16.0F - from.y());
/// ```
///
/// **Those six lines are not what this function returns, and the difference is real.** They are the
/// game's answer in the game's own terms - the u and v a corner of the box samples, with the `16 -`
/// giving which way each runs. This side's vertices are in the opposite order from the game's (see
/// [`face_vertices`]: the game's winding is read backwards here to put the normals the way the renderer
/// culls them), and reversing a quad flips *both* of its texture axes. So `u` and `v` come back 180
/// degrees round from the game's version of them, and the six lines above are evaluated with both axes
/// turned: `16 - to.x` becomes `from.x` and `16 - from.y` becomes `to.y`.
///
/// That is not a second opinion - it is the same six lines read against this side's vertex order, and
/// `a_face_pairs_its_corners_the_way_the_games_own_uv_lines_do` checks exactly that: it evaluates the
/// game's lines for a full cube, where they say which world axis each texture axis runs along and which
/// way, and asks this table's corners whether they sample what those lines say. The two versions of the
/// lines agree everywhere the axes are symmetric, which is every full cube; they differ on
/// [`face_vertices`]' own order, and that is the half a turned texture hides in.
///
/// The `16 -` in four of the game's lines is not a mirror for its own sake either: `u` runs along `+x`
/// on an UP face and along `-x` on a NORTH one. A negative span - an element whose axes are the other
/// way round - keeps its sign, and swapping the two numbers flips the texture, so nothing is sorted or
/// clamped here.
fn default_face_uv(bounds: ElementBounds, declared: Direction) -> [f32; 4] {
    let [fx, fy, fz] = bounds.from;
    let [tx, ty, tz] = bounds.to;

    // The game's six lines, transcribed with **neither axis turned**.
    //
    // An earlier version of this turned both axes, on the argument that this side's vertices are the
    // game's read backwards (`face_vertices`) and reversing a quad flips both of its texture axes. The
    // argument is wrong, and it produced upside-down textures on every side face of every model that
    // writes no `uv` - the whole of `defaultFaceUV` - which is slabs, stairs, panes and plants. What the
    // argument got wrong is *where* the turn belongs: `face_vertices` is a table of which corner of the
    // box samples which corner of the rectangle, and it is written against the rectangle **as the game
    // defines it**. Turning the rectangle as well turns the face twice, and a face turned twice is a face
    // turned once.
    //
    // It is not a second opinion about those six lines either: they are quoted in the doc comment above
    // and this is them, with `from` and `to` substituted. Nothing here sorts or clamps.
    match declared {
        Direction::Down => [fx, 16.0 - tz, tx, 16.0 - fz],
        Direction::Up => [fx, fz, tx, tz],
        Direction::North => [16.0 - tx, 16.0 - ty, 16.0 - fx, 16.0 - fy],
        Direction::South => [fx, 16.0 - ty, tx, 16.0 - fy],
        Direction::West => [fz, 16.0 - ty, tz, 16.0 - fy],
        Direction::East => [16.0 - tz, 16.0 - ty, 16.0 - fz, 16.0 - fy],
    }
}

/// One face's corners in **this** side's atlas, in whole pixels of it.
///
/// `uv` is the rectangle the face samples, in the model file's own units - the one it wrote, or the
/// one [`default_face_uv`] derived from its element. It is handed in rather than read off the face,
/// because by the time a face gets here the two are the same field and only the caller knows which.
fn get_atlas_uv(
    uv: [f32; 4],
    rotation: u32,
    block_atlas: &Atlas,
    texture: &ResourcePath,
) -> Option<UV> {
    let uv = uv.map(|x| x as u16);
    let atlas_map = block_atlas.uv_map.read();
    atlas_map.get(texture).copied().map(|tex| {
        let tw = (tex.1.0 - tex.0.0, tex.1.1 - tex.0.1);
        let uvs = match rotation {
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

/// One face's corners as fractions of the sprite it samples, in the order the model's own vertices
/// read them: `(0, 0)` is one corner of that sprite and `(1, 1)` the other, whatever the sprite's size
/// in pixels - with the face's own `rotation` applied, about the middle.
///
/// Fractions rather than pixels, because this is the one part of a face that is written the same way
/// in both atlases: a model's `uv` is sixteen units to the sprite (the element's own box when the model
/// does not write one - see [`default_face_uv`]), and how many *pixels* that is depends on which atlas
/// the face is being baked for. The turn is Minecraft's, a quarter turn at a time, and these are the
/// same four cases [`get_atlas_uv`] writes out in whole pixels of this side's atlas.
fn sprite_fractions(uv: [f32; 4], rotation: u32) -> ((f32, f32), (f32, f32)) {
    let uv = uv.map(|unit| unit / 16.0);

    match rotation {
        0 => ((uv[0], uv[1]), (uv[2], uv[3])),
        90 => ((1.0 - uv[1], uv[0]), (1.0 - uv[3], uv[2])),
        180 => ((1.0 - uv[0], 1.0 - uv[1]), (1.0 - uv[2], 1.0 - uv[3])),
        270 => ((uv[1], 1.0 - uv[0]), (uv[3], 1.0 - uv[2])),
        _ => unreachable!("a model face is turned by one of four quarter turns"),
    }
}

/// One game-atlas coordinate, in the sixteen bits a vertex holds it in - and the decode the shader
/// runs, `value / 65535`, run the other way. See [`UV_GAME_SCALE`].
pub(crate) fn game_bits(value: f32) -> u16 {
    (value * UV_GAME_SCALE).round().clamp(0.0, 65535.0) as u16
}

/// The game's rectangle for a sprite, in the same bits - so that [`lock_uv`], which turns corners
/// about the middle of the rectangle it is handed, can be given this and the corners together.
fn game_rect_to_bits(rect: [f32; 4]) -> UV {
    (
        (game_bits(rect[0]), game_bits(rect[1])),
        (game_bits(rect[2]), game_bits(rect[3])),
    )
}

/// One face's four corners in **Minecraft's** own atlas, as the vertex holds them.
///
/// The face's `uv` is a fraction of the sprite, whatever the sprite's size in pixels, so the corners
/// are that fraction of the game's rectangle across and down. That is the one arithmetic that works
/// for both atlases, and the reason the game's atlas never has to be measured here: the sprite's
/// rectangle already says where it is in an atlas this side does not own. See
/// [`UV_GAME_ATLAS`] for why a face is sent there at all.
fn get_game_atlas_uv(uv: [f32; 4], rotation: u32, rect: [f32; 4]) -> UV {
    let corners = sprite_fractions(uv, rotation);

    let to_game = |(x, y): (f32, f32)| {
        (
            game_bits(rect[0] + x * (rect[2] - rect[0])),
            game_bits(rect[1] + y * (rect[3] - rect[1])),
        )
    };

    (to_game(corners.0), to_game(corners.1))
}

/// One corner of an element's box, as a bit per axis: **bit 0 is `z`, bit 1 is `y`, bit 2 is `x`**, each
/// set when that axis takes the element's `to` bound rather than its `from`. So `6` is `p110`, and
/// [`corner_position`] is the function that turns one of these back into a point.
///
/// The order of the bits is the order the baker's corner list is indexed in - which is *not* the order of
/// `x`, `y`, `z`. Reading it as that mirrors the box, and a mirrored box is a symmetry of the six faces:
/// every test that only asks which corner is which still passes, and the mirror only shows up when a
/// pairing is checked against the game's tables by axis - which took a slab, whose sides are not a full
/// block in `y`, to see.
type Corner = u8;

/// Where one vertex of a face samples its sprite: `(the rectangle's high u, the rectangle's high v)`.
type SpriteCorner = (bool, bool);

/// The four vertices of one face of an element's box: **which corner of the box**, and **which corner of
/// the sprite's `uv` rectangle** it samples, in the order the baker writes them.
///
/// Both halves come from the game, and a mistake in either is a face turned about its own middle -
/// invisible on a texture with no direction to it, and plain on every texture that has one:
///
/// > some blocks' faces are rotated against the game's - 90, 180 or 270 degrees
///
/// The corners are the game's `FaceInfo` row for the face **in its own order**, and the sprite corners
/// are `CuboidFace.UVs#getVertexU`/`#getVertexV` for the same index - which is two lines rather than four
/// cases, so the four corners of the sprite they name are always the same four in the same walking order,
/// and what changes from face to face is where the box's corners sit in it.
///
/// **The game's order is the renderer's winding, and this file spent a while believing otherwise.**
/// `FaceInfo` is written for `calculateFacing`, which only asks which direction a quad is *about* - and
/// the conclusion drawn from that was that its quads wind against this renderer's `Ccw`, so the order was
/// reversed here. It is not true: the cross product of the first three corners of every one of the game's
/// six rows points *out* of the block, which is what `Ccw` front faces want, and
/// `every_face_turns_the_way_the_renderer_draws_it` checks it on every run. Turning a quad over is also a
/// symmetry that nothing here can see - a reversed quad is still a quad, and its winding is still a
/// winding - so the wrong belief cost nothing until it was combined with the table below, and then every
/// side face of every model that writes no `uv` came out **upside down**: `default_face_uv` was turned as
/// well, on the same argument, and a face turned twice is a face turned once.
///
/// What found the table was not another hand-written one. It was asking the two game tables together -
/// `FaceInfo`'s corners and `default_face_uv`'s rectangle - which corner of the sprite each corner of the
/// box samples, and writing down what they said. `a_face_pairs_its_corners_the_way_the_games_own_uv_lines_do`
/// runs that comparison, and `every_face_turns_the_way_the_renderer_draws_it` checks the winding, on every
/// test run.
fn face_vertices(dir: Direction) -> [(Corner, SpriteCorner); 4] {
    // The four corners of the face in the game's own `FaceInfo` order, each paired with the corner of the
    // sprite `default_face_uv` puts it at. Both columns were derived from those two tables rather than
    // argued about; see the note above for how long arguing about them took.
    //
    // Three tables got this wrong before, and both of the first two passed every test of the day. What
    // found the working one is the two checks that run on every test now:
    // `every_face_turns_the_way_the_renderer_draws_it` for the winding, and
    // `a_face_pairs_its_corners_the_way_the_games_own_uv_lines_do` for the pairing.
    let pairs: [(Corner, SpriteCorner); 4] = match dir {
        // from.x, 16 - to.z -> `u` runs with `x`, `v` against `z`.
        Direction::Down => [
            (1, (false, false)),
            (0, (false, true)),
            (4, (true, true)),
            (5, (true, false)),
        ],
        // from.x, from.z -> `u` with `x`, `v` against `z`.
        Direction::Up => [
            (2, (false, false)),
            (3, (false, true)),
            (7, (true, true)),
            (6, (true, false)),
        ],
        // 16 - to.x, 16 - to.y -> `u` against `x`, `v` against `y`.
        Direction::North => [
            (6, (false, false)),
            (4, (false, true)),
            (0, (true, true)),
            (2, (true, false)),
        ],
        // from.x, 16 - to.y -> `u` with `x`, `v` against `y`.
        Direction::South => [
            (3, (false, false)),
            (1, (false, true)),
            (5, (true, true)),
            (7, (true, false)),
        ],
        // from.z, 16 - to.y -> `u` with `z`, `v` against `y`.
        Direction::West => [
            (2, (false, false)),
            (0, (false, true)),
            (1, (true, true)),
            (3, (true, false)),
        ],
        // 16 - to.z, 16 - to.y -> `u` against `z`, `v` against `y`.
        Direction::East => [
            (7, (false, false)),
            (5, (false, true)),
            (4, (true, true)),
            (6, (true, false)),
        ],
    };

    pairs
}

/// The four vertices of one face, ready to bake: the corner of the box each one is at, and the point of
/// the face's sprite it samples. See [`face_vertices`] for the table itself.
fn sprite_vertices(dir: Direction, corners: &[glam::Vec3; 8], uv: UV) -> [BlockMeshVertex; 4] {
    face_vertices(dir).map(|(corner, (max_u, max_v))| BlockMeshVertex {
        position: corners[corner as usize],
        tex_coords: [
            if max_u { uv.1.0 } else { uv.0.0 },
            if max_v { uv.1.1 } else { uv.0.1 },
        ],
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

/// The per-axis factor an element's `rescale` asks for, or one on every axis when it asks for none.
///
/// A turned element is a *narrower* element: a 14.4-wide cuboid turned 45 degrees about `y` covers
/// `14.4 * cos(45) = 10.18` of the block, so a plant drawn as a cross is 29% smaller than the one the
/// game draws. `rescale` is the model saying "and now stretch it back", and this is Minecraft's own
/// answer to by how much - `CuboidRotation#computeRescale`, which for each axis takes the unit vector,
/// turns it with the same matrix, and returns the reciprocal of its largest component:
///
/// ```java
/// private static float scaleFactorForAxis(Matrix4fc rotation, Direction.Axis axis, Vector3f scratch) {
///     Vector3f transformedAxisUnit = rotation.transformDirection(scratch.set(axis.getPositive().getUnitVec3f()));
///     return 1.0F / Math.max(Math.max(abs(x), abs(y)), abs(z));
/// }
/// ```
///
/// For the 45 degree `y` turn every cross model uses, the turned `x` unit is `(cos45, 0, -sin45)` and
/// its largest component is `cos45`, so the factor is `1.4142` on `x` and `z` and `1` on `y` - which is
/// exactly the `1/cos` that undoes the narrowing. A 90 degree turn comes out at `1` on every axis
/// because a quarter turn preserves lengths, and so does the identity a zero angle gives.
///
/// `Minecraft` composes this as `transform.scale(...)` on a JOML matrix, which multiplies the
/// *columns* - `R * S`, rotation first and scale second. The order is not a detail: `S * R` would
/// stretch along the block's axes and then turn the stretched shape, which is a different block.
pub fn element_rescale(rotation: &schemas::models::ElementRotation, matrix: &Mat3) -> Vec3 {
    let apply = |axis_vec: Vec3| {
        let turned = *matrix * axis_vec;

        1.0 / turned.x.abs().max(turned.y.abs()).max(turned.z.abs())
    };

    // Minecraft skips the rescale entirely for a rotation that is the identity, which matters because
    // the identity has no largest component to divide by in any meaningful sense - the guard is what
    // keeps a zero-angle element from being stretched by whatever `1/1` rounds to.
    if !rotation.rescale || *matrix == Mat3::IDENTITY {
        return Vec3::ONE;
    }

    Vec3::new(apply(Vec3::X), apply(Vec3::Y), apply(Vec3::Z))
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
                    .ok_or_else(|| {
                        MeshBakeError::UnresolvedResourcePath(model_resource_path.clone())
                    })?;

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
                        .values()
                        .filter_map(|texture| {
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

                                    // Once per path, not once per model that names it: one block whose
                                    // texture is missing is a model, a multipart set and every state
                                    // that selects them, and the line says all of it the first time.
                                    // The set behind this lives in the counter rather than beside the
                                    // warning, because a reload has to be able to forget it.
                                    if UNREADABLE_TEXTURES.warn_once(&texture_path) {
                                        log::warn!(
                                            "wgpu-mc: {texture_path} is named by a model but cannot \
                                            be read; the faces using it are left untextured"
                                        );
                                    }
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
                        // Each face is handed to `face_data`, which reads its UVs - the ones the model
                        // wrote, or the ones its element's box implies - its layer and the direction it
                        // declared for culling, and applies the face's own rotation followed by the
                        // variant's `uvlock` to the UVs.
                        //
                        // The default is why the element's bounds go with each face: a face with no `uv`
                        // samples the part of the sprite its own box covers, which is the element's
                        // `from` and `to` and nothing else. See `default_face_uv`.
                        let bounds = ElementBounds {
                            from: element.from,
                            to: element.to,
                        };

                        // Walked by direction rather than six hand-written lookups, because the
                        // direction a face is looked up by is now a parameter and not a label: it is
                        // what `default_face_uv` switches on, and what `uv_lock_matrix` needs. The
                        // order is this table's rather than the file's, so that a model with several
                        // faces bakes the same mesh every load - the faces are merged per direction
                        // anyway, and an order that came out of a `HashMap` would be an order that
                        // changed between runs.
                        let faces: Vec<(Direction, FaceData)> =
                            MODEL_FACES
                                .into_iter()
                                .filter_map(|(face, direction)| {
                                    element
                                        .faces
                                        .get(&face)
                                        .and_then(|tex| {
                                            face_data(
                                                tex, bounds, direction, block_atlas, rotation,
                                                uv_lock,
                                            )
                                        })
                                        .map(|data| (direction, data))
                                })
                                .collect();

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

                        // The element's `rescale`, when it asks for one. See [`element_rescale`]: a
                        // turned element is a *narrower* element, and this is the factor that turns it
                        // back. Without it a cross model's plants - which are all `rescale: true` and
                        // all 45 degrees - come out 29% small, which is what they did.
                        let rescale = element_rescale(rot, &matrix);

                        let vertex_transform = |v: Vec3| {
                            // The element's own rotation first, then its rescale, then the variant's -
                            // which is the order Minecraft applies them in, and the reason the
                            // variant's rotation is per model property rather than per model: the same
                            // model file baked under two variants is two different meshes.
                            //
                            // The rescale is about the same origin the rotation is, because
                            // `CuboidRotation` composes it as `R * S` and the whole thing is applied
                            // as `origin + R * S * (v - origin)`.
                            let v = matrix * (v - vec_origin) * rescale + vec_origin;

                            rotation.position(v)
                        };

                        let p = std::array::from_fn(|corner| {
                            vertex_transform(corner_position(
                                corner as Corner,
                                Vec3::from_array(element.from) / 16.0,
                                Vec3::from_array(element.to) / 16.0,
                            ))
                        });

                        faces
                            .into_iter()
                            .map(|(direction, face): (Direction, FaceData)| BlockModelFace {
                                vertices: sprite_vertices(direction, &p, face.uv),
                                normal: rotation.direction(direction.normal()),
                                tint_index: face.tint_index,
                                uv_flags: face.uv_flags,
                                layer: model_layer.stronger(face.layer),
                                cull: face.cull,
                            })
                            .collect::<Vec<BlockModelFace>>()
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
/// The pairing of a face's four corners with the four corners of its sprite. See [`face_vertices`].
/// The position of a corner of an element's box, **named the way the baker names it**.
///
/// The baker writes its eight corners out as `p000`..`p111`, and this is that list as a function: the
/// three bits of a [`Corner`] are `x` in 4, `y` in 2, `z` in 1.
///
/// Written by name and not by shifting, because the bit order is the *reverse* of `x`, `y`, `z` - and a
/// mirrored box is a symmetry of the six faces, so nothing about a full cube looks wrong when it is read
/// the wrong way. It took a test on a slab, whose sides are not a full block in `y`, for the mirror to
/// show up at all.
#[cfg_attr(not(test), allow(dead_code))]
fn corner_position(corner: Corner, from: Vec3, to: Vec3) -> Vec3 {
    match corner {
        0 => vec3(from.x, from.y, from.z), // p000
        4 => vec3(to.x, from.y, from.z),   // p100
        2 => vec3(from.x, to.y, from.z),   // p010
        6 => vec3(to.x, to.y, from.z),     // p110
        1 => vec3(from.x, from.y, to.z),   // p001
        5 => vec3(to.x, from.y, to.z),     // p101
        3 => vec3(from.x, to.y, to.z),     // p011
        7 => vec3(to.x, to.y, to.z),       // p111
        _ => unreachable!("a corner is one of the box's eight"),
    }
}

#[cfg(test)]
mod element_rescale_tests {
    use super::*;

    /// The factor for the turn every cross model in the game writes: 45 degrees about `y`.
    ///
    /// The turned `x` unit is `(cos45, 0, -sin45)`, so the largest component of *both* the turned `x`
    /// and the turned `z` is `cos45` and the factor is `1/cos45 = sqrt(2)`; `y` is the axis and does not
    /// move. This is the whole of what makes a plant the size the game draws it.
    #[test]
    fn the_cross_models_turn_is_scaled_back_to_full_size() {
        let rot = schemas::models::ElementRotation {
            origin: [8.0, 8.0, 8.0],
            axis: schemas::models::Axis::Y,
            angle: 45.0,
            rescale: true,
        };
        let matrix = Mat3::from_rotation_y(45f32.to_radians());

        let s = element_rescale(&rot, &matrix);

        let expected = 1.0 / (45f32.to_radians()).cos();
        assert!(
            (s.x - expected).abs() < 1e-5,
            "x should be 1/cos45 = {expected}, got {}",
            s.x
        );
        assert!(
            (s.z - expected).abs() < 1e-5,
            "z should be 1/cos45 = {expected}, got {}",
            s.z
        );
        assert!(
            (s.y - 1.0).abs() < 1e-5,
            "the axis of the turn does not shrink, so y is 1, got {}",
            s.y
        );
    }

    /// **The plant is a block wide after the turn, and 71% of one without it.**
    ///
    /// The first element of the real `minecraft:block/tinted_cross` - the parent of `short_grass`, and
    /// of every other plant the game draws as a cross - turned exactly the way the bake turns it. This
    /// is the model the game ships, so the numbers are the game's own: `from` 0.8 to `to` 15.2 is 14.4
    /// units of block, and a 45 degree turn takes that to `14.4 * cos45 = 10.18`, which is the 29% that
    /// plants were missing.
    #[test]
    fn a_turned_cross_keeps_its_width_and_loses_it_without_the_rescale() {
        let origin = Vec3::new(8.0, 8.0, 8.0) / 16.0;
        let from = Vec3::new(0.8, 0.0, 8.0) / 16.0;
        let to = Vec3::new(15.2, 16.0, 8.0) / 16.0;
        let matrix = Mat3::from_rotation_y(45f32.to_radians());

        // The `rescale` the model asks for, and what the flat `1.0` of ignoring it comes to.
        let with = element_rescale(
            &schemas::models::ElementRotation {
                origin: [8.0, 8.0, 8.0],
                axis: schemas::models::Axis::Y,
                angle: 45.0,
                rescale: true,
            },
            &matrix,
        );

        let transform = |scale: Vec3, corner: usize| {
            let v = corner_position(corner as Corner, from, to);
            matrix * (v - origin) * scale + origin
        };

        // Corner 0 is `from`, corner 4 is `to` - the two ends of the element's 14.4 units of `x`.
        let width = |scale: Vec3| {
            let a = transform(scale, 0);
            let b = transform(scale, 4);

            (b.x - a.x).abs()
        };

        let full = 14.4 / 16.0;
        let turned = full * 45f32.to_radians().cos();

        assert!(
            (width(with) - full).abs() < 1e-5,
            "with the rescale the element is {full} of a block wide, got {}",
            width(with)
        );
        assert!(
            (width(Vec3::ONE) - turned).abs() < 1e-5,
            "without it the element is {turned} of a block wide, got {}",
            width(Vec3::ONE)
        );
    }

    /// A turned element is stretched back along its own axes, not along the block's.
    ///
    /// `S * R` and `R * S` differ, and only one of them is what `CuboidRotation` builds. The visible
    /// difference for a 45 degree `y` turn is where the element's ends land: stretched about the
    /// element's own axes the element grows along the diagonal it was turned onto, so a corner that was
    /// at the `x` end stays at the same `z` as the other end; stretched about the block's it would also
    /// move in `z`. Checked by the fact that the end-to-end delta is parallel to the turned `x` axis.
    #[test]
    fn the_rescale_is_applied_before_the_rotation_not_after() {
        let origin = Vec3::new(8.0, 8.0, 8.0) / 16.0;
        let from = Vec3::new(0.8, 0.0, 8.0) / 16.0;
        let to = Vec3::new(15.2, 16.0, 8.0) / 16.0;
        let matrix = Mat3::from_rotation_y(45f32.to_radians());

        let s = element_rescale(
            &schemas::models::ElementRotation {
                origin: [8.0, 8.0, 8.0],
                axis: schemas::models::Axis::Y,
                angle: 45.0,
                rescale: true,
            },
            &matrix,
        );

        let a = matrix * (corner_position(0, from, to) - origin) * s + origin;
        let b = matrix * (corner_position(4, from, to) - origin) * s + origin;

        // The direction the element runs in, and the turned `x` axis it should be parallel to.
        let delta = (b - a).normalize();
        let turned_x = (matrix * Vec3::X).normalize();

        assert!(
            delta.dot(turned_x) > 1.0 - 1e-5,
            "the ends should lie along the turned x axis, got {delta:?} against {turned_x:?}"
        );
    }

    /// The guard: nothing is scaled when the model did not ask, and nothing is scaled when there is no
    /// turn to undo - a quarter turn and a zero turn both preserve lengths, so both come out at one.
    #[test]
    fn a_rescale_is_one_without_a_turn_and_absent_without_the_flag() {
        let asked = |axis, angle, rescale| schemas::models::ElementRotation {
            origin: [8.0, 8.0, 8.0],
            axis,
            angle,
            rescale,
        };
        let turned = |axis, angle| match axis {
            schemas::models::Axis::X => Mat3::from_rotation_x(f32::to_radians(angle)),
            schemas::models::Axis::Y => Mat3::from_rotation_y(f32::to_radians(angle)),
            schemas::models::Axis::Z => Mat3::from_rotation_z(f32::to_radians(angle)),
        };

        // `rescale` absent: every axis one, whatever the angle.
        let matrix = turned(schemas::models::Axis::Y, 45.0);
        assert_eq!(
            element_rescale(&asked(schemas::models::Axis::Y, 45.0, false), &matrix),
            Vec3::ONE,
            "a model that did not ask for a rescale does not get one"
        );

        // A quarter turn preserves lengths, so there is nothing to undo.
        let matrix = turned(schemas::models::Axis::Y, 90.0);
        let s = element_rescale(&asked(schemas::models::Axis::Y, 90.0, true), &matrix);
        for axis in [s.x, s.y, s.z] {
            assert!((axis - 1.0).abs() < 1e-5, "a quarter turn is 1, got {axis}");
        }

        // And so does no turn at all - the identity is where `1/max` would divide by itself.
        let s = element_rescale(&asked(schemas::models::Axis::X, 0.0, true), &Mat3::IDENTITY);
        assert_eq!(s, Vec3::ONE, "no turn, nothing to rescale");
    }
}

#[cfg(test)]
mod block_offset_tests {
    use super::*;

    fn offsetting(max_y: f32) -> FaceFlags {
        FaceFlags {
            offset_max_y: max_y,
            offset_xz: true,
            ..FaceFlags::default()
        }
    }

    /// **The hash is Java's, bit for bit**, which is the one thing here that cannot be checked by
    /// reading the Rust: `Mth.getSeed` is a chain of multiplications that overflow a `long` on
    /// purpose, and an ordinary `*` panics in debug and wraps silently in release.
    ///
    /// The expected values are Java's own, evaluated from
    ///
    /// ```java
    /// long seed = x * 3129871 ^ z * 116129781L ^ y;
    /// seed = seed * seed * 42317861L + seed * 11L;
    /// return seed >> 16;
    /// ```
    ///
    /// at the coordinates written down here. One of them is negative in both coordinates, which is the
    /// case a sign extension would get wrong.
    #[test]
    fn the_seed_is_javas_hash() {
        for (x, y, z, expected) in [
            (0, 0, 0, 0i64),
            (1, 0, 0, 133_076_631_897_947),
            (0, 0, 1, -20_769_809_646_864),
            (-7, 0, 13, 125_444_078_969_910),
            (10, 0, 10, -20_383_890_839_690),
        ] {
            assert_eq!(
                block_seed(x, y, z),
                expected,
                "the hash of ({x}, {y}, {z}) is not Java's"
            );
        }
    }

    /// The game's own `getOffset` for one plant, to nine places.
    ///
    /// `short_grass` at `(10, 0, 10)` with the default vertical limit of `0.2`. Java evaluates
    ///
    /// ```java
    /// double y = ((float)(seed >> 4 & 15L) / 15.0F - 1.0) * 0.2;
    /// double x = clamp(((float)(seed & 15L) / 15.0F - 0.5) * 0.5, -0.25, 0.25);
    /// double z = clamp(((float)(seed >> 8 & 15L) / 15.0F - 0.5) * 0.5, -0.25, 0.25);
    /// ```
    ///
    /// and this side has to land on the same numbers or a field of grass is subtly the wrong field.
    #[test]
    fn the_offset_is_the_games_own_numbers() {
        let plant = offsetting(0.2);
        let offset = plant.block_offset(10, 10);

        assert!(
            (offset.x - -0.049_999_997).abs() < 1e-6,
            "x was {}",
            offset.x
        );
        assert!(
            (offset.y - -0.106_666_67).abs() < 1e-6,
            "y was {}",
            offset.y
        );
        assert!(
            (offset.z - -0.016_666_666).abs() < 1e-6,
            "z was {}",
            offset.z
        );
    }

    /// A plant is nudged, twice over: the offset depends on the coordinates and on nothing else.
    ///
    /// The three properties that matter and that a wrong implementation loses: the offset is **not**
    /// zero (which is the bug this exists for - every plant dead centre), it **differs between
    /// neighbours** (so a field is a field), and it is the **same for the same column** - the hash is
    /// taken with `y = 0`, so a plant at the top of a hill and one at the bottom agree. The last is the
    /// one a plausible-looking implementation that hashes the block's own `y` gets wrong.
    #[test]
    fn a_plant_is_nudged_by_the_block_it_is_in_and_not_by_its_height() {
        let plant = offsetting(0.2);

        let origin = plant.block_offset(10, 10);
        assert_ne!(origin, Vec3::ZERO, "a plant with an offset is not centred");

        // Every axis is inside the block, and the vertical one is the game's `-maxY .. 0`: a plant may
        // sink and may not float.
        for x in -30..30 {
            for z in -30..30 {
                let offset = plant.block_offset(x, z);

                assert!(
                    offset.x >= -0.25 && offset.x <= 0.25,
                    "x {} is outside the clamp at ({x}, {z})",
                    offset.x
                );
                assert!(
                    offset.z >= -0.25 && offset.z <= 0.25,
                    "z {} is outside the clamp at ({x}, {z})",
                    offset.z
                );
                assert!(
                    offset.y <= 0.0 && offset.y >= -0.2 - 1e-6,
                    "y {} is outside -0.2 .. 0 at ({x}, {z})",
                    offset.y
                );
            }
        }

        // Neighbours disagree - that is the whole point of the nudge.
        assert_ne!(
            plant.block_offset(10, 10),
            plant.block_offset(11, 10),
            "two neighbouring plants were given the same offset"
        );

        // `XZ` is the same function with the vertical term held at zero - which is a *different* thing
        // from a block with no offset, and the one the test above is named for. A flower is nudged
        // horizontally and not vertically, so its limit is zero and the bit is what says so.
        let flower = offsetting(0.0);
        assert!(flower.has_offset(), "a flower is offset, just not upwards");
        assert_eq!(flower.block_offset(10, 10).y, 0.0);
        assert_eq!(
            flower.block_offset(10, 10).x,
            plant.block_offset(10, 10).x,
            "the horizontal nudge does not depend on the vertical limit"
        );
    }

    /// A block that did not ask to be offset does not move, whatever the coordinates.
    ///
    /// The default is the whole reason the limit travels instead of a type: `Default` is `0.0`, and a
    /// bridge that forgot to send it leaves every block in the world where it was placed rather than
    /// scattering the terrain.
    #[test]
    fn a_block_without_an_offset_stays_where_it_was_placed() {
        let stone = FaceFlags::default();

        assert!(!stone.has_offset());
        assert_eq!(stone.block_offset(0, 0), Vec3::ZERO);
        assert_eq!(stone.block_offset(-1234, 5678), Vec3::ZERO);
    }

    /// Two states of one key keep the offset only if they agree on it exactly.
    ///
    /// A packed key can be worn by more than one state, and the offset is per position - so two states
    /// that disagree about how far up they float cannot both be right, and the smaller error is neither
    /// of them floating. This is the one flag here that is dropped on disagreement rather than
    /// intersected, because its values are not bits.
    #[test]
    fn a_key_whose_states_disagree_about_the_height_does_not_offset() {
        assert_eq!(offsetting(0.2).and(offsetting(0.2)).offset_max_y, 0.2);
        assert_eq!(offsetting(0.2).and(offsetting(0.0)).offset_max_y, 0.0);
        assert_eq!(offsetting(0.0).and(offsetting(0.2)).offset_max_y, 0.0);
        assert_eq!(offsetting(0.0).and(offsetting(0.0)).offset_max_y, 0.0);
    }
}

#[cfg(test)]
mod face_orientation_tests {
    use super::*;

    /// **What one face's four vertices actually draw**, which is the pairing read the way the renderer
    /// reads it: [`sprite_vertices`] and [`face_vertices`] together, on a full cube against a sprite with
    /// four corners the test can tell apart.
    ///
    /// The three tests around this one check the table against the game's tables; this one checks that
    /// the thing the vertex buffer ends up holding is the same table. It is here because the two are not
    /// one function: the corner of the box comes from [`face_vertices`], the point of the sprite it
    /// samples comes from the same table, and a face whose corners are right and whose texture
    /// coordinates are turned is the whole of the report this work started from.
    ///
    /// A full sprite of sixteen texels is used, and the rectangle is written in *texture* coordinates
    /// `0..1` the way the vertex holds them - so a corner of it is `0.0` or `1.0`, and the four corners
    /// are the four pairs of those.
    #[test]
    fn a_faces_vertices_sample_the_corners_its_table_names() {
        // A full cube in blocks, and its corners in the same `x` in 4, `y` in 2, `z` in 1 reading.
        let corners: [glam::Vec3; 8] = std::array::from_fn(|n| {
            glam::Vec3::new(
                if n & 4 == 0 { 0.0 } else { 1.0 },
                if n & 2 == 0 { 0.0 } else { 1.0 },
                if n & 1 == 0 { 0.0 } else { 1.0 },
            )
        });

        // The whole sprite, in the units the vertex holds a texture coordinate in: `0` and `1` are the
        // two ends of the rectangle, and `sprite_vertices` copies them straight into the vertex.
        let uv = ((0u16, 0u16), (1u16, 1u16));

        for dir in [
            Direction::Up,
            Direction::Down,
            Direction::North,
            Direction::South,
            Direction::West,
            Direction::East,
        ] {
            let expected = face_vertices(dir);
            let vertices = sprite_vertices(dir, &corners, uv);

            assert_eq!(vertices.len(), 4);

            for (index, vertex) in vertices.iter().enumerate() {
                let (corner, (high_u, high_v)) = expected[index];

                assert_eq!(
                    vertex.position, corners[corner as usize],
                    "{dir:?}: vertex {index} is not at the corner of the box the table names",
                );

                assert_eq!(
                    vertex.tex_coords,
                    [if high_u { 1u16 } else { 0 }, if high_v { 1u16 } else { 0 },],
                    "{dir:?}: vertex {index} does not sample the corner of the sprite the table names",
                );
            }

            // All four corners of the sprite, once each: a pairing that sampled one twice would leave a
            // quarter of the sprite unread and draw the face with a corner of the texture stretched
            // across half of it.
            let mut sampled: Vec<(u16, u16)> = vertices
                .iter()
                .map(|vertex| (vertex.tex_coords[0], vertex.tex_coords[1]))
                .collect();
            sampled.sort_unstable();
            sampled.dedup();

            assert_eq!(
                sampled.len(),
                4,
                "{dir:?}: the four vertices sample {} different corners of the sprite - {sampled:?}",
                sampled.len(),
            );
        }
    }

    /// The four sides of a block all walk their sprite the same way round, which is what makes them one
    /// table entry rather than four, and an UP face walks it from the corner its first vertex is at.
    #[test]
    fn the_six_faces_walk_their_sprites_the_same_way_round() {
        for dir in [
            Direction::Up,
            Direction::Down,
            Direction::North,
            Direction::South,
            Direction::West,
            Direction::East,
        ] {
            let corners = face_vertices(dir);

            // Adjacent vertices are adjacent corners of the box, and their sprite corners are adjacent
            // too: a walk that jumped across the rectangle would mirror or turn the face.
            for i in 0..4 {
                let (corner, sprite) = corners[i];
                let (next_corner, next_sprite) = corners[(i + 1) % 4];

                assert_eq!(
                    (corner ^ next_corner).count_ones(),
                    1,
                    "{dir:?}: vertices {i} and {} are not neighbours on the box",
                    (i + 1) % 4
                );

                assert_ne!(
                    sprite, next_sprite,
                    "{dir:?}: two neighbouring vertices sample the same corner of the sprite"
                );
            }
        }
    }

    /// **The test that catches a pairing turned against the game.** The two tests above are about the
    /// *shape* of the pairing - which way the sprite's axes run, and that the walk around the box is the
    /// walk around the sprite - and both of them passed while four of the six faces were a quarter turn
    /// out, because a pairing that is turned still has both of those properties.
    ///
    /// This one is the one that found it, and it is not a second copy of the table: it asks the game's
    /// own six lines, [`default_face_uv`], which corner of the sprite each corner of the box samples, and
    /// then asks the table whether the vertex at that corner of the box really samples it.
    ///
    /// **The axes are derived, not assumed.** Each of the game's lines writes each of its two coordinates
    /// out of one of the box's axes only, so the axis a coordinate runs along is the one that changes it:
    /// move that axis and the coordinate moves, move the other two and it does not. Asking that of
    /// `default_face_uv` itself - for a slab, where all three axes have different extents - reads the six
    /// lines' choices back out of them instead of restating them here, which matters because assumptions
    /// about those choices are exactly what the three wrong tables got wrong.
    ///
    /// The box corner is read by [`corner_position`], the same function the bake reads it by: a test that
    /// works the bits out for itself can mirror the box, and a mirrored box is a symmetry of the six faces
    /// that nothing else here notices.
    #[test]
    fn a_face_pairs_its_corners_the_way_the_games_own_uv_lines_do() {
        // A slab: `x` and `z` are a block, `y` is half of one, so no two axes are confusable.
        let bounds = ElementBounds {
            from: [0.0, 0.0, 0.0],
            to: [16.0, 8.0, 16.0],
        };

        for (dir, outward) in [
            (Direction::Down, glam::Vec3::NEG_Y),
            (Direction::Up, glam::Vec3::Y),
            (Direction::North, glam::Vec3::NEG_Z),
            (Direction::South, glam::Vec3::Z),
            (Direction::West, glam::Vec3::NEG_X),
            (Direction::East, glam::Vec3::X),
        ] {
            let uv = default_face_uv(bounds, dir);

            // Which axis each of the rectangle's two coordinates runs along. Moving one end of one axis
            // moves exactly that axis's coordinate: the low end sets which of the two numbers is the low
            // one, so the coordinate that ends up with the *smaller* value is the one this axis runs
            // along - and the other coordinate is untouched.
            // Which axis each of the rectangle's two coordinates runs along, asked of the game's own
            // line: moving one end of one axis moves exactly that axis's coordinate, and nothing else.
            // Both the low and the high end of the axis are tried, because the six lines run each of
            // their two coordinates either way round.
            let runs_along = |axis: usize| {
                let mut u_moved = false;
                let mut v_moved = false;

                for end in [true, false] {
                    let mut moved = bounds;

                    if end {
                        moved.to[axis] += 4.0;
                    } else {
                        moved.from[axis] += 4.0;
                    }

                    let moved = default_face_uv(moved, dir);

                    u_moved |= moved[0] != uv[0] || moved[2] != uv[2];
                    v_moved |= moved[1] != uv[1] || moved[3] != uv[3];
                }

                (u_moved, v_moved)
            };

            let mut u_axis = None;
            let mut v_axis = None;

            for axis in 0..3 {
                match runs_along(axis) {
                    (true, false) => u_axis = Some(axis),
                    (false, true) => v_axis = Some(axis),
                    _ => {}
                }
            }

            let u_axis = u_axis.unwrap_or_else(|| panic!("{dir:?}: no axis moves `u`"));
            let v_axis = v_axis.unwrap_or_else(|| panic!("{dir:?}: no axis moves `v`"));

            assert_ne!(u_axis, v_axis, "{dir:?}: `u` and `v` follow the same axis");

            // And which way each runs: growing the axis, does the coordinate the game gives grow with it?
            // Read off the line rather than off the rectangle, because a rectangle with both ends at the
            // same value - any full-face of an element that fills the block - says nothing about which end
            // is which, and that is exactly the case a full cube is made of.
            let u_grows_with_axis = {
                let mut moved = bounds;
                moved.to[u_axis] += 4.0;

                let moved = default_face_uv(moved, dir);

                // The coordinate that moved is `u`; the direction it moved in is the answer.
                moved[0] > uv[0] || moved[2] > uv[2]
            };

            let v_grows_with_axis = {
                let mut moved = bounds;
                moved.to[v_axis] += 4.0;

                let moved = default_face_uv(moved, dir);

                moved[1] > uv[1] || moved[3] > uv[3]
            };

            println!(
                "{dir:?} uv {uv:?} axes u{u_axis} v{v_axis} grows ({u_grows_with_axis}, \
                 {v_grows_with_axis}) wind {outward}"
            );

            for (index, (corner, sprite)) in face_vertices(dir).into_iter().enumerate() {
                let p = corner_position(corner, glam::Vec3::ZERO, glam::Vec3::ONE);
                let at = |axis: usize| match axis {
                    0 => p.x,
                    1 => p.y,
                    _ => p.z,
                };

                // The sprite corner the game's line puts this corner of the box at. `from` is zero and
                // `to` is one here, so a fraction above a half is the `to` end - and which end that
                // samples which side of the sprite is what the two directions above say.
                let high = |grows: bool, value: f32| (value > 0.5) == grows;

                let wanted = (
                    high(u_grows_with_axis, at(u_axis)),
                    high(v_grows_with_axis, at(v_axis)),
                );

                assert_eq!(
                    sprite, wanted,
                    "{dir:?}: vertex {index} is at box corner {corner} - world {p} - and the game's own \
                     default uv says a corner there samples {wanted:?}, while the table says {sprite:?}. A \
                     corner of the box joined to the wrong corner of the sprite is that face turned about \
                     its own middle, which is the report this test was written for: 'some blocks' faces \
                     are rotated against the game's'",
                );
            }
        }
    }

    /// The order the four vertices are written in is what the renderer culls on, and this is the check
    /// the game itself runs on it: `FaceBakery#calculateFacing` takes the normal of the first three
    /// quad, and the quad is only wound the way the renderer draws it if that normal is the outward one.
    ///
    /// It is worth a test of its own because the order is what the other two tests cannot see. A table of
    /// pairings turned as a whole - every corner of the box still joined to a corner of the sprite, the
    /// walk still going the right way round - draws every face with its texture turned, and it was three
    /// tables before this one that had the order right.
    ///
    /// The renderer's `front_face` is `Ccw` and its cull mode is back, so "the outward normal" here is
    /// also what stops a face being culled: an inward one is a face that is never drawn.
    #[test]
    fn every_face_turns_the_way_the_renderer_draws_it() {
        for (dir, outward) in [
            (Direction::Down, glam::Vec3::NEG_Y),
            (Direction::Up, glam::Vec3::Y),
            (Direction::North, glam::Vec3::NEG_Z),
            (Direction::South, glam::Vec3::Z),
            (Direction::West, glam::Vec3::NEG_X),
            (Direction::East, glam::Vec3::X),
        ] {
            // A full cube in blocks, read by the same function the baker reads its corners by - so that
            // this test and the bake agree about which corner is which, which is the disagreement that
            // hid a mirrored box from every test here for a while.
            let corner = |n: Corner| corner_position(n, glam::Vec3::ZERO, glam::Vec3::ONE);

            let [a, b, c, _d] =
                face_vertices(dir).map(|(corner_index, _sprite)| corner(corner_index));

            let normal = (b - a).cross(c - a).normalize();

            assert!(
                normal.abs_diff_eq(outward, 1e-6),
                "{dir:?}: the first three vertices wind to {normal}, and this face points {outward}. \
                 The renderer culls back faces, so an inward normal here is a face that is never drawn \
                 - and `FaceBakery#calculateFacing`, which reads the facing off these same three \
                 vertices, would call this quad the opposite face"
            );
        }
    }
}

#[cfg(test)]
/// The `uv` a face gets when its model writes none. See [`default_face_uv`].
mod default_uv_tests {
    use super::*;

    /// **What this test is for.** A face that writes no `uv` was covering the whole sprite, which is
    /// the one answer that is wrong for every model that is not a full cube: a bottom slab, a stair, a
    /// pane and every plant are all *part* of a block sampling the sprite the game cut out for them.
    ///
    /// The numbers are `FaceBakery#defaultFaceUV`'s six lines for a bottom slab - `from [0, 0, 0]`,
    /// `to [16, 8, 16]` - quoted above and transcribed with **neither axis turned**. Every one of the six
    /// is different, so nothing here can pass by the axes being right and the signs being wrong, or the
    /// other way round.
    ///
    /// **A turned version of these was here first and it was wrong.** It read `16 - y` as `y` on the four
    /// sides and turned `Up`/`Down` as well, on the argument that this side's vertices are the game's read
    /// backwards and reversing a quad flips both texture axes. It is a regression that shipped: every side
    /// face of every model that writes no `uv` - slabs, stairs, panes, plants - came out upside down.
    /// `face_vertices` is already the answer to "which corner of the box samples which corner of the
    /// rectangle", written against the rectangle the game defines, so turning the rectangle as well turns
    /// the face twice.
    #[test]
    fn a_face_without_a_uv_covers_the_element_and_not_the_sprite() {
        let bottom_slab = ElementBounds {
            from: [0.0, 0.0, 0.0],
            to: [16.0, 8.0, 16.0],
        };

        for (dir, expected) in [
            (Direction::Up, [0.0, 0.0, 16.0, 16.0]),
            (Direction::Down, [0.0, 0.0, 16.0, 16.0]),
            (Direction::North, [0.0, 8.0, 16.0, 16.0]),
            (Direction::South, [0.0, 8.0, 16.0, 16.0]),
            (Direction::West, [0.0, 8.0, 16.0, 16.0]),
            (Direction::East, [0.0, 8.0, 16.0, 16.0]),
        ] {
            assert_eq!(
                default_face_uv(bottom_slab, dir),
                expected,
                "{dir:?} of a bottom slab",
            );
        }

        // An element that does not fill the block across and down either, so that a face which took
        // the whole sprite on one axis is not hidden by the other axis being full.
        let post = ElementBounds {
            from: [4.0, 0.0, 4.0],
            to: [12.0, 16.0, 12.0],
        };

        for (dir, expected) in [
            (Direction::Up, [4.0, 4.0, 12.0, 12.0]),
            (Direction::Down, [4.0, 4.0, 12.0, 12.0]),
            (Direction::North, [4.0, 0.0, 12.0, 16.0]),
            (Direction::South, [4.0, 0.0, 12.0, 16.0]),
            (Direction::West, [4.0, 0.0, 12.0, 16.0]),
            (Direction::East, [4.0, 0.0, 12.0, 16.0]),
        ] {
            assert_eq!(default_face_uv(post, dir), expected, "{dir:?} of a post");
        }
    }

    /// The default is what a face's four vertices sample, which is the end of the chain: a bottom slab's
    /// side is **half the sprite**, and a face that stretched the sprite over itself would have corners
    /// at `0` and `1` instead.
    ///
    /// `v` is the one that shows it, because `16 - y` is where a slab's height enters the uv: the side
    /// of a slab that is half a block tall samples the half of the sprite that is eight sixteenths,
    /// whatever half that turns out to be.
    #[test]
    fn a_slab_side_covers_half_the_sprite() {
        let bottom_slab = ElementBounds {
            from: [0.0, 0.0, 0.0],
            to: [16.0, 8.0, 16.0],
        };

        // A sprite that covers the whole of a normalized rectangle, so that a fraction of the sprite
        // is the number itself.
        let uv = get_game_atlas_uv(
            default_face_uv(bottom_slab, Direction::North),
            0,
            [0.0, 0.0, 1.0, 1.0],
        );

        let vs: Vec<u16> = face_vertices(Direction::North)
            .map(|(_corner, (max_u, max_v))| {
                let _ = max_u;
                if max_v { uv.1.1 } else { uv.0.1 }
            })
            .to_vec();

        let span = *vs.iter().max().expect("four") as i32 - *vs.iter().min().expect("four") as i32;

        // Within one bit rather than exact, and that is the sixteen-bit encoding and not the geometry:
        // `game_bits` puts a fraction into `0..65535`, so the *middle* of the sprite is `0.5 * 65535` -
        // 32767.5, which a `f32` lands either side of depending on how the two halves were computed.
        // The question this test asks is whether the side covers half the sprite or all of it, and one
        // bit in sixteen is four orders of magnitude away from that.
        assert!(
            (span - game_bits(0.5) as i32).abs() <= 1,
            "a bottom slab's side sampled {vs:?}, which is not half the sprite - a slab is half a block \
             tall and its sprite is not, so this is the texture drawn at the wrong scale",
        );
    }
}

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
            north_face(&resolved).texture.0,
            "minecraft:block/stone",
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

/// The counters a bake reports through, and the reload that has to be able to clear them. See
/// [`MissingSprites`].
#[cfg(test)]
mod diagnostic_tests {
    use super::*;

    /// Which atlas a face belongs to, over every combination of what the decision reads.
    ///
    /// The three inputs are a setting (`Fast`/`Fancy` on the options screen), whether the pass that
    /// would draw the face has the game's atlas, and whether the game animates the sprite at all. A
    /// truth table rather than three examples because a "yes" that should have been a "no" is a face
    /// drawn with the wrong texture on it, and that is the one outcome nobody would guess from a still
    /// picture.
    #[test]
    fn a_face_goes_to_the_game_atlas_only_when_every_input_agrees() {
        let rect = [0.25, 0.25, 0.5, 0.5];

        for animation_on in [false, true] {
            for atlas_bound in [false, true] {
                for animated in [false, true] {
                    let decided =
                        decide_game_atlas(animation_on, atlas_bound, animated, Some(rect));
                    let expected = (animation_on && atlas_bound && animated).then_some(rect);

                    assert_eq!(
                        decided, expected,
                        "animation {animation_on}, atlas bound {atlas_bound}, animated sprite \
                         {animated}"
                    );
                }
            }
        }

        // And a sprite with no rectangle to point at is this side's copy whatever the other three say:
        // there is nowhere in the game's atlas for the face to go.
        assert_eq!(decide_game_atlas(true, true, true, None), None);
    }

    /// The switch is on unless something turned it off, which is the game's own behaviour and what a
    /// player who has never opened the Quality page sees.
    #[test]
    fn the_animation_is_on_by_default() {
        assert!(animated_textures());
    }

    /// A warning is one line per path, and a reload makes the next one a line again.
    ///
    /// The second half is the part worth a test: the set that suppresses repeat warnings is also what
    /// would suppress the *new* pack's warning about a path the old pack was missing too, and the two
    /// are the same line of code apart. A pack that fixes a texture, then a pack that drops it again,
    /// has to be two warnings - otherwise the second failure is silent.
    #[test]
    fn a_warning_is_once_per_path_and_comes_back_after_a_reload() {
        let counters = MissingSprites::new();
        let path = ResourcePath::from("minecraft:block/missing_thing");

        assert!(counters.warn_once(&path), "the first warning is written");
        assert!(!counters.warn_once(&path), "and not the second");

        counters.note(&path);
        assert_eq!(counters.faces(), 1);
        assert_eq!(counters.names(), vec!["minecraft:block/missing_thing"]);

        counters.reset();

        assert_eq!(
            counters.faces(),
            0,
            "a reload forgets what the last pack lost"
        );
        assert!(counters.names().is_empty());
        assert!(
            counters.warn_once(&path),
            "and a path that goes missing again is worth saying again"
        );
    }
}

/// Whether faces whose sprite the game animates are drawn from the game's own atlas.
///
/// The options screen's Quality page, as `Fancy` (on, the default) and `Fast` (off), applied from the
/// settings through [`set_animated_textures`]. On is what the game does: the game animates its block
/// atlas by rendering each due frame into it, so a face drawn from *that* atlas moves for free. Off is
/// the picture this renderer drew before any of it existed - every animated sprite frozen on the frame
/// it was copied at - and it is a fidelity choice rather than a speed one.
///
/// Read once per face, while it is baked, which is why the switch costs a re-bake to move: the answer
/// is written into the vertex, as the flag [`UV_GAME_ATLAS`] and the coordinates that go with it.
static ANIMATED_TEXTURES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Whether animated block textures move. See [`ANIMATED_TEXTURES`].
pub fn animated_textures() -> bool {
    ANIMATED_TEXTURES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Sets [`ANIMATED_TEXTURES`]. Called when the settings are applied.
///
/// Nothing is re-baked here: the caller is the settings path, and the bake is asked for separately -
/// see `BlockCache.blockTexturesChanged` on the JVM side, which the settings path calls.
pub fn set_animated_textures(enabled: bool) {
    ANIMATED_TEXTURES.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

/// Which atlas a face belongs to: the game's own rectangle for its sprite, or `None` for this side's.
///
/// The whole of the decision, in one place, and it takes three answers that come from three different
/// places so that each can be checked on its own:
///
///  - `animated_sprite` - does the **game** animate this sprite? Read from the `.mcmeta` files this side
///    downloads while it packs its own atlas, so it is the game's own answer rather than a list;
///  - `rect` - where the game put that sprite, sent by the JVM (`registerSprite`);
///  - [`animated_textures`] and [`crate::render::graph::game_atlas_bound`] - whether this is wanted at
///    all, and whether the pass that would draw it has the atlas.
///
/// A "no" anywhere is this side's copy of the sprite: frozen, and correct for everything that is not
/// animated. A "yes" that should have been a "no" is a face with the wrong texture on it, so every one
/// of these has to agree.
pub fn face_uses_game_atlas(animated_sprite: bool, rect: Option<[f32; 4]>) -> Option<[f32; 4]> {
    decide_game_atlas(
        animated_textures(),
        crate::render::graph::game_atlas_bound(),
        animated_sprite,
        rect,
    )
}

/// The gate itself, with every input handed in rather than read.
///
/// Split out so that the decision can be tested: the three globals behind it are a setting, a handed-over
/// GPU texture and a table the game fills, and none of them can be moved in a test. The truth table is
/// the contract - all three have to agree, and the failure modes are not symmetric: a "no" is a face
/// drawn from this side's copy of its sprite, while a "yes" that should have been a "no" is a face with
/// the wrong texture on it.
fn decide_game_atlas(
    animation_on: bool,
    atlas_bound: bool,
    animated_sprite: bool,
    rect: Option<[f32; 4]>,
) -> Option<[f32; 4]> {
    if animation_on && atlas_bound && animated_sprite {
        rect
    } else {
        None
    }
}

/// The corners of a face in the *game's* atlas, which is the other half of an animated texture. See
/// [`get_game_atlas_uv`] and [`sprite_fractions`].
#[cfg(test)]
mod game_atlas_tests {
    use super::*;

    /// Runs `check` over the `uv` and `rotation` one north face of a full cube was written with - the
    /// two fields every test here is about, in the units the model file wrote them in.
    ///
    /// The model is parsed on the way, so that a fixture which is not valid JSON - or not a model this
    /// build can read - fails here rather than testing arithmetic on a rectangle nothing wrote.
    fn with_face(uv: &str, rotation: u32, check: impl FnOnce([f32; 4], u32)) {
        let json = format!(
            r##"{{
                "textures": {{ "all": "minecraft:block/fire_0" }},
                "elements": [
                    {{
                        "from": [0, 0, 0],
                        "to": [16, 16, 16],
                        "faces": {{
                            "north": {{ "texture": "#all", "uv": {uv}, "rotation": {rotation} }}
                        }}
                    }}
                ]
            }}"##
        );

        let model = parse_model(&json).expect("a model this build can read");
        let elements = model.elements.expect("elements");

        let face = elements[0]
            .faces
            .get(&schemas::models::BlockFace::North)
            .expect("the face the fixture wrote");

        check(face.uv.expect("the fixture writes a uv"), face.rotation);
    }

    /// A face that writes no `uv` covers the whole sprite: sixteen units to the sprite, whatever the
    /// sprite's size in the game's atlas, so the corners are the rectangle's own.
    #[test]
    fn a_face_that_names_no_uv_covers_the_whole_sprite() {
        with_face("[0, 0, 16, 16]", 0, |uv, rotation| {
            let corners = get_game_atlas_uv(uv, rotation, [0.25, 0.5, 0.75, 1.0]);

            assert_eq!(
                corners,
                ((16384, 32768), (49151, 65535)),
                "the game's coordinates, in the sixteen bits a vertex holds them in - the rectangle's \
                 own corners, rounded to the nearest bit"
            );
        });
    }

    /// The face's `uv` is a fraction of the sprite and not of the atlas, which is the whole reason the
    /// game's atlas does not have to be measured here: half a sprite is half of the *rectangle*.
    #[test]
    fn a_face_is_a_fraction_of_the_sprite_and_not_of_the_atlas() {
        with_face("[0, 0, 8, 8]", 0, |uv, rotation| {
            let corners = get_game_atlas_uv(uv, rotation, [0.0, 0.0, 0.5, 0.5]);

            assert_eq!(
                corners,
                ((0, 0), (16384, 16384)),
                "the top-left quarter of the sprite, which is a quarter of the rectangle"
            );
        });
    }

    /// A turned face stays on its own sprite, which is what says the turn is about the *sprite's*
    /// middle and not the atlas's: a corner that named the atlas would land in the next sprite along
    /// as soon as the sprite was not at the origin.
    #[test]
    fn a_quarter_turn_stays_inside_the_sprite() {
        with_face("[0, 0, 8, 16]", 90, |uv, rotation| {
            let rect = [0.25, 0.25, 0.5, 0.5];
            let corners = get_game_atlas_uv(uv, rotation, rect);

            for (u, v) in [corners.0, corners.1] {
                // The bounds are the rectangle through the same rounding the corners went through:
                // a rectangle edge is a bit like any other, and measuring it another way would test
                // the test.
                let inside = game_bits(rect[0])..=game_bits(rect[2]);
                let down = game_bits(rect[1])..=game_bits(rect[3]);

                assert!(
                    inside.contains(&u) && down.contains(&v),
                    "({u}, {v}) is outside the sprite's own rectangle, so a face turned by 90 degrees \
                     would sample whatever the game packed beside it"
                );
            }
        });
    }

    /// The fractions are the same four quarter turns [`get_atlas_uv`] writes out in whole pixels of
    /// this side's atlas, so a face on a sixteen-texel sprite - which is what nearly every block
    /// texture is - bakes to the corners it always did. This is the regression test for that
    /// rewrite: the numbers on the right are the old arithmetic, by hand.
    #[test]
    fn the_fractions_are_the_pixels_this_side_has_always_baked() {
        for (uv, rotation, expected) in [
            ("[0, 0, 4, 8]", 0, ((0.0, 0.0), (4.0, 8.0))),
            ("[0, 0, 4, 8]", 90, ((16.0, 0.0), (8.0, 4.0))),
            ("[0, 0, 4, 8]", 180, ((16.0, 16.0), (12.0, 8.0))),
            ("[0, 0, 4, 8]", 270, ((0.0, 16.0), (8.0, 12.0))),
            ("[2, 3, 9, 14]", 0, ((2.0, 3.0), (9.0, 14.0))),
            ("[2, 3, 9, 14]", 180, ((14.0, 13.0), (7.0, 2.0))),
        ] {
            with_face(uv, rotation, |uv, rotation| {
                let corners = sprite_fractions(uv, rotation);

                let to_pixels = |(x, y): (f32, f32)| ((x * 16.0).round(), (y * 16.0).round());

                assert_eq!(
                    (to_pixels(corners.0), to_pixels(corners.1)),
                    expected,
                    "uv {uv:?} turned by {rotation} degrees"
                );
            });
        }
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
        assert_eq!(
            turn.rotate_direction(Direction::Up),
            Direction::Up,
            "the axis it turns about"
        );
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
        assert_eq!(
            tip.rotate_direction(Direction::East),
            Direction::East,
            "the axis it turns about"
        );

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

            assert!((out - corner).length() < 1e-5, "{corner} landed on {out}");
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
                for declared in [
                    Direction::North,
                    Direction::East,
                    Direction::Up,
                    Direction::Down,
                ] {
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
                for declared in [
                    Direction::North,
                    Direction::South,
                    Direction::Up,
                    Direction::Down,
                ] {
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
