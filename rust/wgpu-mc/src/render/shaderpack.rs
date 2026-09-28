//! Serde implementation of the [shaderpack specification](https://github.com/wgpu-mc/shader-spec)

use linked_hash_map::LinkedHashMap;
use serde_derive::*;

/// semver
pub const CONFIG_VERSION: &str = "v0.0.1";
/// (major, minor, patch)
pub const CONFIG_VERSION_TRIPLE: (u32, u32, u32) = (0, 0, 1);

pub type Mat3 = [[f32; 3]; 3];
pub type Mat4 = [[f32; 4]; 4];

#[derive(Deserialize, Debug)]
pub struct ShaderPackConfig {
    pub version: String,
    pub support: String,
    pub resources: ResourcesConfig,
    pub pipelines: PipelinesConfig,
}

impl ShaderPackConfig {
    /// Returns true if the first two numbers (major and minor) are as expected.
    /// If the format is incorrect or they're different, this returns false.
    pub fn is_correct_version(&self) -> bool {
        let numbers: Vec<u32> = self
            .version
            .strip_prefix('v')
            .unwrap_or_default() // if it couldn't find the default, numbers will be empty
            .split('.')
            .map(|number| &number[..number.len() - 1])
            .map(|num_str| num_str.parse().unwrap_or(u32::MAX))
            .collect();

        numbers.len() == 3
            && numbers[0] == CONFIG_VERSION_TRIPLE.0
            && numbers[1] == CONFIG_VERSION_TRIPLE.1
            && numbers[2] != u32::MAX
    }
}

#[derive(Deserialize, Debug)]
pub struct ResourcesConfig {
    #[serde(flatten)]
    pub resources: LinkedHashMap<String, ShorthandResourceConfig>,
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
pub enum ShorthandResourceConfig {
    Int(i64),
    Float(f64),
    Mat3(Mat3),
    Mat4(Mat4),
    Longhand(LonghandResourceConfig),
}

#[derive(Deserialize, Debug)]
pub struct LonghandResourceConfig {
    #[serde(flatten)]
    pub common: CommonResourceConfig,

    #[serde(flatten)]
    pub typed: TypeResourceConfig,
}

#[derive(Deserialize, Debug)]
pub struct CommonResourceConfig {
    #[serde(default)]
    pub desc: String,

    #[serde(default)]
    pub show: bool,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TypeResourceConfig {
    Blob {
        src: String,
        #[serde(default)]
        size: usize,
    },
    #[serde(rename = "texture_3d")]
    Texture3d {
        #[serde(default)]
        src: String,
        #[serde(default)]
        clear_after_frame: bool,
    },
    #[serde(rename = "texture_2d")]
    Texture2d {
        #[serde(default)]
        src: String,
    },
    #[serde(rename = "texture_depth")]
    TextureDepth,
    F32 {
        #[serde(default)]
        range: [f32; 2],
        value: f32,
    },
    F64 {
        #[serde(default)]
        range: [f64; 2],
        value: f64,
    },
    I64 {
        #[serde(default)]
        range: [i64; 2],
        value: i64,
    },
    I32 {
        #[serde(default)]
        range: [i32; 2],
        value: i32,
    },
    Mat3(Mat3ValueOrMult),
    Mat4(Mat4ValueOrMult),
}

#[derive(Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum Mat3ValueOrMult {
    Value { value: Mat3 },
    Mult { mult: Vec<String> },
}

#[derive(Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum Mat4ValueOrMult {
    Value { value: Mat4 },
    Mult { mult: Vec<String> },
}

#[derive(Deserialize, Debug)]
pub struct PipelinesConfig {
    #[serde(flatten)]
    pub pipelines: LinkedHashMap<String, PipelineConfig>,
}

fn blend_default() -> String {
    "alpha_blending".into()
}

/// The default of [`PipelineConfig::depth_write`]: every pipeline writes depth unless it says otherwise,
/// which is what every pipeline but the translucent terrain one does.
fn depth_write_default() -> bool {
    true
}

fn output_format_default() -> String {
    "bgra8unorm".into()
}

#[derive(Deserialize, Debug, Clone, Hash, PartialEq, Eq)]
#[serde(untagged)]
pub enum BindGroupDef {
    Entries(LinkedHashMap<u64, String>),
    Resource(String),
}

#[derive(Deserialize, Debug, Clone, Hash, PartialEq, Eq)]
pub struct PipelineConfig {
    pub geometry: String,

    /// The shader this pipeline draws with, as a resource path without the extension.
    ///
    /// **`None` means the pipeline's own name**, which is how every pipeline but one works: a config that
    /// writes `terrain:` gets `wgpu_mc:shaders/terrain.wgsl`. It is spelled out here when two pipelines
    /// are the same shader with two sets of pipeline state - the two terrain passes are exactly that, and
    /// a pipeline that named `translucent_terrain` was looking for a shader that does not exist and being
    /// **skipped in silence**: `WgslShader::init` answers `None` for "no such resource", the graph logs it
    /// at a level the game's log file may not carry, and the frame goes on without that pass.
    #[serde(default)]
    pub shader: Option<String>,

    #[serde(default)]
    pub output: Vec<String>,

    /// The format of the colour attachments this pipeline draws into.
    ///
    /// Part of the pipeline rather than of the pass, because wgpu builds the pipeline against it: a
    /// pipeline built for `bgra8unorm` cannot be set in a pass whose target is `rgba8unorm`, and the
    /// error is a validation error at the first draw rather than at the pass. A renderer that presents
    /// to a swapchain is `bgra8unorm` there and `rgba8unorm` when it draws into a texture of its own.
    #[serde(default = "output_format_default")]
    pub output_format: String,

    pub depth: Option<String>,

    /// Whether this pipeline writes the depth it tests against.
    ///
    /// True for everything that draws a surface, and **false for the one pass that blends**: a
    /// translucent face has to be tested against the depth of what is behind it and must not become
    /// what the next translucent face is tested against, or the nearest pane of glass in a window hides
    /// all the others. Minecraft's own translucent terrain draws with the depth test on and the depth
    /// write off, which is what this says.
    #[serde(default = "depth_write_default")]
    pub depth_write: bool,

    #[serde(default)]
    pub clear: bool,

    #[serde(default)]
    pub bind_groups: LinkedHashMap<u64, BindGroupDef>,

    #[serde(default)]
    pub immediates: LinkedHashMap<u64, String>,

    #[serde(default = "blend_default")]
    pub blending: String,
}

#[derive(Deserialize, Debug, Clone, Hash, PartialEq, Eq)]
pub struct Uniform {
    pub resource: String,
    // pub visibility: Vec<UniformVisibility>,
}

#[derive(Deserialize, Debug, Clone, Hash, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum UniformVisibility {
    Vert,
    Frag,
}

#[cfg(test)]
mod tests {
    use std::fmt::Debug;

    use serde::Deserialize;

    use super::{
        BindGroupDef, LonghandResourceConfig, Mat3ValueOrMult, Mat4ValueOrMult, ShaderPackConfig,
        ShorthandResourceConfig, TypeResourceConfig,
    };

    fn deserialize_and_print_error<'a, T: Debug + Deserialize<'a>>(input: &'a str) -> T {
        let config: Result<T, _> = serde_norway::from_str(input);
        if let Err(err) = &config {
            if let Some(loc) = err.location() {
                let lines: Vec<&str> = input.lines().collect();
                println!("{}:{} {:?}", loc.line(), loc.column(), lines[loc.line()]);
            }
            panic!("{err}");
        }

        config.unwrap()
    }

    /// Every field the schema has, in one file: one resource of each type, in both the shorthand and
    /// the longhand spelling, and pipelines that use each field a pipeline has.
    ///
    /// This is what the schema is read against, and it is not only about the file parsing: a field
    /// this file spells and `serde` no longer knows is dropped in *silence*. `uniforms:` was the name
    /// of what is `bind_groups:` now, and a file written against the old spelling deserializes into a
    /// pipeline with no bindings and no complaint - which is a pass that draws nothing, three layers
    /// away from the line that would have said so. [`complete_file`] checks the values came out where
    /// the schema says they go, so a rename lands here rather than in a screenshot.
    const FULL_YAML: &str = r#"
version: "0.0.1"
support: glsl # could also be wgsl
resources:
  # Shorthands: a bare number or matrix is a value.
  shorthand_int: 2
  shorthand_float: 0.5
  shorthand_mat3:
    - [1.0, 0.0, 0.0]
    - [0.0, 1.0, 0.0]
    - [0.0, 0.0, 1.0]
  # Longhand, one per type the schema names.
  blob_test:
    type: blob
    src: "minecraft:textures/effect/dither.png"
    size: 256
  shadowmap_texture_depth:
    type: texture_depth
    desc: The depth buffer the shadow pass writes
    show: true
  texture_3d_test:
    type: texture_3d
    src: "minecraft:textures/atlas/blocks.png"
    clear_after_frame: true
  texture_2d_test:
    type: texture_2d
    src: "minecraft:textures/environment/sun.png"
  i32_test:
    type: i32
    range: [0, 100]
    value: 2
  int_test:
    type: i64
    range: [0, 100]
    value: 2
  f32_test:
    type: f32
    range: [-1.0, 1.0]
    value: 0.0
  f64_test:
    type: f64
    range: [-1000.0, 1000.0]
    value: 0.0
  shadow_ortho_mat4:
    type: mat4
    value: # this is just an identity matrix, it would be something different in practice
      - [1.0, 0.0, 0.0, 0.0]
      - [0.0, 1.0, 0.0, 0.0]
      - [0.0, 0.0, 1.0, 0.0]
      - [0.0, 0.0, 0.0, 1.0]
  model_view_mat4:
    type: mat4
    mult: [wm_model_mat4, wm_view_mat4]
  ortho_mat3:
    type: mat3
    mult: [wm_model_mat3]
pipelines:
  terrain_shadows:
    geometry: "@geo_terrain" # one
    depth: shadowmap_texture_depth
    output: [wm_framebuffer_texture]
    clear: true
    blending: replace
    bind_groups:
      0:
        0: shadow_ortho_mat4
        1: model_view_mat4
      1: "@bg_ssbo_chunks"
    immediates:
      0: "@pc_section_position"
  entities:
    geometry: "@geo_entities"
    depth: wm_framebuffer_depth
    output: [wm_framebuffer_texture]
    bind_groups:
      0: "@bg_entity"
"#;

    #[test]
    fn complete_file() {
        let config: ShaderPackConfig = deserialize_and_print_error(FULL_YAML);

        assert_eq!(config.version, "0.0.1");
        assert_eq!(config.support, "glsl");

        let resources = &config.resources.resources;

        // The shorthands, which are a value with no type written down.
        assert!(matches!(
            resources.get("shorthand_int"),
            Some(ShorthandResourceConfig::Int(2))
        ));
        assert!(matches!(
            resources.get("shorthand_float"),
            Some(ShorthandResourceConfig::Float(value)) if *value == 0.5
        ));
        assert!(matches!(
            resources.get("shorthand_mat3"),
            Some(ShorthandResourceConfig::Mat3(_))
        ));

        // Longhand, by type.
        let Some(ShorthandResourceConfig::Longhand(LonghandResourceConfig { common, typed })) =
            resources.get("shadowmap_texture_depth")
        else {
            panic!("shadowmap_texture_depth is a longhand resource");
        };
        assert!(matches!(typed, TypeResourceConfig::TextureDepth));
        assert!(common.show);
        assert_eq!(common.desc, "The depth buffer the shadow pass writes");

        let Some(ShorthandResourceConfig::Longhand(LonghandResourceConfig { typed, .. })) =
            resources.get("blob_test")
        else {
            panic!("blob_test is a longhand resource");
        };
        assert!(matches!(typed, TypeResourceConfig::Blob { size: 256, .. }));

        let Some(ShorthandResourceConfig::Longhand(LonghandResourceConfig { typed, .. })) =
            resources.get("texture_3d_test")
        else {
            panic!("texture_3d_test is a longhand resource");
        };
        assert!(matches!(
            typed,
            TypeResourceConfig::Texture3d {
                clear_after_frame: true,
                ..
            }
        ));

        let Some(ShorthandResourceConfig::Longhand(LonghandResourceConfig { typed, .. })) =
            resources.get("i32_test")
        else {
            panic!("i32_test is a longhand resource");
        };
        assert!(matches!(
            typed,
            TypeResourceConfig::I32 {
                range: [0, 100],
                value: 2
            }
        ));

        // A matrix is either a value or a product of other resources, and the two are told apart by
        // which key is there rather than by the type.
        let Some(ShorthandResourceConfig::Longhand(LonghandResourceConfig { typed, .. })) =
            resources.get("shadow_ortho_mat4")
        else {
            panic!("shadow_ortho_mat4 is a longhand resource");
        };
        assert!(matches!(
            typed,
            TypeResourceConfig::Mat4(Mat4ValueOrMult::Value { .. })
        ));

        let Some(ShorthandResourceConfig::Longhand(LonghandResourceConfig { typed, .. })) =
            resources.get("model_view_mat4")
        else {
            panic!("model_view_mat4 is a longhand resource");
        };
        let TypeResourceConfig::Mat4(Mat4ValueOrMult::Mult { mult }) = typed else {
            panic!("model_view_mat4 is a product, not a value");
        };
        assert_eq!(mult.join(","), "wm_model_mat4,wm_view_mat4");

        let Some(ShorthandResourceConfig::Longhand(LonghandResourceConfig { typed, .. })) =
            resources.get("ortho_mat3")
        else {
            panic!("ortho_mat3 is a longhand resource");
        };
        assert!(matches!(
            typed,
            TypeResourceConfig::Mat3(Mat3ValueOrMult::Mult { .. })
        ));

        // A pipeline, field by field, including the defaults of the ones it leaves out.
        let terrain = config
            .pipelines
            .pipelines
            .get("terrain_shadows")
            .expect("the file has a terrain_shadows pipeline");

        assert_eq!(terrain.geometry, "@geo_terrain");
        assert_eq!(terrain.depth.as_deref(), Some("shadowmap_texture_depth"));
        assert_eq!(terrain.output, ["wm_framebuffer_texture"]);
        assert!(terrain.clear);
        assert_eq!(terrain.blending, "replace");
        assert_eq!(
            terrain.immediates.get(&0).map(String::as_str),
            Some("@pc_section_position")
        );

        let Some(BindGroupDef::Entries(entries)) = terrain.bind_groups.get(&0) else {
            panic!("bind group 0 is written out entry by entry");
        };
        assert_eq!(
            entries.get(&0).map(String::as_str),
            Some("shadow_ortho_mat4")
        );
        assert_eq!(entries.get(&1).map(String::as_str), Some("model_view_mat4"));

        assert!(matches!(
            terrain.bind_groups.get(&1),
            Some(BindGroupDef::Resource(resource)) if resource == "@bg_ssbo_chunks"
        ));

        let entities = config
            .pipelines
            .pipelines
            .get("entities")
            .expect("the file has an entities pipeline");

        assert!(!entities.clear, "clear is off unless it is asked for");
        assert_eq!(
            entities.blending, "alpha_blending",
            "blending has a default, and a pipeline that leaves it out gets it"
        );
        assert!(entities.immediates.is_empty());
        assert!(entities.depth.is_some());
    }
}
