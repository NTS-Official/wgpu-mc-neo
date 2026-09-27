use byteorder::LittleEndian;
use jni::JNIEnv;
use jni::objects::{AutoElements, JClass, JFloatArray, ReleaseMode};
use jni::sys::{jfloat, jint};
use jni_fn::jni_fn;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::io::Cursor;
use std::slice;
use std::sync::Arc;
use wgpu_mc::mc::entity::BundledEntityInstances;
use wgpu_mc::texture::BindableTexture;

use crate::RENDERER;
use crate::application::{SHOULD_STOP, load_shaders};

pub static MATRICES: Lazy<Mutex<Matrices>> = Lazy::new(|| {
    Mutex::new(Matrices {
        projection: [[0.0; 4]; 4],
        view: [[0.0; 4]; 4],
        terrain_transformation: [[0.0; 4]; 4],
    })
});

pub struct Matrices {
    pub projection: [[f32; 4]; 4],
    pub view: [[f32; 4]; 4],
    pub terrain_transformation: [[f32; 4]; 4],
}

/// The game's **fog** for the frame being drawn, as the terrain shader reads it.
///
/// Twelve floats, in the order the shader's `FogEnvironment` struct declares them: the fog colour, the
/// four distances (`FogEnvironmentalStart`/`End` and `FogRenderDistanceStart`/`End`, which is the pair
/// the game's own `Fog` block carries for terrain), and the camera's offset inside its own section -
/// which is not part of the game's block at all, and is here because the shader needs the vertex's
/// *camera-relative* position to measure a fog distance from, and the position it computes is relative
/// to the camera's section.
///
/// All zeroes until the JVM sends one, and a fog colour of `(0, 0, 0, 0)` is a fog whose alpha scales
/// the blend to nothing - so a frame drawn before the first send is the unfogged picture this renderer
/// drew before it had fog at all, rather than a world fading to black.
pub static FOG: Mutex<[f32; 12]> = Mutex::new([0.0; 12]);

/// Takes the fog block the JVM read out of the frame's camera render state. See [`FOG`].
#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn setFogEnvironment(mut env: JNIEnv, _class: JClass, values: JFloatArray) {
    let Ok(elements) = (unsafe { env.get_array_elements(&values, ReleaseMode::NoCopyBack) }) else {
        log::warn!("wgpu-mc: the fog block could not be read out of the JVM's array");
        return;
    };

    let slice = unsafe { slice::from_raw_parts(elements.as_ptr(), elements.len()) };

    if slice.len() < 12 {
        log::warn!(
            "wgpu-mc: the fog block arrived with {} value(s) instead of 12; keeping the last one",
            slice.len()
        );
        return;
    }

    let mut fog = FOG.lock();

    for (slot, value) in fog.iter_mut().zip(slice.iter()) {
        *slot = *value;
    }
}

#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn reloadShaders(_env: JNIEnv, _class: JClass) {
    load_shaders(RENDERER.get().unwrap());
}

#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn setMatrix(mut env: JNIEnv, _class: JClass, id: jint, float_array: JFloatArray) {
    let elements: AutoElements<jfloat> =
        unsafe { env.get_array_elements(&float_array, ReleaseMode::NoCopyBack) }.unwrap();

    let slice = unsafe { slice::from_raw_parts(elements.as_ptr(), elements.len()) };

    let mut cursor = Cursor::new(bytemuck::cast_slice::<f32, u8>(slice));
    let mut converted = Vec::with_capacity(slice.len());

    for _ in 0..slice.len() {
        use byteorder::ReadBytesExt;
        converted.push(cursor.read_f32::<LittleEndian>().unwrap());
    }

    let slice_4x4: [[f32; 4]; 4] = *bytemuck::from_bytes(bytemuck::cast_slice(&converted));

    match id {
        0 => {
            MATRICES.lock().projection = slice_4x4;
        }
        1 => {
            // MATRICES.lock(). = slice_4x4;
        }
        2 => {
            MATRICES.lock().view = slice_4x4;
        }
        3 => {
            MATRICES.lock().terrain_transformation = slice_4x4;
        }
        _ => {}
    }
}

#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn scheduleStop(_env: JNIEnv, _class: JClass) {
    let _ = SHOULD_STOP.set(());
}

// The texture and entity-instance tables are the JVM side's, declared here before the calls that
// fill them arrived: nothing in this crate reads either, and `WgpuNative` has no binding for them
// yet. Marked rather than deleted so that the day one arrives, the table it fills is already here.
#[allow(dead_code)]
#[derive(Copy, Clone, Hash, Eq, PartialEq)]
pub enum MCTextureId {
    BlockAtlas,
    Lightmap,
}

#[allow(dead_code)]
pub static ENTITY_INSTANCES: Lazy<Mutex<HashMap<String, BundledEntityInstances>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

#[allow(dead_code)]
pub static MC_TEXTURES: Lazy<Mutex<HashMap<MCTextureId, Arc<BindableTexture>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
