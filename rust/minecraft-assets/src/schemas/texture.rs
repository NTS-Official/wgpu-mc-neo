//! Serde-(de)serializable data types for
//! `assets/<namespace>/textures/{block}/*.mcmeta`
//!
//! Start here: [`Texture`].
//!
//! See <https://minecraft.fandom.com/wiki/Resource_Pack>.

use serde::{Deserialize, Serialize};

///Describes the order of a single frame in a texture animation
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
#[serde(untagged)]
pub enum Frame {
    ///Normal frame order
    Index(u32),
    ///Describes if a frame should display differently than in the default order. Unused in vanilla
    Override {
        ///Index of the frame in the texture
        index: u32,
        ///How long this specific frame should last for
        time: u32,
    },
}

fn one() -> i32 {
    1
}

///A struct representing a textures animation within a texture .mcmeta file
#[derive(Deserialize, Serialize, Debug, Default, Clone, PartialEq)]
pub struct TextureAnimation {
    ///Describes whether or not the game should "blend" between frames
    #[serde(default)]
    pub interpolate: bool,
    ///Not used in vanilla's asset files
    pub width: Option<u32>,
    ///Not used in vanilla's asset files
    pub height: Option<u32>,
    ///How quickly, in ticks, the animation should progress. Defaults to one.
    #[serde(default = "one")]
    pub frametime: i32,
    ///Extra information about each frame
    pub frames: Option<Vec<Frame>>,
}

///The struct representing an entire texture .mcmeta
#[derive(Deserialize, Serialize, Debug, Default, Clone, PartialEq)]
pub struct Texture {
    ///Optional, describes the animation a texture will have in-game
    pub animation: Option<TextureAnimation>,
}
