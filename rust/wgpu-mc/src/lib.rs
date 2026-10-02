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

/// How many times the arena was asked for room while it still had some, rather than at a refusal.
/// See the growth trigger in the section drain.
static ARENA_GROWTH_ASKED_EARLY: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

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

            // The arena's plan is *not* printed here: this runs before `env_logger` exists, so a line
            // written from here is dropped. It is written beside the device capabilities instead, once a
            // world - see `report_device_capabilities`' caller in `wgpu-mc-jni`.
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

/// What an error scope said, **polled once rather than awaited**.
///
/// `wgpu`'s `ErrorScopeGuard::pop` hands back a future, and the only backend where that future is really
/// asynchronous is WebGPU, where it is a JavaScript promise. On `wgpu-core` - which is every native
/// backend, and therefore this renderer - it is `Box::pin(ready(scope.error))`: the error is already
/// captured when `pop` returns, which is what the documentation means by "the pop takes effect
/// immediately; the future does not need to be awaited".
///
/// So one poll with a no-op waker is the whole of it, and the third case is the one that must not be
/// guessed at: a `Pending` answer means a backend that resolves later, and the honest response is to say
/// so rather than to block the render thread on a promise or to read "not yet" as "no error".
enum ScopeResult {
    Error(wgpu::Error),
    Clean,
    Pending,
}

fn resolve_error_scope_now(
    future: impl std::future::Future<Output = Option<wgpu::Error>>,
) -> ScopeResult {
    use std::task::{Context, Poll, Waker};

    let mut context = Context::from_waker(Waker::noop());

    // `pin!` rather than `Box::pin`: the future never leaves this frame, and `wgpu-core`'s is already a
    // box - a second allocation per growth would be the only thing this helper cost.
    match std::pin::pin!(future).poll(&mut context) {
        Poll::Ready(Some(error)) => ScopeResult::Error(error),
        Poll::Ready(None) => ScopeResult::Clean,
        Poll::Pending => ScopeResult::Pending,
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

        let mut moved = 0usize;

        // Once per frame, before the frame's own updates: this is the rotation that gives back the
        // ranges parked a whole `frames in flight` ago, i.e. those whose submission the present has
        // waited for. Doing it per update would free a range parked earlier in the *same* frame.
        //
        // **And the sections that were baked and had nowhere to go come back here**, in the order they
        // were parked. They are offered to the arena *before* this frame's own updates, which is the
        // fair order rather than the cheap one: a section that has been waiting has had stale ground
        // for longer, and if the room runs out it is the fresh work that gets parked rather than the
        // old. The one overlap is handled by that same order - a section this frame also carries
        // arrives twice and the newer geometry lands, because its update is applied after the parked
        // one. See `SectionStorage::park` for what parking saves, which is the whole of steps two
        // through five of the old recovery: Minecraft's chunk build, its 4096-position compile, the
        // 27-section payload and the JNI call that applied it.
        let parked = {
            let mut storage = scene.section_storage.write();

            storage.free_deferred();

            storage.take_parked()
        };

        let updates = parked.into_iter().chain(receiver.try_iter());

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
            // **Ask for room before the room is gone**, rather than only once a refusal has already cost
            // this section its place in the frame. A request made at the refusal is made too late for the
            // section that made it: growth is applied by the next frame's drain, so that section keeps the
            // geometry it had and the ground under it stops changing until the JVM offers it again. Asking
            // while there is still room for the largest section this side has ever baked, twice over - the
            // one being allocated now and the one after it - is what removes those refusals.
            //
            // The margin is deliberately not a frame's worth of bakes: growth appends an arena sized to
            // what the view wants (see `grow_arena_if_asked`), and repeated asks coalesce into one marker,
            // but how many sections a frame's burst holds is not something this side knows - a margin that
            // claimed to cover it would be a guess dressed as a constant.
            const GROWTH_MARGIN: u32 = 2;

            if !storage.at_capacity()
                && storage.free_slots()
                    < mc::chunk::largest_section_slots().saturating_mul(GROWTH_MARGIN)
            {
                let asking_at_a_refusal = storage.refusals_waiting() > 0
                    || scene
                        .pending_arena_growth
                        .load(std::sync::atomic::Ordering::Relaxed)
                        != 0;

                if !asking_at_a_refusal {
                    ARENA_GROWTH_ASKED_EARLY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }

                scene
                    .pending_arena_growth
                    .store(1, std::sync::atomic::Ordering::Relaxed);
            }
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

                // **And keep the geometry that did not fit**, rather than throwing it away and asking
                // Minecraft to produce it again. What the refusal costs when it is dropped is out of
                // all proportion to what it is: the section has been baked, the vertices and indices
                // are in this frame's hand, and the only thing missing is a range to put them in -
                // while the recovery was a rebuild of the section through the game's own compiler, a
                // 27-section payload over JNI, and an answer that could be refused all over again.
                //
                // The return value is deliberately not read, because both answers are already the
                // state this side wants: kept means the geometry waits for the next free slot, and not
                // kept means the position is still in `refused_pending` - where `allocate` put it -
                // which is the path that hands it to the JVM and has Minecraft mesh the section. The
                // two ways to get `false` are the arena being at the device's limit, where there is no
                // later slot to wait for, and the queue being full, where waiting is how the memory
                // would get away.
                storage.park(pos, layers);

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

            // **And the game's list may not have heard of this section yet.** Recorded at the publish,
            // which is the moment the claim becomes true: before it the section is not in the arena and
            // the gather cannot reach it. See `Scene::sections_since_the_list` for what the gather does
            // with the difference between a section the game's list is late for and one it left out.
            scene.note_section_taken(pos);

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

        // **What the arena is holding because it had nowhere to put it**, which is the state this
        // queue exists to make visible. Silence means no section ever had to keep its geometry
        // waiting, and that is the whole point of the line being conditional: a run that never
        // refuses prints nothing, and one that does says how much is waiting, what it costs, how many
        // sections were parked over the run, and how many were let go when the view left them behind.
        //
        // Once a second while it is not empty, **and once when it empties** - so a run ends with the
        // answer rather than with the last count from before the queue drained.
        let (parked_now, parked_bytes) = {
            let storage = scene.section_storage.read();

            (storage.parked_len(), storage.parked_bytes())
        };

        {
            static PARKED_REPORTED: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            static PARKED_HELD: std::sync::atomic::AtomicUsize =
                std::sync::atomic::AtomicUsize::new(0);

            let was = PARKED_HELD.swap(parked_now, std::sync::atomic::Ordering::Relaxed);

            if parked_now != 0 || was != 0 {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|since| since.as_secs())
                    .unwrap_or(0);

                if parked_now == 0
                    || PARKED_REPORTED.swap(now, std::sync::atomic::Ordering::Relaxed) != now
                {
                    log::info!(
                        "wgpu-mc: the arena is holding {parked_now} section(s) of baked geometry while it \
                         waits for a slot ({:.1} MB); {} section(s) have been parked over the run and {} \
                         were let go when the view left them behind",
                        parked_bytes as f64 / (1024.0 * 1024.0),
                        mc::chunk::sections_parked(),
                        mc::chunk::sections_parked_abandoned(),
                    );
                }
            }
        }

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
    /// The doubling means a session pays one allocation per growth rather than one per refusal, and the
    /// cap on a single arena is the device's own `max_buffer_size`. See [`ARENA_MEMORY_BUDGET`] for the
    /// bound on all of them together, and this function for what happens when the device says no.
    ///
    /// Grows the arena by appending another buffer, **when a section did not fit and the device will
    /// still make one**.
    ///
    /// # Why the buffer is created before the pool is grown, and inside an error scope
    ///
    /// This used to grow the pool first, create the buffer second, and let a failed `create_buffer` be
    /// whatever wgpu makes of it - which is fatal. A run on a machine whose `max_buffer_size` is 0.31 GB
    /// reached 99% of its first arena with sections being refused, asked for a second of the same size,
    /// and died:
    ///
    /// ```text
    /// arena 82,006,452 of 82,140,000 slot(s) handed out (99%, 312 MB), 4 section(s) refused by the arena
    /// panicked at wgpu-30.0.1/src/backend/wgpu_core.rs:1619     (inside `create_buffer`)
    /// wgpu error: Out of Memory
    /// ```
    ///
    /// Three things were wrong with that and all three are fixed here. The **order**: a pool grown for a
    /// buffer that does not exist is an allocator handing out ranges in a buffer nobody has. The
    /// **error**: an out-of-memory scope around the creation catches it, so the answer becomes
    /// [`SectionStorage::at_capacity`] - "there is no more room, keep Minecraft's own mesh" - which is
    /// the state this path already had for that question. And the **budget**, which is now
    /// [`Scene::arena_memory_budget`], derived from the device rather than written for one.
    ///
    /// See [`ARENA_MEMORY_BUDGET`] for why a budget cannot be *sufficient*: no Vulkan device tells wgpu
    /// how much memory it has, so no arithmetic here can know what the driver will refuse. That is what
    /// the catch is for - the budget decides when to stop trying, and the catch handles being wrong.
    fn grow_arena_if_asked(&self, scene: &Scene) {
        let asked = scene
            .pending_arena_growth
            .swap(0, std::sync::atomic::Ordering::Relaxed);

        if asked == 0 {
            return;
        }

        // **How the ask arrived**, which is the point of the trigger above: a growth asked for while
        // the arena still had room is one applied before a section was refused, and one asked for at a
        // refusal was too late for the section that made it. Logged here rather than once a second
        // because growth happens a handful of times per world, not as a rate.
        log::info!(
            "wgpu-mc: the arena is growing; {} ask(s) so far were made with room still free for the largest section, rather than at a refusal",
            ARENA_GROWTH_ASKED_EARLY.load(std::sync::atomic::Ordering::Relaxed)
        );

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
        let target = mc::chunk::arena_slots(scene.section_storage.read().width().max(0) as u32);
        let budget = scene.arena_memory_budget;
        let mut at_limit = false;

        loop {
            // **Sized by what the view asks for, not by the device's ceiling.** See
            // [`mc::arena_growth_slots`], which is where the arithmetic and the 17 GB it used to ask for
            // are written down. The short version: the ceiling is a *cap*, and asking for it is what
            // killed a run in `create_buffer`.
            let pool_slots = scene.section_storage.read().pool_slots();

            let slots = mc::arena_growth_slots(
                target,
                pool_slots,
                mc::chunk::largest_section_slots(),
                scene.arena_cap_slots,
                budget,
            );

            // Nothing to ask for: no width has been reported and nothing has been meshed, so the pool has
            // no size to grow to and this refusal is one growth cannot answer.
            if slots == 0 {
                at_limit = true;

                break;
            }

            // **The byte budget, which is the bound that was missing.** A per-buffer limit is not a
            // memory bound, and an arena that appends rather than refuses had no other one: a player
            // flying forward grew it until the driver complained - which turned a bug about holes into
            // one about memory. Past this the arena stops growing and `at_capacity` tells the JVM to
            // keep Minecraft's mesh for whatever does not fit, the same answer as at the device's limit.
            //
            // `arena_growth_slots` has already held the request under it, so this is the case where
            // there is no room left even for one more section.
            if pool_slots as u64 * 4 >= budget {
                at_limit = true;

                break;
            }

            // **And what wgpu is already holding**, which is the half the arena cannot see: the two
            // block atlases, the game's own render targets, the staging buffers the uploads go through.
            // The arena's own bytes can be well inside the budget while the device is full, and this is
            // the only number in the process that includes both. It is the same device call the failure
            // path reports, so a log with both in it is one comparison.
            if let Some(report) = self.gpu.device.generate_allocator_report()
                && report.total_reserved_bytes >= budget
            {
                log::warn!(
                    "wgpu-mc: the section arena will not grow: the device already has {} MB \
                     reserved of a {} MB budget ({} MB of it live)",
                    report.total_reserved_bytes / (1024 * 1024),
                    budget / (1024 * 1024),
                    report.total_allocated_bytes / (1024 * 1024),
                );
                at_limit = true;

                break;
            }

            // **The buffer, and the pool only if the device made one.** See this function's own note for
            // the three things that were wrong with the other order.
            let scope = self
                .gpu
                .device
                .push_error_scope(wgpu::ErrorFilter::OutOfMemory);
            let buffer = Arc::new(BindableBuffer::new_deferred(
                self,
                slots as u64 * 4,
                mc::ARENA_USAGE,
                "ssbo",
            ));
            let caught = resolve_error_scope_now(scope.pop());

            let refused = match caught {
                // The device said no. This is the case that used to end the process.
                ScopeResult::Error(error) => {
                    let report = self.gpu.device.generate_allocator_report();

                    log::error!(
                        "wgpu-mc: the section arena asked the device for another {} MB buffer and was \
                         refused: {error}. It stops at {} arena(s) and {} MB, and the sections that do \
                         not fit stay with Minecraft ({} already have). The device reports {} MB \
                         reserved{}.",
                        slots as u64 * 4 / (1024 * 1024),
                        scene.section_storage.read().arena_count(),
                        scene.section_storage.read().pool_slots() as u64 * 4 / (1024 * 1024),
                        scene.section_storage.read().refusals_waiting(),
                        report
                            .as_ref()
                            .map_or("an unknown amount".to_string(), |report| format!(
                                "{} MB",
                                report.total_reserved_bytes / (1024 * 1024)
                            )),
                        // The whole report is a few hundred lines on a big frame and this is one line per
                        // refusal burst, so the total travels and the per-allocation detail does not.
                        if report.is_some() {
                            ", see the arena log line for the rest"
                        } else {
                            ""
                        },
                    );

                    true
                }
                ScopeResult::Clean => false,
                // **A scope that has not resolved yet.** `wgpu-core` resolves one immediately
                // (`Box::pin(ready(scope.error))`), so this is the WebGPU backend's shape rather than
                // this renderer's - and blocking the render thread on a promise to find out is worse
                // than the risk, which is that an allocation that failed is used anyway.
                ScopeResult::Pending => {
                    log::warn!(
                        "wgpu-mc: the out-of-memory scope around the arena's growth did not resolve \
                         immediately, so a failed allocation here would still be fatal"
                    );

                    false
                }
            };

            if refused
                || !scene
                    .section_storage
                    .write()
                    .grow_pool(mc::ARENA_BUFFERS, slots)
            {
                // At the buffer limit, or the device would not make another: the two states where a
                // refusal is permanent, and the JVM is told so that it keeps Minecraft's mesh for the
                // sections this side cannot take.
                at_limit = true;

                break;
            }

            let mut buffers = scene.chunk_buffers.load().as_ref().clone();
            buffers.push(buffer);
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
            "wgpu-mc: the section arena was full, so it grew to {} arena(s) holding {} slot(s), {} MB \
             total ({} slot(s) handed out, the largest section meshed so far {} slot(s), and the view \
             asked for {target} slot(s))",
            stored.arena_count(),
            stored.pool_slots(),
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

#[cfg(test)]
mod arena_budget_tests {
    use super::*;
    use crate::mc::arena_memory_budget;

    /// **The budget is whichever of the device and the policy is smaller**, which is the whole of the fix
    /// for a constant that was calibrated on somebody else's machine.
    ///
    /// The constant said `ARENA_BUFFERS` x `max_buffer_size` "on the machine this was measured on", where
    /// a buffer is 1.31 GB. That is a 5 GB budget there, and a 5 GB budget here, where a buffer is
    /// 0.31 GB - so it is fifteen arenas away and the check that uses it never fires, while
    /// `ARENA_BUFFERS` still allows four, which is more than the device handed over.
    #[test]
    fn the_arena_budget_is_the_smaller_of_the_device_and_the_policy() {
        // A device that will make a small buffer: four of them is what binds.
        assert_eq!(arena_memory_budget(300_000_000), 1_200_000_000);

        // The device this was found on, whose `max_buffer_size` is 0.31 GB. The budget is four of those
        // rather than the 5 GB written for 1.31 GB buffers.
        assert_eq!(arena_memory_budget(328_560_000), 1_314_240_000);
        assert!(arena_memory_budget(328_560_000) < crate::mc::ARENA_MEMORY_BUDGET);

        // A device that will make a very large one: the policy binds, so the arena is not allowed to grow
        // to several times the ceiling by arithmetic alone.
        assert_eq!(
            arena_memory_budget(1 << 40),
            crate::mc::ARENA_MEMORY_BUDGET,
            "a device with a huge per-buffer limit must not raise the total budget with it"
        );
        assert_eq!(
            arena_memory_budget(u64::MAX),
            crate::mc::ARENA_MEMORY_BUDGET
        );

        // And the multiply saturates rather than wrapping, which is the case that would otherwise make
        // `u64::MAX` a *tiny* budget after an overflow check.
        assert!(arena_memory_budget(u64::MAX) >= crate::mc::ARENA_MEMORY_BUDGET);
    }

    /// **An error scope is read without being awaited** - one poll - and "not yet" is not "no error".
    ///
    /// The catch that keeps a refused arena from ending the process depends on the first two: an
    /// allocation that failed has to be visible right after the call that made it. The third case is why
    /// `ScopeResult` has three arms rather than being an `Option`: reading a `Pending` scope as "clean"
    /// would be a guess in the one direction that matters.
    #[test]
    fn an_error_scope_is_read_without_awaiting_it() {
        assert!(matches!(
            resolve_error_scope_now(std::future::ready(None)),
            ScopeResult::Clean
        ));

        assert!(matches!(
            resolve_error_scope_now(std::future::ready(Some(wgpu::Error::OutOfMemory {
                source: Box::new(std::fmt::Error),
            }))),
            ScopeResult::Error(_)
        ));

        assert!(
            matches!(
                resolve_error_scope_now(std::future::pending::<Option<wgpu::Error>>()),
                ScopeResult::Pending
            ),
            "a scope that has not resolved must not be read as one that found nothing"
        );
    }
}

#[cfg(test)]
mod arena_growth_tests {
    use crate::mc::{ARENA_MEMORY_BUDGET, arena_growth_slots};

    /// **The bug, in the numbers a run actually printed.**
    ///
    /// `arena_cap_slots` was `u32::MAX` on the machine that died, because `max_buffer_size` is past 16 GB
    /// there. Every growth asked for that many slots - **seventeen gigabytes in one buffer** - and the
    /// driver refused. The historical line from the machine the code was written for says what it used to
    /// ask for and why the ceiling looked reasonable: `536870911` slots, 2 GB, on a device whose
    /// `max_buffer_size` is exactly that.
    #[test]
    fn an_appended_arena_is_sized_by_the_view_not_by_the_device_ceiling() {
        let budget = ARENA_MEMORY_BUDGET;

        // The twelve-chunk world of the failing run: 82,140,000 slots of pool, the largest section 77,132
        // slots, and a device that will make a 17 GB buffer.
        let slots = arena_growth_slots(82_140_000, 82_140_000, 77_132, u32::MAX, budget);

        assert_eq!(
            slots, 82_140_000,
            "the request is the view's own size; it must not become the device's ceiling"
        );
        assert!(
            slots as u64 * 4 < budget,
            "one appended arena must fit inside the budget on its own"
        );

        // The same case on the machine the code was written for, whose ceiling is 2 GB: the cap binds
        // when the view asks for more than the device will make.
        assert_eq!(
            arena_growth_slots(3_000_000_000, 82_140_000, 77_132, 536_870_911, budget),
            536_870_911,
            "a view larger than the device's per-buffer limit is clamped to it"
        );
    }

    /// The floor and the two ways to ask for nothing.
    #[test]
    fn an_appended_arena_is_at_least_one_section_and_never_past_the_budget() {
        // A view smaller than the largest section meshed: the section is the floor, because an arena that
        // cannot hold one is an arena that cannot answer the refusal it is being added for.
        assert_eq!(
            arena_growth_slots(1_000, 0, 77_132, u32::MAX, ARENA_MEMORY_BUDGET),
            77_132
        );

        // No width reported and nothing meshed: no size to ask for.
        assert_eq!(
            arena_growth_slots(0, 0, 0, u32::MAX, ARENA_MEMORY_BUDGET),
            0
        );

        // A pool already at the budget allows no room at all, whatever the view wants.
        assert_eq!(
            arena_growth_slots(
                650_000_000,
                (ARENA_MEMORY_BUDGET / 4) as u32,
                77_132,
                u32::MAX,
                ARENA_MEMORY_BUDGET
            ),
            0
        );

        // And a pool one section short of the budget gets exactly the room that is left, not the view's
        // size - the budget is the bound on the total and it is the binding one there.
        let pool = (ARENA_MEMORY_BUDGET / 4) as u32 - 1_000_000;
        let slots = arena_growth_slots(650_000_000, pool, 77_132, u32::MAX, ARENA_MEMORY_BUDGET);

        assert!(
            slots <= 1_000_000,
            "the request went past the budget: {slots}"
        );
        assert!(
            slots >= 77_132,
            "the request fell below one section: {slots}"
        );
    }
}
