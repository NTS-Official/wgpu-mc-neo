#![feature(debug_closure_helpers)]
#![feature(ptr_metadata)]
// `TERRAIN_MATRICES` is a `Lazy<Mutex<Option<TerrainMatrices>>>` through wgpu's `Buffer`, and proving
// it `Sync` walks the same auto-trait chain `wgpu-mc` has to walk for `WgslShader` - deeper than the
// default limit of 128. See the note on the same attribute in `wgpu-mc`'s `lib.rs`.
#![recursion_limit = "512"]
pub extern crate wgpu_mc;

use arc_swap::ArcSwap;
use core::slice;
use glam::{IVec3, ivec3};
use jni::objects::{
    AutoElements, GlobalRef, JByteArray, JClass, JObject, JString, JValue, JValueOwned,
    ReleaseMode, WeakRef,
};
use jni::sys::{jboolean, jbyte, jfloat, jint, jlong, jlongArray, jstring};
use jni::{JNIEnv, JavaVM};
use jni_fn::jni_fn;
use once_cell::sync::{Lazy, OnceCell};
use parking_lot::{Mutex, RwLock};
use rayon::{ThreadPool, ThreadPoolBuilder};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Debug;
use std::io::{Write, stdout};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use wgpu_mc::render::graph::{Geometry, RenderGraph};

use wgpu_mc::WmRenderer;
use wgpu_mc::mc::block::{BlockstateKey, ChunkBlockState, FaceFlags};
use wgpu_mc::mc::chunk::{BlockStateProvider, LightLevel, RenderLayer, bake_section};
use wgpu_mc::mc::resource::{ResourcePath, ResourceProvider};
use wgpu_mc::minecraft_assets::schemas::blockstates::multipart::StateValue;
use wgpu_mc::render::pipeline::BLOCK_ATLAS;

use crate::section::{
    CENTER, CachedBlockstateProvider, Payload, REJECTED_MASK, RESYNC, SECTIONS, SectionBlocks,
    SectionLight, WORLD, bump_section_generation, neighbour_offset, section_generation,
    section_key,
};
use crate::settings::Settings;

mod alloc;
mod application;
pub mod blaze;
mod debug;
mod device;
pub mod entity;
mod gl;
mod lighting;
mod palette;
mod pia;
mod pix;
pub mod preprocessing;
mod renderer;
mod section;
mod settings;
mod shader_cache;
mod timing;

/// Checks that the JVM side of the two bridges still matches this crate: the JNI declarations in
/// `WgpuNative.kt`, the hand-written C-ABI bindings in `WmNative.kt`, and the struct layouts and
/// enum numbers `bindings.h` describes. Nothing else checks those, and each of them fails at
/// runtime rather than at compile time.
#[cfg(test)]
mod abi_tests;

#[derive(Debug)]
struct MinecraftRenderState {
    //draw_queue: Vec<>,
    _render_world: bool,
}

#[allow(dead_code)]
struct MouseState {
    pub x: f64,
    pub y: f64,
}

// static ENTITIES: OnceCell<HashMap<>> = OnceCell::new();
static RENDERER: OnceCell<WmRenderer> = OnceCell::new();

pub static RENDER_GRAPH: OnceCell<Mutex<RenderGraph>> = OnceCell::new();
pub static CUSTOM_GEOMETRY: OnceCell<Mutex<HashMap<String, Box<dyn Geometry>>>> = OnceCell::new();

static RUN_DIRECTORY: OnceCell<PathBuf> = OnceCell::new();

static MC_STATE: Lazy<ArcSwap<MinecraftRenderState>> = Lazy::new(|| {
    ArcSwap::new(Arc::new(MinecraftRenderState {
        _render_world: false,
    }))
});

/// The block state a section's holes are, or `None` while the block registry is empty.
///
/// `None` is a real state of the world, not an error: this build has no block atlas yet, so
/// `bake_blocks` bakes nothing and `minecraft:air` is simply not in the registry. Baking without it
/// would have to guess what "air" is, and guessing wrong is geometry built out of nothing, so the
/// bake refuses instead - see `bakeSection`.
static AIR: Lazy<Option<BlockstateKey>> = Lazy::new(|| {
    RENDERER.get().and_then(|renderer| {
        renderer
            .mc
            .block_manager
            .read()
            .blocks
            .get_full("minecraft:air")
            .map(|(id, _, _)| BlockstateKey {
                block: id as u16,
                augment: 0,
            })
    })
});

static BLOCKS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// One block state as `RegistryMixin` offers it, before the block registry is built.
struct BlockStateRegistration {
    block_name: String,
    state_key: String,
    global_ref: GlobalRef,
}

static BLOCK_STATES: Mutex<Vec<BlockStateRegistration>> = Mutex::new(Vec::new());

/// What each block state says about the faces around it, keyed by the packed state key.
///
/// Filled by [`registerBlockStateFaceFlags`] while [`cacheBlockStates`] hands the keys out, and drained
/// into the block manager at the end of that same call. See `wgpu_mc::mc::block::FaceFlags` for what
/// the two masks mean and why they come from the JVM rather than from the block's model.
static BLOCK_STATE_FACE_FLAGS: Mutex<Vec<(u32, FaceFlags)>> = Mutex::new(Vec::new());

/// The blocks whose states baked to a mesh with no faces in it, over the whole run.
///
/// A block state with an empty mesh is drawn as nothing while still occluding its neighbours - see
/// [`note_empty_mesh`] - and it is the shape of failure this renderer has spent the most time on, so
/// the names are kept for the JVM to log rather than left in the Rust log, which the game's log file
/// does not carry.
static EMPTY_MESH_BLOCKS: Mutex<Vec<(String, u64)>> = Mutex::new(Vec::new());

/// How many blocks that baked to nothing are remembered by name.
const EMPTY_MESH_NAMES: usize = 24;

/// Notes that one state of `block` baked to a mesh with no faces in it.
fn note_empty_mesh(block: &str) {
    let mut blocks = EMPTY_MESH_BLOCKS.lock();

    if let Some(entry) = blocks.iter_mut().find(|(name, _)| name == block) {
        entry.1 += 1;
        return;
    }

    if blocks.len() < EMPTY_MESH_NAMES {
        blocks.push((block.to_string(), 1));
    }
}

/// The blocks whose states had to take the bedrock fallback, by name, over the whole run.
///
/// A state whose *block* is not in the registry at all - its blockstate file could not be read, or its
/// models did not bake - is drawn as bedrock, and the count of them has always been printed. Which
/// blocks they are is the part that was missing, and it is the part that says what to look at: a
/// resource pack whose copy of one file is malformed, a block this renderer cannot read, a model whose
/// texture is nowhere. Kept for [`blockBakeDiagnostics`], which the JVM logs.
static UNMODELLED_BLOCKS: Mutex<Vec<(String, u64)>> = Mutex::new(Vec::new());

/// How many unmodelled blocks are remembered by name.
const UNMODELLED_NAMES: usize = 24;

/// Notes that one state of `block` had no model and took the bedrock fallback.
fn note_unmodelled(block: &str) {
    let mut blocks = UNMODELLED_BLOCKS.lock();

    if let Some(entry) = blocks.iter_mut().find(|(name, _)| name == block) {
        entry.1 += 1;
        return;
    }

    if blocks.len() < UNMODELLED_NAMES {
        blocks.push((block.to_string(), 1));
    }
}

/// Set once [`cacheBlockStates`] has built the block manager from the game's resources.
///
/// Everything that bakes geometry needs it: `AIR` and the model lookup behind [`bake_layers`] are
/// built from that registry, and asking for them before it exists used to be a panic - which, on a
/// `#[jni_fn]` frame, is the JVM aborting. A section rebuild can arrive first, because the client
/// caches block states on the title screen while a quickplay launch is already loading chunks.
static BLOCKS_CACHED: AtomicBool = AtomicBool::new(false);
pub static SETTINGS: RwLock<Option<Settings>> = RwLock::new(None);

pub static CLASSLOADER: OnceCell<WeakRef> = OnceCell::new();

/// Looks up a class through the loader the game was started with, and calls a static method on it.
///
/// `FindClass` on a thread that was attached from native code resolves against the *system* class
/// loader, which cannot see NeoForge's transformed game classes, so the loader has to be handed
/// over from the JVM side by [`setClassLoader`].
///
/// Every failure here is an `Err`, never a panic. This is reached from
/// [`MinecraftResourceManagerAdapter::get_bytes`], which wgpu-mc calls from whatever thread is
/// loading a resource, and a panic inside a `#[jni_fn]` cannot unwind - it aborts the whole
/// process, which is how a missing class loader used to take the game down.
pub fn call_static_from_class_loader<'env>(
    env: &mut JNIEnv<'env>,
    class: &str,
    method: &str,
    sig: &str,
    args: &[JValue],
) -> jni::errors::Result<JValueOwned<'env>> {
    let Some(class_loader) = CLASSLOADER.get() else {
        return Err(jni::errors::Error::NullPtr(
            "the game's class loader was never registered - see setClassLoader",
        ));
    };

    // Only a weak reference is held, so it is legitimate for the JVM to have collected it.
    let Some(class_loader) = class_loader.upgrade_local(&*env)? else {
        return Err(jni::errors::Error::NullPtr(
            "the game's class loader has been garbage collected",
        ));
    };

    let arg = env.new_string(class)?;
    // `loadClass`, not `findClass`. `findClass` is the loader's *define* hook: it skips the cache and
    // asks the loader to produce the class again, which for a class that is already loaded ends in
    // "attempted duplicate class definition" - a LinkageError, on a thread whose every later JNI call
    // then fails as well. `loadClass` is the public lookup: parent first, cache included.
    let class_obj: JClass = env
        .call_method(
            class_loader,
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[JValue::Object(&arg)],
        )?
        .l()?
        .into();

    env.call_static_method(class_obj, method, sig, args)
}

/// Registers the class loader Rust calls back through.
///
/// Called by the JVM side as part of loading the native library, before anything can ask for a
/// resource. A weak reference is enough and is what [`call_static_from_class_loader`] expects: the
/// loader is owned by the mod loader for the lifetime of the process, so it cannot go away while
/// the game is running, and holding it strongly here would keep it - and every class it loaded -
/// alive past shutdown.
#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn setClassLoader(env: JNIEnv, _class: JClass, class_loader: JObject) {
    match env.new_weak_ref(class_loader) {
        Ok(Some(weak)) => {
            if CLASSLOADER.set(weak).is_err() {
                log::warn!("wgpu-mc: the game's class loader was registered more than once");
            }
        }
        Ok(None) => log::error!("wgpu-mc: the game's class loader was null"),
        Err(err) => log::error!("wgpu-mc: could not register the game's class loader: {err}"),
    }
}

struct MinecraftResourceManagerAdapter {
    jvm: JavaVM,
}

impl ResourceProvider for MinecraftResourceManagerAdapter {
    /// Reads a resource through the game's own resource provider.
    ///
    /// Nothing in here may panic. wgpu-mc calls this from whichever thread is loading a resource,
    /// and a panic on a `#[jni_fn]` frame cannot unwind - it aborts the JVM, so a single missing
    /// or unreadable file would take the whole game down instead of just that texture. The trait
    /// already models "no such resource" as `None`, so every failure becomes one, with a log line.
    fn get_bytes(&self, id: &ResourcePath) -> Option<Vec<u8>> {
        let mut env = match self.jvm.attach_current_thread() {
            Ok(env) => env,
            Err(err) => {
                log::error!(
                    "wgpu-mc: could not attach to the JVM to read {}: {err}",
                    id.0
                );
                return None;
            }
        };

        let path = match env.new_string(&id.0) {
            Ok(path) => path,
            Err(err) => {
                // A failure here almost always means an *earlier* call on this thread left an
                // exception pending: every JNI call after that fails too, which is how one bad
                // resource read turns into "nothing can be read". Describing and clearing it is what
                // puts the original Java stack in the log and lets the next read succeed.
                describe_and_clear(&mut env, &id.0);
                log::error!("wgpu-mc: could not pass {} to the JVM: {err}", id.0);
                return None;
            }
        };

        let bytes: JByteArray = match call_static_from_class_loader(
            &mut env,
            "dev.birb.wgpu.rust.WgpuResourceProvider",
            "getResource",
            "(Ljava/lang/String;)[B",
            &[JValue::Object(&path.into())],
        )
        .and_then(|value| value.l())
        {
            Ok(bytes) => bytes.into(),
            Err(err) => {
                // The Java stack of whatever `getResource` threw, printed before the exception is
                // cleared: a pending exception makes every later JNI call on this thread fail, so
                // the original failure would otherwise be reported as a second, unrelated one.
                describe_and_clear(&mut env, &id.0);
                log::error!("wgpu-mc: {} could not be read: {err}", id.0);
                return None;
            }
        };

        // The provider answers with an empty array for a resource it does not have.
        if bytes.is_null() {
            return None;
        }

        let elements: AutoElements<jbyte> =
            match unsafe { env.get_array_elements(&bytes, ReleaseMode::NoCopyBack) } {
                Ok(elements) => elements,
                Err(err) => {
                    log::error!("wgpu-mc: could not read the bytes of {}: {err}", id.0);
                    return None;
                }
            };

        let size = elements.len();
        if size == 0 {
            return None;
        }

        Some(Vec::from(unsafe {
            slice::from_raw_parts(elements.as_ptr() as *const u8, size)
        }))
    }
}

/// Says what a pending Java exception is, and clears it.
///
/// A pending exception makes *every* later JNI call on the same thread fail, so leaving one behind
/// turns "this one resource could not be read" into "nothing on this thread can be read". Printing
/// it first is what keeps the original Java stack in the log, and clearing it is what lets the next
/// call have a chance.
fn describe_and_clear(env: &mut JNIEnv, what: &str) {
    if !env.exception_check().unwrap_or(false) {
        return;
    }

    log::error!("wgpu-mc: {what}: the JVM threw while reading this resource");

    if let Err(err) = env.exception_describe() {
        log::error!("wgpu-mc: {what}: and the exception could not be described: {err}");
    }

    let _ = env.exception_clear();
}

#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn getSettingsStructure(env: JNIEnv, _class: JClass) -> jstring {
    env.new_string(crate::settings::SETTINGS_INFO_JSON.clone())
        .unwrap()
        .into_raw()
}

#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn getSettings(env: JNIEnv, _class: JClass) -> jstring {
    let json = match SETTINGS.read().as_ref() {
        Some(settings) => serde_json::to_string(settings).unwrap_or_else(|_| "{}".to_string()),
        None => {
            // The options screen is reachable before client setup in principle, and an `unwrap`
            // here would run the panic hook, which exits the game.
            log::warn!("wgpu-mc: settings were read before the run directory was sent");
            "{}".to_string()
        }
    };

    env.new_string(json).unwrap().into_raw()
}

/// Applies the settings the options screen sent, and persists them.
///
/// The write to disk is not optional: `sendRunDirectory` loads the settings from
/// `config/wgpu-mc-renderer.json` at startup, so a setting that is only stored in memory is lost
/// on the next launch. That matters most for `backend`, which by design cannot take effect until
/// the game is restarted - forgetting it would make the switch look like it did nothing at all.
#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn sendSettings(mut env: JNIEnv, _class: JClass, settings: JString) -> bool {
    if SETTINGS.read().is_none() {
        // `getSettings` hands out `{}` in this state, and every field has a serde default, so
        // accepting that would quietly write the defaults over the player's config.
        log::error!("wgpu-mc: refusing to save settings before the run directory was sent");
        return false;
    }

    let json: String = env.get_string(&settings).unwrap().into();
    let Ok(settings) = serde_json::from_str::<Settings>(json.as_str()) else {
        log::error!("wgpu-mc: the options screen sent settings that could not be parsed");
        return false;
    };

    if !settings.write() {
        // The settings are still applied below, so the running game behaves as asked; only the
        // next launch will not see them.
        log::error!("wgpu-mc: the renderer settings could not be saved and will be lost on exit");
    }

    // The debug switches are read on the draw path, so they are copied out of the settings rather
    // than looked up per draw. This is what makes an option on the debug page take effect the
    // moment it is applied - the switch is on the next draw, not on the next launch.
    //
    // Read before the apply, so that what moved can be told apart from what was merely stored: one
    // setting here (`animated_textures`) is written into baked geometry rather than read per draw.
    let animated_textures_before = SETTINGS
        .read()
        .as_ref()
        .map(|settings| settings.animated_textures());

    crate::debug::apply(&settings);

    let animated_textures_after = settings.animated_textures();

    *SETTINGS.write() = Some(settings);

    // A setting that is *baked* cannot be applied by storing it: `animated_textures` decides, face by
    // face, which atlas that face samples, and the answer went into the vertices when the block models
    // were baked. So the JVM is asked to bake them again - it owns the block registry and the level
    // renderer - and it does that on its own thread: see `BlockCache.blockTexturesChanged`.
    //
    // Asked for on a *change* rather than on every apply, because the re-bake costs a second or two and
    // re-meshes every loaded section: an Apply that touched nothing but the debug switches must not
    // rebuild the world.
    if animated_textures_before != Some(animated_textures_after) {
        log::info!(
            "wgpu-mc: animated block textures are now {}; asking the JVM to bake the block models again",
            if animated_textures_after { "on" } else { "off" }
        );

        if let Err(err) = call_static_from_class_loader(
            &mut env,
            "dev.birb.wgpu.BlockCache",
            "blockTexturesChanged",
            "()V",
            &[],
        ) {
            log::warn!(
                "wgpu-mc: the animated-texture switch moved, but the JVM could not be asked to bake \
                 the block models again, so the old answer stays in the world until something else \
                 bakes them: {err}"
            );
        }
    }

    // `vsync` only picks the swapchain's present mode, so unlike `backend` it can be applied here
    // and now: this re-resolves the mode from the settings that were just stored and reconfigures
    // the surface when it differs. A no-op for every other setting, and for a value that did not
    // change.
    crate::device::reapply_present_mode();

    true
}

#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn sendRunDirectory(mut env: JNIEnv, _class: JClass, dir: JString) {
    let dir: String = env.get_string(&dir).unwrap().into();
    let path = PathBuf::from(dir);

    // Called twice on purpose: once from the mod constructor, before the renderer exists and the
    // backend setting still matters, and once from client setup, which is where this used to live.
    // `OnceCell::set` fails the second time, and unwrapping that failure used to be a panic - which
    // runs the panic hook, which exits the game.
    if RUN_DIRECTORY.set(path).is_err() {
        return;
    }

    let mut write = SETTINGS.write();
    let settings = Settings::load_or_default();
    // Before the renderer exists in most launches, so the debug switches are already resolved by
    // the time the first draw asks for them.
    crate::debug::apply(&settings);
    *write = Some(settings);
}

#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn getBackend(env: JNIEnv, _class: JClass) -> jstring {
    let renderer = RENDERER.get().unwrap();
    let backend = renderer.get_backend_description();

    env.new_string(backend).unwrap().into_raw()
}

/// The adapter's vendor, name, API and driver, one per line.
///
/// Unlike [`getBackend`] this one answers with an empty string when there is no renderer yet - the
/// F3 overlay can be opened before one exists, and the JVM side has its own wording to fall back
/// to. See `WmRenderer#get_adapter_description` for why the four fields are the four it asks for.
#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn getAdapterInfo(env: JNIEnv, _class: JClass) -> jstring {
    let description = RENDERER
        .get()
        .map(WmRenderer::get_adapter_description)
        .unwrap_or_default();

    env.new_string(description).unwrap().into_raw()
}

#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn registerBlockState(
    mut env: JNIEnv,
    _class: JClass,
    block_state: JObject,
    block_name: JString,
    state_key: JString,
) {
    let global_ref = env.new_global_ref(block_state).unwrap();

    let block_name: String = env.get_string(&block_name).unwrap().into();
    let state_key: String = env.get_string(&state_key).unwrap().into();

    BLOCK_STATES.lock().push(BlockStateRegistration {
        block_name,
        state_key,
        global_ref,
    });
}

/// One sprite's place in the game's atlas and the layer the game files it under, on the way from
/// [`registerSprite`] to the atlas this side packs.
///
/// A queue rather than a direct write, because of when the call arrives: the game registers its sprites
/// before [`cacheBlockStates`], and the atlas this side packs does not exist until that bake runs. See
/// [`wgpu_mc::render::atlas::Atlas::register_sprite`].
static SPRITE_REGISTRATIONS: Mutex<Vec<(String, [f32; 4], u8)>> = Mutex::new(Vec::new());

/// Whether anything is waiting in that queue.
///
/// Read by the drain, which sits where a bake holds an atlas - inside a per-state loop, so the question
/// has to be one atomic load rather than a lock. A flag set by the writer and cleared by the reader,
/// rather than a "have we drained yet": a resource reload registers every sprite again, and a drain that
/// only ever ran once would leave that second set in the queue for good.
static SPRITE_REGISTRATIONS_PENDING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// One sprite of the game's block atlas: where it is, and which layer the game puts it in.
///
/// Both in one call on purpose. They are the two things only the game knows - the rectangle is the
/// game's own atlas layout, and the layer is the game's reading of the sprite's transparency, which
/// this side otherwise guesses from the pixels - and they are read at the same moment, so splitting
/// them into two calls would only be two chances to be half-registered.
///
/// The layer is one of `WgpuNative`'s `LAYER_*` numbers: 0 solid, 1 cutout, 2 transparent. They are the
/// numbers this function maps, and they are deliberately *not* taken from the Rust enum's
/// discriminants: the JVM side names them, so a variant added here cannot silently move what a
/// registered layer means. Note the one name that differs across the bridge - the game calls its third
/// chunk layer `TRANSLUCENT` and this side calls it `Transparent` - because a rename that only happens
/// on one side is a layer that quietly stops matching.
#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn registerSprite(
    mut env: JNIEnv,
    _class: JClass,
    name: JString,
    u0: jfloat,
    v0: jfloat,
    u1: jfloat,
    v1: jfloat,
    layer: jint,
) {
    let Ok(name) = env.get_string(&name) else {
        // A name that is not readable UTF-8 is a sprite this side cannot look up by anything, and
        // there is nothing to fall back to: the faces that name it keep the sprite they would have had.
        log::warn!("wgpu-mc: a registered sprite's name could not be read; skipping it");
        return;
    };

    SPRITE_REGISTRATIONS_PENDING.store(true, std::sync::atomic::Ordering::Relaxed);

    SPRITE_REGISTRATIONS.lock().push((
        name.into(),
        [u0, v0, u1, v1],
        layer as u8 & 0b0000_0011,
    ));
}

/// The layer a registered sprite is filed under. See [`registerSprite`] for the numbers.
fn registered_layer(layer: u8) -> RenderLayer {
    match layer {
        1 => RenderLayer::Cutout,
        2 => RenderLayer::Transparent,
        _ => RenderLayer::Solid,
    }
}

/// What one block state says about the faces around it, keyed by the packed state key it wears.
///
/// Sent from `Wgpu#helperSetBlockStateIndex` rather than from the registration mixin, and that is not
/// an accident of plumbing: the masks are read from the state's occlusion *shapes*, which
/// `BlockStateBase#initCache` fills in - and the game runs that at the end of `Blocks`' class
/// initializer, so during block registration every one of them is still null. Asking there is a null
/// dereference during bootstrap, which is exactly how this crashed the game once.
///
/// The keys are the ones [`cacheBlockStates`] is handing out at the same moment, so the two arrive in
/// step: this is called from inside the Java callback that sets a state's key, and the map is built
/// after that loop returns.
#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn registerBlockStateFaceFlags(
    _env: JNIEnv,
    _class: JClass,
    key: jint,
    occlusion: jint,
    self_hide: jint,
    shades: jint,
    blocks_motion: jint,
) {
    BLOCK_STATE_FACE_FLAGS.lock().push((
        key as u32,
        FaceFlags {
            // Six bits of six directions, from `Direction.ordinal()`; the high bits are not
            // directions and are dropped rather than carried into a mask the baker shifts.
            occlusion: occlusion as u8 & 0b0011_1111,
            self_hide: self_hide as u8 & 0b0011_1111,
            // Whether the block darkens the corners it touches - the game's own
            // `getShadeBrightness`, which is not a shape question the masks above could answer.
            shades: shades != 0,
            // And whether it blocks motion, which is what a fluid looks past when it decides where it
            // is flowing (`FlowingFluid#getFlow`). See `FaceFlags::blocks_motion`.
            blocks_motion: blocks_motion != 0,
        },
    ));
}

struct MinecraftBlockStateProviderWrapper<'a> {
    internal: CachedBlockstateProvider,
    env: RefCell<JNIEnv<'a>>,
}

impl<'a> BlockStateProvider for MinecraftBlockStateProviderWrapper<'a> {
    fn get_state(&self, pos: IVec3) -> ChunkBlockState {
        self.internal.get_state(pos)
    }

    fn get_light_level(&self, pos: IVec3) -> LightLevel {
        self.internal.get_light_level(pos)
    }

    /// The fluid a block holds, which is the only thing that says a section has lava or water in it.
    ///
    /// Forwarded like the rest, and for a reason worth writing down: the trait's own default answers
    /// "no fluid" for every block, and this wrapper is the provider the bake actually runs with - so
    /// without this method the fluid mesher asked a provider that always said no, and lava went
    /// missing without a single line in any log. It was the counters on the bridge that found it.
    fn get_fluid(&self, pos: IVec3) -> u8 {
        self.internal.get_fluid(pos)
    }

    fn is_section_empty(&self, rel_pos: IVec3) -> bool {
        self.internal.is_section_empty(rel_pos)
    }

    /// The biome tint for one tinted face, asked of the game itself: the tint is a property of the
    /// world at that position, and this side only has the section data.
    ///
    /// This runs on a bake thread, which is a plain native thread attached to the JVM, so the class
    /// has to be looked up through the game's own loader - `FindClass` on such a thread resolves
    /// against the system loader, which cannot see NeoForge's transformed classes, and the
    /// `.unwrap()` that used to be here took the pool thread (and, through the second panic, the
    /// process) down the moment a tinted face was baked. White is what a face with no tint gets, so
    /// that is the answer when the call cannot be made, once per failure mode with a log line.
    fn get_block_color(&self, pos: IVec3, tint_index: i32) -> u32 {
        let mut env = self.env.borrow_mut();

        let result = call_static_from_class_loader(
            &mut env,
            "dev.birb.wgpu.render.Wgpu",
            "helperGetBlockColor",
            "(IIII)I",
            &[
                JValue::Int(pos.x),
                JValue::Int(pos.y),
                JValue::Int(pos.z),
                JValue::Int(tint_index),
            ],
        )
        .and_then(|value| value.i());

        match result {
            Ok(color) => color as u32,
            Err(err) => {
                static WARNED: AtomicBool = AtomicBool::new(false);
                if !WARNED.swap(true, Ordering::Relaxed) {
                    // The Java side of the call is the interesting part: a `JavaException` here can
                    // be the game's own, and it stays pending on this thread until it is described
                    // and cleared - which would make every later call on the thread fail too.
                    describe_and_clear(&mut env, "helperGetBlockColor");
                    log::warn!(
                        "wgpu-mc: could not ask the game for a biome tint ({err}); tinted faces are \
                         left untinted"
                    );
                }

                0xffff_ffff
            }
        }
    }
}

/// One section rebuild, in one call.
///
/// The payload is written by `RustChunkBake` into a buffer it owns and reuses, and `address`/`length`
/// describe it: a structure of records followed by their blobs - Minecraft's own storage longs, the
/// palette translation table, and the light layers that changed. One call rather than four arrays and
/// a handle per section, because the old shape was ~57 JNI calls and 60 arrays per rebuild, all of it
/// for data the JVM already had in exactly this form.
///
/// Returns an answer the JVM reads as two things: a bit per section record it did *not* accept (see
/// [`REJECTED_MASK`]), and whether the caller has to send the whole neighbourhood again
/// ([`RESYNC`]).
///
/// A record is refused when it was written for another generation of the world - a level change while
/// this chunk build was already running - and the JVM's reaction is to leave those sections out of
/// its "what have I sent" table, so the next rebuild carries them again. The resync bit is the older
/// signal: this side keeps the light of each section and drops the ones far from the player, so a
/// section the JVM counts as already sent can be gone here, and then the caller has to clear its
/// bookkeeping and call once more with everything it has.
#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn bakeSections(
    _env: JNIEnv,
    _class: JClass,
    x: jint,
    y: jint,
    z: jint,
    address: jlong,
    length: jint,
) -> jint {
    // A rebuild can arrive before the client has cached block states, and the cache itself cannot
    // build anything while no block atlas is registered. `AIR` is the registry's, so without it
    // there is no way to tell a hole from a block: say so once and let the caller try again later.
    // Nothing was accepted, which is what the answer says - the sections stay unsent on the JVM side
    // and come back with the next rebuild.
    let Some(air) = *AIR else {
        static WARNED: AtomicBool = AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::Relaxed) {
            log::warn!(
                "wgpu-mc: a section was offered for baking while the block registry was empty; \
                 skipping until it is built"
            );
        }
        return REJECTED_MASK as jint;
    };

    if length <= 0 || address == 0 {
        log::error!("wgpu-mc: a section bake arrived with no payload; dropping it");
        return RESYNC as jint;
    }

    // The caller's buffer, read and left alone: everything kept is copied out below, before this
    // returns and the JVM writes the next payload over it.
    let bytes = unsafe { slice::from_raw_parts(address as *const u8, length as usize) };

    let generation = section_generation();

    // A payload this side cannot read is one it did not receive. Saying so - rather than answering
    // "all good" - is what makes the JVM forget what it thought had been sent and hand the whole
    // neighbourhood over again, which is the only way back to a cache the two sides agree on.
    let Some(mut payload) = Payload::parse(bytes, generation) else {
        return RESYNC as jint;
    };

    let target = ivec3(x, y, z);

    let mut world = WORLD.write();

    // Apply the payload first, so a bake queued below never sees a half-applied neighbourhood: the
    // sections the caller sent replace what was held for them, and the ones it says to forget are
    // dropped - those are the sections that became air or were unloaded.
    let rejected = payload.apply(&mut world, target);

    // A section the JVM marked as "already yours" but that this side does not have - a cache that was
    // trimmed, a new world, a section that became empty under us - means a bake against holes. The
    // caller sends everything again instead, and this call queues nothing.
    let known_blocks = payload.known_blocks & !payload.present;
    let known_light = payload.known_light
        & !payload
            .light
            .iter()
            .fold(0u32, |mask, (index, _)| mask | (1 << index));

    let (missing_blocks, missing_light) = world.missing(target, known_blocks, known_light);

    if missing_blocks != 0 || missing_light != 0 {
        static RESYNCS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let resyncs = RESYNCS.fetch_add(1, Ordering::Relaxed);
        if resyncs < 8 || resyncs.is_multiple_of(256) {
            log::info!(
                "wgpu-mc: around {target:?} the JVM counts {missing_blocks:#b} (blocks) and \
                 {missing_light:#b} (light) as already sent, and this side does not have them; asking \
                 for the neighbourhood again ({resyncs} resync(s) so far)"
            );
        }

        // What did arrive stays: it is the newest version of those sections either way.
        return (rejected | RESYNC) as jint;
    }

    let mut blocks: [Option<Arc<SectionBlocks>>; SECTIONS] = Default::default();
    let mut light: [Option<Arc<SectionLight>>; SECTIONS] = Default::default();

    // Every slot is resolved from the cache, not just the ones this payload carried: a section the
    // caller did not send is one it believes is already here, and a slot that is still empty is a
    // section that is not loaded - which is air, as it is for Minecraft's own mesher.
    for index in 0..SECTIONS {
        let pos = target + neighbour_offset(index);
        blocks[index] = world.blocks(pos);
        light[index] = world.light(pos);
    }

    let provider = CachedBlockstateProvider { blocks, light, air };

    world.trim(target);
    drop(world);

    let jvm = match _env.get_java_vm() {
        Ok(jvm) => jvm,
        Err(err) => {
            log::error!("wgpu-mc: could not get the JVM handle for a bake: {err}");

            // No bake was queued, so the section this call was about is not on its way to the arena -
            // and the JVM must not be told it was. Nothing of it will be drawn until it is offered
            // again, and a rebuild only draws what changed: without this bit the section would be
            // recorded as sent, the next rebuild would find nothing to say, and the hole would be
            // permanent. The 26 neighbours *were* applied, and they are still the newest version of
            // those sections, so the refusal is the one slot.
            return (rejected | (1 << CENTER)) as jint;
        }
    };

    // The Java thread is done with this section: everything the bake needs is owned by now, so it
    // goes to the pool and the caller returns to Minecraft's chunk build. See [BakeTask] for what
    // crosses the thread boundary and what deliberately does not.
    match BakeTask::new(target, provider, jvm) {
        Some(task) => THREAD_POOL.spawn(move || task.run()),
        // The queue is full, so this bake is dropped on the floor - the same hole as above, from the
        // same cause: the section was applied here and nothing is going to bake it. The warning is
        // inside `reserve_bake_slot`; this is the bit that makes the JVM offer it again.
        None => return (rejected | (1 << CENTER)) as jint,
    }

    rejected as jint
}
/// The pool the section bakes run on.
///
/// Baking is CPU work over data the Java side has already handed over, and it used to run on
/// whichever chunk-build thread asked for it - a thread Minecraft wants back for the next section,
/// held while palettes were turned into vertices. Rayon's own default sizing is used (one thread per
/// core): these tasks are independent, they take only read locks, and the results funnel back
/// through the chunk update queue that was already the hand-off to the render thread.
static THREAD_POOL: Lazy<ThreadPool> = Lazy::new(|| {
    ThreadPoolBuilder::new()
        .thread_name(|index| format!("wgpu-mc bake {index}"))
        .build()
        .expect("wgpu-mc: could not start the section bake pool")
});

/// How many bakes may be waiting before further sections are dropped on the floor.
///
/// A section rebuild is offered every time Minecraft decides one is out of date, so a dropped offer
/// is not lost work - it comes back. What it buys is a bound on the memory waiting in this queue:
/// each queued bake owns 27 sections' worth of palettes, storages and light layers, which is a few
/// hundred kilobytes, and a player moving quickly can offer thousands of sections in a second.
const MAX_QUEUED_BAKES: usize = 256;

/// How many bakes are queued or running, against [MAX_QUEUED_BAKES].
static QUEUED_BAKES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Takes a slot in the bake queue, or answers false when it is full.
fn reserve_bake_slot() -> bool {
    let queued = QUEUED_BAKES.fetch_add(1, Ordering::Relaxed);

    if queued >= MAX_QUEUED_BAKES {
        QUEUED_BAKES.fetch_sub(1, Ordering::Relaxed);

        static WARNED: AtomicBool = AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::Relaxed) {
            log::warn!(
                "wgpu-mc: {MAX_QUEUED_BAKES} section bakes are already waiting; dropping this one, \
                 which Minecraft will offer again"
            );
        }

        return false;
    }

    true
}

/// Gives a slot back. Called however a bake ends, including when it never started.
fn release_bake_slot() {
    QUEUED_BAKES.fetch_sub(1, Ordering::Relaxed);
}

/// One section bake, on its way to the pool.
///
/// Everything it needs is owned: the JNI arrays it was built from are only valid on the thread that
/// received them, so the palettes, the storages and the two light layers are moved out before the
/// task is spawned. The `JNIEnv` is deliberately *not* carried across - a pool thread attaches
/// itself in [`BakeTask::run`], which is also where the callback into Java for biome tints gets a
/// usable environment from.
struct BakeTask {
    pos: IVec3,
    provider: CachedBlockstateProvider,
    jvm: JavaVM,
}

impl BakeTask {
    /// Queues a bake, or drops it if the queue is full. `None` when it was dropped.
    fn new(pos: IVec3, provider: CachedBlockstateProvider, jvm: JavaVM) -> Option<Self> {
        if !reserve_bake_slot() {
            return None;
        }

        Some(Self { pos, provider, jvm })
    }

    fn run(self) {
        // Counts down however this returns, including the early returns below.
        struct Queued;
        impl Drop for Queued {
            fn drop(&mut self) {
                release_bake_slot();
            }
        }
        let _queued = Queued;

        let Some(wm) = RENDERER.get() else {
            return;
        };

        // The bake asks Java for a biome tint per tinted face (`Wgpu.helperGetBlockColor`), so this
        // thread needs its own attachment to the JVM. A daemon attachment is the right one: it does
        // not hold the JVM open once the game is done, and it is what a worker pool thread wants.
        let env = match self.jvm.attach_current_thread_as_daemon() {
            Ok(env) => env,
            Err(error) => {
                log::warn!("wgpu-mc: could not attach a bake thread to the JVM: {error}");
                return;
            }
        };

        let wrapper = MinecraftBlockStateProviderWrapper {
            internal: self.provider,
            env: RefCell::new(env),
        };

        bake_section(self.pos, wm, &wrapper);
    }
}

#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn blocksCached(_env: JNIEnv, _class: JClass) -> jboolean {
    BLOCKS_CACHED.load(Ordering::Acquire) as jboolean
}

/// The sections the arena had no room for since the last call, as the keys the JVM knows them by.
///
/// The other half of the refusal counter the terrain line prints: the counter says *how much* of the
/// view was dropped, and this says *which* - because a section the JVM has been told was baked, and
/// which never reached the arena, is a hole nothing will fill on its own. Its rebuild has already
/// happened, and a rebuild carries only what changed; so the JVM takes these keys out of its
/// "what have I sent" table and the next rebuild of each section carries its blocks again.
///
/// Drains, so one call per client tick is the whole protocol: what is not handed over by then is
/// handed over by the next call. The keys are packed the way the JVM packs them - see
/// `section::section_key` - so nothing has to be unpacked on either side.
#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn refusedSections(env: JNIEnv, _class: JClass) -> jlongArray {
    let refused = RENDERER
        .get()
        .and_then(|wm| wm.scene())
        .map(|scene| scene.section_storage.write().refused())
        .unwrap_or_default();

    let keys: Vec<jlong> = refused.iter().map(|pos| section_key(*pos)).collect();

    let array = env.new_long_array(keys.len() as i32).unwrap();

    if !keys.is_empty() {
        env.set_long_array_region(&array, 0, &keys).unwrap();
    }

    array.into_raw()
}

/// Forgets every section of the world the bake was built against, and answers the new generation.
///
/// Called from `LevelRenderer#setLevel`, so it covers a dimension change, a new world and the return
/// to the title screen (`setLevel(null)`) alike. Three things go, and each is a way the old world
/// would otherwise come back:
///
///  - the cached 27-section neighbourhoods, which is what a bake reads its neighbours from;
///  - the bakes *already queued* for the arena, which are the old world's geometry on its way in;
///  - the arena itself, whose contents are the old world's meshes - and whose ranges go straight back
///    to the pool, the one free in the section storage that is not deferred: a level change arrives
///    between frames, and the new world needs every slot it has (see `SectionStorage::forget`).
///
/// What cannot be cancelled is a bake already running on the pool: it lands in the queue after this
/// returns and puts one section of the old world into the arena. That is a flash rather than a
/// permanent state, because the JVM forgets its own bookkeeping at the same moment - every section
/// the game rebuilds after the switch is sent again in full, and the new world's mesh for that
/// position replaces it.
///
/// The returned number is what the JVM stamps its payloads with; a record stamped with anything else
/// is refused rather than applied, which is how a chunk build that was already running is kept from
/// writing the old world's blocks under the new one's coordinates.
#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn clearSections(_env: JNIEnv, _class: JClass) -> jint {
    let generation = bump_section_generation();

    WORLD.write().clear();

    let mut queued = 0usize;
    let mut freed = 0usize;

    if let Some(wm) = RENDERER.get() {
        {
            let receiver = wm.chunk_update_queue.1.lock();
            while receiver.try_recv().is_ok() {
                queued += 1;
            }
        }

        if let Some(scene) = wm.scene() {
            let mut storage = scene.section_storage.write();
            freed = storage.len();
            storage.forget();
        }
    }

    log::info!(
        "wgpu-mc: the section bake was cleared for a level change: generation {generation}, {queued} \
         queued bake(s) dropped, {freed} section(s) released from the arena"
    );

    generation as jint
}

#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn registerBlock(mut env: JNIEnv, _class: JClass, name: JString) {
    let name: String = env.get_string(&name).unwrap().into();

    BLOCKS.lock().push(name);
}

/// The first half of a bake: forget what the last one was given, so it can be offered again.
///
/// Every bake rebuilds the whole block registry from the JVM's registrations, and the native side
/// *drops* those registrations at the end of each one - the states cross as JNI global references and
/// are released as soon as their keys are out. So a bake that is not the first needs this first, and
/// what it forgets depends on what changed:
///
///  - the block list and any state still queued, always: the JVM offers the whole registry again
///    (`BlockRegistryFeed.replay`) and a second copy of every block would bake every model twice;
///  - the diagnostics, always: they count what the *last* bake failed to read, and a count carried
///    into the next one describes neither;
///  - the **block atlas** - its packed rectangles, its animation table, the layer and rectangle tables
///    the game filled, and its image - only when `reload` says the resource *pack* changed. A reload
///    can change a sprite's size, so the rectangles packed from one pack are not the next pack's; a
///    setting that only changes how the models are baked (`animated_textures`) must leave the atlas
///    alone, because re-packing it would throw away the game's rectangles for the animated sprites and
///    nothing in this call would ask for them again.
///
/// Deliberately *not* cleared either way: the block manager's meshes and the face-flag masks, which are
/// still being drawn from while this runs on another thread. They are replaced wholesale by the bake
/// that follows - `bake_blocks` overwrites each block's meshes in place, and the state keys the JVM
/// holds stay valid because the blocks are registered back in the same order.
///
/// Called from the block cache thread. See `BlockCache.Bake` on the JVM side for the three callers.
#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn beginBlockBake(_env: JNIEnv, _class: JClass, reload: jboolean) {
    let Some(wm) = RENDERER.get() else {
        return;
    };

    BLOCKS.lock().clear();
    BLOCK_STATES.lock().clear();
    BLOCK_STATE_FACE_FLAGS.lock().clear();

    EMPTY_MESH_BLOCKS.lock().clear();
    UNMODELLED_BLOCKS.lock().clear();

    wgpu_mc::mc::block::MISSING_SPRITES.reset();
    wgpu_mc::mc::block::UNREADABLE_TEXTURES.reset();

    if reload == 0 {
        log::info!(
            "wgpu-mc: baking the block models again: the registry and the bake diagnostics are \
             cleared, the block atlas is kept"
        );

        return;
    }

    let atlases = wm.mc.texture_manager.atlases.read();

    if let Some(atlas) = atlases.get(BLOCK_ATLAS) {
        atlas.clear();
    }

    log::info!(
        "wgpu-mc: a resource reload has begun: the block atlas, the sprite tables and the bake \
         diagnostics are cleared, and the block registry is about to be offered again"
    );
}

#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn cacheBlockStates(mut env: JNIEnv, _class: JClass) {
    let wm = RENDERER.get().unwrap();

    // The sprites the game registered before this bake, handed over **before a single model is baked**.
    //
    // Where this sits is the whole of whether an animated texture animates. `bake_blocks` below bakes
    // every `variants` model, and 1074 of the game's 1170 blockstate files are `variants` - the campfire,
    // the magma block, the sea lantern and every other block whose sprite the game animates among them.
    // A face baked before this has run has no rectangle in the game's atlas to be sent to, so it is
    // baked against this side's copy of its sprite and stays on one frame for the whole session; a face
    // baked after it carries the flag and animates. This used to run inside the per-state loop further
    // down, which is *after* `bake_blocks`: the only blocks that animated were the 96 `multipart` ones -
    // fire among them, which is exactly how it behaved.
    //
    // See [`registerSprite`] and `Atlas::register_sprite` for the two things a registration carries, and
    // `Atlas::game_atlas_rect` for the decision it feeds.
    if SPRITE_REGISTRATIONS_PENDING.swap(false, std::sync::atomic::Ordering::Relaxed) {
        let atlases = wm.mc.texture_manager.atlases.read();

        match atlases.get(BLOCK_ATLAS) {
            Some(atlas) => {
                let registrations = std::mem::take(&mut *SPRITE_REGISTRATIONS.lock());

                for (name, rect, layer) in registrations {
                    atlas.register_sprite(
                        &ResourcePath::from(&name[..]),
                        rect,
                        registered_layer(layer),
                    );
                }
            }
            None => {
                // Nowhere to put them. Left in the queue for the bake that has an atlas rather than
                // dropped: the registry cannot be built without one either, so this is the same "no
                // atlas, nothing to do" the rest of this function already reports.
                SPRITE_REGISTRATIONS_PENDING.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    {
        let blocks = BLOCKS.lock();

        let blockstates = blocks
            .iter()
            .map(|identifier| {
                (
                    identifier.clone(),
                    ResourcePath::from(&identifier[..])
                        .prepend("blockstates/")
                        .append(".json"),
                )
            })
            .collect::<Vec<_>>();

        wm.mc.bake_blocks(
            wm,
            blockstates
                .iter()
                .map(|(string, resource)| (string, resource)),
        );
    }

    let mut states = BLOCK_STATES.lock();

    let mut block_manager = wm.mc.block_manager.write();

    // Nothing was baked, so there is nothing to map and `AIR` would not be in the registry either.
    // Leave `BLOCKS_CACHED` false: the section baker checks it, and a bake against an empty registry
    // would have to invent what "air" is.
    if block_manager.blocks.is_empty() {
        log::error!(
            "wgpu-mc: no block states were registered, so the native block registry is empty and the \
             terrain baker cannot run ({} state(s) were offered)",
            states.len()
        );
        return;
    }
    let mut mappings = Vec::new();

    let mut stdout = stdout().lock();

    // Every state whose block has no model is drawn as bedrock, which is what the game itself falls
    // back to. Bedrock missing too - a pack whose bedrock blockstate fails to bake, which
    // `bake_blocks` skips with a warning - leaves the first block that did bake standing in; the
    // registry is known to be non-empty here, because an empty one returned above.
    let fallback_id = block_manager
        .blocks
        .get_index_of("minecraft:bedrock")
        .unwrap_or(0);

    // How many states had to take the fallback, so the log says it once rather than once per state.
    let mut unmodelled = 0usize;

    states.iter().for_each(|registration| {
        let BlockStateRegistration {
            block_name,
            state_key,
            global_ref,
        } = registration;

        // A block whose blockstate file is missing or malformed is not in the registry at all,
        // and `get_full(..).unwrap()` here used to take the block cache thread down with it -
        // meaning nothing downstream, including the Rust terrain baker, ever saw a registry.
        let Some(id_key) = block_manager.blocks.get_index_of(block_name.as_str()) else {
            unmodelled += 1;
            note_unmodelled(block_name);
            mappings.push((
                BlockstateKey {
                    block: fallback_id as u16,
                    augment: 0,
                },
                global_ref,
            ));
            return;
        };

        let key_iter = if !state_key.is_empty() {
            state_key
                .split(',')
                .filter_map(|kv_pair| {
                    let mut split = kv_pair.split('=');
                    if kv_pair.is_empty() {
                        return None;
                    }

                    Some((
                        split.next().unwrap(),
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
        let atlases = wm.mc.texture_manager.atlases.write();
        let atlas = &atlases[BLOCK_ATLAS];

        let wm_block = &block_manager.blocks[id_key];
        let model = wm_block.get_model_by_key(
            key_iter
                .iter()
                .filter(|(a, _)| *a != "waterlogged")
                .map(|(a, b)| (*a, b)),
            &*wm.mc.resource_provider,
            atlas,
            0,
        );

        let key = match model {
            Some((mesh, augment)) => {
                // A mesh with no faces in it is a block that is drawn as nothing: the state has a
                // key, so it culls its neighbours and the block behind it loses the face between
                // them, and there is no other line anywhere that says so. See `MISSING_SPRITES`
                // for the other half of this, and `blockBakeDiagnostics` for where it is reported.
                if mesh.is_empty() {
                    note_empty_mesh(block_name);
                }

                BlockstateKey {
                    block: id_key as u16,
                    augment,
                }
            }
            None => {
                unmodelled += 1;

                // The half of "this state is drawn as bedrock" that this did not name: the block *is*
                // registered, and a mesh for the state could not be baked - every variant of a `variants`
                // file failed, or a `multipart`'s pieces did. The reason is a `warn!` on the console and
                // the *block* is what a person can search for. See `blockBakeDiagnostics`.
                note_unmodelled(block_name);

                BlockstateKey {
                    block: fallback_id as u16,
                    augment: 0,
                }
            }
        };

        mappings.push((key, global_ref));
    });

    if unmodelled != 0 {
        writeln!(
            &mut stdout,
            "wgpu-mc: {unmodelled} block state(s) have no model and are drawn as bedrock"
        )
        .unwrap();
    }

    drop(stdout);

    mappings.iter().for_each(|(blockstate_key, global_ref)| {
        env.call_static_method(
            "dev/birb/wgpu/render/Wgpu",
            "helperSetBlockStateIndex",
            "(Ljava/lang/Object;I)V",
            &[
                JValue::Object(global_ref.as_obj()),
                JValue::Int(blockstate_key.pack() as i32),
            ],
        )
        .unwrap();
    });

    // The block models are baked in two passes and this is the seam between them: `bake_blocks` baked
    // every `variants` model and uploaded the atlas, and asking for a mesh per state above is what
    // baked the `multipart` ones - each of which can name a sprite no earlier model named, allocated
    // into the atlas *image* at that moment. Without this upload those sprites are not on the GPU, and
    // the faces that sample them are discarded by the shader's alpha test: a block that is baked,
    // keyed, culls its neighbours, and is invisible. See `Atlas::upload_if_dirty`.
    {
        let atlases = wm.mc.texture_manager.atlases.read();

        if let Some(atlas) = atlases.get(BLOCK_ATLAS)
            && atlas.upload_if_dirty(wm)
        {
            writeln!(
                std::io::stdout().lock(),
                "wgpu-mc: the block atlas was uploaded again: the multipart models added sprites \
                     after the first upload"
            )
            .unwrap();
        }
    }

    // Every state's key has now been handed over - and that is also where the JVM reads what each
    // state says about the faces around it, because it is the first moment those shapes exist (see
    // `registerBlockStateFaceFlags`, and the crash that taught us).
    //
    // They arrive keyed by the key just handed out. A key can be worn by more than one state (a
    // property the blockstate file does not vary its model on, `waterlogged` for one), and the masks
    // are ANDed in that case: a bit survives only where every state wearing this key agrees, because
    // what it decides is whether a face is left out of the mesh - and a disagreement that way costs
    // an invisible face between two blocks rather than a hole in the world.
    let mut face_flags: HashMap<u32, FaceFlags> = HashMap::new();

    for (key, flags) in BLOCK_STATE_FACE_FLAGS.lock().drain(..) {
        face_flags
            .entry(key)
            .and_modify(|known: &mut FaceFlags| *known = known.and(flags))
            .or_insert(flags);
    }

    // How many keys came back with masks. A registry with none of them draws every face of every
    // block, which is more geometry than the game meshes and no hole in it - the failure a missing
    // mask would otherwise hide.
    writeln!(
        // Spelled out rather than through the `stdout` name, which the lock above still holds.
        std::io::stdout().lock(),
        "wgpu-mc: {} block state key(s) carry face flags (occlusion and self-hiding)",
        face_flags.len()
    )
    .unwrap();

    block_manager.face_flags = face_flags;

    // The blocks whose faces the baker counts by name. Resolved here because this is the one place the
    // registry and the watch list are both in hand, and stored as indices because that is all a baked
    // section carries. See `wgpu_mc::mc::chunk::WATCHED_BLOCKS`.
    block_manager.watched = wgpu_mc::mc::chunk::WATCHED_BLOCKS
        .iter()
        .enumerate()
        .filter_map(|(slot, (name, _))| {
            block_manager
                .blocks
                .get_index_of(*name)
                .map(|index| (index as u16, slot as u8))
        })
        .collect();

    writeln!(
        // Spelled out rather than through the `stdout` name: the lock above was dropped when the keys
        // were handed over, and this is the same shape the face-flag line below uses.
        std::io::stdout().lock(),
        "wgpu-mc: watching {} block(s) by name for the face counts ({})",
        block_manager.watched.len(),
        wgpu_mc::mc::chunk::WATCHED_BLOCKS
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
            .join(", ")
    )
    .unwrap();

    let instant = Instant::now();

    let state_count = states.len();

    states.clear();

    // Everything that bakes geometry reads the registry this just built, so it is only from here on
    // that a bake is allowed to run - see `BLOCKS_CACHED`.
    BLOCKS_CACHED.store(true, Ordering::Release);

    let debug_message = format!(
        "Released {} global refs to BlockState objects in {}ms",
        state_count,
        Instant::now().duration_since(instant).as_millis()
    );

    let debug_jstring = env.new_string(debug_message).unwrap();

    env.call_static_method(
        "dev/birb/wgpu/render/Wgpu",
        "rustDebug",
        "(Ljava/lang/String;)V",
        &[JValue::Object(&unsafe {
            JObject::from_raw(debug_jstring.into_raw())
        })],
    )
    .unwrap();
}

/// What the model baker could not draw, for the JVM's log - empty when there is nothing to say.
///
/// The two ways a block ends up invisible while looking perfectly registered, and neither of them
/// says anything on its own:
///
///  - **a face whose sprite the atlas does not have** is dropped, which leaves a hole in the block -
///    or the whole block gone, when that face was the only one its model had (see
///    [`wgpu_mc::mc::block::MISSING_SPRITES`]);
///  - **a state whose mesh has no faces at all**, which is the same thing one step further along: the
///    state has a key, it occludes its neighbours, and the block behind it loses the face between
///    them - so what the player sees is a hole in the world with no visible cause.
///
/// Both are reported here rather than in the Rust log because the Rust log does not reach the game's
/// log file, and a run that is being read afterwards is the run this has to explain. Called by the
/// JVM right after [`cacheBlockStates`], which is where both are decided.
#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn blockBakeDiagnostics(env: JNIEnv, _class: JClass) -> jstring {
    let mut report = String::new();

    let unreadable = wgpu_mc::mc::block::UNREADABLE_TEXTURES.faces();

    if unreadable != 0 {
        report.push_str(&format!(
            "{} texture(s) a model names could not be read (so their faces are untextured): {}",
            unreadable,
            wgpu_mc::mc::block::UNREADABLE_TEXTURES.names().join(", ")
        ));
    }

    let sprites = wgpu_mc::mc::block::MISSING_SPRITES.faces();

    if sprites != 0 {
        if !report.is_empty() {
            report.push_str("; ");
        }

        report.push_str(&format!(
            "{} face(s) were dropped for a sprite the atlas does not have: {}",
            sprites,
            wgpu_mc::mc::block::MISSING_SPRITES.names().join(", ")
        ));
    }

    let empty = EMPTY_MESH_BLOCKS.lock().clone();

    if !empty.is_empty() {
        if !report.is_empty() {
            report.push_str("; ");
        }

        let states: u64 = empty.iter().map(|(_, states)| states).sum();

        report.push_str(&format!(
            "{} block state(s) baked to a mesh with no faces (drawn as nothing, and still occluding \
             their neighbours): {}",
            states,
            empty
                .iter()
                .map(|(block, states)| format!("{block} x{states}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    let unmodelled = UNMODELLED_BLOCKS.lock().clone();

    if !unmodelled.is_empty() {
        if !report.is_empty() {
            report.push_str("; ");
        }

        let states: u64 = unmodelled.iter().map(|(_, states)| states).sum();

        report.push_str(&format!(
            "{} block state(s) have no model at all and are drawn as bedrock: {}",
            states,
            unmodelled
                .iter()
                .map(|(block, states)| format!("{block} x{states}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    env.new_string(report).unwrap().into_raw()
}

/// What the watched blocks have been seen, drawn and culled for - empty until one of them is baked.
///
/// The one line that says why a block is invisible: `seen 0` is a state that never reached a bake,
/// `drawn 0 culled N` is a model whose every face a neighbour test removed, and `drawn N` is faces that
/// are in the section - which puts the fault after the bake rather than in it. See
/// `wgpu_mc::mc::chunk::WATCHED_BLOCKS` for the list and for the four answers.
#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn watchedBlockFaces(env: JNIEnv, _class: JClass) -> jstring {
    env.new_string(wgpu_mc::mc::chunk::watched_faces())
        .unwrap()
        .into_raw()
}

#[allow(unused_must_use)]
#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn setPanicHook(env: JNIEnv, _class: JClass) {
    // `env_logger::init` alone drops everything below `error`, which hides the renderer's own
    // reporting (swapchain configuration, adapter choice, the frame dump). The default filter keeps
    // this crate and `wgpu-mc` at `info` while leaving `wgpu` and `naga` at `warn`, so what is
    // printed is the mod's own, and `RUST_LOG` still overrides it.
    env_logger::Builder::from_env(
        env_logger::Env::default()
            .default_filter_or("wgpu_mc_jni=info,wgpu_mc=info,wgpu=warn,naga=warn"),
    )
    .init();

    let jvm = env.get_java_vm().unwrap();
    let jvm_ptr = jvm.get_java_vm_pointer() as usize;

    std::panic::set_hook(Box::new(move |panic_info| {
        println!("{panic_info}");

        // A panic that unwinds through the C ABI ends the process, and the process ending takes
        // whatever stderr still had buffered with it - a crash report with no message in the log is
        // exactly what the last screenshot crash looked like. So it also goes to a file, which is
        // flushed as it is written.
        if let Some(run_directory) = RUN_DIRECTORY.get() {
            let path = run_directory.join("wgpu-panic.txt");
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                use std::io::Write;
                let _ = writeln!(file, "{panic_info}");
            }
        }

        // Nothing below may panic. A panic while a panic is being handled aborts the process -
        // "thread panicked while processing panic" - and that abort replaces the report this hook
        // exists to write, which is exactly what happened when a bake thread panicked with a Java
        // exception pending: every JNI call from the hook failed, and the first `unwrap` in it turned
        // a reported panic into a silent abort.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let Ok(jvm) = (unsafe { JavaVM::from_raw(jvm_ptr as _) }) else {
                return;
            };

            let Ok(mut env) = jvm.attach_current_thread_permanently() else {
                return;
            };

            // A pending exception would make the calls below fail one after another, and the one
            // that describes it is also the one that clears it. This is the last chance to say what
            // Java threw.
            describe_and_clear(&mut env, "the panic hook");

            let Ok(jstring) = env.new_string(format!(
                "wgpu-mc has panicked. Minecraft will now exit.\n{panic_info}"
            )) else {
                return;
            };

            //Does not return
            let _ = env.call_static_method(
                "dev/birb/wgpu/render/Wgpu",
                "rustPanic",
                "(Ljava/lang/String;)V",
                &[JValue::Object(&JObject::from(jstring))],
            );
        }));
    }))
}

#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn setWorldRenderState(_env: JNIEnv, _class: JClass, boolean: jboolean) {
    MC_STATE.store(Arc::new(MinecraftRenderState {
        _render_world: boolean != 0,
    }));
}

#[cfg(test)]
mod bake_queue_tests {
    use super::*;

    /// The cap is what keeps a fast-moving player from queueing a world's worth of sections, and it
    /// has to give the slots back - a leak here would stop baking for the rest of the run, quietly.
    #[test]
    fn the_bake_queue_refuses_past_its_cap_and_gives_the_slots_back() {
        // The counter is global, so this test owns it for its duration.
        while QUEUED_BAKES.load(Ordering::Relaxed) > 0 {
            release_bake_slot();
        }

        for taken in 0..MAX_QUEUED_BAKES {
            assert!(
                reserve_bake_slot(),
                "slot {taken} of {MAX_QUEUED_BAKES} was refused"
            );
        }

        assert!(!reserve_bake_slot(), "the queue took more than its cap");
        assert_eq!(QUEUED_BAKES.load(Ordering::Relaxed), MAX_QUEUED_BAKES);

        release_bake_slot();
        assert!(reserve_bake_slot(), "a released slot was not reusable");
        assert_eq!(QUEUED_BAKES.load(Ordering::Relaxed), MAX_QUEUED_BAKES);

        for _ in 0..MAX_QUEUED_BAKES {
            release_bake_slot();
        }
        assert_eq!(QUEUED_BAKES.load(Ordering::Relaxed), 0);
    }
}
