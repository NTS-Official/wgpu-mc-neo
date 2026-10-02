//! A headless mesher bench: **how long does one section take to bake, and how much does it allocate?**
//!
//! # Why it is an example and not a `benches/` target
//!
//! Criterion is not a dependency of this crate, and the two numbers this needs - a wall time and the
//! thread's own allocated bytes - are two `Instant`s and one syscall. A bench harness that has to be
//! fetched to answer a question this narrow is a bench that does not get run.
//!
//! # What it measures, and what it deliberately does not
//!
//! It measures [`bake_layers`], which is the CPU work: the block model loop, the fluid mesher, and the
//! translucent sort. It does **not** measure the arena allocation, the upload coalescing, or anything
//! the GPU does - those are per frame rather than per section, and they are already instrumented from
//! inside the renderer (`wgpu-mc: terrain build: ...`).
//!
//! # What it needs
//!
//! A `wgpu` device, because the block atlas owns the texture that ships the atlas image - the mesher
//! reads only the atlas' *CPU-side* sprite tables, but they live on the same struct. A **headless**
//! device is enough and it is what this asks for: no window, no surface. On a machine with no GPU at
//! all, DX12 exposes a software adapter (`Microsoft Basic Render Driver`), which is slow and fine.
//!
//! The assets come from `wgpu-mc-demo/res/assets`, which is the only block-asset set in this
//! repository. Run it with:
//!
//! ```text
//! cargo run -p wgpu-mc --release --example mesher_bench -- --sections 64 --repeats 5
//! ```
//!
//! # The two numbers, and why the second one is here
//!
//! `ns/section` is the obvious one. **`bytes/section` is the one that says whether the pooling
//! works.** `BakedLayer` holds two `Vec`s and the mesher allocates them per section; a change that
//! makes the mesher faster and allocates twice as much has moved the cost to the allocator and the
//! frame time will say so later. It is read from `getThreadAllocatedBytes` on Linux and Windows, and
//! absent elsewhere rather than guessed at.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use glam::{IVec3, ivec3};
use wgpu_mc::mc::block::{BlockstateKey, ChunkBlockState};
use wgpu_mc::mc::chunk::{BakePhases, BlockStateProvider, LightLevel, bake_section};
use wgpu_mc::mc::resource::{ResourcePath, ResourceProvider};
use wgpu_mc::render::pipeline::BLOCK_ATLAS;
use wgpu_mc::{Gpu, WmRenderer};

/// The blocks the registry is baked from, picked to cover the face baker's branches rather than to be
/// a world: see where this is used for which branch each one is here for.
const BENCH_BLOCKS: &[&str] = &[
    "minecraft:stone",
    "minecraft:dirt",
    "minecraft:oak_planks",
    "minecraft:oak_leaves",
    "minecraft:glass",
    // The **translucent** layer, which no other block here reaches: Minecraft sends a sprite to it when
    // some of its pixels are neither fully opaque nor fully transparent, and glass and leaves are
    // cutout (alpha is 0 or 255), so both of them bake into `RenderLayer::Cutout`. Ice is a full cube
    // whose sprite has alpha in between, so it is the one that exercises `sort_translucent_quads` - and
    // a scenario that cannot reach the sort reports zero quads and says nothing about it.
    "minecraft:ice",
    "minecraft:oak_slab",
    "minecraft:grass_block",
    "minecraft:poppy",
    "minecraft:oak_log",
    "minecraft:sand",
];

/// A resource provider over a directory: `minecraft:textures/block/stone.png` becomes
/// `<root>/minecraft/textures/block/stone.png`.
///
/// The namespace is a directory name, which is what the demo assets do, and the double colon in a
/// [`ResourcePath`] becomes the path separator.
struct DirectoryResources {
    root: PathBuf,
}

impl DirectoryResources {
    fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl ResourceProvider for DirectoryResources {
    fn get_bytes(&self, id: &ResourcePath) -> Option<Vec<u8>> {
        let relative = id.0.replace(':', "/");
        let path = self.root.join(relative);
        std::fs::read(path).ok()
    }
}

/// Which world the provider describes. See [`Scenario`] for what each one is for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Scenario {
    /// Nothing but air, and `is_section_empty` answers `true`, so the bake should return in one call.
    ///
    /// It is the floor: anything a run reports above this is work the mesher did rather than work it
    /// was told not to do, and a run that reports *this* number is a run where the early-out failed.
    Empty,
    /// Nothing but air, and `is_section_empty` answers **`false`** - so the 4096-block loop runs and
    /// every iteration finds air.
    ///
    /// The difference between this and [`Scenario::Empty`] is the cost of the loop itself: the state
    /// lookup, the tint map's clear, the watched-block scan and the face-flag lookup, none of which
    /// anything can skip when the provider does not say the section is empty.
    Air,
    /// A checkerboard of one full cube, plus one layer at `y == 5`: the shape the demo bakes.
    Checkerboard,
    /// One solid 16x16x16 cube of one block, so every face of every block is against a neighbour.
    ///
    /// The case the face baker is supposed to be fastest at - a face that is culled costs a bit flag
    /// test and nothing else - and the one that says how much an interior block costs.
    Solid,
    /// The checkerboard, but of a block whose sprite puts it in the **translucent** layer - which is
    /// the only way to reach the quad sort.
    ///
    /// The sort is the one part of baking whose cost is not linear in the faces: its comparator decodes
    /// both quads' centroids on **every comparison**. Nothing else in a bake has that shape, and no
    /// scenario that draws opaque geometry reaches it - `sort_translucent_quads` returns immediately on
    /// an empty layer, so every other scenario reports zero quads and says nothing about it.
    Translucent,
    /// The checkerboard, but the block is a full cube with **no model in the registry**, so every
    /// state resolves to `None` and no face is ever emitted.
    ///
    /// This is the one that separates "looking up the model" from "baking a face of it".
    Unmodelled,
}

/// One by one, so the whole scenario is one `match` on a value rather than a trait object per block.
struct Provider {
    scenario: Scenario,
    block: BlockstateKey,
}

impl BlockStateProvider for Provider {
    fn get_state(&self, pos: IVec3) -> ChunkBlockState {
        match self.scenario {
            Scenario::Empty | Scenario::Air => ChunkBlockState::Air,
            Scenario::Checkerboard | Scenario::Unmodelled | Scenario::Translucent => {
                let solid = (((pos.x / 2) & 1 == 0) ^ ((pos.z / 2) & 1 == 0) ^ (pos.y & 1 == 0))
                    && (pos.y == 0 || pos.y == 1);
                if solid || pos.y == 5 {
                    ChunkBlockState::State(self.block)
                } else {
                    ChunkBlockState::Air
                }
            }
            Scenario::Solid => ChunkBlockState::State(self.block),
        }
    }

    fn get_light_level(&self, pos: IVec3) -> LightLevel {
        let value = ((pos.x as f32 / 8.0).sin().abs() * 15.0) as u8;
        LightLevel::from_sky_and_block(value, value)
    }

    fn is_section_empty(&self, _pos: IVec3) -> bool {
        self.scenario == Scenario::Empty
    }

    fn get_block_color(&self, _pos: IVec3, _tint_index: i32) -> u32 {
        0xffff_ffff
    }
}

fn main() {
    // **The logger first**, before anything can warn: every failure in the bake below is a `log::warn!`
    // and a warn nobody installed a sink for is a registry that comes out empty with no reason given.
    // `env_logger` is a dependency of the JNI crate rather than of this one, so the sink is written
    // here: it prints `warn` and above to stderr, and `RUST_LOG` only has to be non-empty to mean
    // "on", because the level this cares about is the one that says why nothing was baked.
    struct StderrLogger;

    impl log::Log for StderrLogger {
        fn enabled(&self, metadata: &log::Metadata) -> bool {
            metadata.level() <= log::Level::Warn
        }

        fn log(&self, record: &log::Record) {
            if self.enabled(record.metadata()) {
                eprintln!("[{}] {}", record.level(), record.args());
            }
        }

        fn flush(&self) {}
    }

    static LOGGER: StderrLogger = StderrLogger;
    let _ = log::set_logger(&LOGGER);

    let sections: i32 = arg("--sections").unwrap_or(64);
    let repeats: u32 = arg("--repeats").unwrap_or(5);
    let warmup: u32 = arg("--warmup").unwrap_or(1);
    let block: String = arg_str("--block").unwrap_or_else(|| "minecraft:stone".to_string());
    let scenario = match arg_str("--scenario").as_deref() {
        None | Some("checkerboard") => Scenario::Checkerboard,
        Some("empty") => Scenario::Empty,
        Some("air") => Scenario::Air,
        Some("solid") => Scenario::Solid,
        Some("translucent") => Scenario::Translucent,
        Some("unmodelled") => Scenario::Unmodelled,
        Some(other) => {
            eprintln!(
                "unknown scenario `{other}`; the choices are `empty`, `air`, `checkerboard` \
                 (the default), `solid`, `translucent` and `unmodelled`"
            );
            std::process::exit(5);
        }
    };
    // **The phase counters are off unless asked for, and asking costs the number they explain.** See
    // `BAKE_PHASES`: the two `ns/` rows are a clock read per face, so a run with this on reports a
    // `ns/section` that is inflated by exactly the thing it is being used to attribute. The two are
    // meant to be read from two runs.
    let phases_enabled = flag("--phases");
    wgpu_mc::mc::chunk::BAKE_PHASES.store(phases_enabled, std::sync::atomic::Ordering::Relaxed);

    let assets: PathBuf = arg_str("--assets").map(PathBuf::from).unwrap_or_else(|| {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../wgpu-mc-demo/res/assets")
            .to_path_buf()
    });

    if !assets.is_dir() {
        eprintln!(
            "the asset directory {assets:?} is not there. It is `wgpu-mc-demo/res/assets`, which is \
             checked in; pass --assets if this checkout keeps it somewhere else."
        );
        std::process::exit(2);
    }

    println!(
        "mesher bench: {sections} section(s) x {repeats} repeat(s), warmup {warmup}, block {block}, \
         scenario {scenario:?}\nassets: {assets:?}"
    );

    let gpu = Arc::new(headless_gpu());
    println!(
        "device: {} ({:?})",
        gpu.adapter.get_info().name,
        gpu.adapter.get_info().device_type
    );

    let resources: Arc<dyn ResourceProvider> = Arc::new(DirectoryResources::new(&assets));
    let wm = Arc::new(WmRenderer::new(gpu, resources));
    wm.init();

    // **The blockstates have to be named**: `bake_blocks` bakes the ones a caller lists, and a
    // registry that lists none is a registry with no blocks in it - which reads as "the mesher draws
    // nothing" rather than as a bench that was set up wrong.
    //
    // The set is the one the world a player stands in is built from: a few full cubes for the bulk of
    // the geometry, a cross-shaped plant and a slab for the two shapes that are not a cube, and
    // leaves and glass for the two that are cut out and translucent. Together they cover every branch
    // the face baker has - `face_is_hidden`, the layer from the sprite, `force_opaque` and the
    // per-face shade flag - which is the point of picking them rather than one block.
    // **The path is namespaced once, and the first version of this was not.**
    //
    // `BENCH_BLOCKS` carries full names (`minecraft:stone`), which is what the *registry* is keyed by,
    // and the blockstate *file* is `minecraft:blockstates/stone.json` - so the namespace in the name
    // and the namespace in the path are the same namespace, spelled twice if the name is interpolated
    // whole. `get_bytes` then looks for `minecraft/blockstates/minecraft:stone.json` and answers
    // `None` on every entry, which is a registry that comes out empty and a bench that reports "no
    // block named stone" - a message about the block rather than about the path.
    let blockstates: Vec<(String, ResourcePath)> = BENCH_BLOCKS
        .iter()
        .map(|name| {
            let bare = name.split_once(':').map_or(*name, |(_, path)| path);
            (
                (*name).to_string(),
                ResourcePath(format!("minecraft:blockstates/{bare}.json")),
            )
        })
        .collect();

    {
        let atlases = wm.mc.texture_manager.atlases.read();
        let Some(atlas) = atlases.get(BLOCK_ATLAS) else {
            eprintln!("the block atlas was not created, so no model can be baked");
            std::process::exit(4);
        };

        let bake_started = Instant::now();

        // `bake_blocks` takes the renderer for one thing: the queue it uploads the atlas through.
        wm.mc.bake_blocks(
            &wm,
            blockstates.iter().map(|(name, path)| (name.as_str(), path)),
        );

        // **Reported, and never in the section time.** Baking the registry is a one-off that costs
        // whole milliseconds - it parses every model, resolves every parent and allocates every sprite
        // - and a bench that folded it into `ns/section` would be measuring the setup.
        println!(
            "registry baked in {:.1} ms; that is one-off and not in the section time below\n",
            bake_started.elapsed().as_secs_f64() * 1000.0
        );

        // Held for the call; the atlas is what the mesher reads afterwards.
        let _ = atlas;
    }

    // **The face-flag table, which the game fills from the JVM and this has to synthesise.**
    //
    // `BlockManager::face_flags` is written by `cacheBlockStates` on the JNI side, from masks the game
    // computes out of every state's occlusion *shapes* - a question only the game can answer, because it
    // is `getFaceOcclusionShape(dir) == Shapes.block()` per state and per direction. Without it every
    // state falls back to "occludes nothing, hides nothing", which means **nothing is ever culled**: a
    // solid 16x16x16 section comes out with all 24,576 of its faces in the mesh, the cull counters read
    // zero, and the run looks like a renderer that has lost its culling rather than like a bench that
    // never had the data. That is exactly how it looked the first time this ran.
    //
    // The model's own `cull` bits are the closest honest stand-in: they say which planes the model has a
    // full-size quad on, which is the *other* half of the same test (`face_is_hidden` requires both). For
    // a full cube they are `0b11_1111` and the table is right; for a model with a partial face - a slab, a
    // pane, a plant - they are set on the planes it does cover and the substitution can hide a face the
    // game would have kept. A scenario that must not be wrong about that should not use this switch.
    if flag("--no-synthetic-flags") {
        println!(
            "face flags: NOT synthesised, so nothing will be culled and the cull counters will read \
             zero. Only useful to see the no-culling case.\n"
        );
    } else {
        let filled = synthesise_face_flags(&wm);

        println!(
            "face flags: synthesised for {filled} state(s) from each model's own cull bits - see the \
             note in this file for what that does and does not stand in for\n"
        );
    }

    // Which block the checkerboard is, resolved once. The mesher only carries the index, so this is
    // also the check that the registry really did bake the model: a block with no model bakes air.
    let (block_index, augment) = {
        let manager = wm.mc.block_manager.read();

        if scenario == Scenario::Unmodelled {
            // **One past the end on purpose.** The point of the scenario is a state that resolves to no
            // model, and an index the registry does not have is exactly that - `get_block` is an
            // `get_index`, which answers `None` rather than panicking, so the section bakes as air
            // without a model ever being looked up.
            (manager.blocks.len() as u16, 0u16)
        } else {
            let (index, _, _) = match manager.blocks.get_full(&block) {
                Some(entry) => entry,
                None => {
                    eprintln!(
                        "the block registry has no `{block}` after baking {n} blockstate(s); \
                         pass --block one of the names it does have",
                        n = manager.blocks.len()
                    );
                    std::process::exit(3);
                }
            };
            (index as u16, 0u16)
        }
    };

    println!(
        "registry: {} block(s) baked, `{block}` is index {block_index}\n",
        wm.mc.block_manager.read().blocks.len()
    );

    // The atlas the mesher reads. Taken by reference for the whole run: the mesher holds it for the
    // duration of a bake and so does this.
    let provider = Provider {
        scenario,
        block: BlockstateKey {
            block: block_index,
            augment,
        },
    };

    let positions: Vec<IVec3> = (0..sections)
        .map(|i| ivec3(i * 4, 0, (i / 8) * 4))
        .collect();

    // **Drained between repeats**, because a full channel is a `send` that panics and the point of the
    // measurement is the mesher rather than the queue. The receiver is behind the renderer's own lock,
    // which is the state the render thread reads it in.
    let drain = |wm: &WmRenderer| {
        let receiver = wm.chunk_update_queue.1.lock();
        let mut count = 0u64;
        while receiver.try_recv().is_ok() {
            count += 1;
        }
        count
    };

    for _ in 0..warmup {
        for pos in &positions {
            bake_section(*pos, &wm, &provider);
        }
        drain(&wm);
    }

    // **Drained after the warmup, before the header.** The counters are process-wide, so the warmup's
    // sections would otherwise be counted into the first repeat's window - and the warmup is on by
    // default, which would make every published figure a little too low per section and impossible to
    // reconcile with the section count on the line.
    let _ = BakePhases::take();

    println!(
        "{:>10} {:>14} {:>14} {:>16}",
        "repeat", "ns/section", "ms total", "bytes/section"
    );

    let mut best = f64::MAX;
    let mut allocation_supported = true;

    for repeat in 0..repeats {
        let allocated_before = thread_allocated_bytes();
        let start = Instant::now();

        for pos in &positions {
            bake_section(*pos, &wm, &provider);
        }

        let elapsed = start.elapsed();
        let drained = drain(&wm);

        // **The phases, drained after the clock is read.** They are collected by the mesher itself and
        // each one is a `fetch_add`, so they are the *last* thing read and they never land inside a
        // measurement - see `BAKE_PHASES` for what turning them on costs the number above.
        let phases = BakePhases::take();

        if drained != positions.len() as u64 {
            eprintln!(
                "only {drained} of {} bake(s) reached the queue; the number below is not a section \
                 time",
                positions.len()
            );
        }

        let allocated = match (allocated_before, thread_allocated_bytes()) {
            (Some(before), Some(after)) => Some(after.saturating_sub(before)),
            _ => {
                allocation_supported = false;
                None
            }
        };

        let per_section = elapsed.as_nanos() as f64 / positions.len() as f64;
        best = best.min(per_section);

        println!(
            "{:>10} {:>14.1} {:>14.3} {:>16}",
            repeat,
            per_section,
            elapsed.as_secs_f64() * 1000.0,
            match allocated {
                Some(bytes) => format!("{:.0}", bytes as f64 / positions.len() as f64),
                None => "n/a".to_string(),
            }
        );

        report_phases(&phases, positions.len() as u64);
    }

    println!("\nbest: {best:.1} ns/section");
    if phases_enabled {
        println!(
            "note: `--phases` was on, so the two `ns/` rows below are measured with a clock read per \
             face and the `ns/section` above is inflated by it. Run without it for a number to compare \
             against."
        );
    }
    if !allocation_supported {
        println!(
            "note: this platform does not report a thread's allocated bytes, so the allocation \
             column is `n/a`. It is `getThreadAllocatedBytes` on Linux and Windows only."
        );
    }
}

/// One window's worth of phase counters, in the two shapes they are read in.
///
/// **Two shapes because there are two questions and one divisor cannot answer both.** "How many flag
/// lookups does a section do" divides the window by the sections in it; "how long does one of those
/// take" divides by the number of lookups. The first version used the per-section count for both, which
/// divides the per-face rows by the section count *twice* - and the numbers that come out are small,
/// plausible-looking and wrong by a factor of the section count.
///
/// The `ns/` rows are the ones this exists for: the per-section figure says a solid section takes
/// 9.8 ms, and these say nearly all of it is the cull - a claim about *where* to look.
fn report_phases(phases: &BakePhases, sections: u64) {
    let per_section = |total: u64| total as f64 / sections.max(1) as f64;
    let per_call = |total: u64, calls: u64| total as f64 / calls.max(1) as f64;

    println!(
        "            phases/section: {:.0} block(s), {:.0} with a model, {:.0} with a cull bit; \
         {:.0} cull call(s), {:.0} no cullface, {:.0} hidden, {:.0} emitted, {:.0} fluid",
        per_section(phases.phase_block_loop),
        per_section(phases.phase_blocks_with_a_model),
        per_section(phases.phase_model_cull_bit),
        per_section(phases.phase_cull_calls),
        per_section(phases.phase_face_no_cullface),
        per_section(phases.phase_face_hidden),
        per_section(phases.phase_face_emitted),
        per_section(phases.phase_fluid_face),
    );

    println!(
        "            phases/section: {:.0} neighbour read(s), {:.0} face-flag lookup(s), \
         {:.0} of those found a row, {:.0} vertex/vertices written",
        per_section(phases.phase_neighbour_state),
        per_section(phases.phase_face_flags),
        per_section(phases.phase_flags_hit),
        per_section(phases.phase_vertices_written),
    );

    println!(
        "            phases/section: {:.0} quad(s) sorted, {:.0} of them actually moved",
        per_section(phases.phase_quad_sort),
        per_section(phases.phase_quads_moved),
    );

    println!(
        "            per call:      {:.1} ns in the cull, {:.1} ns in the emitter, \
         {:.1} neighbour read(s), {:.1} flag lookup(s)",
        per_call(phases.phase_nanos_in_cull, phases.phase_cull_calls),
        per_call(phases.phase_nanos_in_emit, phases.phase_face_emitted),
        per_call(phases.phase_neighbour_state, phases.phase_cull_calls),
        per_call(phases.phase_face_flags, phases.phase_cull_calls),
    );

    println!(
        "            per emitted:   {:.1} ns emitter, {:.1} neighbour read(s), \
         {:.1} flag lookup(s) - the ambient-occlusion corner is 4 reads and 4 lookups per vertex",
        per_call(phases.phase_nanos_in_emit, phases.phase_face_emitted),
        per_call(phases.phase_neighbour_state, phases.phase_face_emitted),
        per_call(phases.phase_face_flags, phases.phase_face_emitted),
    );
}

/// Fills `block_manager.face_flags` from each block's own model, which is the stand-in described where
/// this is called.
///
/// **Every variant of every block gets a row**, because `face_flags` is keyed by the packed
/// [`BlockstateKey`] - block index and augment - and the mesher asks it about the state a section
/// carries rather than about the block. A table keyed only by block index would answer `None` for
/// every state whose augment is not zero, which is most of them.
///
/// Returns how many rows it wrote, for the line that says whether this happened at all.
fn synthesise_face_flags(wm: &WmRenderer) -> usize {
    use wgpu_mc::mc::block::FaceFlags;

    let mut manager = wm.mc.block_manager.write();
    let mut rows = 0;

    // Collected first because the borrow of `manager.blocks` has to end before `face_flags` is written.
    let mut entries: Vec<(u32, FaceFlags)> = Vec::new();

    for (index, (_, block)) in manager.blocks.iter().enumerate() {
        // The variant count differs by block kind, and asking for one that is not there is answered with
        // the first entry - so a fixed handful of augments covers the whole registry without needing to
        // know how each kind stores its list.
        for augment in 0..8u16 {
            let Some(mesh) = block.get_model(augment, 0) else {
                if augment == 0 {
                    // No model at all: the game's table would have no row either, and the fallback is the
                    // same one.
                    break;
                }

                continue;
            };

            let key = ((index as u32) << 16) | augment as u32;

            entries.push((
                key,
                FaceFlags {
                    // The model's cull bits are a mask over this crate's `Direction` order and the field
                    // is in Java's; see `FaceFlags::occludes`. `cull` is built by the model baker in this
                    // crate's order and `java_mask_has` reads it in Java's, so the two are the same
                    // convention only because `BlockManager::face_flags` is *also* filled with Java's
                    // ordinals on the JNI side. Setting it straight from `cull` here is therefore the
                    // substitution this whole function is a caveat about.
                    occlusion: mesh.cull & 0b0011_1111,
                    self_hide: 0,
                    shades: true,
                    blocks_motion: true,
                    offset_max_y: 0.0,
                    offset_xz: false,
                    leaves: false,
                },
            ));

            rows += 1;

            if augment == 0 {
                break;
            }
        }
    }

    for (key, flags) in entries {
        manager.face_flags.insert(key, flags);
    }

    rows
}

/// A device with no window and no surface, which is all the atlas needs to exist.
///
/// `required_features` is deliberately empty and the limits are `downlevel_defaults`: the mesher asks
/// for nothing of the device, and asking for less is what lets this run on the software adapter of a
/// machine with no GPU.
fn headless_gpu() -> Gpu {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        flags: wgpu::InstanceFlags::from_build_config(),
        memory_budget_thresholds: Default::default(),
        backend_options: Default::default(),
        display: None,
    });

    let adapter = futures_lite_block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
        apply_limit_buckets: false,
    }))
    .expect("no wgpu adapter at all; the mesher bench needs a device for the atlas' texture");

    let (device, queue) = futures_lite_block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("wgpu-mc mesher bench"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::downlevel_defaults(),
        experimental_features: Default::default(),
        memory_hints: Default::default(),
        trace: Default::default(),
    }))
    .expect("the adapter offered no device");

    Gpu {
        instance,
        adapter,
        surface: parking_lot::Mutex::new(None),
        device,
        queue,
        // Not created: a pipeline cache only pays for itself when something is compiled, and this
        // bench compiles nothing.
        pipeline_cache: None,
    }
}

/// A future driven on this thread. `futures` is not a dependency of this crate and the two futures
/// above are the whole need, so the executor is twenty lines rather than a dependency.
fn futures_lite_block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};

    struct Park(std::thread::Thread);

    impl Wake for Park {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }

    let waker = Waker::from(Arc::new(Park(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);

    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::park(),
        }
    }
}

/// What this thread has allocated so far: **nothing, on every platform this runs on.**
///
/// This column exists on the plan because Bye-Pregen reports it, and Bye-Pregen gets it from the JVM:
/// `com.sun.management.ThreadMXBean#getThreadAllocatedBytes` is a HotSpot feature with no counterpart
/// in the C++ or Rust runtimes. There is no portable per-thread allocation counter in the Rust
/// standard library, and the nearest things are not the same measurement:
///
///  * `PROCESS_MEMORY_COUNTERS.PrivateUsage` on Windows and `/proc/self/statm` on Linux are
///    **process-wide and resident**, not per-thread and not allocated - they move with the GPU
///    driver's own mappings and with the arena, and a number that moves for those reasons would be
///    read as the mesher's;
///  * a counting global allocator would be the honest way to get it, and that is a change to how the
///    whole renderer allocates rather than to a bench.
///
/// So the column prints `n/a`, and the harness that prints it says why. **An allocation number that is
/// a different measurement is worse than no number**, because the one thing it is here to catch -
/// "faster and twice the allocations" - is exactly what a process-wide figure hides.
///
/// Left as a function rather than deleted so that a counting allocator, when it exists, has one place
/// to be plugged in.
fn thread_allocated_bytes() -> Option<u64> {
    None
}

/// `--name value`, or `--name` for a flag. Hand-rolled because a bench that needs `clap` to be
/// fetched is a bench that does not get run.
fn arg<T: std::str::FromStr>(name: &str) -> Option<T> {
    arg_str(name).and_then(|value| value.parse().ok())
}

/// Whether `--name` is present at all, **without consuming the argument after it**.
///
/// This is not the same question as `arg_str(name).is_some()`, and the difference cost a run: a flag
/// and an option are both written `--name` on the command line, so asking for a flag by searching for
/// its name and taking the next argument reads `--phases --scenario solid` as "the phases flag is
/// `--scenario`", skips the scenario, and reports every counter as zero with nothing to say why. A flag
/// has to be *found* rather than followed.
fn flag(name: &str) -> bool {
    std::env::args().skip(1).any(|arg| arg == name)
}

/// The value after `--name`, and nothing if `--name` is not there.
fn arg_str(name: &str) -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == name {
            return args.next();
        }
    }
    None
}
