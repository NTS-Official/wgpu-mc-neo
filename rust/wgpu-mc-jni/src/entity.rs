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

#[derive(Debug, Deserialize)]
pub struct ModelCuboidData {
    pub name: Option<String>,
    pub offset: HashMap<String, f32>,
    pub dimensions: HashMap<String, f32>,
    pub mirror: bool,
    #[serde(rename(deserialize = "textureUV"))]
    pub texture_uv: HashMap<String, f32>,
    #[serde(rename(deserialize = "textureScale"))]
    pub texture_scale: HashMap<String, f32>,
}

#[derive(Debug, Deserialize)]
pub struct ModelTransform {
    #[serde(rename(deserialize = "pivotX"))]
    pub pivot_x: f32,
    #[serde(rename(deserialize = "pivotY"))]
    pub pivot_y: f32,
    #[serde(rename(deserialize = "pivotZ"))]
    pub pivot_z: f32,
    pub pitch: f32,
    pub yaw: f32,
    pub roll: f32,
}

#[derive(Debug, Deserialize)]
pub struct ModelPartData {
    #[serde(rename(deserialize = "cuboidData"))]
    pub cuboid_data: Vec<ModelCuboidData>,
    #[serde(rename(deserialize = "rotationData"))]
    pub transform: ModelTransform,
    pub children: HashMap<String, ModelPartData>,
}

#[derive(Debug, Deserialize)]
pub struct ModelData {
    pub data: ModelPartData,
}

#[derive(Debug, Deserialize)]
pub struct TextureDimensions {
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Deserialize)]
pub struct TexturedModelData {
    pub data: ModelData,
    pub dimensions: TextureDimensions,
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
            x: 0.0,
            y: 0.0,
            z: 0.0,
            pivot_x: part.transform.pivot_x,
            pivot_y: part.transform.pivot_y,
            pivot_z: part.transform.pivot_z,
            yaw: part.transform.yaw,
            pitch: part.transform.pitch,
            roll: part.transform.roll,
            scale_x: 1.0,
            scale_y: 1.0,
            scale_z: 1.0,
        },
        cuboids: part
            .cuboid_data
            .iter()
            .map(|cuboid_data| {
                let pos = [
                    *cuboid_data.texture_uv.get("x")? as u16,
                    *cuboid_data.texture_uv.get("y")? as u16,
                ];
                let dimensions = [
                    *cuboid_data.dimensions.get("x")? as u16,
                    *cuboid_data.dimensions.get("y")? as u16,
                    *cuboid_data.dimensions.get("z")? as u16,
                ];

                Some(Cuboid {
                    x: *cuboid_data.offset.get("x")?,
                    y: *cuboid_data.offset.get("y")?,
                    z: *cuboid_data.offset.get("z")?,
                    width: *cuboid_data.dimensions.get("x")?,
                    height: *cuboid_data.dimensions.get("y")?,
                    length: *cuboid_data.dimensions.get("z")?,
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

#[derive(Deserialize)]
pub struct Wrapper2 {
    data: ModelPartData,
}

#[derive(Deserialize)]
pub struct Wrapper1 {
    data: Wrapper2,
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
            |(name, value)| match serde_json::from_value::<Wrapper1>(value) {
                Ok(wrapper) => Some((name, wrapper.data.data)),
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
