/*!
# wgpu-mc
wgpu-mc is a pure-Rust crate which is designed to be usable by anyone who needs to render
Minecraft-style scenes using Rust. The main user of this crate at this time is the Minecraft mod
Electrum which replaces Minecraft's official renderer with wgpu-mc.
However, anyone is able to use this crate, and the API is designed to be completely independent
of any single project, allowing anyone to use it. It is mostly batteries-included, except for a
few things.

# Considerations

This crate is unstable and subject to change. The basic structure for features such
as terrain rendering and entity rendering are already in-place but could very well change significantly
in the future.

# Setup

wgpu-mc, as you could have probably guessed, uses the [wgpu](https://github.com/gfx-rs/wgpu) crate
for communicating with the GPU. Assuming you aren't running wgpu-mc headless (if you are, I assume
you already know what you're doing), wgpu-mc can handle surface and device setup for you, as long
as you pass in a valid window handle. See [init_wgpu]

# Rendering

wgpu-mc makes use of a trait called `WmPipeline` to describe any struct which is used for
rendering. There are multiple built in pipelines, but they aren't required to use while rendering.

## Terrain Rendering

The first step to begin terrain rendering is to implement [BlockStateProvider](cr).
This is a trait that provides a block state key for a given coordinate.

## Entity Rendering

To render entities, you need an entity model. wgpu-mc makes no assumptions about how entity models are defined,
so it's up to you to provide them to wgpu-mc.

See the [render::entity] module for an example of rendering an example entity.
 */

// `impl WmShader for WgslShader` has to prove `Send + Sync`, and proving that has to walk wgpu's
// auto-trait chain - `ShaderModule` -> `DispatchShaderModule` -> `Arc<CoreShaderModule>` ->
// `ContextWgpuCore` -> `Global` -> `Hub` -> every registry in it, including the ray-tracing one -
// which is deeper than the default limit of 128. The default is exceeded rather than any trait
// genuinely being unsatisfiable, so the limit is what moves; raising it costs nothing at runtime.
#![recursion_limit = "512"]

use std::borrow::Borrow;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, Sender, channel};

use glam::{IVec3, ivec2};
use mc::Scene;
use mc::chunk::BakedLayer;
pub use minecraft_assets;
use parking_lot::Mutex;
pub use wgpu;
use wgpu::{BindGroupDescriptor, BindGroupEntry, BindGroupLayout, BufferDescriptor, Surface};

use crate::mc::MinecraftState;
use crate::mc::resource::ResourceProvider;
use crate::render::atlas::Atlas;
use crate::render::pipeline::{BLOCK_ATLAS, ENTITY_ATLAS, create_bind_group_layouts};
use crate::util::BindableBuffer;

/// Sprite uploads that happened after the first one, and the second the last line about them was
/// written. See `WmRenderer::upload_late_sprites`.
static LATE_SPRITE_UPLOADS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static LATE_SPRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static LATE_SPRITES_REPORTED_AT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// How long a burst of late sprites is allowed to be one line. One second is the interval the terrain
/// line uses, and for the same reason: long enough that a steady state is one line a second, short
/// enough that a burst is not a minute of silence.
const LATE_SPRITES_REPORT_SECONDS: u64 = 1;

pub mod mc;
pub mod render;
pub mod texture;
pub mod util;

pub use treeculler::Frustum;

/// Provides access to wgpu
pub struct Gpu {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub surface: Mutex<Option<Arc<Surface<'static>>>>,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    /// Driver-side pipeline compilation results, carried across runs where the backend supports it.
    ///
    /// Owned by the device it was created for: a cache from another device - or from the same one
    /// after a driver update - is rejected entry by entry, so it is kept beside the device rather
    /// than in a process-wide cell that a backend fallback would leave pointing at a dead device.
    /// `None` on the backends that do not implement one, which is DX12.
    pub pipeline_cache: Option<wgpu::PipelineCache>,
}

/// Tuple of chunk positions and baked layers
pub type ChunkUpdateData = (IVec3, Vec<BakedLayer>);

/// The main wgpu-mc renderer struct
/// Resources pertaining to Minecraft go in `MinecraftState`.
///
/// `RenderGraph` is used in tandem with `World` to render scenes.
pub struct WmRenderer {
    pub gpu: Arc<Gpu>,
    pub bind_group_layouts: Arc<HashMap<String, BindGroupLayout>>,
    pub mc: MinecraftState,
    pub chunk_update_queue: (Sender<ChunkUpdateData>, Mutex<Receiver<ChunkUpdateData>>),
    /// What the renderer draws, and the GPU resources that belong to the world rather than to the
    /// renderer: the section arena the Rust terrain baker writes into, the buffer it lives in, the
    /// frame's depth texture, and the camera's section position.
    ///
    /// A `OnceLock` rather than a field built in [`WmRenderer::new`] because a scene needs the
    /// renderer it belongs to - the device, for the arena's buffer - and because it needs a
    /// *framebuffer size*, which is only known once there is a surface: the JVM passes the window's
    /// size when it creates the renderer, and a renderer created before the window has one gets its
    /// scene on the first presented frame instead.
    pub scene: std::sync::OnceLock<Scene>,
    /// The staging bytes a frame's section uploads are coalesced into. See
    /// [`WmRenderer::submit_chunk_updates`], which is the only thing that touches it.
    ///
    /// `Mutex` rather than a plain field because the renderer is shared and a shared reference is what
    /// `submit_chunk_updates` has. The render thread is the only writer, so this is one uncontended
    /// compare-and-swap per frame - `parking_lot`'s lock, not a syscall - to protect a buffer with one
    /// writer.
    upload_scratch: Mutex<Vec<u8>>,
    /// How many `write_buffer` calls the last drain made, and how many it would have made one per
    /// range. Reported once a second beside the other counters, because "coalescing helps" is a claim
    /// about these two numbers and nothing else.
    uploads_written: std::sync::atomic::AtomicU64,
    uploads_saved: std::sync::atomic::AtomicU64,
}

/// One run of contiguous arena bytes waiting to be written: where it goes, what it is, and where its
/// bytes sit in the frame's upload scratch.
///
/// The bytes are identified by an offset into the scratch buffer rather than by a slice because the
/// runs are collected *while* that buffer is still being appended to - see
/// [`WmRenderer::submit_chunk_updates`], which writes them all once the gathering is done.
struct PendingUpload {
    /// Which arena, as its index in `Scene::chunk_buffers`.
    buffer: u32,
    /// Byte offset in that arena.
    offset: u64,
    /// How many bytes this run covers, which is the sum of the ranges merged into it.
    len: u64,
    /// Where those bytes start in the scratch buffer.
    at: usize,
}

#[derive(Copy, Clone)]
pub struct WindowSize {
    pub width: u32,
    pub height: u32,
}

pub trait HasWindowSize {
    fn get_window_size(&self) -> WindowSize;
}

impl WmRenderer {
    /// The scene, or `None` while the renderer has no framebuffer size yet.
    pub fn scene(&self) -> Option<&Scene> {
        self.scene.get()
    }

    /// The scene, created if it is not there, with its depth texture brought to `framebuffer_size`.
    ///
    /// The size is checked on every call rather than only at creation because this is also the
    /// resize path: a frame that presents at a different size from the one the depth texture was
    /// made for is a frame whose terrain pass would render into the wrong attachment - and wgpu
    /// refuses a pass whose attachment does not match the pipeline's, so the failure would be an
    /// error rather than a wrong picture. `None` for a zero-sized framebuffer, which is a
    /// minimised window: there is nothing to render into and nothing to rebuild.
    pub fn ensure_scene(&self, framebuffer_size: wgpu::Extent3d) -> Option<&Scene> {
        if framebuffer_size.width == 0 || framebuffer_size.height == 0 {
            return self.scene.get();
        }

        let created = self.scene.get().is_none();
        let scene = self
            .scene
            .get_or_init(|| Scene::new(self, framebuffer_size));

        if created {
            log::info!(
                "wgpu-mc: the scene is up, sized {}x{}",
                framebuffer_size.width,
                framebuffer_size.height
            );
        }

        let (width, height) = {
            let depth = scene.depth_texture.read();
            (depth.width(), depth.height())
        };

        if width != framebuffer_size.width || height != framebuffer_size.height {
            scene.resize_depth_texture(self, framebuffer_size.width, framebuffer_size.height);
            log::info!(
                "wgpu-mc: the scene's depth texture is now {}x{} (was {width}x{height})",
                framebuffer_size.width,
                framebuffer_size.height
            );
        }

        Some(scene)
    }
}

impl WmRenderer {
    pub fn new(display: Arc<Gpu>, resource_provider: Arc<dyn ResourceProvider>) -> WmRenderer {
        let mc = MinecraftState::new(&display, resource_provider);
        let (sender, receiver) = channel();
        Self {
            bind_group_layouts: Arc::new(create_bind_group_layouts(&display.device)),
            gpu: display,
            mc,
            chunk_update_queue: (sender, Mutex::new(receiver)),
            scene: std::sync::OnceLock::new(),
            upload_scratch: Mutex::new(Vec::new()),
            uploads_written: std::sync::atomic::AtomicU64::new(0),
            uploads_saved: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub fn init(&self) {
        let atlases = [BLOCK_ATLAS, ENTITY_ATLAS]
            .iter()
            .map(|&name| (name.into(), Atlas::new(&self.gpu, false)))
            .collect();

        *self.mc.texture_manager.atlases.write() = atlases;
    }

    pub fn upload_animated_block_buffer(&self, data: Vec<f32>) {
        let d = data.as_slice();

        let buf = self.mc.animated_block_buffer.borrow().load_full();

        if buf.is_none() {
            let animated_block_buffer = self.gpu.device.create_buffer(&BufferDescriptor {
                label: None,
                size: (d.len() * 8) as wgpu::BufferAddress,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let animated_block_bind_group =
                self.gpu.device.create_bind_group(&BindGroupDescriptor {
                    label: None,
                    layout: self.bind_group_layouts.get("ssbo").unwrap(),
                    entries: &[BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::Buffer(
                            animated_block_buffer.as_entire_buffer_binding(),
                        ),
                    }],
                });

            self.mc
                .animated_block_buffer
                .store(Arc::new(Some(animated_block_buffer)));
            self.mc
                .animated_block_bind_group
                .store(Arc::new(Some(animated_block_bind_group)));
        }

        self.gpu.queue.write_buffer(
            (**self.mc.animated_block_buffer.load()).as_ref().unwrap(),
            0,
            bytemuck::cast_slice(d),
        );
    }

    /// Moves every section the baker finished into the arena, and drops the ones the camera has left
    /// behind.
    ///
    /// Called once per frame, before the frame is presented: the bakes themselves ran on the pool,
    /// and this is where their results become the arena's contents - one `write_buffer` per layer,
    /// into the ranges the arena handed out. Trimming here rather than in the baker is what keeps the
    /// arena's size a function of where the camera is: a section's ranges are freed when the camera
    /// is more than the render distance plus two chunks away from it, and a section that is walked
    /// back into is baked again, because Minecraft re-meshes what it unloads.
    pub fn tick_scene(&self, scene: &Scene) {
        self.submit_chunk_updates(scene);
        self.upload_late_sprites();

        let camera = *scene.camera_section_pos.read();

        {
            let mut last = scene.trimmed_section_pos.write();
            // The trim is horizontal: see `Scene::camera_section_pos` for why the camera's section
            // carries a height this does not use.
            let horizontal = ivec2(camera.x, camera.z);

            if *last == horizontal {
                return;
            }

            *last = horizontal;
        }

        // **What the trim dropped, into the same channel a refusal goes down.** Both mean one thing to
        // the other side: a section this renderer is no longer drawing, which Minecraft's mesh has to
        // come back for - `rustHas` is a claim, and this is where the claim stops being true. It costs
        // nothing when the trim removed nothing, which is most frames.
        let trimmed = scene
            .section_storage
            .write()
            .trim(ivec2(camera.x, camera.z));

        if !trimmed.is_empty() {
            let mut storage = scene.section_storage.write();

            for pos in trimmed {
                storage.forget_trimmed(pos);
            }
        }
    }

    /// Uploads any sprite the atlas has gained since its texture was last written.
    ///
    /// A net under the two places that bake block models: the seam between them is where the mushroom
    /// blocks went missing (`cacheBlockStates` uploads it), and a resource reload, a mod that registers
    /// a block later, or anything else that allocates a sprite outside that seam lands here instead -
    /// once, on the frame after it was allocated, before the frame is recorded. The check is one atomic
    /// load per frame and the upload is a few milliseconds when it happens, which is the trade against
    /// a block that is not on screen at all. See `Atlas::upload_if_dirty`.
    ///
    /// **At most one line a second**, because a sprite arriving per frame is a legitimate state - the
    /// title screen allocates them while it settles, and a resource reload allocates hundreds - and one
    /// warning per upload is a console with nothing else in it. The line carries the total since the
    /// last one, so the reader still sees how much arrived and how many uploads it took; the first
    /// upload after each line is reported immediately, so nothing is swallowed either.
    fn upload_late_sprites(&self) {
        let atlases = self.mc.texture_manager.atlases.read();

        for (name, atlas) in atlases.iter() {
            let pending = atlas.sprites_since_upload();

            if pending == 0 {
                continue;
            }

            atlas.upload_if_dirty(self);

            let uploads = LATE_SPRITE_UPLOADS.fetch_add(1, Ordering::Relaxed) + 1;
            let sprites = LATE_SPRITES.fetch_add(pending, Ordering::Relaxed) + pending;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_secs())
                .unwrap_or(0);
            let reported_at = LATE_SPRITES_REPORTED_AT.load(Ordering::Relaxed);

            if reported_at != 0 && now.saturating_sub(reported_at) < LATE_SPRITES_REPORT_SECONDS {
                continue;
            }

            LATE_SPRITES_REPORTED_AT.store(now, Ordering::Relaxed);
            LATE_SPRITES.store(0, Ordering::Relaxed);
            LATE_SPRITE_UPLOADS.store(0, Ordering::Relaxed);

            log::warn!(
                "wgpu-mc: {name} was uploaded again after the frame: {sprites} sprite(s) over \
                 {uploads} upload(s) since the last line, the last one adding {pending}"
            );
        }
    }

    pub fn submit_chunk_updates(&self, scene: &Scene) {
        // Growing the arena, if a section did not fit in it last frame, happens *here* - before the
        // buffer this drain writes into is loaded. The two cannot overlap: the ranges being written
        // below were handed out by the pool, and a pool that grew halfway through a frame would leave
        // the rest of the frame's writes aimed at the old buffer. See `grow_arena`.
        self.grow_arena_if_asked(scene);

        let receiver = self.chunk_update_queue.1.lock();
        let updates = receiver.try_iter();

        let mut moved = 0usize;

        // Once per frame, before the frame's own updates: this is the rotation that gives back the
        // ranges parked a whole `frames in flight` ago, i.e. those whose submission the present has
        // waited for. Doing it per update would free a range parked earlier in the *same* frame.
        scene.section_storage.write().free_deferred();

        // The arena's buffers, held for the whole drain: the list is replaced when an arena is added, and
        // the ranges being written below were handed out by the pool that goes with the buffer that is
        // loaded here. One load for all of them, so an addition cannot land between two writes of the
        // same frame.
        let chunk_buffers = scene.chunk_buffers.load_full();

        // **One `write_buffer` per run of contiguous bytes, rather than two per layer per section.**
        //
        // `Queue::write_buffer` is not free and the cost is not the copy: wgpu-core allocates a
        // *staging buffer* per call (`StagingBuffer::new`) and frees it after the next submission, so a
        // frame that moves a thousand sections over three layers was making thousands of short-lived
        // allocations and thousands of small transfers - which is exactly the shape of "loading a world"
        // and "flying forward", the two states where this work is real. When the world is settled the
        // drain moves nothing and none of this runs.
        //
        // The ranges of consecutive sections are not adjacent in general - they come from a free list -
        // so *contiguity is what is detected rather than assumed*: ranges that happen to be adjacent in
        // the same buffer are merged into one write, and a run that turns out to be a single range
        // degenerates to the old behaviour. The bytes of a run are gathered into one scratch buffer,
        // which costs one copy that the old path paid anyway inside `write_buffer`.
        let mut scratch = self.upload_scratch.lock();

        // Emptied, not freed: the point of keeping it is that the capacity survives the frame, so a
        // world being loaded grows it to the largest frame's worth of uploads once and then reuses it.
        scratch.clear();

        let mut written = 0u64;
        let mut saved = 0u64;

        // The runs to write, gathered before any of them is written. See the loop at the end.
        let mut runs: Vec<PendingUpload> = Vec::new();

        // The run being gathered, or `None` when the next range starts a new one.
        let mut pending: Option<PendingUpload> = None;

        updates.for_each(|(pos, layers)| {
            moved += 1;

            let mut storage = scene.section_storage.write();

            // Allocate, write, publish - in that order. The section is only in the storage once its
            // bytes are queued, so the frame that draws it draws what was written rather than whatever
            // the range held before; and the ranges it replaced are only reused a frame from now.
            //
            // No arena with room leaves the section exactly as it was: its ranges are not given back to
            // the allocator and it is not replaced, so the world keeps drawing the geometry it has. A
            // section that cannot be baked is stale ground, which is a wrong picture; replacing it
            // with nothing is a hole, which is not a picture at all.
            let Some((section, freed)) = storage.allocate(pos, &layers) else {
                // Ask for another arena rather than only counting the refusal. A fixed ceiling sized
                // from a guess about what a section costs is one a real world outgrows, and the failure
                // mode of that is the worst one this path has: a section that is never replaced keeps
                // the geometry it had, so the terrain it is part of stops changing - the player breaks a
                // block and nothing happens. The request is applied by the next frame's drain, and the
                // JVM re-offers this section (see `RustChunkBake.forgetRefused`).
                //
                // A marker rather than a size: growth appends one arena at the device's own ceiling, so
                // there is no size to ask for - see `grow_arena_if_asked`.
                scene
                    .pending_arena_growth
                    .store(1, std::sync::atomic::Ordering::Relaxed);
                return;
            };

            for (i, ranges) in section.layers.iter().enumerate() {
                if let Some(ranges) = ranges {
                    // **Which arena this layer was handed out of, and not any other.** The write has to
                    // land in the buffer whose offsets these are; with one arena that was a detail, and
                    // with several it is the difference between a section and somebody else's geometry.
                    //
                    // It is named here and resolved when the runs are written, because resolving it now
                    // would mean holding a borrow of `chunk_buffers` across the whole drain. A range that
                    // names an arena the renderer does not have is dropped at that point instead: the two
                    // lists are kept in step, so it is a bug rather than a state, and skipping one range
                    // beats panicking mid-drain.
                    //
                    // Vertices and indices, in whichever order they are laid out: a free-list allocator
                    // can hand out the index range before the vertex range, and a run has to be in
                    // ascending offsets to be one write.
                    let mut pieces = [
                        (ranges.vertex_range.clone(), &layers[i].vertices),
                        (ranges.index_range.clone(), &layers[i].indices),
                    ];

                    pieces.sort_by_key(|(range, _)| range.start);

                    for (range, bytes) in pieces {
                        let start = range.start as u64 * 4;

                        let joins = pending.as_ref().is_some_and(|run| {
                            run.buffer == ranges.buffer && run.offset + run.len == start
                        });

                        if !joins {
                            if let Some(run) = pending.take() {
                                runs.push(run);
                            }

                            pending = Some(PendingUpload {
                                buffer: ranges.buffer,
                                offset: start,
                                len: 0,
                                at: scratch.len(),
                            });
                        }

                        let run = pending.as_mut().expect("just set");
                        run.len += bytes.len() as u64;
                        scratch.extend_from_slice(bytes);
                        saved += 1;
                    }
                }
            }

            storage.insert(pos, section);
            storage.defer_free(freed);
        });

        // The last run, which nothing follows to flush it.
        if let Some(run) = pending.take() {
            runs.push(run);
        }

        // **The writes, after the gathering**, so that the bytes being written are the scratch buffer's
        // and nothing else's. Doing it inline would mean holding a borrow of `scratch` across the next
        // `extend_from_slice` into it.
        for run in &runs {
            let Some(buffer) = chunk_buffers.get(run.buffer as usize) else {
                continue;
            };

            self.gpu.queue.write_buffer(
                &buffer.buffer,
                run.offset,
                &scratch[run.at..run.at + run.len as usize],
            );

            written += 1;
        }

        self.uploads_written
            .store(written, std::sync::atomic::Ordering::Relaxed);
        self.uploads_saved
            .store(saved, std::sync::atomic::Ordering::Relaxed);

        // The count of sections that became the arena's contents in this frame: the one number that
        // says the baker's output is reaching the buffer the terrain pass will draw from, and that
        // the queue is being drained rather than growing behind it.
        if moved != 0 && mc::chunk::DIAGNOSTIC_LOGGING.load(std::sync::atomic::Ordering::Relaxed) {
            log::info!(
                "wgpu-mc: {moved} baked section(s) moved into the arena ({} in it now), {saved} upload \
                 range(s) in {written} write(s) - {} merge(s)",
                scene.section_storage.read().len(),
                saved.saturating_sub(written)
            );
        }
    }

    /// Grows the section arena to the size a refusal asked for, if one did.
    ///
    /// The pool is sized from the render distance and a per-section estimate, and an estimate is
    /// exactly the thing a real world is entitled to beat: sections are different sizes, and the pool
    /// has to hold all of them at once. When one does not fit, the arena used to record the refusal and
    /// stop there - the section kept the geometry it already had, and the next rebuild of it was refused
    /// too. A world in that state draws its past: the player breaks a block and the ground does not
    /// change, and the sections that were never baked at all stay holes.
    ///
    /// So the arena grows instead, and the growth is cheap because of what it is: `RangeAllocator`
    /// grows at the *end*, so every range that is already handed out keeps its offset and the buffer's
    /// contents stay exactly where they are. What is left is one device-to-device copy into a bigger
    /// buffer, a swap of the bind group the terrain pass reads, and the pool number. Nothing is meshed
    /// again, and the frame that is already recorded keeps drawing from the old buffer until it is done
    /// with it - `BindableBuffer` is held by that frame's pass, and this only drops our reference.
    ///
    /// The doubling means a session pays a copy per growth rather than one per refusal, and the cap is
    /// the device's own `max_buffer_size`: a request past it is clamped rather than attempted, because
    /// a buffer that cannot be created is a validation error and a validation error here ends the
    /// process. Reaching the cap leaves the refusals to the return channel, which is the state this
    /// path was in before it could grow at all.
    fn grow_arena_if_asked(&self, scene: &Scene) {
        let asked = scene
            .pending_arena_growth
            .swap(0, std::sync::atomic::Ordering::Relaxed);

        if asked == 0 {
            return;
        }

        // **Another arena, rather than a bigger one.** This used to allocate a larger buffer and copy
        // the old one into it; appending costs one allocation and no copy, because every range already
        // handed out names the buffer it was handed out in (`SectionRanges::buffer`). See
        // `SectionStorage::grow_pool`.
        //
        // Each arena is created at the device's own ceiling, because there is no reason for one to be
        // smaller: the limit that matters is how many of them there are, and that is `ARENA_BUFFERS`.
        //
        // **As many as the target asks for, in one go.** A world is entered at whatever render distance
        // the server can serve, which is often far less than the view becomes: a run was measured sizing
        // its arena to **192 MB for 12 chunks** and then being asked to draw 32. `set_arena_slots`
        // cannot resize an arena that holds anything, so the only way up is by appending - and adding
        // *one* arena per refusal made that a race against the burst of bakes that a growing view
        // produces. The refusals in that window are sections Minecraft's mesh was already dropped for,
        // which is the hole this file has spent several rounds on.
        //
        // `pending_arena_growth` is a marker rather than a size - the refusal that sets it only knows
        // that something did not fit - so the target is read from the pool the world actually wants:
        // `arena_slots` at the current width.
        let slots = scene.arena_cap_slots;
        let target = mc::chunk::arena_slots(scene.section_storage.read().width().max(0) as u32);
        let mut at_limit = false;

        loop {
            // **The byte budget, which is the bound that was missing.** A per-buffer limit is not a
            // memory bound, and an arena that appends rather than refuses had no other one: a player
            // flying forward grew it until the driver complained - which turned a bug about holes into
            // one about memory. Past this the arena stops growing and `at_capacity` tells the JVM to
            // keep Minecraft's mesh for whatever does not fit, the same answer as at the device's limit.
            if scene.section_storage.read().pool_slots() as u64 * 4 >= mc::ARENA_MEMORY_BUDGET {
                at_limit = true;

                break;
            }

            let added = scene
                .section_storage
                .write()
                .grow_pool(mc::ARENA_BUFFERS, slots);

            if !added {
                // At the buffer limit: the other state where a refusal is permanent, and the JVM is told
                // so that it keeps Minecraft's mesh for the sections this side cannot take.
                at_limit = true;

                break;
            }

            let added = Arc::new(BindableBuffer::new_deferred(
                self,
                slots as u64 * 4,
                mc::ARENA_USAGE,
                "ssbo",
            ));

            let mut buffers = scene.chunk_buffers.load().as_ref().clone();
            buffers.push(added);
            scene.chunk_buffers.store(Arc::new(buffers));

            // Enough room for what the view asks for, or as many arenas as there may be.
            if scene.section_storage.read().pool_slots() >= target {
                break;
            }
        }

        // Set from what happened rather than from the branch taken last: a growth that reached the
        // target is room, and the only way this is true is that another arena was refused. It is what
        // decides whether the JVM keeps Minecraft's mesh, so it is cleared as deliberately as it is set.
        scene.section_storage.write().set_at_capacity(at_limit);

        let stored = scene.section_storage.read();

        log::warn!(
            "wgpu-mc: the section arena was full, so it grew to {} arena(s) of {} slot(s), {} MB total \
             ({} slot(s) handed out, the largest section meshed so far {} slot(s))",
            stored.arena_count(),
            slots,
            stored.pool_slots() as u64 * 4 / (1024 * 1024),
            stored.used_slots(),
            mc::chunk::largest_section_slots(),
        );
    }

    /// The wgpu version this build was compiled against, as the build script saw it.
    ///
    /// The same string `get_backend_description` reports, for the callers that need a version without
    /// a renderer: the processed-shader cache stamps its entries with it, so shaders translated by one
    /// wgpu are not fed to another.
    pub fn wgpu_version() -> &'static str {
        env!("WGPUMC_WGPU_VER")
    }

    pub fn get_backend_description(&self) -> String {
        format!(
            "wgpu {} ({})",
            env!("WGPUMC_WGPU_VER"),
            self.gpu.adapter.get_info().backend.to_str()
        )
    }

    /// How the adapter behind this renderer introduces itself, one field per line.
    ///
    /// The F3 overlay's vanilla system block asks the device for a vendor, a renderer name, a
    /// backend name and a version. On the OpenGL backend those four answers are `GL_VENDOR`,
    /// `GL_RENDERER`, "OpenGL" and `GL_VERSION` - the graphics driver, introducing itself - and
    /// wgpu carries the same information in `AdapterInfo`. Handing it over lets that block keep
    /// looking the way it does on GL instead of reading "wgpu / wgpu-mc / vulkan / wgpu 30".
    ///
    /// Four lines in this order, none of them empty:
    ///
    /// 1. the vendor (`NVIDIA`, `AMD`, ...), named from the PCI id
    /// 2. the adapter's own name (`NVIDIA GeForce RTX 4060 Laptop GPU`)
    /// 3. the API it is driven through (`Vulkan`, `DirectX 12`, ...)
    /// 4. the driver and its version
    ///
    /// The fourth line joins `driver` and `driver_info` rather than choosing between them, because
    /// the backends fill them in differently: Vulkan reports the driver's name and version
    /// ("NVIDIA" and "552.44"), while DX12 puts the version in `driver` and leaves `driver_info`
    /// empty ("31.0.101.5333"). Either way the result names the installed graphics driver.
    pub fn get_adapter_description(&self) -> String {
        let info = self.gpu.adapter.get_info();

        let vendor = match vendor_name(info.vendor) {
            "" => format!("vendor 0x{:04x}", info.vendor),
            name => name.to_string(),
        };
        let driver = [info.driver.trim(), info.driver_info.trim()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ");

        [
            vendor,
            info.name,
            backend_name(info.backend).to_string(),
            if driver.is_empty() {
                "unknown driver".to_string()
            } else {
                driver
            },
        ]
        .join("\n")
    }
}

/// PCI vendor ids, as the names the drivers use for themselves.
///
/// `AdapterInfo#vendor` is the raw id the driver reports, so this is the only place it becomes
/// something a person can read; an id that is not listed stays empty and the caller falls back to
/// printing the number.
fn vendor_name(vendor: u32) -> &'static str {
    match vendor {
        0x10DE => "NVIDIA",
        0x1002 | 0x1022 => "AMD",
        0x8086 | 0x8087 => "Intel",
        0x106B => "Apple",
        0x13B5 => "ARM",
        0x5143 => "Qualcomm",
        0x1010 => "Imagination Technologies",
        0x1AE0 => "Google",
        _ => "",
    }
}

/// The API's own name, rather than the identifier wgpu uses for it.
///
/// Matched exhaustively on purpose: a backend added to wgpu should show up as a compile error here
/// rather than as a blank line in the overlay.
fn backend_name(backend: wgpu::Backend) -> &'static str {
    match backend {
        wgpu::Backend::Vulkan => "Vulkan",
        wgpu::Backend::Dx12 => "DirectX 12",
        wgpu::Backend::Metal => "Metal",
        wgpu::Backend::Gl => "OpenGL",
        wgpu::Backend::BrowserWebGpu => "WebGPU",
        wgpu::Backend::Noop => "no backend",
    }
}
