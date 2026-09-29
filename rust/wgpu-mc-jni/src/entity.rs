use jni::JNIEnv;
use jni::objects::{JClass, JString, JValue};
use jni::sys::jint;
use jni_fn::jni_fn;
use std::{collections::HashMap, sync::Arc};

use serde::Deserialize;

use crate::RENDERER;
use wgpu_mc::mc::entity::Entity;
use wgpu_mc::mc::entity::{Cuboid, CuboidUV, EntityPart, PartTransform};
use wgpu_mc::render::pipeline::ENTITY_ATLAS;

/// One cube, as `CubeDefinition` serialises.
///
/// **The names here are the game's fields and not a format of this side's.** `EntityModelUpload` hands
/// `LayerDefinitions.createRoots()` to Gson and sends what comes out, so every name below is a field of
/// a Minecraft class: `origin`, `dimensions`, `mirror`, `texCoord`. They were `offset`, `textureUV` and
/// `textureScale`, which is a shape no version of the game has ever sent - see [`ModelDefinition`] for
/// what that cost.
#[derive(Debug, Deserialize)]
pub struct ModelCuboidData {
    /// Absent on most cubes: only the ones the game annotates carry it.
    #[serde(default)]
    pub comment: Option<String>,
    /// `Vector3f`'s own fields, which is what Gson emits for `Vector3fc`.
    pub origin: HashMap<String, f32>,
    pub dimensions: HashMap<String, f32>,
    #[serde(default)]
    pub mirror: bool,
    #[serde(rename(deserialize = "texCoord"))]
    pub texture_uv: HashMap<String, f32>,
    /// Added to each dimension, the way `CubeDefinition#bake` grows a cube. Absent on a definition
    /// built before the field existed, and every field here is optional in practice because Gson is
    /// reflecting over classes this side does not own.
    #[serde(default)]
    pub grow: Option<HashMap<String, f32>>,
}

/// A part's pose, as `PartPose` serialises.
///
/// A record, so Gson emits its component names verbatim: `x`, `y`, `z`, `xRot`, `yRot`, `zRot` and the
/// three scales. **The scales are carried through now** - they were dropped, which is why a baby
/// variant of a model rendered at adult proportions: the game expresses it as `PartPose.scaled(0.5)`
/// on the pose and nothing else.
#[derive(Debug, Deserialize)]
pub struct ModelTransform {
    #[serde(default)]
    pub x: f32,
    #[serde(default)]
    pub y: f32,
    #[serde(default)]
    pub z: f32,
    #[serde(rename(deserialize = "xRot"), default)]
    pub pitch: f32,
    #[serde(rename(deserialize = "yRot"), default)]
    pub yaw: f32,
    #[serde(rename(deserialize = "zRot"), default)]
    pub roll: f32,
    #[serde(rename(deserialize = "xScale"), default = "one")]
    pub scale_x: f32,
    #[serde(rename(deserialize = "yScale"), default = "one")]
    pub scale_y: f32,
    #[serde(rename(deserialize = "zScale"), default = "one")]
    pub scale_z: f32,
}

/// `PartPose`'s own default for a scale field that is missing: unscaled.
fn one() -> f32 {
    1.0
}

/// One node of a model, as `PartDefinition` serialises.
#[derive(Debug, Deserialize)]
pub struct ModelPartData {
    #[serde(default)]
    pub cubes: Vec<ModelCuboidData>,
    #[serde(rename(deserialize = "partPose"), default = "unposed")]
    pub transform: ModelTransform,
    #[serde(default)]
    pub children: HashMap<String, ModelPartData>,
}

/// A part with no pose of its own, which is what a definition missing `partPose` means.
fn unposed() -> ModelTransform {
    ModelTransform {
        x: 0.0,
        y: 0.0,
        z: 0.0,
        pitch: 0.0,
        yaw: 0.0,
        roll: 0.0,
        scale_x: 1.0,
        scale_y: 1.0,
        scale_z: 1.0,
    }
}

/// One layer, as `LayerDefinition` serialises: a mesh and the texture it is measured against.
///
/// **This is the struct that was wrong, and the warning was the whole of the symptom.**
///
/// `LayerDefinitions.createRoots()` returns `Map<ModelLayerLocation, LayerDefinition>`, and
/// `LayerDefinition` has two fields: `mesh` and `material`. This side expected `{ "data": { "data":
/// ... } }` - the shape of a *baked* `ModelPart` rather than of a `LayerDefinition` - so **every one of
/// the 416 layers failed to parse** with `missing field 'data'`, and the failure was survivable rather
/// than fatal only because the parse is per layer and the ones that fail are skipped:
///
/// ```text
/// wgpu-mc: 416 entity model layer(s) could not be read: minecraft:sheep#wool_undercoat
///          (missing field `data`), minecraft:hanging_sign/crimson/wall#main (missing field `data`), ...
/// ```
///
/// Nothing about the game's output had to change to fix it. What it sends is
/// `{ "mesh": { "root": PartDefinition }, "material": { "xTexSize": .., "yTexSize": .. } }`, and this
/// now says so. The unused fields - `material` here, `comment`, `visibleFaces`, `texScale` below - are
/// simply not named: serde ignores what it is not asked for, which is the right property for a shape
/// this side is a guest in.
#[derive(Debug, Deserialize)]
pub struct ModelDefinition {
    pub mesh: MeshDefinition,
}

#[derive(Debug, Deserialize)]
pub struct MeshDefinition {
    pub root: ModelPartData,
}

#[derive(Debug, Copy, Clone)]
pub struct AtlasPosition {
    pub width: u32,
    pub height: u32,
    pub x: f32,
    pub y: f32,
}

impl AtlasPosition {
    pub fn map(&self, pos: (f32, f32)) -> (f32, f32) {
        (
            (self.x + pos.0) / (self.width as f32),
            (self.y + pos.1) / (self.height as f32),
        )
    }
}

pub fn tmd_to_wm(name: String, part: &ModelPartData) -> Option<EntityPart> {
    Some(EntityPart {
        name,
        transform: PartTransform {
            // **The cube's own origin is the part's translation, and the pose's offsets are the
            // pivot.** That is what the game's `PartDefinition#bake` does: each cube is translated by
            // its own `origin` and then rotated about the part's pivot by the pose. Reading the
            // pose's `x`/`y`/`z` as the translation - which is what an "offset" field suggested - puts
            // every part in the wrong place, because a pose's offsets are almost always zero.
            x: 0.0,
            y: 0.0,
            z: 0.0,
            pivot_x: part.transform.x,
            pivot_y: part.transform.y,
            pivot_z: part.transform.z,
            yaw: part.transform.yaw,
            pitch: part.transform.pitch,
            roll: part.transform.roll,
            // Carried through now. This was `1.0` on all three, which is what made every baby variant
            // of every model render at adult proportions - the game scales the pose and nothing else.
            scale_x: part.transform.scale_x,
            scale_y: part.transform.scale_y,
            scale_z: part.transform.scale_z,
        },
        cuboids: part
            .cubes
            .iter()
            .map(|cuboid_data| {
                // `CubeDefinition#bake` grows a cube by its deformation before it becomes geometry, so
                // the same is done here: a hat that is "the head plus a quarter" is a cube of the
                // head's dimensions with a quarter added to each of them, not the head's dimensions.
                //
                // **`growX`/`growY`/`growZ`, and not `x`/`y`/`z`** - `grow` is a `CubeDeformation`
                // whose fields are named for what they are, where `origin` is a `Vector3f` whose fields
                // are not. Getting that wrong is silent: the lookup misses, the deformation reads as
                // zero, and every armoured or hatted model is drawn a little too small.
                let grow = |axis: &str| {
                    cuboid_data
                        .grow
                        .as_ref()
                        .and_then(|grow| grow.get(axis).copied())
                        .unwrap_or(0.0)
                };

                let pos = [
                    *cuboid_data.texture_uv.get("u")? as u16,
                    *cuboid_data.texture_uv.get("v")? as u16,
                ];
                let dimensions = [
                    (*cuboid_data.dimensions.get("x")? + grow("growX")) as u16,
                    (*cuboid_data.dimensions.get("y")? + grow("growY")) as u16,
                    (*cuboid_data.dimensions.get("z")? + grow("growZ")) as u16,
                ];

                Some(Cuboid {
                    x: *cuboid_data.origin.get("x")?,
                    y: *cuboid_data.origin.get("y")?,
                    z: *cuboid_data.origin.get("z")?,
                    width: *cuboid_data.dimensions.get("x")? + grow("growX"),
                    height: *cuboid_data.dimensions.get("y")? + grow("growY"),
                    length: *cuboid_data.dimensions.get("z")? + grow("growZ"),
                    textures: CuboidUV {
                        west: (
                            (
                                pos[0] + dimensions[0],
                                pos[1] + (dimensions[2] + dimensions[1]),
                            ),
                            (pos[0], pos[1] + dimensions[2]),
                        ),
                        east: (
                            (
                                pos[0] + (dimensions[0] * 3),
                                pos[1] + dimensions[2] + dimensions[1],
                            ),
                            ((pos[0] + (dimensions[0] * 2)), pos[1] + dimensions[2]),
                        ),
                        north: (
                            (
                                pos[0] + (dimensions[0] * 2),
                                pos[1] + dimensions[2] + dimensions[1],
                            ),
                            (pos[0] + dimensions[0], pos[1] + dimensions[2]),
                        ),
                        south: (
                            (
                                (pos[0] + (dimensions[0] * 4)),
                                pos[1] + (dimensions[2] + dimensions[1]),
                            ),
                            ((pos[0] + (dimensions[0] * 3)), pos[1] + dimensions[2]),
                        ),
                        up: (
                            ((pos[0] + (dimensions[0] * 3)), pos[1] + (dimensions[2])),
                            ((pos[0] + (dimensions[0] * 2)), pos[1]),
                        ),
                        down: (
                            (pos[0] + (dimensions[0] * 2), pos[1] + dimensions[2]),
                            (pos[0] + dimensions[0], pos[1]),
                        ),
                    },
                })
            })
            .collect::<Option<Vec<Cuboid>>>()?,
        children: part
            .children
            .iter()
            .map(|(name, part)| tmd_to_wm(name.clone(), part))
            .collect::<Option<Vec<EntityPart>>>()?,
    })
}

#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn registerEntities(mut env: JNIEnv, _class: JClass, string: JString) {
    let wm = RENDERER.get().unwrap();

    let entities_json_javastr = env.get_string(&string).unwrap();
    let entities_json: String = entities_json_javastr.into();

    // What arrives is Gson's reflection over the game's own layer definitions, not a shape this side
    // owns, so a field it does not expect - or one that is missing - is a parse failure, and the
    // whole upload used to be an `unwrap`: the process ended on the way *out* of a world, because
    // the renderer is recreated when a world is left and the upload runs again. The layers that
    // cannot be read are skipped instead, which leaves the ones from the first upload in place.
    let raw: HashMap<String, serde_json::Value> = match serde_json::from_str(&entities_json) {
        Ok(raw) => raw,
        Err(err) => {
            log::error!(
                "wgpu-mc: the entity models could not be read ({err}); keeping the ones already registered"
            );
            return;
        }
    };

    let mut skipped = Vec::new();
    let mpd: HashMap<String, ModelPartData> = raw
        .into_iter()
        .filter_map(
            |(name, value)| match serde_json::from_value::<ModelDefinition>(value) {
                Ok(model) => Some((name, model.mesh.root)),
                Err(err) => {
                    skipped.push(format!("{name} ({err})"));
                    None
                }
            },
        )
        .collect();

    if !skipped.is_empty() {
        log::warn!(
            "wgpu-mc: {} entity model layer(s) could not be read: {}",
            skipped.len(),
            skipped.join(", ")
        );
    }

    // Nothing readable at all: the entity registry is left as it was rather than emptied. The
    // renderer is recreated when a world is left, and the upload that follows does not have to
    // produce the same graph - clearing the registry there would take every entity's model away for
    // the rest of the session.
    if mpd.is_empty() {
        log::error!(
            "wgpu-mc: no entity model layer could be read, so the ones already registered are kept"
        );
        return;
    }

    let atlases = wm.mc.texture_manager.atlases.write();
    let Some(_atlas) = atlases.get(ENTITY_ATLAS) else {
        log::error!(
            "wgpu-mc: no entity atlas is registered, so entity models cannot be built - the entity \
             pass needs one (see `WmRenderer::init`)"
        );
        return;
    };

    let entities: HashMap<String, Arc<Entity>> = mpd
        .iter()
        .filter_map(|(name, mpd)| {
            // The same trade as above: a layer whose parts do not describe a single cuboid is one
            // entity that does not draw, rather than an upload that ends the process.
            let Some(entity_part) = tmd_to_wm("root".into(), mpd) else {
                log::warn!(
                    "wgpu-mc: entity model {name} has parts this side cannot read; skipping it"
                );
                return None;
            };

            Some((
                name.clone(),
                Arc::new(Entity::new(name.clone(), entity_part, &wm.gpu)),
            ))
        })
        .collect();

    entities.iter().for_each(|(_entity_name, entity)| {
        let entity_string = env.new_string(&entity.name).unwrap();

        entity.parts.iter().for_each(|(name, index)| {
            let part_string = env.new_string(name).unwrap();

            env.call_static_method(
                "dev/birb/wgpu/render/Wgpu",
                "helperSetPartIndex",
                "(Ljava/lang/String;Ljava/lang/String;I)V",
                &[
                    JValue::Object(&entity_string),
                    JValue::Object(&part_string),
                    JValue::Int(*index as jint),
                ],
            )
            .unwrap();
        });
    });

    *wm.mc.entity_models.write() = entities;
}

/// **The shape the game actually sends, pinned.** Every field name here is a field of a Minecraft class
/// rather than a format this side chose, which is exactly why it drifted without anything noticing:
///
///  * `LayerDefinition` -> `mesh` (a `MeshDefinition`) and `material`;
///  * `MeshDefinition` -> `root` (a `PartDefinition`);
///  * `PartDefinition` -> `cubes`, `partPose`, `children`;
///  * `PartPose` is a **record**, so Gson emits its component names: `x`, `y`, `z`, `xRot`, `yRot`,
///    `zRot`, `xScale`, `yScale`, `zScale`;
///  * `CubeDefinition` -> `origin`, `dimensions`, `grow`, `mirror`, `texCoord`, `texScale`,
///    `visibleFaces` - the last two and `visibleFaces` unused here, because serde ignores what it is
///    not asked for and that is the right property for a guest in somebody else's shape.
///
/// This side expected `{ "data": { "data": { "cuboidData", "rotationData", ... } } }`, so **all 416
/// layers failed** with `missing field 'data'` and the warning was the only sign. A thousand lines of
/// rendering code do not fail loudly for a field name; they quietly render nothing, or render a shape
/// built from defaults.
#[cfg(test)]
mod model_shape_tests {
    use super::*;

    /// One layer, as Gson writes it, including the fields this side ignores.
    const SENT: &str = r#"{
        "mesh": {
            "root": {
                "cubes": [
                    {
                        "comment": "head",
                        "origin": { "x": -4.0, "y": -8.0, "z": -4.0 },
                        "dimensions": { "x": 8.0, "y": 8.0, "z": 8.0 },
                        "grow": { "growX": 0.0, "growY": 0.0, "growZ": 0.0 },
                        "mirror": false,
                        "texCoord": { "u": 0.0, "v": 0.0 },
                        "texScale": { "u": 1.0, "v": 1.0 },
                        "visibleFaces": ["up", "down", "north"]
                    }
                ],
                "partPose": {
                    "x": 0.0, "y": 24.0, "z": 0.0,
                    "xRot": 0.0, "yRot": 0.0, "zRot": 0.0,
                    "xScale": 1.0, "yScale": 1.0, "zScale": 1.0
                },
                "children": {
                    "hat": {
                        "cubes": [],
                        "partPose": {
                            "x": 0.0, "y": -8.0, "z": 0.0,
                            "xRot": 0.0, "yRot": 0.0, "zRot": 0.0,
                            "xScale": 2.0, "yScale": 2.0, "zScale": 2.0
                        },
                        "children": {}
                    }
                }
            }
        },
        "material": { "xTexSize": 64, "yTexSize": 64 }
    }"#;

    fn read() -> ModelPartData {
        serde_json::from_str::<ModelDefinition>(SENT)
            .expect("the shape the game sends has to parse")
            .mesh
            .root
    }

    /// The layer parses, and what comes out is the part of it this side draws.
    #[test]
    fn a_layer_the_game_sends_reads_as_a_parts_tree() {
        let root = read();

        assert_eq!(root.cubes.len(), 1, "the root has one cube");
        assert_eq!(root.children.len(), 1, "and one child, `hat`");

        let head = &root.cubes[0];

        assert_eq!(head.origin.get("x"), Some(&-4.0));
        assert_eq!(head.dimensions.get("y"), Some(&8.0));
        assert_eq!(
            head.texture_uv.get("u"),
            Some(&0.0),
            "the texture coordinate is `texCoord`'s `u`, not an `x` of some other struct"
        );
        assert!(!head.mirror);

        // The pose's own numbers, which are the pivot in the game's bake and not a translation.
        assert_eq!(root.transform.y, 24.0);
        assert_eq!(root.transform.roll, 0.0);
    }

    /// **A part's scale reaches the transform**, which is what a baby variant of a model is.
    #[test]
    fn a_scaled_pose_is_carried_through() {
        let root = read();
        let hat = &root.children["hat"];

        assert_eq!(
            hat.transform.scale_x, 2.0,
            "`PartPose.scaled` is what the game expresses a baby model with, and dropping it renders \
             every baby at adult proportions"
        );

        // And the conversion keeps it.
        let part = tmd_to_wm("hat".to_string(), hat).expect("the child converts");

        assert_eq!(part.transform.scale_x, 2.0);
        assert_eq!(part.transform.scale_y, 2.0);
        assert_eq!(part.transform.scale_z, 2.0);
    }

    /// **The pivot is the pose's offset and the cube keeps its own origin.**
    ///
    /// The game's `PartDefinition#bake` translates each cube by its own `origin` and rotates the part
    /// about the pose's pivot. Treating the pose's `x`/`y`/`z` as the part's translation - which is what
    /// an "offset" field suggests - moves every part by a number that is almost always zero, and
    /// ignores the pivot that is almost never zero.
    #[test]
    fn the_pose_is_the_pivot_and_the_cube_keeps_its_origin() {
        let root = read();
        let part = tmd_to_wm("head".to_string(), &root).expect("the root converts");

        assert_eq!(
            part.transform.pivot_y, 24.0,
            "the pose's offset is the pivot the part rotates about"
        );
        assert_eq!(
            part.transform.y, 0.0,
            "and it is not the part's translation, which the cube's own origin carries"
        );

        let head = &part.cuboids[0];

        assert_eq!(
            (
                (head.x * 16.0).round(),
                (head.y * 16.0).round(),
                (head.z * 16.0).round()
            ),
            (-64.0, -128.0, -64.0),
            "the cube's origin is where the game puts it, in the sixteenths this side stores"
        );
    }

    /// **A `grow` widens the cube and moves none of it.** `CubeDefinition#bake` adds the deformation to
    /// each dimension; an armour layer or a hat is the same cube with a little added.
    #[test]
    fn a_grown_cube_is_its_dimensions_plus_the_growth() {
        let grown = SENT.replace(
            r#""grow": { "growX": 0.0, "growY": 0.0, "growZ": 0.0 }"#,
            r#""grow": { "growX": 2.0, "growY": 0.5, "growZ": 2.0 }"#,
        );

        assert_ne!(grown, SENT, "the fixture did not change");

        let root = serde_json::from_str::<ModelDefinition>(&grown)
            .expect("a grown cube parses")
            .mesh
            .root;

        let part = tmd_to_wm("head".to_string(), &root).expect("it converts");
        let head = &part.cuboids[0];

        assert_eq!(head.width, 10.0, "eight wide plus two");
        assert_eq!(head.height, 8.5, "eight tall plus a half");
        assert_eq!(head.length, 10.0);

        assert_eq!(
            (head.x, head.y, head.z),
            (-4.0, -8.0, -4.0),
            "and a grow does not move the cube - the game grows it about its own centre, which is \
             arithmetic on the dimensions and not on the origin"
        );
    }
}
