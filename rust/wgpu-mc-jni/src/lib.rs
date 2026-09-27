#![feature(debug_closure_helpers)]
#![feature(ptr_metadata)]
pub extern crate wgpu_mc;

use arc_swap::access::Access;
use arc_swap::{ArcSwap, ArcSwapAny};
use byteorder::{LittleEndian, ReadBytesExt};
use core::slice;
use crossbeam_channel::{Receiver, Sender, unbounded};
use glam::{IVec3, Mat4, ivec2, ivec3};
use jni::objects::{
    AutoElements, GlobalRef, JByteArray, JClass, JIntArray, JLongArray, JObject, JObjectArray,
    JPrimitiveArray, JString, JValue, JValueOwned, ReleaseMode, WeakRef,
};
use jni::sys::{JNI_FALSE, JNI_TRUE, jboolean, jbyte, jint, jlong, jsize, jstring};
use jni::{JNIEnv, JavaVM};
use jni_fn::jni_fn;
use once_cell::sync::{Lazy, OnceCell};
use palette::PALETTE_STORAGE;
use parking_lot::{Mutex, RwLock};
use pia::PIA_STORAGE;
use rayon::{ThreadPool, ThreadPoolBuilder};
use renderer::MATRICES;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Debug;
use std::io::{Cursor, Write, stdout};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use std::{mem, thread};
use wgpu::Extent3d;
use wgpu_mc::render::graph::{Geometry, RenderGraph, ResourceBacking};
use wgpu_mc::wgpu::util::DeviceExt;

use wgpu_mc::mc::block::{BlockstateKey, ChunkBlockState};
use wgpu_mc::mc::chunk::{BlockStateProvider, LightLevel, bake_section};
use wgpu_mc::mc::resource::{ResourcePath, ResourceProvider};
use wgpu_mc::mc::SkyState;
use wgpu_mc::minecraft_assets::schemas::blockstates::multipart::StateValue;
use wgpu_mc::render::pipeline::BLOCK_ATLAS;
use wgpu_mc::texture::{BindableTexture, TextureAndView};
use wgpu_mc::wgpu::{self, CurrentSurfaceTexture, TextureFormat};
use wgpu_mc::{Frustum, WmRenderer};

use crate::section::{
    CachedBlockstateProvider, Payload, SECTIONS, SectionBlocks, SectionLight, WORLD,
    neighbour_offset,
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
mod shader_cache;
mod settings;
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
static JVM: OnceCell<RwLock<JavaVM>> = OnceCell::new();
static YARN_CLASS_LOADER: OnceCell<GlobalRef> = OnceCell::new();

type Task = Box<dyn FnOnce() + Send + Sync>;

static TASK_CHANNELS: Lazy<(Sender<Task>, Receiver<Task>)> = Lazy::new(unbounded);
static MC_STATE: Lazy<ArcSwap<MinecraftRenderState>> = Lazy::new(|| {
    ArcSwap::new(Arc::new(MinecraftRenderState {
        _render_world: false,
    }))
});

static CLEAR_COLOR: Lazy<ArcSwap<[f32; 3]>> = Lazy::new(|| ArcSwap::new(Arc::new([0.0; 3])));

/// The block state a section's holes are, or `None` while the block registry is empty.
///
/// `None` is a real state of the world, not an error: this build has no block atlas yet, so
/// `bake_blocks` bakes nothing and `minecraft:air` is simply not in the registry. Baking without it
/// would have to guess what "air" is, and guessing wrong is geometry built out of nothing, so the
/// bake refuses instead - see `bakeSection`.
static AIR: Lazy<Option<BlockstateKey>> = Lazy::new(|| {
    RENDERER
        .get()
        .and_then(|renderer| {
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
static BLOCK_STATES: Mutex<Vec<(String, String, GlobalRef)>> = Mutex::new(Vec::new());

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
pub fn setClassLoader(mut env: JNIEnv, _class: JClass, class_loader: JObject) {
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
                log::error!("wgpu-mc: could not attach to the JVM to read {}: {err}", id.0);
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
    crate::debug::apply(&settings);

    *SETTINGS.write() = Some(settings);

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

    BLOCK_STATES
        .lock()
        .push((block_name, state_key, global_ref));
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
/// Returns whether the caller has to send the whole neighbourhood again: this side keeps the light of
/// each section and drops the ones far from the player, so a section the JVM counts as already sent
/// can be gone here. `true` means that happened, no bake was queued, and the caller should clear its
/// own bookkeeping and call once more with everything it has.
#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn bakeSections(
    _env: JNIEnv,
    _class: JClass,
    x: jint,
    y: jint,
    z: jint,
    address: jlong,
    length: jint,
) -> jboolean {
    // A rebuild can arrive before the client has cached block states, and the cache itself cannot
    // build anything while no block atlas is registered. `AIR` is the registry's, so without it
    // there is no way to tell a hole from a block: say so once and let the caller try again later.
    let Some(air) = *AIR else {
        static WARNED: AtomicBool = AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::Relaxed) {
            log::warn!(
                "wgpu-mc: a section was offered for baking while the block registry was empty; \
                 skipping until it is built"
            );
        }
        return false as jboolean;
    };

    if length <= 0 || address == 0 {
        log::error!("wgpu-mc: a section bake arrived with no payload; dropping it");
        return false as jboolean;
    }

    // The caller's buffer, read and left alone: everything kept is copied out below, before this
    // returns and the JVM writes the next payload over it.
    let bytes = unsafe { slice::from_raw_parts(address as *const u8, length as usize) };

    // A payload this side cannot read is one it did not receive. Saying so - rather than answering
    // "all good" - is what makes the JVM forget what it thought had been sent and hand the whole
    // neighbourhood over again, which is the only way back to a cache the two sides agree on.
    let Some(mut payload) = Payload::parse(bytes) else {
        return true as jboolean;
    };

    let target = ivec3(x, y, z);

    let mut world = WORLD.write();

    // Apply the payload first, so a bake queued below never sees a half-applied neighbourhood: the
    // sections the caller sent replace what was held for them, and the ones it says to forget are
    // dropped - those are the sections that became air or were unloaded.
    for (index, slot) in payload.blocks.iter_mut().enumerate() {
        let pos = target + neighbour_offset(index);

        if payload.absent & (1 << index) != 0 {
            world.remove_blocks(pos);
        } else if let Some(blocks) = slot.take() {
            world.set_blocks(pos, Arc::new(blocks));
        }
    }

    for (index, light) in &payload.light {
        world.set_light(target + neighbour_offset(*index), light.clone());
    }

    for index in 0..SECTIONS {
        if payload.light_absent & (1 << index) != 0 {
            world.remove_light(target + neighbour_offset(index));
        }
    }

    // A section the JVM marked as "already yours" but that this side does not have - a cache that was
    // trimmed, a new world, a section that became empty under us - means a bake against holes. The
    // caller sends everything again instead, and this call queues nothing.
    let known_blocks = payload.known_blocks & !payload.present;
    let known_light = payload.known_light & !payload.light.iter().fold(0u32, |mask, (index, _)| {
        mask | (1 << index)
    });

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
        return true as jboolean;
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

    let provider = CachedBlockstateProvider {
        blocks,
        light,
        air,
    };

    world.trim(target);
    drop(world);

    let jvm = match _env.get_java_vm() {
        Ok(jvm) => jvm,
        Err(err) => {
            log::error!("wgpu-mc: could not get the JVM handle for a bake: {err}");
            return false as jboolean;
        }
    };

    // The Java thread is done with this section: everything the bake needs is owned by now, so it
    // goes to the pool and the caller returns to Minecraft's chunk build. See [BakeTask] for what
    // crosses the thread boundary and what deliberately does not.
    if let Some(task) = BakeTask::new(target, provider, jvm) {
        THREAD_POOL.spawn(move || task.run());
    }

    false as jboolean
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

#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn registerBlock(mut env: JNIEnv, _class: JClass, name: JString) {
    let name: String = env.get_string(&name).unwrap().into();

    BLOCKS.lock().push(name);
}

#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn cacheBlockStates(mut env: JNIEnv, _class: JClass) {
    let wm = RENDERER.get().unwrap();
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

    let block_manager = wm.mc.block_manager.write();

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

    states
        .iter()
        .for_each(|(block_name, state_key, global_ref)| {
            // A block whose blockstate file is missing or malformed is not in the registry at all,
            // and `get_full(..).unwrap()` here used to take the block cache thread down with it -
            // meaning nothing downstream, including the Rust terrain baker, ever saw a registry.
            let Some(id_key) = block_manager.blocks.get_index_of(block_name.as_str()) else {
                unmodelled += 1;
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
                Some((_, augment)) => BlockstateKey {
                    block: id_key as u16,
                    augment,
                },
                None => {
                    unmodelled += 1;
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
            if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
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

            let Ok(jstring) =
                env.new_string(format!("wgpu-mc has panicked. Minecraft will now exit.\n{panic_info}"))
            else {
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
            assert!(reserve_bake_slot(), "slot {taken} of {MAX_QUEUED_BAKES} was refused");
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