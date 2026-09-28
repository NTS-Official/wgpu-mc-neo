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

use log::warn;

use crate::settings::{DebugSettings, Settings};

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

/// Whether the wgpu instance is created with the driver's GPU-based validation.
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

#[inline]
pub fn gpu_based_validation() -> bool {
    GPU_BASED_VALIDATION.load(Ordering::Relaxed)
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
        gpu_based_validation,
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
        atlas_base_mip_only,
    } = settings.debug();

    set(&LOGGING, logging);
    set_dynamic_offsets(dynamic_offsets);
    set(&DIAGNOSTICS, diagnostics);
    set(&BIND_GROUP_CACHE, bind_group_cache);
    set(&TRACE_DYNAMIC_OFFSETS, trace_dynamic_offsets);
    set(&BINDING_VERBOSITY, binding_verbosity);
    set(&DUMP_SHADERS, dump_shaders);
    set(&GPU_BASED_VALIDATION, gpu_based_validation);
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

    // The atlas mip clamp *is* built into the samplers, which are created with the graph - so moving it
    // invalidates the same thing `terrain_no_cull` does, and for the same reason.
    if wgpu_mc::render::graph::atlas_base_mip_only() != atlas_base_mip_only {
        wgpu_mc::render::graph::set_atlas_base_mip_only(atlas_base_mip_only);
        PIPELINES_STALE.store(true, Ordering::Relaxed);
    }

    // The animated-texture switch, which is a *graphics* setting rather than one of the debug switches
    // above: it decides, per face, which atlas that face samples, and the answer is baked into the
    // vertex. Moving it therefore invalidates every baked block model, and the re-bake is asked for by
    // [`sendSettings`], which is the only caller that knows a setting moved rather than was loaded.
    wgpu_mc::mc::block::set_animated_textures(settings.animated_textures());

    // The `wgpu-mc` crate writes lines of its own - the per-bake report, for one - and the switch
    // that decides whether they are sampled or written is the same one this file just resolved.
    wgpu_mc::mc::chunk::DIAGNOSTIC_LOGGING
        .store(LOGGING.load(Ordering::Relaxed), Ordering::Relaxed);

    crate::timing::set_enabled(gpu_timestamps);
    crate::pix::set_capturing(pix_capture);
}

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
    if !PIPELINES_STALE.swap(false, Ordering::Relaxed) {
        return;
    }

    log::info!(
        "wgpu-mc: rebuilding the render graph (no-cull {}, greater depth {}, game block atlas {})",
        terrain_no_cull(),
        terrain_greater_depth(),
        if wgpu_mc::render::graph::game_atlas_bound() {
            "bound"
        } else {
            "not bound yet"
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
        _ => "gpu based validation",
    }
}
