//! The debug switches, which are options on the options screen and nothing else.
//!
//! Every diagnostic this renderer has grew as a file in the run directory - `wgpu-dump-frames`,
//! `wgpu-no-bind-group-cache`, `wgpu-trace-dynamic-offsets`, `wgpu-dump-shaders` - because a file
//! needs no launcher support and no rebuild. They are options now, and **the files are gone**: a run
//! that still has one in its directory takes the setting's answer, because the setting is the only
//! source. That is the point of moving them - a file nothing in the game can show you, that has no way
//! to be turned off from inside it, and that four of the switches were spelled as the *negation* of,
//! so the file won over the setting and the option could not override it at all.
//!
//! The flags live in atomics rather than being read from the settings on use. They are read on the
//! draw path - once per draw for `diagnostics`, once per pipeline bind for the rest - and the
//! settings are behind a lock. [`apply`] is what moves a setting into the atomics: it runs whenever
//! the settings are loaded or sent, which is startup and every Apply on the options screen.
//!
//! Two of them are not read on the draw path at all. `terrain_no_cull` and `terrain_greater_depth`
//! are state built into a pipeline when it is created, so they are handed to the crate that creates
//! them and the graph is built again when either moves - see [`rebuild_pipelines_if_stale`].
//!
//! Three of the accessors have no caller in this crate: [`diagnostics`], [`gpu_timestamps`] and
//! [`section_timing`] are read where the work they describe is done, and the switches that do have
//! callers - [`logging`], [`pix_capture`], the two pipeline-state flags - go through the same shape.
//! They are kept as the complete set, one per switch, so that a new call site reads its flag the way
//! every existing one does rather than reaching for the atomic itself.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::JNIEnv;
use log::{info, warn};

use crate::settings::{DebugSettings, FullscreenMode, Settings};

/// Whether to report what the renderer is doing: pipeline binds, passes, counters, shader dumps.
static DIAGNOSTICS: AtomicBool = AtomicBool::new(false);

/// Whether to write the renderer's diagnostic *log lines*.
///
/// Separate from [`DIAGNOSTICS`], which is the *dump* switch: a run that wants a frame written out
/// should be able to get it without a line a second of counters, and a run that wants the counters
/// should be able to get them without writing frames to disk. Most of the lines this side writes are
/// the native half of one the JVM side writes too, so the two flags are read in pairs.
static LOGGING: AtomicBool = AtomicBool::new(false);

/// Whether a draw may reuse a bind group built for another draw at a different dynamic offset.
static BIND_GROUP_CACHE: AtomicBool = AtomicBool::new(true);

/// Whether uniform bindings carry their offset dynamically instead of baking it into the set.
static DYNAMIC_OFFSETS: AtomicBool = AtomicBool::new(true);

/// Whether the `dynamic offsets` switch has been read yet. See [`set_dynamic_offsets`].
static DYNAMIC_OFFSETS_READ: AtomicBool = AtomicBool::new(false);

/// The `dynamic offsets` switch, which is read **once for the session** and then never again.
///
/// It was a live switch, on the reasoning that turning it off should take effect on the next draw
/// rather than on the next launch. It cannot: what the switch decides is whether a uniform offset
/// travels with the draw or is baked into the bind group it is bound with, and that is part of what
/// the JVM side mints a *number* for - the identity of a draw's bind groups. Two draws of one frame
/// numbered under one policy and drawn under the other disagree about what a bind group is, and the
/// way that surfaces is the process ending: the game died the first time anyone flipped it in a
/// running world.
///
/// So it is latched at the first read - the settings arriving, before anything is drawn - and a later
/// change is reported and ignored. Off is also the *slow* setting, and worth saying out loud when it
/// is taken: a baked offset makes every distinct offset a bind group of its own, which in a world of
/// thousands of draws a frame is thousands of bind groups a frame. See `dynamic_offset` in `blaze.rs`.
fn set_dynamic_offsets(value: bool) {
    if DYNAMIC_OFFSETS_READ.swap(true, Ordering::Relaxed) {
        if DYNAMIC_OFFSETS.load(Ordering::Relaxed) != value {
            warn!(
                "wgpu-mc: the `dynamic offsets` setting cannot be changed while the game is running - \
                 it decides whether a uniform offset travels with the draw or is baked into the bind \
                 group, which is part of how a draw identifies its bind groups. Restart to apply it."
            );
        }

        return;
    }

    set(&DYNAMIC_OFFSETS, value);

    if !value {
        warn!(
            "wgpu-mc: `dynamic offsets` is off: every distinct uniform offset becomes a bind group of \
             its own, which is thousands of them a frame in a world - the feature is measured on, and \
             off is for diagnosis rather than for play"
        );
    }
}

/// Whether every draw's bindings are logged, which is a line per draw.
static TRACE_DYNAMIC_OFFSETS: AtomicBool = AtomicBool::new(false);

/// Whether the verbose log of how binding names were resolved is on.
///
/// The resolution itself happens on the JVM side, which reads this setting through `getSettings`
/// like the options screen does; the flag is kept here so that every debug switch is in one place,
/// and so a future reader of this file sees that the switch exists.
static BINDING_VERBOSITY: AtomicBool = AtomicBool::new(false);

/// Whether the GLSL that reaches the shader compiler is written out.
static DUMP_SHADERS: AtomicBool = AtomicBool::new(false);

/// Whether frame timings are measured with GPU timestamp queries.
static GPU_TIMESTAMPS: AtomicBool = AtomicBool::new(false);

/// Whether a PIX timing capture has been asked for.
static PIX_CAPTURE: AtomicBool = AtomicBool::new(false);

/// Whether the wgpu instance is created with wgpu's own host-side validation.
///
/// The outer of the two validation layers, and the one that decides whether a mistake is *reported* at
/// all: it is wgpu checking, in this process, that every call the renderer records is a legal one. With
/// it off, an illegal call is whatever the driver makes of it - a device loss, a validation error from
/// the driver's own layer if one is loaded, or a frame that is quietly wrong.
///
/// Read once, when the instance is built: an instance flag cannot be changed afterwards, which is why
/// the setting that feeds it is marked as needing a restart.
static HOST_VALIDATION: AtomicBool = AtomicBool::new(false);

/// Whether the wgpu instance is created with the driver's GPU-based validation.
///
/// **The inner layer, and it needs the outer one.** GPU-based validation checks the commands wgpu
/// recorded, so with [`HOST_VALIDATION`] off there is nothing for it to check and wgpu ignores the flag.
/// The two are therefore resolved together in [`validation_flags`] rather than independently - a player
/// who turns this on alone gets host-side validation as well, because that is the only state in which
/// this switch does anything.
///
/// Read once, when the instance is built: an instance flag cannot be changed afterwards, which is
/// why the setting that feeds it is marked as needing a restart.
static GPU_BASED_VALIDATION: AtomicBool = AtomicBool::new(false);

/// Whether the section feed is timed. See [`section_timing`].
static SECTION_TIMING: AtomicBool = AtomicBool::new(false);

/// Whether the graph's pipelines are built with their back faces kept.
///
/// One of the two debug switches whose effect is not a flag read on the draw path: a cull mode is
/// part of the pipeline, so the switch is resolved into `wgpu_mc`'s own static and the graph's
/// pipelines are rebuilt when it moves - see [`rebuild_pipelines_if_stale`].
static TERRAIN_NO_CULL: AtomicBool = AtomicBool::new(false);

/// Whether the graph's pipelines are built with the depth test the other way round. The other half
/// of the pair above.
static TERRAIN_GREATER_DEPTH: AtomicBool = AtomicBool::new(false);

/// Whether shaders and objects are built with debug information - wgpu's `DEBUG` instance flag.
///
/// The flags that are decided when the wgpu instance is created rather than read as something is drawn.
/// They are kept here with the rest of the switches because that is what they are - an options-screen
/// switch with an atomic behind it - and read once, in `device::instance_flags`, for the same reason:
/// an instance flag cannot be changed after the instance exists.
static SHADER_DEBUG_INFO: AtomicBool = AtomicBool::new(true);

/// Whether an indirect draw whose arguments are out of bounds is turned into a no-op.
static VALIDATE_INDIRECT_CALLS: AtomicBool = AtomicBool::new(true);

/// Whether object labels are kept from the backend.
static DISCARD_BACKEND_LABELS: AtomicBool = AtomicBool::new(false);

/// Whether an adapter whose driver is not compliant with the graphics API may be chosen.
static ALLOW_NONCOMPLIANT_ADAPTER: AtomicBool = AtomicBool::new(false);

/// The four instance flags above, as one answer, because they are always wanted together.
///
/// Returned as a struct rather than read one at a time by the caller for the reason
/// [`validation_flags`] is a pair: the flag set is only correct as a whole, and four separate reads are
/// four chances to combine them wrongly.
#[derive(Copy, Clone, Debug)]
pub struct InstanceFlagSettings {
    pub shader_debug_info: bool,
    pub validate_indirect_calls: bool,
    pub discard_backend_labels: bool,
    pub allow_noncompliant_adapter: bool,
}

#[inline]
pub fn instance_flag_settings() -> InstanceFlagSettings {
    InstanceFlagSettings {
        shader_debug_info: SHADER_DEBUG_INFO.load(Ordering::Relaxed),
        validate_indirect_calls: VALIDATE_INDIRECT_CALLS.load(Ordering::Relaxed),
        discard_backend_labels: DISCARD_BACKEND_LABELS.load(Ordering::Relaxed),
        allow_noncompliant_adapter: ALLOW_NONCOMPLIANT_ADAPTER.load(Ordering::Relaxed),
    }
}

/// Whether a pipeline-state switch has moved since the graph was last built. See
/// [`rebuild_pipelines_if_stale`], which is what spends it.
static PIPELINES_STALE: AtomicBool = AtomicBool::new(false);

/// Says the graph has to be built again, for anything that is decided when it is built.
///
/// The two pipeline-state switches above are the usual caller; the other one is the JVM handing over
/// the game's block atlas (`bind_game_block_atlas`), which is a *resource* a pipeline binds rather
/// than a flag it reads - and a graph built before that arrived has the placeholder in that slot,
/// with the terrain pass sampling white texels on the faces that were baked for the game's atlas.
///
/// Nothing is rebuilt here. This is called from a tick, and the graph is replaced at the end of a
/// frame, which is the one point a frame is known to be between: see
/// [`rebuild_pipelines_if_stale`].
pub fn mark_pipelines_stale() {
    PIPELINES_STALE.store(true, Ordering::Relaxed);
}

#[allow(dead_code)]
#[inline]
pub fn diagnostics() -> bool {
    DIAGNOSTICS.load(Ordering::Relaxed)
}

/// Whether this side's diagnostic log lines are turned on. See [`LOGGING`].
#[inline]
pub fn logging() -> bool {
    LOGGING.load(Ordering::Relaxed)
}

#[inline]
pub fn bind_group_cache() -> bool {
    BIND_GROUP_CACHE.load(Ordering::Relaxed)
}

#[inline]
pub fn dynamic_offsets() -> bool {
    DYNAMIC_OFFSETS.load(Ordering::Relaxed)
}

#[inline]
pub fn trace_dynamic_offsets() -> bool {
    TRACE_DYNAMIC_OFFSETS.load(Ordering::Relaxed)
}

#[inline]
pub fn binding_verbosity() -> bool {
    BINDING_VERBOSITY.load(Ordering::Relaxed)
}

#[inline]
pub fn dump_shaders() -> bool {
    DUMP_SHADERS.load(Ordering::Relaxed)
}

#[allow(dead_code)]
#[inline]
pub fn gpu_timestamps() -> bool {
    GPU_TIMESTAMPS.load(Ordering::Relaxed)
}

#[inline]
pub fn pix_capture() -> bool {
    PIX_CAPTURE.load(Ordering::Relaxed)
}

/// Whether the section feed is timed, phase by phase.
///
/// Read on Minecraft's chunk-build threads, once per rebuild, so the switch is what keeps the clock
/// reads out of the path entirely when it is off.
#[allow(dead_code)]
#[inline]
pub fn section_timing() -> bool {
    SECTION_TIMING.load(Ordering::Relaxed)
}

/// The two validation switches, resolved into the pair they actually are.
///
/// **The order between them is not a preference, it is a dependency.** GPU-based validation checks the
/// commands wgpu recorded; it is the driver's layer looking over wgpu's shoulder. With host-side
/// validation off, wgpu records without checking and the driver's layer has nothing to compare
/// against, so wgpu ignores `GPU_BASED_VALIDATION` in that state. Asking for the inner layer therefore
/// turns on the outer one too, and the pair returned here is the only honest reading of "the player
/// asked for validation".
///
/// Returned together rather than as two accessors because a caller that read them separately would be
/// free to combine them wrongly, and one place combining them wrongly is a validation layer that
/// silently does nothing.
#[inline]
pub fn validation_flags() -> (bool, bool) {
    let gpu = GPU_BASED_VALIDATION.load(Ordering::Relaxed);

    (HOST_VALIDATION.load(Ordering::Relaxed) || gpu, gpu)
}

/// Whether the graph's pipelines are built without back-face culling.
///
/// The switch is read where the pipelines are built, in `wgpu_mc`; this is the same answer kept on
/// this side so that the line about rebuilding them can say what they are being rebuilt *with*.
#[inline]
pub fn terrain_no_cull() -> bool {
    TERRAIN_NO_CULL.load(Ordering::Relaxed)
}

/// Whether the graph's pipelines are built with the depth test the other way round.
#[inline]
pub fn terrain_greater_depth() -> bool {
    TERRAIN_GREATER_DEPTH.load(Ordering::Relaxed)
}

/// Resolves every flag from the settings.
///
/// **The settings are the only source.** A marker file next to the game used to be the other one, and
/// every switch here resolved as "the setting, or the marker" - which meant the answer a run took
/// depended on a file that nothing in the game could tell you about, that had no way to be turned off
/// from inside it, and that four of the switches were even spelled as the *negation* of, so the file
/// won over the setting and could not be overridden at all. They are on the options screen now, which
/// is where a switch belongs: it is visible, it is saved, and it is the same place as everything else.
pub fn apply(settings: &Settings) {
    let DebugSettings {
        host_validation,
        gpu_based_validation,
        shader_debug_info,
        validate_indirect_calls,
        discard_backend_labels,
        allow_noncompliant_adapter,
        diagnostics,
        bind_group_cache,
        dynamic_offsets,
        trace_dynamic_offsets,
        binding_verbosity,
        dump_shaders,
        gpu_timestamps,
        pix_capture,
        section_timing,
        logging,
        terrain_no_cull,
        terrain_greater_depth,
        terrain_occlusion,
        adv_culling,
        terrain_indirect,
        atlas_base_mip_only,
        atlas_lod_bias,
        game_atlas_blend_mips,
    } = settings.debug();

    set(&LOGGING, logging);
    set_dynamic_offsets(dynamic_offsets);
    set(&DIAGNOSTICS, diagnostics);
    set(&BIND_GROUP_CACHE, bind_group_cache);
    set(&TRACE_DYNAMIC_OFFSETS, trace_dynamic_offsets);
    set(&BINDING_VERBOSITY, binding_verbosity);
    set(&DUMP_SHADERS, dump_shaders);
    set(&HOST_VALIDATION, host_validation);
    set(&GPU_BASED_VALIDATION, gpu_based_validation);
    set(&SHADER_DEBUG_INFO, shader_debug_info);
    set(&VALIDATE_INDIRECT_CALLS, validate_indirect_calls);
    set(&DISCARD_BACKEND_LABELS, discard_backend_labels);
    set(&ALLOW_NONCOMPLIANT_ADAPTER, allow_noncompliant_adapter);
    // These two are not flags to be read somewhere: they *are* the action, so the switch does
    // something the moment it moves. See the setting docs for why neither is set here.
    set(&GPU_TIMESTAMPS, gpu_timestamps);
    set(&PIX_CAPTURE, pix_capture);
    set(&SECTION_TIMING, section_timing);

    // The two pipeline-state switches are the odd ones out: they are not flags the draw path reads
    // but state built into every pipeline, which is why they are handed to the crate that builds
    // them instead of being kept here. It answers whether either of them moved, and that is the one
    // thing a change to them invalidates - the pipelines already in the graph carry the old answer.
    let no_cull = terrain_no_cull;
    let greater_depth = terrain_greater_depth;

    set(&TERRAIN_NO_CULL, no_cull);
    set(&TERRAIN_GREATER_DEPTH, greater_depth);

    if wgpu_mc::render::graph::set_pipeline_diagnostics(no_cull, greater_depth) {
        // Not rebuilt here: this runs when the settings are handed over, which is not a point a frame
        // is known to be between. The frame's own end is - see [`rebuild_pipelines_if_stale`].
        PIPELINES_STALE.store(true, Ordering::Relaxed);
    }

    // The occlusion switch is not one of those: it decides an `if` inside the gather, so it takes
    // effect on the next frame and nothing has to be rebuilt for it.
    wgpu_mc::render::graph::set_terrain_occlusion(terrain_occlusion);

    // The occlusion walk's budget is the same kind of switch, one step further: it decides *which list*
    // that `if` tests against, so it too takes effect on the next frame with nothing rebuilt.
    wgpu_mc::render::graph::set_adv_culling(adv_culling);
    // **Whether the terrain pass batches its draws, as far as the setting is concerned.**
    //
    // Where the setting and the device are *combined* is not here, and the reason is a measured one:
    // this runs from the mod constructor, before there is a device, so a check made here reads a
    // capability no adapter has answered for yet and answers "no" on every launch. The first run of the
    // batched path did exactly that - `indirect draws: execution yes, real batched multi-draw yes,
    // non-zero first instance yes` on one line and `the terrain pass is drawing one section at a time`
    // on the next. The device half is recorded beside the caps that answer it
    // (`device::try_create_renderer`) and the two are one answer in
    // `wgpu_mc::render::graph::terrain_batches_draws`.
    //
    // Nothing is printed here either, for a second reason: `setPanicHook` has not installed
    // `env_logger` yet, so a line written here is dropped. `device::report_device_capabilities` writes
    // the answer once a world.
    wgpu_mc::render::graph::set_terrain_indirect(terrain_indirect);

    // The atlas mip clamp *is* built into the samplers, which are created with the graph - so moving it
    // invalidates the same thing `terrain_no_cull` does, and for the same reason.
    if wgpu_mc::render::graph::atlas_base_mip_only() != atlas_base_mip_only {
        wgpu_mc::render::graph::set_atlas_base_mip_only(atlas_base_mip_only);
        PIPELINES_STALE.store(true, Ordering::Relaxed);
    }

    // **Whether the game atlas blends between mip levels, which is built into its sampler** - so like the
    // clamp above it invalidates the pipelines rather than taking effect on its own.
    if wgpu_mc::render::atlas::game_atlas_blend_mips() != game_atlas_blend_mips {
        wgpu_mc::render::atlas::set_game_atlas_blend_mips(game_atlas_blend_mips);
        PIPELINES_STALE.store(true, Ordering::Relaxed);
    }

    // **The level-of-detail bias, which is neither of the two above** - not baked, and not built into a
    // sampler. It is written into the per-draw immediate block, so storing it is the whole of applying
    // it and the next frame uses it. That is exactly why it is not a shader constant any more: as a
    // constant it needed the shader copied into the build directory and a restart, and the two readings
    // "it works" and "it never reached the GPU" were the same picture.
    wgpu_mc::render::atlas::set_atlas_lod_bias(atlas_lod_bias);

    // The animated-texture switch, which is a *graphics* setting rather than one of the debug switches
    // above: it decides, per face, which atlas that face samples, and the answer is baked into the
    // vertex. Moving it therefore invalidates every baked block model, and the re-bake is asked for by
    // [`sendSettings`], which is the only caller that knows a setting moved rather than was loaded.
    wgpu_mc::mc::block::set_animated_textures(settings.animated_textures());

    // **The window mode, which is the one setting here that this side does not own the effect of.**
    //
    // `DisplayMode` on the JVM side is what puts the window into a mode - three GLFW calls, and GLFW is
    // reached from there because `Window` is there. So what is recorded here is only the *value*, and the
    // change is reported by [`reapply_window_mode`], which `sendSettings` calls once the settings are
    // stored. Recording it unconditionally is what that function's comparison reads.
    WINDOW_MODE.store(settings.fullscreen_mode() as u8, Ordering::Relaxed);

    // The `wgpu-mc` crate writes lines of its own - the per-bake report, for one - and the switch
    // that decides whether they are sampled or written is the same one this file just resolved.
    wgpu_mc::mc::chunk::DIAGNOSTIC_LOGGING
        .store(LOGGING.load(Ordering::Relaxed), Ordering::Relaxed);

    crate::timing::set_enabled(gpu_timestamps);
    crate::pix::set_capturing(pix_capture);
}

/// **Tells the JVM to put the window into the mode the setting now names**, if it moved.
///
/// The window is the JVM's - GLFW is reached from there because `Window` is there, and the three modes
/// are three GLFW calls - so this side cannot apply it. What it *can* do is notice that it changed and
/// say so, which is what this is: the comparison is here rather than on the JVM because the settings
/// document is here.
///
/// **Called from `sendSettings` after the settings are stored**, so the value the JVM reads back through
/// `RendererSettings` is the new one. That is why nothing is passed: one source of truth for which mode
/// it is, and this call is only the moment to look it up.
///
/// A window that is already in the mode, or a settings apply that touched nothing, is a no-op - there is
/// a GLFW call behind this that switches the display mode, and running it on every Apply would flicker
/// the screen for a player who moved only the vsync switch.
pub fn reapply_window_mode(env: &mut JNIEnv) {
    let mode = WINDOW_MODE.load(Ordering::Relaxed);

    let moved = PREVIOUS_WINDOW_MODE.swap(mode, Ordering::Relaxed) != mode;

    crate::set_window_mode(mode, moved);

    if !moved {
        return;
    }

    info!("wgpu-mc: the window mode is now {mode:?}; asking the JVM to put the window in it");

    if let Err(err) = crate::call_static_from_class_loader(
        env,
        "dev.birb.wgpu.backend.DisplayMode",
        "reapply",
        "()V",
        &[],
    ) {
        warn!(
            "wgpu-mc: the window mode moved, but the JVM could not be asked to apply it, so the window \
             keeps the mode it has until the next launch: {err}"
        );
    }
}

/// The window mode [`apply`] last resolved from the settings.
///
/// Here rather than read back out of `SETTINGS` in [`reapply_window_mode`], because that function runs
/// after the settings have been *stored* and what it needs to compare against is what the last apply
/// resolved - the two are the same value in every ordinary sequence and saying so once is cheaper than
/// reasoning about the order.
static WINDOW_MODE: std::sync::atomic::AtomicU8 =
    std::sync::atomic::AtomicU8::new(FullscreenMode::Exclusive as u8);

/// The window mode the last apply ended with, for [`reapply_window_mode`]'s comparison.
///
/// A `u8` set to a value no variant has (`0xff`), so the first apply always counts as a move: the window
/// is created by the game before this side has read a config, so its mode at that point is whatever
/// `options.txt` asked for and putting it into the configured one is a real change.
static PREVIOUS_WINDOW_MODE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0xff);

/// Rebuilds the render graph if a pipeline-state switch has moved since it was last built.
///
/// Every other switch here is read *as* the frame is drawn, so setting the flag is the whole of
/// applying it. These two are built into a pipeline when it is created, so the ones already in the
/// graph keep the answer they were built with until the graph is built again - and a resource reload
/// is not something a player should have to go and trigger.
///
/// Called once a frame, with the frame behind it - see `present_surface` - because that is the one
/// point a frame is known to be finished with the graph: a graph replaced between two of a frame's
/// passes would draw half of that frame one way and half the other. Nothing happens on the frames
/// where the switches have not moved, which is all of them but the one after an Apply.
pub fn rebuild_pipelines_if_stale(wm: &wgpu_mc::WmRenderer) {
    // **The atlas mode is a second reason to rebuild, and it is not a switch.** `terrain.wgsl` has a
    // fragment entry point that samples one atlas and one that picks between two, and the graph picks
    // between them from what the bake actually did. A face that falls back to this side's copy moves that
    // answer, and the graph has to be built again with the two-atlas shader before the face can be drawn.
    //
    // That it is safe to do this *here* rather than where the flag is set is the arena's own ordering, and
    // it is worth writing down: the faces are fed into the arena at the blit, which is after the frame's
    // terrain passes have been recorded, so a face baked during frame N is first drawn in frame N+1 - and
    // this runs at frame N's present, in that gap. See `block::note_our_atlas_face`.
    let atlas_moved = wgpu_mc::mc::block::take_atlas_mode_moved();

    if !PIPELINES_STALE.swap(false, Ordering::Relaxed) && !atlas_moved {
        return;
    }

    log::info!(
        "wgpu-mc: rebuilding the render graph (no-cull {}, greater depth {}, game block atlas {}{})",
        terrain_no_cull(),
        terrain_greater_depth(),
        if wgpu_mc::render::graph::game_atlas_bound() {
            "bound"
        } else {
            "not bound yet"
        },
        if atlas_moved {
            ", atlas mode moved"
        } else {
            ""
        }
    );

    crate::application::load_shaders(wm);
}

fn set(flag: &AtomicBool, value: bool) {
    if flag.swap(value, Ordering::Relaxed) != value {
        log::info!(
            "wgpu-mc: {} is now {}",
            name(flag),
            if value { "on" } else { "off" }
        );
    }
}

/// A flag's name for the log, which is the one its setting has.
fn name(flag: &AtomicBool) -> &'static str {
    match flag {
        f if std::ptr::eq(f, &LOGGING) => "logging",
        f if std::ptr::eq(f, &DIAGNOSTICS) => "diagnostics",
        f if std::ptr::eq(f, &BIND_GROUP_CACHE) => "bind group cache",
        f if std::ptr::eq(f, &DYNAMIC_OFFSETS) => "dynamic offsets",
        f if std::ptr::eq(f, &TRACE_DYNAMIC_OFFSETS) => "trace dynamic offsets",
        f if std::ptr::eq(f, &DUMP_SHADERS) => "dump shaders",
        f if std::ptr::eq(f, &GPU_TIMESTAMPS) => "gpu timestamps",
        f if std::ptr::eq(f, &PIX_CAPTURE) => "pix capture",
        f if std::ptr::eq(f, &SECTION_TIMING) => "section timing",
        f if std::ptr::eq(f, &TERRAIN_NO_CULL) => "terrain no-cull",
        f if std::ptr::eq(f, &TERRAIN_GREATER_DEPTH) => "terrain greater depth",
        f if std::ptr::eq(f, &HOST_VALIDATION) => "host validation",
        f if std::ptr::eq(f, &SHADER_DEBUG_INFO) => "shader debug info",
        f if std::ptr::eq(f, &VALIDATE_INDIRECT_CALLS) => "validate indirect calls",
        f if std::ptr::eq(f, &DISCARD_BACKEND_LABELS) => "discard backend labels",
        f if std::ptr::eq(f, &ALLOW_NONCOMPLIANT_ADAPTER) => "allow noncompliant adapter",
        _ => "gpu based validation",
    }
}

#[cfg(test)]
mod validation_tests {
    use super::*;

    /// **The inner layer needs the outer one, and asking for it says so.**
    ///
    /// GPU-based validation checks the commands wgpu recorded, so with host-side validation off wgpu
    /// ignores the flag. A pair that reported `(false, true)` would be a player who turned on the
    /// driver's layer and got nothing at all - no error, no warning, just validation that is not
    /// running. So the outer flag is implied rather than required of the player.
    #[test]
    fn asking_for_the_drivers_layer_turns_on_the_hosts() {
        for (host, gpu, want_host, want_gpu) in [
            // host, gpu, expected host, expected gpu
            (false, false, false, false),
            (true, false, true, false),
            // The one that matters: the driver's layer alone.
            (false, true, true, true),
            (true, true, true, true),
        ] {
            HOST_VALIDATION.store(host, Ordering::Relaxed);
            GPU_BASED_VALIDATION.store(gpu, Ordering::Relaxed);

            let (got_host, got_gpu) = validation_flags();

            // The GPU flag is passed through exactly as asked, and never invented.
            assert_eq!(got_gpu, want_gpu, "gpu based validation is passed through");
            assert_eq!(
                got_host, want_host,
                "host validation is what was asked for, or what gpu based validation needs - \
                 host={host} gpu={gpu}"
            );

            assert!(
                !got_gpu || got_host,
                "no combination may report the inner layer without the outer one: with host={host} \
                 and gpu={gpu} the driver's layer would be on with nothing to check"
            );
        }

        // Left off, which is the state the process starts in and the state other tests read.
        HOST_VALIDATION.store(false, Ordering::Relaxed);
        GPU_BASED_VALIDATION.store(false, Ordering::Relaxed);
    }
}
