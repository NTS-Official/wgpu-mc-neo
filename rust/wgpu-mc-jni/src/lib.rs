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
    AutoElements, GlobalRef, JByteArray, JClass, JLongArray, JObject, JStaticMethodID, JString,
    JValue, JValueOwned, ReleaseMode, WeakRef,
};
use jni::signature::{Primitive, ReturnType};
use jni::sys::{jboolean, jbyte, jfloat, jint, jlong, jlongArray, jstring, jvalue};
use jni::{JNIEnv, JavaVM};
use jni_fn::jni_fn;
use once_cell::sync::{Lazy, OnceCell};
use parking_lot::{Mutex, RwLock};
use rayon::{ThreadPool, ThreadPoolBuilder};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
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

/// Checks the mod's own metadata: the version NeoForge is told in `neoforge/updates.json` against
/// the version this tree builds. See the module for why nothing else can notice that they disagree.
#[cfg(test)]
mod packaging;

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

    // **And the tint helpers, resolved here rather than per call.** This is the one place that has both the
    // loader and a `JNIEnv` *before* a bake can ask for a tint - the library is loaded before any section
    // is - and `loadClass` does not initialise the class, so resolving it here runs none of the game's own
    // code. See [`TintHelpers`] for what a per-call lookup of the same two ids cost.
    //
    // A failure is not fatal and is retried never: the tint path keeps the slow lookup it had before, which
    // is also the path that can still say *why* a tint could not be had. What it must not do is leave the
    // exception pending - `loadClass` throwing is the ordinary way this fails, and a pending exception on
    // the thread that loads the renderer fails every later JNI call on it.
    match resolve_tint_helpers(&mut env) {
        Ok(helpers) => {
            if TINT_HELPERS.set(helpers).is_err() {
                log::warn!("wgpu-mc: the class loader was registered more than once");
            }
        }
        Err(err) => {
            // The reason is kept for the first tint to report - see [`TINT_HELPERS_FAILED`] - and the
            // exception is cleared here, because `loadClass` throwing is the ordinary way this fails and a
            // pending exception on the thread that loads the renderer fails every later call on it.
            describe_and_clear(&mut env, "the tint helpers");
            let _ = TINT_HELPERS_FAILED.set(err.to_string());
        }
    }
}

/// The class the two tint helpers live on, named once.
const TINT_HELPER_CLASS: &str = "dev.birb.wgpu.render.Wgpu";

/// `Wgpu`'s two tint helpers, resolved once: the class, and one method id per helper.
///
/// **Why they are cached.** Every tinted face - every grass side, every leaf, every water surface - used to
/// do the whole lookup: `ClassLoader.loadClass` as a *method call* (a `String` allocation, a local
/// reference, a virtual dispatch into the JVM) and then a `GetStaticMethodID`, which walks the class's
/// method table with a signature string. A `jmethodID` is valid for as long as the class that declares it
/// is loaded and is not moved by a class-loader change, and this class is owned by the game's loader for
/// the life of the process - so all of that was a fixed cost paid per *face* for an answer that cannot
/// change.
///
/// The signature is checked once, here, by `get_static_method_id`: a wrong one is a `MethodNotFound` at
/// this resolution rather than at a call, which matters because the unchecked call at the other end cannot
/// report anything about the method it was handed.
struct TintHelpers {
    /// Global, because a local reference lives only to the end of the JNI frame that made it - and the
    /// frame that resolved this is the library load, not the bake that asks for a tint.
    class: GlobalRef,
    block_color: JStaticMethodID,
    fluid_color: JStaticMethodID,
    /// The bulk call: every colour a whole section needs, in one array. See [`Self::section_tints`].
    section_tints: JStaticMethodID,
    /// The same, for the *fluid* colours of a section, which are a different question with a different
    /// answer (`FluidTintSources.water()` rather than the block's own source). See
    /// [`Self::section_fluid_tints`].
    section_fluid_tints: JStaticMethodID,
}

impl TintHelpers {
    /// The class as a `JClass`, for the unchecked calls below.
    ///
    /// **Safety**: the [`GlobalRef`] this struct holds keeps the class alive for as long as this value does,
    /// and a `JClass` is a `JObject` that happens to name a class - so naming the global reference's object
    /// as one is valid for the call it is passed to. Nothing is released by dropping it: a `JClass` built
    /// here owns no reference of its own, which is why this is a method rather than a field.
    fn class(&self) -> JClass<'static> {
        // Safety: see above - the pointer came from the JVM and the global reference is what keeps it
        // valid for the life of the process.
        unsafe { JClass::from_raw(self.class.as_obj().as_raw()) }
    }

    /// `Wgpu.helperGetBlockColor(x, y, z, tintIndex)`, through the cached id.
    fn block_color(
        &self,
        env: &mut JNIEnv,
        pos: IVec3,
        tint_index: i32,
    ) -> jni::errors::Result<i32> {
        let args = [
            jvalue { i: pos.x },
            jvalue { i: pos.y },
            jvalue { i: pos.z },
            jvalue { i: tint_index },
        ];

        self.call_int(env, self.block_color, &args)
    }

    /// `Wgpu.helperGetFluidColor(x, y, z)`, through the cached id. See [`Self::block_color`].
    fn fluid_color(&self, env: &mut JNIEnv, pos: IVec3) -> jni::errors::Result<i32> {
        let args = [
            jvalue { i: pos.x },
            jvalue { i: pos.y },
            jvalue { i: pos.z },
        ];

        self.call_int(env, self.fluid_color, &args)
    }

    /// Every colour one section needs, in **one call**, unpacked from the layout `Wgpu`'s
    /// `helperGetSectionTints` documents and packs:
    ///
    /// ```text
    /// bits 63..36  the position in the section, in Minecraft's own storage order
    /// bits 35..32  the model's tint index
    /// bits 31..0   the colour, in the same packing `helperGetBlockColor` returns
    /// ```
    ///
    /// **Why a table rather than a call per face.** The single-call path is one JNI call, one `BlockPos` and
    /// one biome lookup *per tinted face* - five for a grass block, six for a leaf block - and a bake knows
    /// every one of them before it draws anything, because it walks the section's blocks in order. This asks
    /// for all of them at once, which is the difference between a callback per face and a callback per
    /// section.
    ///
    /// A local frame, because this is the only call on this path that returns an **object**: the array is a
    /// local reference, and a bake thread's references are not popped per call - the thread stays attached for
    /// the life of the process, so one reference per section would be one reference per section for the whole
    /// run, until the local reference table overflows. `with_local_frame` pops it either way.
    fn section_tints(&self, env: &mut JNIEnv, section: IVec3) -> jni::errors::Result<Vec<u64>> {
        self.fetch_section(env, self.section_tints, section)
    }

    /// Every fluid colour one section needs, in one call, from the other bulk method.
    ///
    /// The same shape as [`Self::section_tints`] with the tint index left out of the packing - a fluid has
    /// one colour per block rather than one per face - and a second call rather than more entries in the
    /// first, because the JVM side walks for a different reason: the fluid walk is only ever asked for a
    /// section the baker already knows holds water. See `Wgpu.helperGetSectionFluidTints`.
    fn section_fluid_tints(
        &self,
        env: &mut JNIEnv,
        section: IVec3,
    ) -> jni::errors::Result<Vec<u64>> {
        self.fetch_section(env, self.section_fluid_tints, section)
    }

    /// One bulk call, whatever it is: three ints in, a `long[]` out.
    fn fetch_section(
        &self,
        env: &mut JNIEnv,
        method: JStaticMethodID,
        section: IVec3,
    ) -> jni::errors::Result<Vec<u64>> {
        env.with_local_frame(8, |env| {
            let args = [
                jvalue { i: section.x },
                jvalue { i: section.y },
                jvalue { i: section.z },
            ];

            // Safety: the id was resolved from this class with `(III)[J` in `resolve_tint_helpers`, which is
            // the signature these three arguments are built for.
            let value = unsafe {
                env.call_static_method_unchecked(self.class(), method, ReturnType::Object, &args)
            }?;

            // The unchecked call does not look for a pending exception, and this path can throw where the
            // primitive ones rarely do - a section that is not there, a level that is not there. See
            // [`Self::call_int`].
            if env.exception_check()? {
                return Err(jni::errors::Error::JavaException);
            }

            let array = JLongArray::from(value.l()?);
            let length = env.get_array_length(&array)? as usize;
            let mut out = vec![0i64; length];

            if length > 0 {
                env.get_long_array_region(&array, 0, &mut out)?;
            }

            Ok(out.into_iter().map(|entry| entry as u64).collect())
        })
    }

    /// One `int`-returning static call through a cached id.
    ///
    /// **The exception is checked here because the unchecked call does not check it.** The checked call
    /// this replaces reported a Java exception as `Error::JavaException`, and the tint path's failure arm
    /// describes and clears it - a pending exception left on a bake thread fails every later call on that
    /// thread. `call_static_method_unchecked` returns the (meaningless) value instead, so the check has to
    /// be made by hand rather than inherited.
    fn call_int(
        &self,
        env: &mut JNIEnv,
        method: JStaticMethodID,
        args: &[jvalue],
    ) -> jni::errors::Result<i32> {
        // Safety: `method` was resolved from this class in `resolve_tint_helpers`, with the signature the
        // matching `args` are built for - `(IIII)I` for the block tint and `(III)I` for the fluid one - and
        // both are held together in this struct, so the id cannot outlive the class that declares it.
        let value = unsafe {
            env.call_static_method_unchecked(
                self.class(),
                method,
                ReturnType::Primitive(Primitive::Int),
                args,
            )
        }?;

        if env.exception_check()? {
            return Err(jni::errors::Error::JavaException);
        }

        value.i()
    }
}

/// Both helper ids and the class they live on, or unset while they could not be resolved.
///
/// A `OnceCell` rather than a lock on the tint path: `get` is one atomic load, and the value never changes,
/// because there is one game, one class loader and one of each helper. It is filled by [`setClassLoader`],
/// which runs as the library loads - before any bake - and a run where that resolution failed keeps the
/// slow path, which is exactly what every tint did before this existed.
static TINT_HELPERS: OnceCell<TintHelpers> = OnceCell::new();

/// Why [`TINT_HELPERS`] could not be resolved, if they could not be.
///
/// Kept rather than only logged, because the resolution happens as the native library loads - which is
/// *before* the JVM side has a logger for these lines, so a line written there is a line nobody sees. The
/// first tint is minutes later and certainly after it is, and that is where the reason is said.
static TINT_HELPERS_FAILED: OnceCell<String> = OnceCell::new();

/// Counts a tint that had to look its helper up, and says why the first one happened.
///
/// The counter is the diagnostic that survives a run: [`TINT_HELPERS`] being empty is not visible anywhere
/// else, and the failure it would be - every tinted face paying for a `loadClass` and a method lookup - is a
/// cost rather than a wrong answer, so nothing would look broken.
fn note_the_slow_tint_path() {
    static REPORTED: AtomicBool = AtomicBool::new(false);

    TINT_SLOW_LOOKUPS.fetch_add(1, Ordering::Relaxed);

    if REPORTED.swap(true, Ordering::Relaxed) {
        return;
    }

    let why = match TINT_HELPERS_FAILED.get() {
        Some(reason) => format!(" ({reason})"),
        None => String::new(),
    };

    log::warn!(
        "wgpu-mc: the game's tint helpers are not cached{why}, so every tinted face looks the class and \
         its method up again; see `TintHelpers`"
    );
}

/// Resolves [`TINT_HELPERS`] through the game's own class loader.
///
/// Split out of [`setClassLoader`] so that the two ways it can fail - no loader registered, and the class
/// not visible through it - are one `Err` in one place rather than a branch inside the loader's own
/// registration.
fn resolve_tint_helpers(env: &mut JNIEnv) -> jni::errors::Result<TintHelpers> {
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

    let arg = env.new_string(TINT_HELPER_CLASS)?;
    // `loadClass`, for the reason [`call_static_from_class_loader`] spells out: `findClass` is the loader's
    // *define* hook and ends in "attempted duplicate class definition" for a class that is already loaded.
    let class: JClass = env
        .call_method(
            class_loader,
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[JValue::Object(&arg)],
        )?
        .l()?
        .into();

    let block_color = env.get_static_method_id(&class, "helperGetBlockColor", "(IIII)I")?;
    let fluid_color = env.get_static_method_id(&class, "helperGetFluidColor", "(III)I")?;
    // The bulk one. Resolved here with the rest, so that a wrong signature is a `MethodNotFound` at load
    // rather than a white world: the unchecked call that uses it can report nothing about the method.
    let section_tints = env.get_static_method_id(&class, "helperGetSectionTints", "(III)[J")?;
    let section_fluid_tints =
        env.get_static_method_id(&class, "helperGetSectionFluidTints", "(III)[J")?;

    Ok(TintHelpers {
        class: env.new_global_ref(&class)?,
        block_color,
        fluid_color,
        section_tints,
        section_fluid_tints,
    })
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
///
/// Two callers, and the sentence is deliberately about neither of them: the resource path, where a
/// resource could not be read, and the class loader's own resolution of
/// [`TintHelpers`], where a class could not be loaded.
fn describe_and_clear(env: &mut JNIEnv, what: &str) {
    if !env.exception_check().unwrap_or(false) {
        return;
    }

    log::error!("wgpu-mc: {what}: the JVM threw");

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

    // **And the window mode, which is the same shape of thing with a different owner.** This side holds
    // the setting; the window belongs to the JVM - GLFW is reached from there, `Window` is there, and the
    // three modes are three GLFW calls. So what is applied here is the *change*, and the JVM is asked to
    // re-read the setting and put the window in it. See `debug::reapply_window_mode`, which is where the
    // comparison lives, and `DisplayMode` on the JVM side, which is what does it.
    //
    // Mirroring the answer rather than passing it: the callback is only a notification, and the JVM reads
    // the same setting through `RendererSettings`, so there is one source of truth for which mode it is
    // and this is only the moment to look.
    crate::debug::reapply_window_mode(&mut env);

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

    // **And the window mode, here rather than on the first frame**, which is where it was and why it was
    // wrong: on the first frame the settings had not been read yet, so `windowMode` answered the static's
    // initial value - `Exclusive` - and the client took the display over before switching to the mode the
    // config actually named. Measured: `the window is in EXCLUSIVE mode`, then eleven seconds later
    // `the window mode is now 2`.
    //
    // The JVM is asked only if there is a window to put into it: this runs from the mod constructor too,
    // where there is not, and `DisplayMode.reapply` returns quietly in that case - so the call is safe
    // either way and the ordering is what matters.
    crate::debug::reapply_window_mode(&mut env);

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
/// One queued sprite: its name, where it sits in the game's atlas, its layer, and how many levels deep its
/// own detail goes. Named rather than a bare tuple because four fields of four different kinds is where a
/// swap stops being visible at the call site.
type SpriteRegistration = (String, [f32; 4], u8, u32);

/// A queue rather than a direct write, because of when the call arrives: the game registers its sprites
/// before [`cacheBlockStates`], and the atlas this side packs does not exist until that bake runs. See
/// [`wgpu_mc::render::atlas::Atlas::register_sprite`].
static SPRITE_REGISTRATIONS: Mutex<Vec<SpriteRegistration>> = Mutex::new(Vec::new());

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
    level_cap: jint,
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
        // The coarsest level this sprite has any detail at, from the JVM: `log2` of its frame in texels. A
        // negative answer is refused rather than wrapped, because "no floor" is the state this renderer was
        // in before any of it and the direction a nonsense number should fail in.
        level_cap.max(0) as u32,
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
    offset_max_y: jfloat,
    offset_xz: jint,
    leaves: jint,
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
            // How far up the block's own random placement may move it, or zero for the common case of
            // a block that stands where it was put. See `FaceFlags::block_offset`.
            offset_max_y,
            // And whether it is offset at all, which is *not* the same question: a flower's offset is
            // horizontal only, so its vertical limit is exactly zero.
            offset_xz: offset_xz != 0,
            // And whether it is a `LeavesBlock`, which is the half of `forceOpaque` the registry can
            // answer. See `FaceFlags::leaves`.
            leaves: leaves != 0,
        },
    ));
}

struct MinecraftBlockStateProviderWrapper<'a> {
    internal: CachedBlockstateProvider,
    env: RefCell<JNIEnv<'a>>,
    /// The section this bake is for, which is what the bulk tint call is asked about.
    section: IVec3,
    /// The section's colours: `(position index << 4 | tint index) -> colour`.
    ///
    /// Fetched on the first tint the bake asks for and kept for the whole section - which is the point of
    /// asking in bulk. A fetch that failed is [`SectionTints::Unavailable`] and stays that way, so a failure
    /// costs one warning rather than a lookup per face.
    tints: RefCell<SectionTints>,
    /// The section's **fluid** colours, keyed by position index alone, fetched the same way and only ever
    /// asked for by a block the baker knows holds water.
    fluid_tints: RefCell<SectionTints>,
}

/// What a bake knows about its section's colours. See [`MinecraftBlockStateProviderWrapper::tints`].
enum SectionTints {
    /// Nobody has asked yet.
    Unasked,
    /// The table, as the bulk call returned it.
    Ready(HashMap<u32, u32>),
    /// The bulk call failed, or the helper ids were never resolved: every tint falls back to the
    /// single-call path, which is what a bake did before the table existed.
    Unavailable,
}

/// The key the section's colours are indexed by: the position in the section, then the tint index.
///
/// Twelve bits of position and four of tint index, which is the layout `Wgpu.helperGetSectionTints` packs
/// into the high half of each `long`. The position is Minecraft's own storage order - `x | z << 4 | y << 8` -
/// which is the order the baker walks blocks in, so a face's position is its own index with the tint along
/// side it rather than a second lookup.
fn tint_key(pos: IVec3, tint_index: i32) -> u32 {
    let index = (pos.x & 15) | ((pos.z & 15) << 4) | ((pos.y & 15) << 8);

    ((index as u32) << 4) | (tint_index as u32 & 15)
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

    /// The variant the game picked for a position, forwarded for **exactly** the reason [`Self::get_fluid`]
    /// is: this wrapper is the provider the bake actually runs with, and the trait's own default answers
    /// `0` - the first variant - for every block.
    ///
    /// Written down because this was got wrong in precisely that way: the blob arrived, parsed, was
    /// stored on the target section, was read back correctly by the inner provider's own test - and the
    /// bake never saw it, because the method that was implemented was the one on `CachedBlockstateProvider`
    /// and the one being *called* was the default here. Every lily pad pointed the same way and no
    /// counter said anything, which is the same shape as the fluid bug the comment above describes.
    fn get_model_variant(&self, pos: IVec3) -> u8 {
        self.internal.get_model_variant(pos)
    }

    fn is_section_empty(&self, rel_pos: IVec3) -> bool {
        self.internal.is_section_empty(rel_pos)
    }

    /// The biome tint for one tinted face, asked of the game itself: the tint is a property of the
    /// world at that position, and this side only has the section data.
    ///
    /// **Answered from the section's table, which is fetched once.** The bake asks for a colour per tinted
    /// face - five for a grass block, six for a leaf block - and this side asks Java for all of a section's
    /// colours in one call the first time one is wanted, then answers from that table. See
    /// [`TintHelpers::section_tints`] for what that saves and [`SectionTints`] for what happens when it
    /// cannot be had.
    ///
    /// This runs on a bake thread, which is a plain native thread attached to the JVM, so the class
    /// has to be looked up through the game's own loader - `FindClass` on such a thread resolves
    /// against the system loader, which cannot see NeoForge's transformed classes, and the
    /// `.unwrap()` that used to be here took the pool thread (and, through the second panic, the
    /// process) down the moment a tinted face was baked. White is what a face with no tint gets, so
    /// that is the answer when the call cannot be made, once per failure mode with a log line.
    fn get_block_color(&self, pos: IVec3, tint_index: i32) -> u32 {
        let mut tints = self.tints.borrow_mut();

        if matches!(*tints, SectionTints::Unasked) {
            let mut env = self.env.borrow_mut();

            // **The switch the bulk table can be taken out with**, for the one comparison no counter can
            // make: a frame baked with the table against a frame baked without it, both against the same
            // vanilla frame - see the dump recipe in the README. It has to be a *frame* comparison because
            // the two paths differ in cost, and a slower bake changes what is loaded by any given frame
            // number, which is what made the first attempt at it read as a camera difference.
            const USE_BULK_TINTS: bool = true;

            *tints = match TINT_HELPERS.get().filter(|_| USE_BULK_TINTS) {
                Some(helpers) => match helpers.section_tints(&mut env, self.section) {
                    Ok(entries) => {
                        TINT_TABLE_FETCHES.fetch_add(1, Ordering::Relaxed);
                        TINT_TABLE_ENTRIES.fetch_add(entries.len() as u64, Ordering::Relaxed);

                        // The entry layout is `Wgpu.helperGetSectionTints`'s: twelve bits of position,
                        // four of tint index, then the colour.
                        SectionTints::Ready(
                            entries
                                .into_iter()
                                .map(|entry| {
                                    let index = ((entry >> 36) & 0xfff) as u32;
                                    let tint = ((entry >> 32) & 0xf) as u32;

                                    ((index << 4) | tint, entry as u32)
                                })
                                .collect(),
                        )
                    }
                    Err(err) => {
                        describe_and_clear(&mut env, "helperGetSectionTints");
                        log::warn!(
                            "wgpu-mc: the biome tints of the section at {} could not be fetched in bulk \
                             ({err}); each tinted face will ask for its own",
                            self.section
                        );

                        SectionTints::Unavailable
                    }
                },
                None => SectionTints::Unavailable,
            };
        }

        if let SectionTints::Ready(table) = &*tints {
            TINT_TABLE_HITS.fetch_add(1, Ordering::Relaxed);

            return match table.get(&tint_key(pos, tint_index)) {
                Some(color) => {
                    TINT_TABLE_FOUND.fetch_add(1, Ordering::Relaxed);

                    // **The bulk table against the per-face path, for the *same* position, while the
                    // diagnostics are on.** They are different code and, if the table's position packing
                    // were wrong, they would be different colours - which is the one thing neither the key
                    // counters nor a `TintProfile` row can see, because both of those read the game's own
                    // functions and not what this side did with the answer. Measured: **zero mismatches in
                    // 20,000 positions**, which is what closed the last way a *block* tint could have been
                    // sampled at a mirrored position. The check costs one JNI call per tinted face *only*
                    // when the diagnostics are on.
                    if wgpu_mc::mc::chunk::DIAGNOSTIC_LOGGING.load(Ordering::Relaxed) {
                        use std::sync::atomic::AtomicU64;

                        static CHECKED: AtomicU64 = AtomicU64::new(0);
                        static MISMATCHES: AtomicU64 = AtomicU64::new(0);

                        if CHECKED.load(Ordering::Relaxed) < 20_000 {
                            CHECKED.fetch_add(1, Ordering::Relaxed);

                            if let Some(helpers) = TINT_HELPERS.get() {
                                let mut env = self.env.borrow_mut();

                                if let Ok(single) = helpers.block_color(&mut env, pos, tint_index)
                                    && single as u32 != *color
                                {
                                    let seen = MISMATCHES.fetch_add(1, Ordering::Relaxed);

                                    if seen < 24 {
                                        log::warn!(
                                            "wgpu-mc: tint mismatch at {pos} index {tint_index}: bulk \
                                             {color:#010x}, per-face {single:#010x}"
                                        );
                                    }
                                }
                            }
                        }
                    }

                    *color
                }
                // A key the table does not hold: a position the JVM's walk did not report, or - the case
                // this counter exists for - a packing the two sides disagree about. White is the game's own
                // answer for "no tint source", so it is the safe answer as well as a silent one.
                None => 0xffff_ffff,
            };
        }

        drop(tints);

        let mut env = self.env.borrow_mut();

        let result = match TINT_HELPERS.get() {
            // **The resolved ids: one `CallStaticIntMethodA` per tinted face and nothing else.** See
            // [`TintHelpers`] for what the class and method lookup that used to be here cost, per face.
            Some(helpers) => helpers.block_color(&mut env, pos, tint_index),
            // Not resolved - the class was not visible through the loader when the library loaded, or the
            // loader was never registered. This is what every tint did before, and it is the path that can
            // still report *why* the call could not be made.
            None => {
                note_the_slow_tint_path();

                call_static_from_class_loader(
                    &mut env,
                    TINT_HELPER_CLASS,
                    "helperGetBlockColor",
                    "(IIII)I",
                    &[
                        JValue::Int(pos.x),
                        JValue::Int(pos.y),
                        JValue::Int(pos.z),
                        JValue::Int(tint_index),
                    ],
                )
                .and_then(|value| value.i())
            }
        };

        match result {
            Ok(color) => {
                // Counted so the answer is *readable* rather than merely error-free: the failure this
                // whole change exists to fix looked exactly like success, because a constant is a valid
                // colour. A total of zero here means the game answered "no tint" for every fluid, which
                // is a different bug from the one this closed and would be invisible without a count.
                if color as u32 != 0xffff_ffff {
                    FLUID_TINTS.fetch_add(1, Ordering::Relaxed);
                }

                color as u32
            }
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

    /// **The colour water is tinted by, asked of the game rather than held as a constant.**
    ///
    /// The same shape as [`Self::get_block_color`] and for the same reasons - one warning per failure mode,
    /// white when the call cannot be made - with two differences that matter. It is a *different* question:
    /// a fluid is tinted by its **fluid** model, not its block model (`FluidRenderer#tesselate` asks
    /// `model.fluidTintSource().colorInWorld(..)`, and the model comes from
    /// `FluidStateModelSet#get(fluidState)`), so the block tint path cannot answer it even in principle - a
    /// water block's block model is not what colours water. And it is asked **per block rather than per
    /// face**, because `bake_fluid_faces_with` asks once and gives the colour to every face of that block.
    ///
    /// **Answered from the section's fluid table**, fetched in one call the first time a fluid block asks -
    /// which is the whole of what the fluid path used to spend: one JNI call, one `BlockPos`, one
    /// `getFluidState` and one biome lookup *per fluid block*, in an ocean. See
    /// [`TintHelpers::section_fluid_tints`].
    ///
    /// The default is `WATER_TINT` rather than white, through the trait's own default: this is only a
    /// failure path, and the colour the game uses where no biome says otherwise is a better guess than
    /// no tint at all. It is also what every test provider answers, so a test's water looks exactly as
    /// it did before this existed.
    fn get_fluid_color(&self, pos: IVec3) -> u32 {
        let mut tints = self.fluid_tints.borrow_mut();

        if matches!(*tints, SectionTints::Unasked) {
            let mut env = self.env.borrow_mut();

            *tints = match TINT_HELPERS.get() {
                Some(helpers) => match helpers.section_fluid_tints(&mut env, self.section) {
                    Ok(entries) => {
                        TINT_TABLE_FETCHES.fetch_add(1, Ordering::Relaxed);
                        TINT_TABLE_ENTRIES.fetch_add(entries.len() as u64, Ordering::Relaxed);

                        // The entry layout is `Wgpu.helperGetSectionFluidTints`'s: twelve bits of
                        // position, then the colour - a fluid has no tint index to carry.
                        SectionTints::Ready(
                            entries
                                .into_iter()
                                .map(|entry| (((entry >> 36) & 0xfff) as u32, entry as u32))
                                .collect(),
                        )
                    }
                    Err(err) => {
                        describe_and_clear(&mut env, "helperGetSectionFluidTints");
                        log::warn!(
                            "wgpu-mc: the fluid tints of the section at {} could not be fetched in bulk \
                             ({err}); each fluid block will ask for its own",
                            self.section
                        );

                        SectionTints::Unavailable
                    }
                },
                None => SectionTints::Unavailable,
            };
        }

        if let SectionTints::Ready(table) = &*tints {
            let key = ((pos.x & 15) | ((pos.z & 15) << 4) | ((pos.y & 15) << 8)) as u32;
            TINT_TABLE_FLUID_HITS.fetch_add(1, Ordering::Relaxed);

            if let Some(color) = table.get(&key) {
                TINT_TABLE_FLUID_FOUND.fetch_add(1, Ordering::Relaxed);

                // **The bulk fluid table against the per-face path, for the same position**, while the
                // diagnostics are on. The fluid side has no key counters and no `TintProfile` row that
                // could see a mirrored position packing - a mirrored *fluid* table would show as water
                // moving the wrong way across a biome boundary and as nothing else at all, which is
                // exactly what the report describes. A mismatch here is that bug; none is this path
                // cleared.
                if wgpu_mc::mc::chunk::DIAGNOSTIC_LOGGING.load(Ordering::Relaxed) {
                    use std::sync::atomic::AtomicU64;

                    static CHECKED: AtomicU64 = AtomicU64::new(0);
                    static MISMATCHES: AtomicU64 = AtomicU64::new(0);

                    if CHECKED.load(Ordering::Relaxed) < 20_000 {
                        let nth = CHECKED.fetch_add(1, Ordering::Relaxed);

                        if let Some(helpers) = TINT_HELPERS.get() {
                            let mut env = self.env.borrow_mut();
                            let single = helpers.fluid_color(&mut env, pos).ok();

                            if let Some(single) = single {
                                if single as u32 != *color {
                                    let seen = MISMATCHES.fetch_add(1, Ordering::Relaxed);

                                    if seen < 24 {
                                        log::warn!(
                                            "wgpu-mc: fluid tint mismatch at {pos}: bulk {color:#010x}, \
                                             per-face {single:#010x}"
                                        );
                                    }
                                }

                                // Once, so that "no mismatches" cannot be "nothing compared".
                                if nth == 0 {
                                    log::info!(
                                        "wgpu-mc: the fluid tint check is running: bulk {color:#010x}, \
                                         per-face {single:#010x} at {pos}"
                                    );
                                }
                            }
                        }
                    }
                }

                return *color;
            }

            // A position the JVM's walk did not report, or a packing the two sides disagree about. The
            // trait's own default is the right answer here rather than white: this is water.
            return wgpu_mc::mc::chunk::WATER_TINT;
        }

        drop(tints);

        let mut env = self.env.borrow_mut();

        let result = match TINT_HELPERS.get() {
            Some(helpers) => helpers.fluid_color(&mut env, pos),
            None => {
                note_the_slow_tint_path();

                call_static_from_class_loader(
                    &mut env,
                    TINT_HELPER_CLASS,
                    "helperGetFluidColor",
                    "(III)I",
                    &[JValue::Int(pos.x), JValue::Int(pos.y), JValue::Int(pos.z)],
                )
                .and_then(|value| value.i())
            }
        };

        match result {
            Ok(color) => {
                // Counted so the answer is *readable* rather than merely error-free: the failure this
                // whole change exists to fix looked exactly like success, because a constant is a valid
                // colour. A total of zero here means the game answered "no tint" for every fluid, which
                // is a different bug from the one this closed and would be invisible without a count.
                if color as u32 != 0xffff_ffff {
                    FLUID_TINTS.fetch_add(1, Ordering::Relaxed);
                }

                color as u32
            }
            Err(err) => {
                static WARNED: AtomicBool = AtomicBool::new(false);
                if !WARNED.swap(true, Ordering::Relaxed) {
                    describe_and_clear(&mut env, "helperGetFluidColor");
                    log::warn!(
                        "wgpu-mc: could not ask the game for a fluid tint ({err}); water is drawn with \
                         the default colour rather than its biome's"
                    );
                }

                wgpu_mc::mc::chunk::WATER_TINT
            }
        }
    }
}

/// **The window mode the renderer's setting names, mirrored for the JVM's callback.**
///
/// The mode itself lives in the settings document, but the callback that applies it runs *after* the
/// settings have been stored and cannot read that document back - `SETTINGS.read()` would be reachable,
/// and the whole point of the callback is to be callable from the paths that have already decided what
/// moved. So the answer is mirrored here, set beside the other values `debug::apply` copies out.
///
/// The low two bits are the variant index (`0` exclusive, `1` borderless, `2` off) and bit 2 is
/// "this moved and needs a restart to take effect". A single `u8` because both fit and because every
/// reader of it wants both.
pub static WINDOW_MODE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Sets [`WINDOW_MODE`]: the variant index, and whether the change it came from needs a restart.
pub fn set_window_mode(mode: u8, restart: bool) {
    WINDOW_MODE.store(
        mode & 0b11 | (restart as u8) << 2,
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// **The window mode the renderer's setting names**, as the variant index the JVM's `Mode` enum is in the
/// same order as: `0` exclusive fullscreen, `1` borderless, `2` off.
///
/// Read by `DisplayMode.Mode.current` on the path that puts the window into a mode, rather than through
/// the settings document, because that path cannot afford a cached read: `RendererSettings` caches for a
/// second, and a mode applied immediately after the setting moved could read the value from before it.
#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn windowMode(_env: JNIEnv, _class: JClass) -> jint {
    // Read once per mode application rather than per frame, and **not** through the settings document:
    // `RendererSettings` caches for a second, and this is read on the path that *applies* a mode, so a
    // cached value would put the window back into the mode it was already in.
    //
    // Zero - `Exclusive` - when no settings have been read yet, which is what `options.txt`'s
    // `fullscreen: true` has always meant here. See `WINDOW_MODE`.
    (WINDOW_MODE.load(std::sync::atomic::Ordering::Relaxed) & 0b11) as jint
}

/// **Stores the window mode and puts the window into it**, for the cycle button in Minecraft's own video
/// settings screen.
///
/// The value lives in this side's config rather than in `options.txt`, because it is three states and
/// `options.txt`'s `fullscreen` is a boolean - but the *control* is on the game's screen, next to
/// fullscreen, where a player looks for it. That is what the user asked for: the setting is this
/// renderer's, the row is Minecraft's.
///
/// Applying it is a callback into the JVM rather than something this side does, because the window is
/// GLFW's and GLFW is reached from the JVM. See `DisplayMode`.
///
/// **This is the only writer of [`WINDOW_MODE`]**, which is why the callback is invoked from here rather
/// than from a settings apply: a value that arrives from the game's own screen does not go through
/// `sendSettings` at all, and a value that does would otherwise need two paths to the same effect.
#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn setFullscreenMode(mut env: JNIEnv, _class: JClass, mode: jint) {
    let mode = (mode.clamp(0, 2)) as u8;

    // Written first, and unconditionally: `windowMode` reads it back, and a cycle button that read a
    // stale value would jump back to the previous mode on the next click.
    let moved = WINDOW_MODE.swap(mode, std::sync::atomic::Ordering::Relaxed) != mode;

    crate::set_window_mode(mode, moved);

    if SETTINGS.read().is_some() {
        if let Some(settings) = SETTINGS.write().as_mut() {
            settings.fullscreen_mode = crate::settings::EnumSetting {
                selected: mode as usize,
            };
        }

        if !SETTINGS
            .read()
            .as_ref()
            .is_some_and(|settings| settings.write())
        {
            log::error!("wgpu-mc: the window mode could not be saved and will be lost on exit");
        }
    }

    if let Err(err) = crate::call_static_from_class_loader(
        &mut env,
        "dev.birb.wgpu.backend.DisplayMode",
        "reapply",
        "()V",
        &[],
    ) {
        log::warn!(
            "wgpu-mc: the window mode was stored but the window could not be put into it: {err}"
        );
    }
}

/// **Whether the window mode moved in the apply that just happened**, so the settings screen can say so:
/// `-1` when it did, and the current variant index when it did not.
///
/// The schema cannot answer this. It says whether a setting *may* need a restart; this is whether it
/// *did* - and a player who opened the page and moved nothing is owed no message.
#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn windowModeReloadResult(_env: JNIEnv, _class: JClass) -> jint {
    let value = WINDOW_MODE.load(std::sync::atomic::Ordering::Relaxed);

    // The variant index is not the answer; whether it *moved* is, and that is bit 2. The index comes
    // back when nothing moved, so a caller that logs the answer says which mode is current.
    if value & 0b100 == 0 {
        (value & 0b11) as jint
    } else {
        -1
    }
}

/// How many fluid faces were asked for a biome colour and got one. See `get_fluid_color`.
///
/// Reported on the same once-a-second line as the atlas and leaves counts, for the same reason: the
/// question is whether a number that *should* be large is zero.
static FLUID_TINTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many tints were answered by looking the class and its method up again instead of using the cache.
///
/// **Zero is the whole point of [`TintHelpers`], and this is the number that says it.** It is drained beside
/// [`FLUID_TINTS`] on the same once-a-second line, so the two are read as a pair: tints large and lookups
/// zero is the cache doing its job, and lookups climbing is the cache not having resolved - which is a
/// working tint path and a needless cost, not a failure, so it is counted rather than logged, except for the
/// first one. See `note_the_slow_tint_path`.
static TINT_SLOW_LOOKUPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many sections have had their colours fetched in bulk, and how many colours came back.
///
/// **The pair that says whether the table is working**: one fetch per section that wanted a colour at all,
/// and an entry count in the hundreds for a section of grass or leaves. A section with no tinted state is
/// never asked about, so a run over stone and air has both at zero and is paying nothing.
static TINT_TABLE_FETCHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static TINT_TABLE_ENTRIES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many tinted faces were answered from a section's table, which is every one of them while it holds.
///
/// The number to read against `FLUID_TINTS`: the calls that are left are the fluids (one per fluid block,
/// their own path) and whatever fell back to the single-call path.
static TINT_TABLE_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many of [`TINT_TABLE_HITS`] actually **found** their key in the table.
///
/// **The only proof in a run that the two sides agree on the packing.** A position index that means one
/// thing to `Wgpu.helperGetSectionTints` and another here is a lookup that misses - and a miss is a white
/// face, which is a valid colour and therefore silent. This number equalling the one above is that
/// agreement, measured rather than argued.
static TINT_TABLE_FOUND: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The same two numbers for the **fluid** table, which is a separate call and a separate key space.
///
/// Kept apart because the failure they would catch is apart: the fluid entry carries no tint index, so a
/// packing that agreed for blocks and disagreed for fluids would read as one number below the other here
/// and nowhere else. See [`TINT_TABLE_FOUND`].
static TINT_TABLE_FLUID_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static TINT_TABLE_FLUID_FOUND: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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
    mut env: JNIEnv,
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
    //
    // The check is made here, before the payload is even parsed, and again where the bake is queued
    // (`queue_bake`, which is where a *retry* meets the same state) - the second one is the one that
    // decides, and this one exists so that a registry that is not ready costs no work at all.
    let Some(_air) = *AIR else {
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

    // **The cache's write lock covers the apply, and nothing else.** It used to be held across the
    // whole call, the 27-neighbour resolve included, which put every chunk-build thread in the game
    // behind one lock for the length of a section's worth of hash-map lookups - and they all offer
    // sections at once exactly when the world is loading and there are dozens of them.
    //
    // The apply needs it: the payload's records replace what was held for them and the ones it says
    // to forget are dropped, and a bake queued below must never see a half-applied neighbourhood.
    // Everything after it only reads.
    let rejected = {
        let mut world = WORLD.write();

        payload.apply(&mut world, target)
    };

    // A section the JVM marked as "already yours" but that this side does not have - a cache that was
    // trimmed, a new world, a section that became empty under us - means a bake against holes. The
    // caller sends everything again instead, and this call queues nothing.
    let known_blocks = payload.known_blocks & !payload.present;
    let known_light = payload.known_light
        & !payload
            .light
            .iter()
            .fold(0u32, |mask, (index, _)| mask | (1 << index));

    {
        let world = WORLD.read();

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
    }

    // **The trim comes last, and that is what makes the shorter lock safe.** It is the only other
    // thing in this call that mutates the cache, and by the time it runs the bake owns everything it
    // needs: `queue_bake` takes its own reference to all 27 slots (`SectionWorld::blocks` is a clone
    // of an `Arc`), so the cache dropping its reference cannot take a slot away from a bake that is
    // already on its way.
    WORLD.write().trim(target);
    wgpu_mc::mc::visibility::trim(target);
    match queue_bake(&mut env, target) {
        // **On its way, or waiting for a slot in a pool that is full.** Neither is an answer the JVM
        // should hear: it counts this section as this side's until it is told otherwise, and both of
        // these say the same thing - the blocks and the light of all 27 slots are here, held by
        // reference count, and in the second case the only thing missing is a place in the queue.
        //
        // Waiting is what makes that case free. The old path handed it to the JVM, whose recovery was
        // to mark the section dirty, have Minecraft schedule a rebuild, have a chunk-build worker
        // assemble the 27-section payload and compile 4096 block positions again, and apply all of it
        // back into this cache - all of it to arrive at this queue a second time.
        Queued::Dispatched | Queued::Waiting => rejected as jint,
        // **Nowhere to bake it and nowhere to wait for one.** The pool is full and
        // `MAX_WAITING_BAKES` sections are already waiting, or the cache no longer holds the section
        // (see `queue_bake`), or the JVM handle could not be had.
        //
        // **The bit below is not enough on its own, and this comment used to claim it was.** It said
        // a dropped offer "is not lost work - it comes back", which is true only if something rebuilds
        // the section again, and nothing does: a rebuild happens when the game decides a section is
        // out of date, and the section this call is about was just brought up to date. The bit makes
        // the JVM forget it was sent, so the *next* rebuild carries its blocks - and the next rebuild
        // is the thing that does not happen. Meanwhile the mesh for it was dropped when this side was
        // believed to have it, so it is drawn by neither renderer.
        //
        // So it is queued for a rebuild the way a refusal is, which is the only mechanism that
        // actively asks the game for one. See `SectionStorage::forget_trimmed` and
        // `RustChunkBake.redirtyDue`.
        Queued::Refused => {
            forget_one(target);

            (rejected | (1 << CENTER)) as jint
        }
    }
}

/// Records that this side is not going to draw one section, so the game is asked to draw it.
///
/// Through the same channel a refusal and a trim use, because all three mean one thing to the other
/// side: a section this renderer does not hold. The JVM drops it from its "already sent" table and asks
/// the game to rebuild it, and the rebuild that comes back has Minecraft's own mesh as its fallback if
/// this side refuses it again. See `RustChunkBake.forgetRefused`.
///
/// Nothing happens without a renderer: a bake cannot have been dropped before there was one.
fn forget_one(pos: IVec3) {
    if let Some(scene) = RENDERER.get().and_then(|wm| wm.scene()) {
        scene.section_storage.write().forget_trimmed(pos);
    }
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
/// is not lost work - it comes back.
///
/// **This is not a memory bound, and the comment here used to say it was.** It said each queued bake
/// "owns 27 sections' worth of palettes, storages and light layers, which is a few hundred kilobytes" -
/// and a bake owns no such thing. It is handed `Arc`s out of the section cache
/// (`SectionWorld::blocks`, which is `get(..).cloned()`, a refcount bump), so what it holds is 54
/// pointers: 27 for the blocks and 27 for the light. `a_queued_bake_holds_pointers_and_not_sections`
/// in `section.rs` measures it, because the arithmetic is the only thing that settles a sentence like
/// that one - **440 bytes per queued bake, so a full queue of this many is 110 KB**, and the payload
/// that arrived over JNI is dropped at the end of the call that built the task.
///
/// What the number is actually for is **latency**, and that is why it is a few hundred rather than a
/// few thousand: a bake that waits behind a thousand others has been overtaken by the player twice, and
/// the section is rebuilt and offered again before it is ever drawn. Dropping it and letting the next
/// offer queue nearer the front is the better answer, and it costs nothing because the offer comes back.
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

/// How many sections may wait for a slot in the bake pool before an offer is refused outright.
///
/// The same order as [`MAX_QUEUED_BAKES`] and for the same reason: a section waiting behind a few
/// hundred others has been overtaken by the player twice, and the ground it is being baked for is
/// ground nobody is looking at by the time it lands. Past this an offer is refused the old way - the
/// JVM is told and Minecraft's own mesh takes the section - which costs a rebuild rather than a hole.
const MAX_WAITING_BAKES: usize = 256;

/// The sections whose bake was refused for want of a slot in the pool, oldest first.
///
/// **This queue is what keeps a refusal from being a round trip.** When the pool is full the payload
/// has already been applied, so the blocks and the light of all 27 slots are here, held by reference
/// count, and the only thing missing is somewhere to run. Waiting for it costs the JVM nothing: it is
/// never told, it never sends the section again, and Minecraft never compiles it again.
static BAKE_WAITERS: Lazy<Mutex<VecDeque<IVec3>>> = Lazy::new(|| Mutex::new(VecDeque::new()));

/// How many bakes have taken a place in [`BAKE_WAITERS`] over the run.
static BAKES_WAITED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many of those reached the pool on a retry, which is the number that says the round trip was
/// avoided rather than merely delayed.
static BAKES_RETRIED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many waiting sections could not be baked at all and went back to the JVM. See [`queue_bake`].
static RETRIES_REFUSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// What became of an attempt to get a section's bake onto the pool.
enum Queued {
    /// It is on its way to the pool.
    Dispatched,
    /// The pool was full, so it is waiting in [`BAKE_WAITERS`] and `retryBakes` will offer it again.
    Waiting,
    /// It cannot be baked by this side, so the caller has to treat it as a refusal: the cache no
    /// longer holds the section, there is nowhere left to wait, or there is no block registry yet.
    Refused,
}

/// Resolves a section's 27 neighbours out of the cache and puts the bake on the pool.
///
/// One function for both callers because the work is the same - a payload that has just arrived and a
/// section that has been waiting resolve exactly the same slots - and only the first of them had
/// anything to apply. That is also why the cache is only *read* here: the write lock belongs to
/// `apply`, which is the one thing a retry does not have to do again. See `bakeSections`.
fn queue_bake(env: &mut JNIEnv, target: IVec3) -> Queued {
    // The registry, for the same reason `bakeSections` checks it: without `AIR` there is no way to
    // tell a hole from a block. A retry can arrive here and a first offer cannot - the registry can
    // be replaced while a section waits - so the check is made where the answer is used.
    let Some(air) = *AIR else {
        return Queued::Refused;
    };

    let provider = {
        let world = WORLD.read();

        // **The section itself, which a payload cannot vouch for.** The JVM counts a section it has
        // sent as one this side holds, so a section the cache has forgotten - trimmed while the
        // player walked away, or cleared by a level change - is the one case where the two disagree.
        // Baking it would bake a hole: every slot resolves to `None`, which is air, and a section of
        // solid ground would come out as nothing at all. Refusing hands it back to the JVM instead,
        // which re-sends it along with everything else it has.
        if world.blocks(target).is_none() {
            return Queued::Refused;
        }

        let mut blocks: [Option<Arc<SectionBlocks>>; SECTIONS] = Default::default();
        let mut light: [Option<Arc<SectionLight>>; SECTIONS] = Default::default();

        // Every slot is resolved from the cache, not just the ones a payload carried: a section the
        // caller did not send is one it believes is already here, and a slot that is still empty is a
        // section that is not loaded - which is air, as it is for Minecraft's own mesher.
        for index in 0..SECTIONS {
            let pos = target + neighbour_offset(index);
            blocks[index] = world.blocks(pos);
            light[index] = world.light(pos);
        }

        CachedBlockstateProvider {
            variants: world.variants(target),
            blocks,
            light,
            air,
        }
    };

    let jvm = match env.get_java_vm() {
        Ok(jvm) => jvm,
        Err(err) => {
            log::error!("wgpu-mc: could not get the JVM handle for a bake: {err}");

            return Queued::Refused;
        }
    };

    // The Java thread is done with this section: everything the bake needs is owned by now, so it goes
    // to the pool and the caller returns to Minecraft's chunk build. See [BakeTask] for what crosses
    // the thread boundary and what deliberately does not.
    match BakeTask::new(target, provider, jvm) {
        Some(task) => {
            THREAD_POOL.spawn(move || task.run());

            Queued::Dispatched
        }
        // **The pool is full, so this section waits here instead of going back to the JVM.** The
        // payload has been applied and the 27 slots are resolved; what is missing is a place to run,
        // and a place will free up as the bakes ahead of it finish. Handing it to the JVM instead
        // means a rebuild of the section through Minecraft's compiler and a second JNI call, to end
        // up back here with the same queue in front of it.
        None => {
            if wait_for_a_bake_slot(target) {
                Queued::Waiting
            } else {
                Queued::Refused
            }
        }
    }
}

/// Puts a section in the queue of bakes waiting for a slot, and answers whether there was room.
///
/// Answers `false` past [`MAX_WAITING_BAKES`], and that is the whole fallback: the offer is then
/// refused the old way, so a pool that cannot keep up costs a rebuild rather than a queue that grows
/// with the world.
fn wait_for_a_bake_slot(target: IVec3) -> bool {
    let mut waiting = BAKE_WAITERS.lock();

    // Already waiting, which happens when the game rebuilds a section before the bake it is waiting
    // for has run: the second offer is the same section and not a second place in the queue.
    if waiting.contains(&target) {
        return true;
    }

    if waiting.len() >= MAX_WAITING_BAKES {
        static WARNED: AtomicBool = AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::Relaxed) {
            log::warn!(
                "wgpu-mc: {MAX_WAITING_BAKES} section bakes are already waiting for a slot in the \
                 pool; refusing this one, which Minecraft will offer again"
            );
        }

        return false;
    }

    waiting.push_back(target);

    BAKES_WAITED.fetch_add(1, Ordering::Relaxed);

    true
}

/// Offers the sections waiting for a slot in the bake pool again, up to `limit` of them.
///
/// **This is the JVM's whole part in a refused bake now.** One call a frame, no payload, nothing to
/// send and nothing to assemble: what it replaced was a chain of four steps that all existed to get
/// the same section back into this queue - the JVM marking it dirty, Minecraft scheduling a rebuild,
/// a chunk-build worker assembling the 27-section payload and compiling 4096 block positions, and a
/// JNI call that took the cache's write lock to apply every one of them again.
///
/// The limit is the caller's, because the rate belongs to the frame it is called from, and what comes
/// back is how many sections this call dealt with. A section the cache has forgotten is handed to the
/// JVM as a refusal (see [`queue_bake`]), and one that meets a pool which is still full goes back to
/// the front of the queue and ends the call - a slot that was not free for it is not free for
/// anything behind it either.
#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn retryBakes(mut env: JNIEnv, _class: JClass, limit: jint) -> jint {
    if limit <= 0 {
        return 0;
    }

    let mut dealt_with = 0;

    while dealt_with < limit {
        // One at a time, and the lock is dropped before the bake is prepared: this queue is fed by
        // Minecraft's own chunk-build threads, and holding it across a 27-slot resolve would stall
        // them for as long as one takes - the serialisation this whole path exists to remove.
        let Some(target) = BAKE_WAITERS.lock().pop_front() else {
            break;
        };

        match queue_bake(&mut env, target) {
            Queued::Dispatched => {
                dealt_with += 1;

                BAKES_RETRIED.fetch_add(1, Ordering::Relaxed);
            }
            Queued::Waiting => {
                // Still no room, so nothing behind it can go either.
                BAKE_WAITERS.lock().push_front(target);

                break;
            }
            Queued::Refused => {
                // The cache no longer holds it, so it is the JVM's section now - the same refusal a
                // full pool produces when there is nowhere left to wait.
                forget_one(target);

                dealt_with += 1;

                RETRIES_REFUSED.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    report_bake_waiters();

    dealt_with as jint
}

/// Says how many bakes are waiting for a slot: once a second while any are, and once when the last
/// one goes.
///
/// Silence is the common case and it means the pool kept up. A line here means the game is rebuilding
/// sections faster than the pool bakes them, which is the state [`BAKE_WAITERS`] exists to absorb, and
/// the three numbers say whether it absorbed it - how many waited, how many reached the pool on a
/// retry, and how many had to go back to Minecraft after all.
fn report_bake_waiters() {
    static REPORTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static WAS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    let waiting = BAKE_WAITERS.lock().len();
    let was = WAS.swap(waiting, Ordering::Relaxed);

    if waiting == 0 && was == 0 {
        return;
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);

    if waiting == 0 || REPORTED.swap(now, Ordering::Relaxed) != now {
        log::info!(
            "wgpu-mc: {waiting} section bake(s) are waiting for a slot in the pool; {} have waited \
             over the run, {} of them reached the pool when it had room, and {} could not be baked \
             here and went back to Minecraft",
            BAKES_WAITED.load(Ordering::Relaxed),
            BAKES_RETRIED.load(Ordering::Relaxed),
            RETRIES_REFUSED.load(Ordering::Relaxed),
        );
    }
}

/// How many bake threads have attached themselves to the JVM, over the whole run.
///
/// **The counter that settles a claim about this path.** A bake thread attaches itself because a bake may
/// call back into Java for a biome tint, and the question that keeps coming back is whether that is once per
/// *bake* or once per *thread*. It is once per thread: `jni`'s `attach_current_thread_as_daemon` returns the
/// environment a thread that is already attached already has (a `GetEnv` and nothing else), and the detach
/// lives in that thread's own TLS and is dropped when the thread *exits*, not when a returned value drops -
/// `JNIEnv` in this crate owns nothing at all. So this number should stay at the size of the bake pool (one
/// thread per core) however many thousands of bakes a run does.
///
/// Reported on the atlas line, beside the tint counts, which is where the rest of this path's numbers are.
static BAKE_THREADS_ATTACHED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Attaches this bake thread to the JVM, counting the first time per thread. See [`BAKE_THREADS_ATTACHED`].
///
/// **A daemon attachment, which is the right one here and the opposite of the panic hook's.** A bake pool
/// thread must not hold the JVM open once the game is done; the panic hook attaches permanently because its
/// whole job is to run *while* the JVM is going down.
///
/// The alternative to what this does - keeping the `JNIEnv` in a thread-local so that even the `GetEnv` this
/// makes is skipped - is written down rather than done: that call is a few tens of nanoseconds against a bake
/// measured in milliseconds, and a cached environment is one more unsafe pointer to reason about.
fn attach_bake_thread(jvm: &JavaVM) -> jni::errors::Result<JNIEnv<'_>> {
    // Set only after a successful attach, so a failed one is retried by the next bake on this thread rather
    // than counted as an attach that happened.
    thread_local! {
        static ATTACHED: Cell<bool> = const { Cell::new(false) };
    }

    let env = jvm.attach_current_thread_as_daemon()?;

    ATTACHED.with(|attached| {
        if !attached.replace(true) {
            BAKE_THREADS_ATTACHED.fetch_add(1, Ordering::Relaxed);
        }
    });

    Ok(env)
}

/// One section bake, on its way to the pool.
///
/// Everything it needs is owned: the JNI arrays it was built from are only valid on the thread that
/// received them, so the palettes, the storages and the two light layers are moved out before the
/// task is spawned. The `JNIEnv` is deliberately *not* carried across - a pool thread attaches itself
/// **once**, in [`BakeTask::run`], which is also where the callback into Java for biome tints gets a
/// usable environment from. See [`attach_bake_thread`].
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

        // The bake asks Java for a biome tint per tinted block (`Wgpu.helperGetBlockColor`), so this thread
        // needs its own attachment to the JVM - once, not once per bake. See [`attach_bake_thread`].
        let env = match attach_bake_thread(&self.jvm) {
            Ok(env) => env,
            Err(error) => {
                log::warn!("wgpu-mc: could not attach a bake thread to the JVM: {error}");
                return;
            }
        };

        let wrapper = MinecraftBlockStateProviderWrapper {
            internal: self.provider,
            env: RefCell::new(env),
            section: self.pos,
            tints: RefCell::new(SectionTints::Unasked),
            fluid_tints: RefCell::new(SectionTints::Unasked),
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

/// How many bakes are queued or running right now, against [`MAX_QUEUED_BAKES`].
///
/// The JVM's refusal drain reads this to decide whether to ask for a rebuild at all: a refused section
/// is one the arena had no room for, and asking the game to rebuild it is worth doing only when there
/// is room in the pool for the result. With the queue backed up the rebuild would wait behind everything
/// already in it and arrive at a section the player has left, so the drain backs off until the pool has
/// caught up - which is what stops "full queue, mark dirty, offer again, still full" from being a
/// rebuild storm.
///
/// See `RustChunkBake.redirtyDue`.
#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn queuedBakes(_env: JNIEnv, _class: JClass) -> jint {
    QUEUED_BAKES.load(Ordering::Relaxed) as jint
}

/// **How many sections the arena is actually drawing**, which is the number the JVM's `rustHas` is a
/// claim about.
///
/// This exists to close a hole that three rounds of reasoning failed to close: `rustHas` means "this
/// side told Rust about the section", and the JVM uses it to suppress Minecraft's own mesh - but a
/// section can be told *and* never published, and then neither renderer draws it. Nothing compared the
/// claim with the fact, so nothing could say whether a hole was one.
///
/// The comparison is a count rather than a list because it is read once a second: `rustHas` also holds
/// sections the arena has legitimately trimmed (the game's bookkeeping is wider than this side's view),
/// so the two are not expected to be equal - what matters is whether the gap *moves*, and a report line
/// with both numbers on it says that.
#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn arenaSections(_env: JNIEnv, _class: JClass) -> jint {
    RENDERER
        .get()
        .and_then(|wm| wm.scene())
        .map(|scene| scene.section_storage.read().len() as jint)
        .unwrap_or(0)
}

/// The ceiling [`queuedBakes`] is measured against - [`MAX_QUEUED_BAKES`], so the two sides agree about
/// what "backed up" means without either writing the other's number down.
#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn maxQueuedBakes(_env: JNIEnv, _class: JClass) -> jint {
    MAX_QUEUED_BAKES as jint
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

    // The occlusion answers describe the world that was just thrown away, under coordinates the next
    // one is about to use. See `wgpu_mc::mc::visibility`.
    wgpu_mc::mc::visibility::clear();

    // And the box the occlusion walk is bounded by, for the same reason: the world the next frame is
    // about is a different one, and a stale horizon is a walk that draws the wrong sections - or, if the
    // camera is no longer inside the old level's layers, none at all. The frame's own push puts the new
    // box back. See `wgpu_mc::mc::world_extent`.
    wgpu_mc::mc::world_extent::clear();

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

        // **For the atlas routing, and it has to be after the arena is empty.** Which atlas a face was
        // baked for is a property of the face, and the faces are gone; the next ones are baked under
        // whatever is bound now. Without this a session that fell back once - one sprite the game's atlas
        // has no rectangle for - would carry the two-atlas shaders for the rest of its life, because the
        // question they answer would never be asked again. See `block::forget_atlas_faces`.
        wgpu_mc::mc::block::forget_atlas_faces();
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

                for (name, rect, layer, level_cap) in registrations {
                    atlas.register_sprite(
                        &ResourcePath::from(&name[..]),
                        rect,
                        registered_layer(layer),
                        Some(level_cap),
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
/// **Which atlas the baked faces went to**, for the JVM's once-a-second report.
///
/// The one thing about the atlas routing that cannot be seen: both atlases are 2048x2048 and both answer
/// to the same filters, so a face on the wrong one is not a face drawn differently - it is a face
/// sampling a mip chain built from the whole packed sheet instead of per sprite, which is a blurred,
/// half-transparent block. The first number should be nearly all of them and the second nearly none; a
/// second number in the thousands is the routing not working.
///
/// Read and **reset**, so what a line reports is what happened since the last one - the counts are
/// otherwise a session total that stops moving, and "it stopped moving" is indistinguishable from "the
/// report is broken" in a log.
///
/// Not part of [`blockBakeDiagnostics`], which is called once right after the block cache is built and
/// therefore before any section has been baked: it would report two zeroes whatever the answer is.
#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn atlasFaceCounts(env: JNIEnv, _class: JClass) -> jstring {
    let game = wgpu_mc::mc::block::FACES_GAME_ATLAS.swap(0, std::sync::atomic::Ordering::Relaxed);
    let own = wgpu_mc::mc::block::FACES_OWN_ATLAS.swap(0, std::sync::atomic::Ordering::Relaxed);
    // The leaves switch's count, read here rather than in a report of its own: it answers the same
    // question - did the faces go where they were supposed to - and this is already the line that
    // answers it. **Zero with the game's `cutoutLeaves` off is the switch not reaching the baker.**
    let forced =
        wgpu_mc::mc::block::FACES_FORCED_OPAQUE.swap(0, std::sync::atomic::Ordering::Relaxed);

    // And the fluid tints, which answer the same shape of question: a number that should be large and
    // must not be zero. See `FLUID_TINTS`.
    let tints = FLUID_TINTS.swap(0, Ordering::Relaxed);

    // **And how many of the tints had to look their helper up again**, which is the other half of that
    // number: a tint is a `CallStaticIntMethodA` and nothing else while [`TintHelpers`] holds, and a
    // `loadClass` plus a method lookup per face while it does not. Zero is the cache working.
    let lookups = TINT_SLOW_LOOKUPS.swap(0, Ordering::Relaxed);

    // **And how many tint callbacks the baker did not have to make at all**, split by the two reasons it
    // did not: the faces of one block sharing a colour, and the faces culled before the colour was asked.
    // The remainder is the `tints` count above, so the three numbers are the whole story of a per-face
    // callback: asked, reused, and never reached.
    let (tints_reused, tinted_faces_culled) = wgpu_mc::mc::chunk::take_tint_face_counts();

    // **And how many bake threads have ever attached**, which is the other half of that path's cost: the
    // attachment exists because a tint may need the JVM, so a run should say whether it happens once per
    // bake or once per thread. Cumulative rather than drained, and labelled so. See
    // [`BAKE_THREADS_ATTACHED`].
    let bake_threads = BAKE_THREADS_ATTACHED.load(Ordering::Relaxed);

    // **And the bulk tint table**, which is what the callbacks above were replaced with: one fetch per
    // section that wanted a colour, the colours it returned, and how many faces were answered from it. Read
    // together with the tint count: `hits` should be most of it and the fetches should be one per section.
    let table_fetches = TINT_TABLE_FETCHES.swap(0, Ordering::Relaxed);
    let table_entries = TINT_TABLE_ENTRIES.swap(0, Ordering::Relaxed);
    let table_hits = TINT_TABLE_HITS.swap(0, Ordering::Relaxed);
    let table_found = TINT_TABLE_FOUND.swap(0, Ordering::Relaxed);
    let fluid_table_hits = TINT_TABLE_FLUID_HITS.swap(0, Ordering::Relaxed);
    let fluid_table_found = TINT_TABLE_FLUID_FOUND.swap(0, Ordering::Relaxed);

    // **And the animated faces, which are the ones the level-of-detail offset applies to.** Read beside the
    // game-atlas count because the ratio is the question: that offset is a compensation for a level chosen
    // too coarse, it is only legitimate on a sprite whose coarse levels move, and on anything else it is a
    // sharpening nobody asked for - the moiré that scoping it was written to remove. See
    // `wgpu_mc::mc::block::FACES_ANIMATED`.
    let animated = wgpu_mc::mc::block::FACES_ANIMATED.swap(0, std::sync::atomic::Ordering::Relaxed);

    // **And how many of those got a floor at all**, which is the pair that a "the floor changed nothing"
    // report has to be read against: a floor of zero is *no* floor, and no floor is the shimmer - so the
    // picture cannot tell "the size never arrived" from "the floor is not what helps". The numbers can. See
    // `wgpu_mc::mc::block::FACES_LOD_FLOORED`.
    let floored =
        wgpu_mc::mc::block::FACES_LOD_FLOORED.swap(0, std::sync::atomic::Ordering::Relaxed);

    // **And the fluids, which the two counters above do not cover at all.** They are the block-model path;
    // a fluid face sets its own flags in `FluidSprite::flags`, so `animated` above has never counted one.
    // A player reported the lava shimmering again, and this pair is what says whether the faces still carry
    // the flag the whole fix rests on. See `FLUID_FACES_ANIMATED`.
    let moved =
        wgpu_mc::mc::chunk::FLUID_FACES_ANIMATED.swap(0, std::sync::atomic::Ordering::Relaxed);
    let still =
        wgpu_mc::mc::chunk::FLUID_FACES_STATIC.swap(0, std::sync::atomic::Ordering::Relaxed);

    // **And of the ones that carried the flag, how many got a floor** - the pair nobody was asking, and the
    // reason a fix that had been measured as working came back. A fluid face with the flag and a floor of
    // zero is a fluid face that shimmers, and that combination is what a missing `level_cap` field produced
    // for every fluid in the world while the flag count above read as healthy. See `FLUID_FACES_FLOORED`.
    let floored_fluid =
        wgpu_mc::mc::chunk::FLUID_FACES_FLOORED.swap(0, std::sync::atomic::Ordering::Relaxed);
    let unfloored_fluid =
        wgpu_mc::mc::chunk::FLUID_FACES_UNFLOORED.swap(0, std::sync::atomic::Ordering::Relaxed);

    let text = format!(
        "{game} game-atlas, {own} own-atlas, {animated} of them animated ({floored} floored), {forced} leaf \
         face(s) forced opaque, {tints} fluid tint(s) read from the game ({lookups} tint(s) that looked the \
         helper up again, and zero there is the cache working; {tints_reused} face(s) reused their block's \
         colour and {tinted_faces_culled} tinted face(s) were culled before asking; {bake_threads} bake \
         thread(s) attached in all; {table_hits}+{fluid_table_hits} face(s) answered from a section table \
         ({table_found}+{fluid_table_found} of them found, and each pair agreeing is a packing agreeing), \
         from {table_fetches} fetch(es) of {table_entries} colour(s)), {moved} fluid face(s) \
         with the \
         animated flag ({floored_fluid} of them floored) and {still} without (and {unfloored_fluid} \
         animated with no floor)"
    );

    env.new_string(text)
        .map(|string| string.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

/// **The game's `cutoutLeaves` option**, pushed before a bake because it is written into the geometry.
///
/// `Options#cutoutLeaves` is the Fancy/Fast leaves switch the graphics presets move, and it reaches
/// baking as `ModelBlockRenderer#forceOpaque`: with it off, a `LeavesBlock`'s faces go to the *solid*
/// layer whatever their sprite says, and the solid layer has no alpha test - so a leaf texture's
/// transparent gaps are filled by its own colour and the block reads as a solid mass.
///
/// This side takes a face's layer from its sprite, so the option did nothing here at all before this
/// existed: `Fast` and `Fancy` drew identical leaves. See `wgpu_mc::mc::block::CUTOUT_LEAVES`.
///
/// Sent on every bake rather than watched for changes, because the value is a `boolean` read from an
/// option object on another thread and comparing it here would need a cached copy to compare *against* -
/// a copy that is one more thing to keep in step. The bake is seconds long; one store is nothing.
#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn setCutoutLeaves(_env: JNIEnv, _class: JClass, cutout: jboolean) {
    wgpu_mc::mc::block::set_cutout_leaves(cutout != 0);
}

/// **The game's `textureFiltering` option, pushed every frame, because it decides how a surface is
/// sampled.**
///
/// `TextureFilteringMethod`'s own ids - `NONE` is 0, `RGSS` is 1, `ANISOTROPIC` is 2 - and the game's own
/// numbering rather than a switch of this side's, for the same reason the cardinal lighting table travels as
/// six floats: this side has no business knowing what a "Fabulous" is, and the value is written straight
/// into an immediate and compared against `1`.
///
/// It changes two things, and they are different algorithms rather than two settings of one:
///
///  * **the sampler's `anisotropy_clamp`**, which is the option's own
///    `Options#maxAnisotropyValue` when this is `ANISOTROPIC` and 1 otherwise - `LevelRenderer` builds its
///    terrain sampler exactly that way, and the default bit is 2, so the game's own answer is 4 rather
///    than the 16 this renderer hardcoded;
///  * **whether the shader runs the game's rotated-grid supersampling**, which no sampler can be asked
///    for. See the port in `terrain.wgsl`.
#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn setTextureFiltering(_env: JNIEnv, _class: JClass, method: jint) {
    wgpu_mc::render::atlas::set_texture_filtering(method.max(0) as u32);
}

/// **The game's `maxAnisotropyBit` option, which is the exponent in `Options#maxAnisotropyValue`.**
///
/// The other half of the terrain sampler's `anisotropy_clamp`, and it travels beside
/// [`setTextureFiltering`] because the two are one line of `LevelRenderer`:
///
/// ```java
/// int maxAnisotropy = this.optionsRenderState.textureFiltering == TextureFilteringMethod.ANISOTROPIC
///     ? this.optionsRenderState.maxAnisotropyValue
///     : 1;
/// ```
///
/// The bit is pushed rather than the value, so the shift happens once on the native side and the number
/// that crosses is the same number `options.txt` holds - which is what makes a hand-edited file and the
/// slider mean the same thing. The game's own range is `1..3`, so its answers are 2, 4 and 8; the native
/// side clamps anything past what `wgpu` accepts rather than shifting off the end of the type.
///
/// **The value is not validated here.** A bit the option could not produce is still a number, and the
/// side that owns the shift is the side that knows what its own ceiling is - see `game_anisotropy`.
#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn setMaxAnisotropyBit(_env: JNIEnv, _class: JClass, bit: jint) {
    wgpu_mc::render::atlas::set_max_anisotropy_bit(bit.max(0) as u32);
}

/// **The dimension's `CardinalLighting`, pushed before a bake because it is written into the geometry.**
///
/// The game keeps two tables and picks between them with `ClientLevel#cardinalLighting`: `DEFAULT`
/// (`0.5, 1.0, 0.8, 0.8, 0.6, 0.6`) and `NETHER` (`0.9, 0.9, 0.8, 0.8, 0.6, 0.6`). They differ **only at up
/// and down**, which is why no side ever looked wrong in either dimension and why the nether's ceiling and
/// floor did: this side used the overworld's six everywhere.
///
/// Six floats rather than a dimension id, because the tables are data the game owns - a mod's dimension can
/// carry its own - and this side has no business knowing what a "nether" is. It only has to scale a colour
/// by what it is told.
///
/// Sent on every bake rather than watched for changes, for the same reason [`setCutoutLeaves`] is: the value
/// lives on an object read from another thread, and a cached copy would be one more thing to keep in step.
/// The bake is seconds long; six stores are nothing.
///
/// **The order is `Direction`'s own** - west, east, down, up, north, south - which is *not* the order the
/// game's record lists them in. The JVM side does that mapping; see `BlockCache`.
#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn setCardinalLighting(
    _env: JNIEnv,
    _class: JClass,
    west: jfloat,
    east: jfloat,
    down: jfloat,
    up: jfloat,
    north: jfloat,
    south: jfloat,
) {
    wgpu_mc::mc::block::set_cardinal_lighting([west, east, down, up, north, south]);
}

#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn blockBakeDiagnostics(env: JNIEnv, _class: JClass) -> jstring {
    let mut report = String::new();

    // **Where every block-model face's atlas went, said in the report the JVM already logs.** The two
    // counts are written by `face_data` - the one place a face's `UV_GAME_ATLAS` bit is decided - and this
    // is the moment right after the block models are baked, so the numbers are that bake's answer rather
    // than a running total read at some other time.
    //
    // It is deliberately first: it is the count of *every* face, so it is the denominator the three
    // diagnostics below are read against. A session where the second number is not near zero is one whose
    // terrain shaders still need the second atlas.
    let (game, ours) = wgpu_mc::mc::block::atlas_face_counts();

    report.push_str(&format!(
        "{game} face(s) are baked against the game's block atlas and {ours} against this side's own"
    ));

    let unreadable = wgpu_mc::mc::block::UNREADABLE_TEXTURES.faces();

    if unreadable != 0 {
        report.push_str("; ");

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

    /// **A section whose bake met a full pool waits here** rather than being handed back to the JVM,
    /// which is what removes the round trip: the payload has been applied, the 27 slots are resolved,
    /// and a place in the queue is the only thing missing.
    ///
    /// One test for the whole queue because it is global, like the counter above: the two of them are
    /// the shared state this module owns for the length of a test and nothing else touches.
    #[test]
    fn a_section_that_meets_a_full_pool_waits_and_the_queue_is_bounded() {
        BAKE_WAITERS.lock().clear();

        let waited = BAKES_WAITED.load(Ordering::Relaxed);

        assert!(wait_for_a_bake_slot(ivec3(1, 2, 3)));
        assert_eq!(BAKE_WAITERS.lock().len(), 1);
        assert!(
            BAKES_WAITED.load(Ordering::Relaxed) > waited,
            "and the run counts it, which is the number that says the round trip was avoided"
        );

        // The same section offered again is one place in the queue and not two: the game can rebuild
        // a section before the bake it is already waiting for has run.
        assert!(wait_for_a_bake_slot(ivec3(1, 2, 3)));
        assert_eq!(BAKE_WAITERS.lock().len(), 1);

        // Oldest first, so what comes off the front is what has been waiting longest - which is what
        // `retryBakes` relies on when it puts a section back on the front.
        assert!(wait_for_a_bake_slot(ivec3(4, 5, 6)));
        assert_eq!(BAKE_WAITERS.lock().pop_front(), Some(ivec3(1, 2, 3)));
        assert_eq!(BAKE_WAITERS.lock().pop_front(), Some(ivec3(4, 5, 6)));

        // **Past the cap an offer is refused outright**, and that is the whole fallback: a pool that
        // cannot keep up costs a rebuild of the section rather than a queue that grows with the world.
        for index in 0..MAX_WAITING_BAKES {
            assert!(
                wait_for_a_bake_slot(ivec3(index as i32, 0, 0)),
                "place {index} of {MAX_WAITING_BAKES}"
            );
        }

        assert_eq!(BAKE_WAITERS.lock().len(), MAX_WAITING_BAKES);
        assert!(
            !wait_for_a_bake_slot(ivec3(-1, 0, 0)),
            "the queue is a bound on how much work may wait, not a hope"
        );
        assert_eq!(BAKE_WAITERS.lock().len(), MAX_WAITING_BAKES);

        // A section that is already waiting is still answered with `true` at the cap: it is not asking
        // for a second place, so refusing it would be refusing one that already has a place.
        assert!(wait_for_a_bake_slot(ivec3(0, 0, 0)));
        assert_eq!(BAKE_WAITERS.lock().len(), MAX_WAITING_BAKES);

        BAKE_WAITERS.lock().clear();
        assert_eq!(BAKE_WAITERS.lock().len(), 0, "and the test leaves it empty");
    }
}
