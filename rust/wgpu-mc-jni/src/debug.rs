//! The debug switches, and the marker files they replace.
//!
//! Every diagnostic this renderer has grew as a file in the run directory - `wgpu-dump-frames`,
//! `wgpu-no-bind-group-cache`, `wgpu-trace-dynamic-offsets`, `wgpu-dump-shaders` - because a file
//! needs no launcher support and no rebuild. They are options on the options screen now, and the
//! markers keep working: a flag is on when the setting is on *or* the marker exists, so a run that
//! was started with a marker behaves as it did. The two that are the wrong way round (a marker that
//! turns something *off*) are handled where they are resolved.
//!
//! The flags live in atomics rather than being read from the settings on use. They are read on the
//! draw path - once per draw for `diagnostics`, once per pipeline bind for the rest - and the
//! settings are behind a lock. [`apply`] is what moves a setting into the atomics: it runs whenever
//! the settings are loaded or sent, which is startup and every Apply on the options screen.
//!
//! Two of them are not read on the draw path at all. `terrain_no_cull` and `terrain_greater_depth`
//! are state built into a pipeline when it is created, so they are handed to the crate that creates
//! them and the graph is built again when either moves - see [`rebuild_pipelines_if_stale`].

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

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

/// Resolves every flag from the settings and the marker files.
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
    } = settings.debug();

    set(&LOGGING, logging || marker("wgpu-logging"));
    set(&DIAGNOSTICS, diagnostics || marker("wgpu-dump-frames"));
    // These two markers are spelled as the *off* switch, so the file wins over the setting.
    set(
        &BIND_GROUP_CACHE,
        bind_group_cache && !marker("wgpu-no-bind-group-cache"),
    );
    set(
        &DYNAMIC_OFFSETS,
        dynamic_offsets && !marker("wgpu-no-dynamic-offsets"),
    );
    set(
        &TRACE_DYNAMIC_OFFSETS,
        trace_dynamic_offsets || marker("wgpu-trace-dynamic-offsets"),
    );
    set(
        &BINDING_VERBOSITY,
        binding_verbosity || marker("wgpu-binding-log"),
    );
    set(&DUMP_SHADERS, dump_shaders || marker("wgpu-dump-shaders"));
    set(&GPU_BASED_VALIDATION, gpu_based_validation);

    // These two are not flags to be read somewhere: they *are* the action, so the switch does
    // something the moment it moves. Both are gated on the setting alone - a marker file has no way
    // to end a capture, and a capture that never ends is worse than none.
    set(&GPU_TIMESTAMPS, gpu_timestamps);
    set(&PIX_CAPTURE, pix_capture);
    set(&SECTION_TIMING, section_timing || marker("wgpu-section-timing"));

    // The two pipeline-state switches are the odd ones out: they are not flags the draw path reads
    // but state built into every pipeline, which is why they are handed to the crate that builds
    // them instead of being kept here. It answers whether either of them moved, and that is the one
    // thing a change to them invalidates - the pipelines already in the graph carry the old answer.
    let no_cull = terrain_no_cull || marker("wgpu-terrain-no-cull");
    let greater_depth = terrain_greater_depth || marker("wgpu-terrain-greater-depth");

    set(&TERRAIN_NO_CULL, no_cull);
    set(&TERRAIN_GREATER_DEPTH, greater_depth);

    if wgpu_mc::render::graph::set_pipeline_diagnostics(no_cull, greater_depth) {
        // Not rebuilt here: this runs when the settings are handed over, which is not a point a frame
        // is known to be between. The frame's own end is - see [`rebuild_pipelines_if_stale`].
        PIPELINES_STALE.store(true, Ordering::Relaxed);
    }

    // The `wgpu-mc` crate writes lines of its own - the per-bake report, for one - and the switch
    // that decides whether they are sampled or written is the same one this file just resolved.
    wgpu_mc::mc::chunk::DIAGNOSTIC_LOGGING.store(LOGGING.load(Ordering::Relaxed), Ordering::Relaxed);

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
        "wgpu-mc: rebuilding the render graph for a pipeline-state switch (no-cull {}, greater depth \
         {})",
        terrain_no_cull(),
        terrain_greater_depth()
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

/// Whether a marker file exists, asked once per process.
///
/// A marker is a file dropped next to the mod, and it used to be read on every draw for the cache
/// and the offset switches. It cannot appear or disappear meaningfully while the game is running -
/// the settings are the live switch now - so it is resolved the first time it is asked for, which
/// is when the settings are applied.
fn marker(file_name: &'static str) -> bool {
    static MARKERS: OnceLock<std::collections::HashSet<&'static str>> = OnceLock::new();

    MARKERS
        .get_or_init(|| {
            const NAMES: [&str; 10] = [
                "wgpu-dump-frames",
                "wgpu-no-bind-group-cache",
                "wgpu-no-dynamic-offsets",
                "wgpu-trace-dynamic-offsets",
                "wgpu-dump-shaders",
                "wgpu-binding-log",
                "wgpu-logging",
                "wgpu-section-timing",
                "wgpu-terrain-no-cull",
                "wgpu-terrain-greater-depth",
            ];

            let mut present = std::collections::HashSet::new();

            for name in NAMES {
                if Path::new(name).exists() || run_directory_marker(name) {
                    present.insert(name);
                }
            }

            present
        })
        .contains(file_name)
}

/// The same marker, next to the game directory rather than in the process's working directory.
///
/// The two are the same directory in a normal launch, and the run directory is the one that is
/// still right when the game is started from somewhere else. It is only known once the JVM has
/// sent it, so both are checked.
fn run_directory_marker(file_name: &str) -> bool {
    crate::RUN_DIRECTORY
        .get()
        .map(|directory| directory.join(file_name).exists())
        .unwrap_or(false)
}
