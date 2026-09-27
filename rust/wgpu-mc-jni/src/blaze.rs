use crate::device::{
    BlazePipeline, LIVE_BIND_GROUP_COUNT, count_bind_groups, count_cache_hit, count_cache_miss,
    count_draw, count_numbered, count_pipeline_bind, count_tableless, count_vertices, fan_indices,
    log_pipeline_once, quad_indices, trace_draw, trace_pipeline,
};
use glsl::syntax::TypeSpecifierNonArray;
use log::info;
use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char};
use std::fmt::{Debug, Display, Formatter};
use std::ops::{Deref, Index, Range};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::vec::IntoIter;
use wgpu_mc::wgpu::{BufferAddress, BufferSize, IndexFormat};
use wgpu_mc::{WmRenderer, wgpu};

#[repr(C)]
pub struct RawArray<T: Sized> {
    contents: *const T,
    size: u64,
}

impl<T> Clone for RawArray<T>
where
    T: Clone,
{
    fn clone(&self) -> Self {
        let cloned_contents: Vec<T> = self.iter().cloned().collect();

        assert_eq!(cloned_contents.len() as u64, self.size);

        Self {
            contents: Box::into_raw(cloned_contents.into_boxed_slice())
                .to_raw_parts()
                .0 as *const _,
            size: self.size,
        }
    }
}

impl<T> RawArray<T> {
    pub(crate) fn iter(&self) -> IntoIter<&T> {
        (0..self.size as usize)
            .map(|index| &self[index])
            .collect::<Vec<&T>>()
            .into_iter()
    }

    pub fn len(&self) -> usize {
        self.size as usize
    }

    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    /// Builds an array that owns a leaked copy of `items`.
    ///
    /// Nothing in this FFI layer frees what it hands out - [`Clone`] above allocates and leaks in
    /// exactly the same way - so an array built here stays valid for the lifetime of the process,
    /// which is what lets a `BlazePipeline` keep its own copy of a descriptor.
    pub fn from_vec(items: Vec<T>) -> Self {
        let size = items.len() as u64;

        Self {
            contents: Box::into_raw(items.into_boxed_slice()) as *const T,
            size,
        }
    }
}

impl<T> IntoIterator for RawArray<T> {
    type Item = T;
    type IntoIter = IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        (0..self.size as usize)
            .map(|index| unsafe { std::ptr::read(self.contents.add(index)) })
            .collect::<Vec<T>>()
            .into_iter()
    }
}

impl<T> Debug for RawArray<T>
where
    T: Debug,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawArray")
            .field("size", &self.size)
            .field_with("contents", |f| {
                let mut list = f.debug_list();

                for i in 0..self.size as usize {
                    list.entry(&self[i]);
                }

                list.finish()
            })
            .finish()
    }
}

impl<T> Index<usize> for RawArray<T> {
    type Output = T;

    fn index(&self, index: usize) -> &Self::Output {
        assert!(index < self.size as usize);

        unsafe { self.contents.add(index).as_ref_unchecked() }
    }
}

#[repr(C)]
#[derive(Debug)]
pub struct BlazeAttachmentDescriptor<'a, ClearVal: Sized + Debug> {
    pub texture_view: &'a wgpu::TextureView,
    pub clear_value: Option<&'a ClearVal>,
}

#[repr(C)]
#[derive(Debug)]
pub struct BlazeRenderPassDescriptor<'a> {
    pub attachments: &'a RawArray<BlazeAttachmentDescriptor<'a, [f32; 4]>>,
    pub depth_attachment: Option<&'a BlazeAttachmentDescriptor<'a, f64>>,
}

#[repr(C)]
#[derive(Clone, Debug)]
pub struct VertexFormatElement {
    pub offset: u64,
    pub format: GpuFormat,
    pub name: FfiStr,
}

/// How many bindings one draw may carry.
///
/// A draw now arrives as one struct, so this is a hard bound rather than a starting capacity; the
/// widest plan this renderer has seen uses nine slots (three uniforms and three combined samplers).
/// A plan that exceeds it is refused when its pipeline is compiled, not truncated silently.
pub const MAX_DRAW_BINDINGS: usize = 32;

/// How many vertex buffer slots a draw may carry.
pub const MAX_VERTEX_BUFFERS: usize = 8;

/// How many bind group sets a plan may have, and so how many offset lists a pass keeps room for.
///
/// The pass used to hold a `Vec<Vec<DynamicOffset>>` it cleared and refilled per draw, which is two
/// levels of allocation on a path that runs thousands of times a frame. A plan's sets are fixed when
/// its pipeline is compiled, so the room is fixed here too and the draw fills it in place.
pub const MAX_DRAW_SETS: usize = 8;

/// What [`DrawCall::bindings`] holds in one slot.
pub const DRAW_BINDING_NONE: u32 = 0;
pub const DRAW_BINDING_BUFFER: u32 = 1;
pub const DRAW_BINDING_TEXTURE: u32 = 2;
pub const DRAW_BINDING_SAMPLER: u32 = 3;

/// One binding of a draw, as the JVM has it.
///
/// The pointers are the addresses of the boxes Rust handed out - the same ones
/// [`invalidate_bind_group_cache`] is called with - so they are the resources themselves rather
/// than a copy of a handle somewhere.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct DrawBinding {
    /// One of the `DRAW_BINDING_*` constants. Zero means "nothing bound in this slot".
    pub kind: u32,
    pub _pad: u32,
    pub resource: *const u8,
    /// Buffer slice start, in bytes. Zero for the other kinds.
    pub offset: u64,
    /// Buffer slice length, in bytes. Zero for the other kinds.
    pub length: u64,
}

/// One vertex buffer of a draw: the same fields `set_vertex_buffer` took on its own.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct DrawVertexBuffer {
    pub buffer: *const u8,
    pub offset: u64,
    pub size: u64,
}

/// Everything one draw needs, packed so that a draw is a single call across the ABI.
///
/// It replaces a sequence that was one call per binding plus one per state change plus one for the
/// draw itself: for a chunk section that was a pipeline bind, three uniform binds, two texture
/// binds, two buffer binds and the draw. The bindings are positional - slot `i` is the `i`-th
/// binding of the pipeline's plan, which [`pipeline_bindings`] hands the JVM once per pipeline - so
/// nothing on this path hashes or compares a name.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct DrawCall {
    pub pipeline: *const BlazePipeline,
    /// Which vertex buffer slots are bound; slot `i` is read when bit `i` is set.
    pub vertex_buffer_mask: u32,
    /// 0 = none, 1 = u16, 2 = u32. The index buffer is only read when this is set.
    pub index_format: u32,
    pub indexed: u32,
    /// How many binding slots the JVM's plan for this pipeline has, which is the array the slots
    /// index into. The plan itself decides what each slot means; this is what says the table is
    /// within the bounds both sides agreed on.
    pub bindings_len: u32,
    /// First vertex, or first index when indexed.
    pub first: u32,
    /// Vertex count, or index count when indexed.
    pub count: u32,
    pub base_vertex: i32,
    pub instance_count: u32,
    pub index_buffer: *const u8,
    pub vertex_buffers: [DrawVertexBuffer; MAX_VERTEX_BUFFERS],
    pub bindings: [DrawBinding; MAX_DRAW_BINDINGS],
    /// The JVM's number for "this set of bindings", or zero when it did not send one.
    ///
    /// A combination is everything a bind group is built from except the offsets that travel with the
    /// draw: the plan and, per slot, the resource, the length and any offset that gets baked in. The
    /// JVM resolves it to a small integer once and hands that over per draw, which is what turns the
    /// per-draw question from a walk over every binding into an integer comparison. Zero means "work
    /// it out from the table below", which is what a JVM that does not number combinations sends.
    pub combo: u32,
    /// Whether [`Self::bindings`] carries what [`Self::combo`] left out.
    ///
    /// The number is the identity of the bind groups, so the table is only ever read for the offsets
    /// of the bindings whose offsets travel with the draw - and a plan whose bindings all bake theirs
    /// (no [`PlanBinding::dynamic`] slot anywhere) sends zero here, which is a draw this side answers
    /// without touching the table at all. When the number is new *and* this is zero, the draw is
    /// refused (`draw_call` returns false) rather than bound from a table that describes whatever the
    /// JVM drew last; the JVM then sends the bindings with it and draws again.
    pub bindings_present: u32,
}

impl Default for DrawCall {
    fn default() -> Self {
        Self {
            pipeline: std::ptr::null(),
            vertex_buffer_mask: 0,
            index_format: 0,
            indexed: 0,
            bindings_len: 0,
            first: 0,
            count: 0,
            base_vertex: 0,
            instance_count: 1,
            index_buffer: std::ptr::null(),
            vertex_buffers: [DrawVertexBuffer {
                buffer: std::ptr::null(),
                offset: 0,
                size: 0,
            }; MAX_VERTEX_BUFFERS],
            bindings: [DrawBinding {
                kind: DRAW_BINDING_NONE,
                _pad: 0,
                resource: std::ptr::null(),
                offset: 0,
                length: 0,
            }; MAX_DRAW_BINDINGS],
            combo: 0,
            bindings_present: 0,
        }
    }
}

/// One binding of a pipeline's plan, in slot order.
#[repr(C)]
#[derive(Debug)]
pub struct PlanBinding {
    /// The name the JVM binds this slot under. A combined sampler is two slots under two names,
    /// `X_wm_texshim` and `X_wm_sampler`.
    pub name: FfiStr,
    /// The name the pipeline declared it under: the same one for a uniform, and `X` for *both*
    /// halves of a combined sampler - which is the name Minecraft actually binds, so this is what
    /// lets the JVM resolve `bindTexture("X", ...)` to the pair of slots.
    pub declared_name: FfiStr,
    /// The set and binding number the layout declares.
    pub set: u32,
    pub binding: u32,
    /// One of the `DRAW_BINDING_*` constants: what has to be bound here.
    pub kind: u32,
    /// Whether this binding's offset may travel with the draw instead of being baked into the bind
    /// group, which is a uniform binding of a plan that has one.
    ///
    /// A slot marked here is the one thing the JVM leaves out of the combination it numbers a draw
    /// under ([`DrawCall::combo`]): the number is then the identity of the *bind groups*, and the
    /// offsets of the slots this side still has to bake are folded back into the key instead. The
    /// flag is deliberately generous - it says "may", not "will" - because a slot that turns out to
    /// be baked is covered by that fold.
    pub dynamic: u32,
}

impl PlanBinding {
    pub fn kind_of(resource: &PlannedResource) -> u32 {
        match resource {
            PlannedResource::Uniform { .. } | PlannedResource::Storage => DRAW_BINDING_BUFFER,
            PlannedResource::Texture { .. } => DRAW_BINDING_TEXTURE,
            PlannedResource::Sampler => DRAW_BINDING_SAMPLER,
        }
    }
}

/// Whether a binding's offset may travel with the draw rather than be baked into the bind group.
///
/// A uniform binding of a plan that has any: it is the only binding whose offset `set_bind_group`
/// can be handed per draw (see [`dynamic_offset`]), and so the only one whose offset a combination
/// may leave out. This is what [`PlanBinding::dynamic`] reports, and the two sides read it as one
/// rule - a draw that arrives without its binding table is one whose plan has no such slot, which is
/// also why the offsets all being zero is not an assumption but the same statement.
///
/// The flag is about what *can* travel, not what does: whether a given offset is aligned enough to
/// travel is decided per draw, and the ones that turn out to be baked are folded into the key.
fn may_travel(has_uniforms: bool, resource: &PlannedResource) -> bool {
    has_uniforms && matches!(resource, PlannedResource::Uniform { .. })
}

/// A render pass, and the bind groups it has built.
///
/// The groups live here rather than in a box the JVM owns and frees, because the JVM has nothing
/// to free them *with*: they are only ever replaced by the next draw's, and the pass is what knows
/// when the pass is over. That removes a `Box` allocation and a `drop_bind_groups` call from every
/// draw, and moves the free to `close()`, where it belongs.
pub struct BlazeRenderPass {
    pass: wgpu::RenderPass<'static>,
    /// The set the last draw used. `None` until the first draw.
    groups: Option<Arc<CachedBindGroups>>,
    /// The key [groups] was looked up under, so a run of draws with the same bindings does not even
    /// touch the cache.
    key: u64,
    /// The dynamic offsets of each set, filled in place per draw from the call's bindings - the
    /// offsets live in the `DrawBinding` the JVM wrote, so nothing has to be collected to hand them
    /// to `set_bind_group`. Only the first `offsets_len[set]` entries are used.
    offsets: [[wgpu::DynamicOffset; MAX_DRAW_BINDINGS]; MAX_DRAW_SETS],
    offsets_len: [usize; MAX_DRAW_SETS],
    /// The pipeline the pass currently has bound, so a run of draws under one pipeline binds it
    /// once - and so the topology does not have to live in a global.
    pipeline: *const BlazePipeline,
    /// The bindings that were emitted for the *current* pipeline, by slot, as the JVM last wrote
    /// them; kept so a pipeline change can re-emit them, since a slot means something different
    /// under a different plan.
    ///
    /// Written and never read: the re-emit it was added for reads the plan's slots back out of the
    /// draw instead. Kept as the place that state belongs when it is needed.
    #[allow(dead_code)]
    emitted: u32,
}

impl BlazeRenderPass {
    pub fn new(pass: wgpu::RenderPass<'static>) -> Self {
        Self {
            pass,
            groups: None,
            key: 0,
            offsets: [[0; MAX_DRAW_BINDINGS]; MAX_DRAW_SETS],
            offsets_len: [0; MAX_DRAW_SETS],
            pipeline: std::ptr::null(),
            emitted: 0,
        }
    }

    /// The pass itself, for the operations that are not draw-related - the scissor rectangle.
    pub fn pass_mut(&mut self) -> &mut wgpu::RenderPass<'static> {
        &mut self.pass
    }

    /// Whether a draw has been recorded yet, which is what the diagnostics count.
    pub fn has_drawn(&self) -> bool {
        self.groups.is_some()
    }
}

/// The bind groups themselves, shared between every set that names them.
///
/// `Arc` rather than a clone: a cached set is handed out to every draw that wants it, and a draw
/// that has been recorded still refers to it after the cache has moved on.
struct CachedBindGroups {
    groups: Vec<wgpu::BindGroup>,
    /// Every buffer, view and sampler address this set refers to, so it can be dropped the moment
    /// one of them is freed - the allocator hands the same address out again, and a cache keyed on
    /// addresses would otherwise answer with a bind group built for the resource that used to be
    /// there.
    addresses: Vec<usize>,
}

/// The cache's entries, one set of bind groups per key that has been built on this thread.
struct BindGroupCache {
    /// `None` until something is stored, so a thread that never binds anything allocates nothing.
    entries: Option<FxHashMap<u64, CacheEntry>>,
    /// Bumped on every hit and every store; the least recently used entry is the lowest one. A plain
    /// field rather than an atomic: only the thread that owns the cache can reach it, and the lock
    /// that used to guard it (and the global `fetch_add` that used to number it) were two atomics
    /// per draw spent on a counter nobody else reads.
    ticks: u64,
    /// The [CACHE_EPOCH] this cache was last reconciled with.
    epoch: u64,
}

impl BindGroupCache {
    const fn new() -> Self {
        Self {
            entries: None,
            ticks: 0,
            epoch: 0,
        }
    }

    /// Drops everything if another thread has invalidated a cached set since the last lookup.
    ///
    /// A set is only reachable from the thread that built it, so an invalidation from anywhere else
    /// (the cleaner that frees a collected pipeline, a reload) cannot name the entry to remove. It
    /// bumps the epoch instead, and the whole cache goes on the next lookup: conservative, and one
    /// relaxed load per draw.
    fn refresh(&mut self) {
        let epoch = CACHE_EPOCH.load(Ordering::Relaxed);
        if self.epoch != epoch {
            self.epoch = epoch;
            self.entries = None;
        }
    }
}

thread_local! {
    /// One cache per thread.
    ///
    /// Draws are recorded on the render thread and nowhere else, so the `Mutex` this used to be
    /// bought nothing but a lock acquisition per draw - there was never another thread to contend
    /// with. A `RefCell` in a thread local is what the access pattern actually is.
    static BIND_GROUP_CACHE: std::cell::RefCell<BindGroupCache> =
        const { std::cell::RefCell::new(BindGroupCache::new()) };
}

/// Bumped when a cached set may have become invalid on a thread other than the one that holds it.
static CACHE_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The thread that records draws, claimed by the first lookup.
static RENDER_THREAD: std::sync::OnceLock<std::thread::ThreadId> = std::sync::OnceLock::new();

/// How many sets to keep. Evicted least-recently-used, which is a scan of a few hundred entries on
/// the rare miss and nothing at all on a hit.
const BIND_GROUP_CACHE_LIMIT: usize = 256;

struct CacheEntry {
    cached: Arc<CachedBindGroups>,
    /// Bumped on every use, so the least recently used one can be found.
    tick: u64,
}

/// The count follows the bind groups themselves, which go away when the cache evicts them and the
/// last recorded draw that used them has been dropped.
impl Drop for CachedBindGroups {
    fn drop(&mut self) {
        LIVE_BIND_GROUP_COUNT.fetch_sub(self.groups.len() as u64, Ordering::Relaxed);
    }
}

/// Forgets every cached set that refers to [address], which is about to be freed.
///
/// Called for buffers, texture views and samplers as they are dropped. The allocator reuses these
/// addresses, so a set kept past the free could be handed out for a resource that is not the one it
/// was built for - which would draw with the wrong buffer rather than fail loudly.
pub fn invalidate_bind_group_cache(address: usize) {
    // On the render thread the entries themselves are reachable, so exactly the sets that name this
    // address go; anywhere else the epoch does it, and the next lookup clears the cache.
    if RENDER_THREAD.get() == Some(&std::thread::current().id()) {
        BIND_GROUP_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();

            if let Some(entries) = cache.entries.as_mut() {
                entries.retain(|_, entry| !entry.cached.addresses.contains(&address));
            }
        });

        return;
    }

    CACHE_EPOCH.fetch_add(1, Ordering::Relaxed);
}

/// Takes a set out of the cache, or `None` if it has never been built.
fn take_cached_bind_groups(key: u64) -> Option<Arc<CachedBindGroups>> {
    claim_render_thread();

    BIND_GROUP_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        cache.refresh();

        let BindGroupCache { entries, ticks, .. } = &mut *cache;
        let entries = entries.as_mut()?;

        *ticks += 1;
        let tick = *ticks;

        let entry = entries.get_mut(&key)?;
        entry.tick = tick;

        count_cache_hit();

        Some(entry.cached.clone())
    })
}

/// Stores a freshly built set, evicting the least recently used one if the cache is full.
fn store_cached_bind_groups(key: u64, cached: Arc<CachedBindGroups>) {
    claim_render_thread();

    BIND_GROUP_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        cache.refresh();

        let BindGroupCache { entries, ticks, .. } = &mut *cache;
        let entries = entries.get_or_insert_with(FxHashMap::default);

        *ticks += 1;
        let tick = *ticks;

        entries.insert(key, CacheEntry { cached, tick });

        while entries.len() > BIND_GROUP_CACHE_LIMIT {
            let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.tick)
                .map(|(key, _)| *key)
            else {
                break;
            };

            entries.remove(&oldest);
        }
    });
}

/// Notes which thread records draws, so an invalidation can tell whether it can reach the cache.
fn claim_render_thread() {
    let _ = RENDER_THREAD.set(std::thread::current().id());
}

/// Frees a set of bind groups the JVM is finished with.
///
/// Only the set is dropped: the bind groups themselves live in the cache, and in every recorded
/// draw that referred to them.
/// What a binding holds, as far as the wgpu layout, the bind group and the shader all need to know.
#[derive(Debug, Clone, PartialEq)]
pub enum PlannedResource {
    /// A uniform buffer. `min_size` is the block size the shader declares, which is what the layout
    /// and the binding both use: leaving it unset let wgpu derive the range from whatever the JVM
    /// passed, and that number was `roundToward(length, 16)` - which can run past the end of the
    /// buffer and turn a slice into an out-of-bounds binding.
    Uniform { min_size: Option<u64> },
    /// A texel buffer, which wgpu sees as a read-only storage buffer with a runtime-sized array.
    Storage,
    /// The texture half of a shimmed combined sampler.
    Texture { cube: bool },
    /// The sampler half of a shimmed combined sampler.
    Sampler,
}

/// One binding, in the order it appears in its set.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedBinding {
    /// The name the JVM binds under, which is what the builder is keyed by.
    pub name: String,
    /// The name the pipeline description used, which is what the shader declared.
    pub declared_name: String,
    pub resource: PlannedResource,
    pub binding: u32,
}

/// Everything about a pipeline's bindings, in one place.
///
/// The three things that have to agree about a binding - the wgpu `BindGroupLayout`, the GLSL
/// `layout(binding = N)` annotations, and the entries and dynamic offsets of the bind group built at
/// draw time - used to be derived separately, by three walks over three different descriptions, and
/// they disagreed: a bind group carried an offset list in one order while the layout expected
/// another, and a slice that was aligned for a 64-byte alignment was treated as unaligned for a
/// 256-byte one. This is the one description they are all derived from.
#[derive(Debug, Clone)]
pub struct BindGroupPlan {
    pub name: String,
    /// One entry per bind group, in binding order.
    pub sets: Vec<Vec<PlannedBinding>>,
    /// Whether this plan has any uniform bindings, which are the ones that can carry an offset
    /// dynamically; without one there is nothing for the offsets list to describe.
    pub has_uniforms: bool,
    /// What this plan contributes to a draw's key, computed once when the plan is built.
    ///
    /// The key is a fold over the bindings rather than a hash of them, and the plan's own identity
    /// has to be part of it - two pipelines with the same slots and the same resources still need
    /// their own bind groups, because a bind group belongs to one layout. Folding the name in on
    /// every draw would walk the string; folding it in here walks it once per compiled pipeline.
    pub salt: u64,
}

/// Folds `value` into a running key.
///
/// The key used to be an `FxHasher` walk over every binding: a hash function with its rounds, run
/// once per draw over a handful of small integers. All it has to do is tell two binding tables
/// apart, and a rotate-and-multiply fold does that with a few instructions and no finalisation -
/// which is what "the key is an integer comparison, not a hash" means in `draw_call`.
#[inline]
fn fold(key: u64, value: u64) -> u64 {
    (key.rotate_left(5) ^ value).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

impl BindGroupPlan {
    /// Numbers the bindings of a pipeline description, before anything is known about the shaders.
    ///
    /// The numbering is the whole point of doing this first: the GLSL annotations, the wgpu layouts
    /// and the bind groups all take their binding numbers from here, so they cannot drift apart.
    pub fn number(descriptor: &RenderPipeline) -> Self {
        let mut sets = Vec::with_capacity(descriptor.bind_group_layouts.len());

        for layout in descriptor.bind_group_layouts.iter() {
            let mut bindings = Vec::with_capacity(layout.entries.len());
            let mut next = 0u32;

            for entry in layout.entries.iter() {
                let name = entry.name.to_string();

                let (planned, count) = match entry.type_ {
                    UniformType::UBO => (
                        PlannedBinding {
                            name: name.clone(),
                            declared_name: name,
                            resource: PlannedResource::Uniform { min_size: None },
                            binding: next,
                        },
                        1,
                    ),
                    UniformType::TexelBuffer => (
                        PlannedBinding {
                            name: name.clone(),
                            declared_name: name,
                            resource: PlannedResource::Storage,
                            binding: next,
                        },
                        1,
                    ),
                    UniformType::Sampler => {
                        // A combined sampler is two bindings and two names, because WGSL has no
                        // combined sampler; the shader declares both after the shim.
                        bindings.push(PlannedBinding {
                            name: format!("{name}{}", crate::preprocessing::SHIM_TEXTURE_SUFFIX),
                            declared_name: name.clone(),
                            resource: PlannedResource::Texture { cube: false },
                            binding: next,
                        });
                        bindings.push(PlannedBinding {
                            name: format!("{name}{}", crate::preprocessing::SHIM_SAMPLER_SUFFIX),
                            declared_name: name,
                            resource: PlannedResource::Sampler,
                            binding: next + 1,
                        });

                        next += 2;
                        continue;
                    }
                };

                bindings.push(planned);
                next += count;
            }

            sets.push(bindings);
        }

        let has_uniforms = sets
            .iter()
            .flatten()
            .any(|binding| matches!(binding.resource, PlannedResource::Uniform { .. }));

        let name = descriptor.name.to_string();
        let salt = {
            let mut salt = fold(0, name.len() as u64);
            for byte in name.as_bytes() {
                salt = fold(salt, *byte as u64);
            }
            fold(salt, has_uniforms as u64)
        };

        Self {
            name,
            sets,
            has_uniforms,
            salt,
        }
    }

    /// The name-to-location map the GLSL annotator needs, which is how the shader's own
    /// `layout(binding = N)` numbers come from this plan rather than from a walk of their own.
    pub fn shader_locations(&self) -> HashMap<String, (u32, u32)> {
        let mut locations = HashMap::new();

        for (set, bindings) in self.sets.iter().enumerate() {
            for binding in bindings {
                locations.insert(binding.name.clone(), (set as u32, binding.binding));

                // A texture buffer is declared by its own name in the shader, not by a shim name.
                if binding.resource == PlannedResource::Storage {
                    locations.insert(binding.declared_name.clone(), (set as u32, binding.binding));
                }
            }
        }

        locations
    }

    /// Adds the uniform blocks the shaders declared that the pipeline never listed.
    ///
    /// They are appended to set 0 with the binding numbers `add_implicit_uniforms` reserved for
    /// them, which is what makes the layout and the annotations agree about a block the pipeline
    /// itself knows nothing about.
    pub fn add_implicit_uniforms(&mut self, implicit: &[(String, u32)]) {
        if implicit.is_empty() {
            return;
        }

        if self.sets.is_empty() {
            self.sets.push(Vec::new());
        }

        for (name, binding) in implicit {
            self.sets[0].push(PlannedBinding {
                name: name.clone(),
                declared_name: name.clone(),
                resource: PlannedResource::Uniform { min_size: None },
                binding: *binding,
            });
        }

        self.sets[0].sort_by_key(|binding| binding.binding);
    }

    /// Fills in what kind of sampler each combined sampler turned out to be.
    pub fn apply_sampler_types(&mut self, types: &HashMap<String, TypeSpecifierNonArray>) {
        for bindings in self.sets.iter_mut() {
            for binding in bindings.iter_mut() {
                if let PlannedResource::Texture { cube } = binding.resource
                    && let Some(sampler_type) = types.get(&binding.declared_name)
                {
                    binding.resource = PlannedResource::Texture {
                        cube: matches!(sampler_type, TypeSpecifierNonArray::SamplerCube),
                    };
                    let _ = cube;
                }
            }
        }
    }

    /// Fills in the block sizes the shaders declare, from naga's view of the preprocessed GLSL.
    pub fn apply_block_sizes(&mut self, sizes: &HashMap<String, u64>) {
        for bindings in self.sets.iter_mut() {
            for binding in bindings.iter_mut() {
                if let PlannedResource::Uniform { .. } = binding.resource
                    && let Some(size) = sizes.get(&binding.declared_name)
                {
                    binding.resource = PlannedResource::Uniform {
                        min_size: Some(*size),
                    };
                }
            }
        }
    }

    /// The wgpu layouts, one per set, derived from the plan and nothing else.
    pub fn create_layouts(&self, device: &wgpu::Device) -> Vec<wgpu::BindGroupLayout> {
        self.sets
            .iter()
            .map(|bindings| {
                let entries = bindings
                    .iter()
                    .map(|binding| wgpu::BindGroupLayoutEntry {
                        binding: binding.binding,
                        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                        ty: match &binding.resource {
                            PlannedResource::Uniform { min_size } => wgpu::BindingType::Buffer {
                                ty: wgpu::BufferBindingType::Uniform,
                                // Minecraft re-binds a uniform slice for every draw it makes, so a
                                // set of bind groups is shared between draws that differ only in
                                // where those slices start.
                                has_dynamic_offset: true,
                                min_binding_size: min_size.and_then(BufferSize::new),
                            },
                            PlannedResource::Storage => wgpu::BindingType::Buffer {
                                ty: wgpu::BufferBindingType::Storage { read_only: true },
                                // DX12 has no offset for a storage descriptor, so these are static.
                                has_dynamic_offset: false,
                                min_binding_size: None,
                            },
                            PlannedResource::Texture { cube } => wgpu::BindingType::Texture {
                                sample_type: Default::default(),
                                view_dimension: if *cube {
                                    wgpu::TextureViewDimension::Cube
                                } else {
                                    wgpu::TextureViewDimension::D2
                                },
                                multisampled: false,
                            },
                            PlannedResource::Sampler => {
                                wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering)
                            }
                        },
                        count: None,
                    })
                    .collect::<Vec<_>>();

                device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some(&self.name),
                    entries: &entries,
                })
            })
            .collect()
    }

    /// How many dynamic offsets this plan's layouts expect, per set.
    pub fn dynamic_counts(&self) -> Vec<usize> {
        self.sets
            .iter()
            .map(|bindings| {
                bindings
                    .iter()
                    .filter(|binding| matches!(binding.resource, PlannedResource::Uniform { .. }))
                    .count()
            })
            .collect()
    }
}

/// A binding a draw did not fill in, which is what an out-of-range slot is read as.
const NO_BINDING: DrawBinding = DrawBinding {
    kind: DRAW_BINDING_NONE,
    _pad: 0,
    resource: std::ptr::null(),
    offset: 0,
    length: 0,
};

/// The plan's bindings in slot order, which is the order [`DrawCall::bindings`] is indexed by.
///
/// Called once per compiled pipeline. The JVM turns the result into its own name-to-slot table, and
/// from then on nothing on the draw path hashes or compares a name. Returns 0 when the table the
/// caller offered is too small, which is a hard error rather than a truncation: a draw with bindings
/// in the wrong slots renders nonsense instead of failing.
#[unsafe(no_mangle)]
pub extern "C" fn pipeline_bindings(
    pipeline: &BlazePipeline,
    out: &mut RawArray<PlanBinding>,
) -> u32 {
    let capacity = out.size as usize;
    let mut slot = 0usize;

    for (set, bindings_in_set) in pipeline.plan.sets.iter().enumerate() {
        for binding in bindings_in_set.iter() {
            if slot >= capacity {
                log::error!(
                    "wgpu-mc: {} has more than {capacity} bindings; the JVM's slot table is too small",
                    pipeline.name
                );
                return 0;
            }

            let entry = PlanBinding {
                name: FfiStr::from(binding.name.as_str()),
                declared_name: FfiStr::from(binding.declared_name.as_str()),
                set: set as u32,
                binding: binding.binding,
                kind: PlanBinding::kind_of(&binding.resource),
                dynamic: u32::from(may_travel(pipeline.plan.has_uniforms, &binding.resource)),
            };

            // Safety: `slot < capacity`, and the caller guarantees `capacity` writable entries.
            unsafe { std::ptr::write(out.contents.add(slot).cast_mut(), entry) };

            slot += 1;
        }
    }

    slot as u32
}

/// Whether a pipeline is one the `wgpu-trace-plan` file asks for.
///
/// Both per-draw traces read the same file, so the JVM's line and this side's line describe the same
/// draws - and a trace that names one family is what keeps a line per draw from being a log that is
/// unusable within a second.
fn trace_filter_matches(pipeline_name: &str) -> bool {
    static FILTER: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

    let filter = FILTER.get_or_init(|| {
        std::fs::read_to_string("wgpu-trace-plan")
            .ok()
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
    });

    filter
        .as_ref()
        .is_none_or(|filter| pipeline_name.contains(filter.as_str()))
}

/// Diagnostics: what every texture slot of one draw carries, in slot order.
///
/// The slot table is walked exactly as `bind_groups_for_call` walks it, so the line says what the
/// bind group built from this draw holds - and the view labels come from the registry rather than
/// from the view, which is the only way to name one that has already been dropped.
fn trace_draw_textures(pipeline: &BlazePipeline, call: &DrawCall) {
    // A run traces one pipeline family at a time: the line is per draw per texture slot, and a frame
    // has hundreds of draws, so "everything" is a log that is unusable within a second. The file
    // holds a substring of the pipeline name to trace, and it is read once.
    static TRACED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    if !trace_filter_matches(&pipeline.name) {
        return;
    }

    // A hard cap, because a trace left on is a log that fills the disk rather than a diagnostic.
    if TRACED.fetch_add(1, Ordering::Relaxed) > 200_000 {
        return;
    }

    let mut slot = 0usize;

    for bindings_in_set in &pipeline.plan.sets {
        for binding in bindings_in_set {
            let index = slot;
            slot += 1;

            if !matches!(binding.resource, PlannedResource::Texture { .. }) {
                continue;
            }

            let entry = call.bindings.get(index).copied().unwrap_or(NO_BINDING);
            let label = if entry.resource.is_null() {
                "<nothing bound>".to_string()
            } else {
                crate::device::view_label(entry.resource as usize).unwrap_or_else(|| {
                    format!("<unregistered view {:#x}>", entry.resource as usize)
                })
            };

            info!(
                "wgpu-mc: trace {} slot '{}' -> '{}'",
                pipeline.name, binding.name, label
            );
        }
    }
}

/// The buffer a draw bound in [slot], as the plan expects it there.
///
/// The pointers a `DrawCall` carries are the ones Rust handed the JVM, so they name live buffers -
/// the caller keeps a pass's resources alive for as long as the pass records. A mismatch here is a
/// JVM-side bug (a name resolved to the wrong slot), and it is worth a panic rather than a draw with
/// the wrong resource bound.
fn call_buffer<'a>(
    plan: &str,
    binding: &PlannedBinding,
    entry: &DrawBinding,
) -> (&'a wgpu::Buffer, usize) {
    if entry.kind != DRAW_BINDING_BUFFER || entry.resource.is_null() {
        panic!(
            "wgpu-mc: {plan}: nothing bound in slot '{}', which the plan declares as a buffer",
            binding.name
        );
    }

    // Safety: the pointer is a live `wgpu::Buffer` box the JVM holds for this pass.
    let buffer = unsafe { &*(entry.resource as *const wgpu::Buffer) };
    (buffer, entry.resource as usize)
}

fn call_texture<'a>(
    plan: &str,
    binding: &PlannedBinding,
    entry: &DrawBinding,
) -> (&'a wgpu::TextureView, usize) {
    if entry.kind != DRAW_BINDING_TEXTURE || entry.resource.is_null() {
        panic!(
            "wgpu-mc: {plan}: nothing bound in slot '{}', which the plan declares as a texture",
            binding.name
        );
    }

    let address = entry.resource as usize;
    report_bound_texture(plan, binding, address);

    // Safety: as above, for a `wgpu::TextureView`.
    let view = unsafe { &*(entry.resource as *const wgpu::TextureView) };
    (view, address)
}

/// Diagnostics: which view a texture slot actually got, and whether it was still there.
///
/// The JVM side already logs the texture it *hands* a name, and this is the other end of that
/// journey: the address the bind group is built from. A model wearing another model's texture is
/// answered by the two lines together - the same name going into the same slot on both sides means
/// the picture is wrong for some other reason, and a view whose label is not the texture the JVM
/// bound means the address stopped naming what it named when it was handed over.
///
/// Both halves are behind `diagnostics`, including the liveness check: it is two registry lookups and
/// two lock acquisitions per texture binding per bind group built, which is a cost on the draw path
/// that a run without the switch should not pay. (It found nothing when it was left on: no draw ever
/// bound a view this side had dropped.) The report never reads the view, only the label recorded
/// while it was alive.
fn report_bound_texture(plan: &str, binding: &PlannedBinding, address: usize) {
    static BOUND_VIEWS: Mutex<Option<std::collections::HashSet<String>>> = Mutex::new(None);
    static DEAD_VIEWS_REPORTED: Mutex<Option<std::collections::HashSet<String>>> = Mutex::new(None);

    if !crate::debug::logging() {
        return;
    }

    // An address this side has dropped, which is the use-after-free: the draw paints with whatever
    // the allocator put there next - usually the next view it handed out, which is another model's
    // skin. Never reported by reading the view: the label came from the registry while it was alive.
    if crate::device::view_is_dead(address) {
        let mut reported = DEAD_VIEWS_REPORTED.lock();
        if reported
            .get_or_insert_with(std::collections::HashSet::new)
            .insert(format!("{plan}/{}/{address:#x}", binding.name))
        {
            log::warn!(
                "wgpu-mc: {plan} bound a texture view in slot '{}' that this side has already \
                 dropped (address {address:#x}, {} dropped view(s) remembered); a view at a reused \
                 address is how one model ends up wearing another's texture",
                binding.name,
                crate::device::dead_view_count()
            );
        }
        return;
    }

    // A live view: the label says which texture, at which mip range, this slot is actually reading.
    // An address that is neither live nor dead is one wgpu made (the swapchain), so it has no label.
    let label = crate::device::view_label(address)
        .unwrap_or_else(|| format!("<unregistered view {address:#x}>"));
    let mut bound = BOUND_VIEWS.lock();
    if bound
        .get_or_insert_with(std::collections::HashSet::new)
        .insert(format!("{plan}/{}/{label}", binding.name))
    {
        info!(
            "wgpu-mc: {plan} sampled '{label}' in slot '{}'",
            binding.name
        );
    }
}

fn call_sampler<'a>(
    plan: &str,
    binding: &PlannedBinding,
    entry: &DrawBinding,
) -> (&'a wgpu::Sampler, usize) {
    if entry.kind != DRAW_BINDING_SAMPLER || entry.resource.is_null() {
        panic!(
            "wgpu-mc: {plan}: nothing bound in slot '{}', which the plan declares as a sampler",
            binding.name
        );
    }

    // Safety: as above, for a `wgpu::Sampler`.
    let sampler = unsafe { &*(entry.resource as *const wgpu::Sampler) };
    (sampler, entry.resource as usize)
}

/// The bit that tells a JVM-numbered combination apart from a folded key.
///
/// Both live in the pass's one identity field and in the cache's one map, so they have to be
/// disjoint: a combination is a small integer the JVM assigned to a set of bindings, and a folded
/// key is whatever the fold over the table produced.
const COMBO_TAG: u64 = 1 << 63;

/// What a draw needs from the cache: the key its bindings fold to, and the groups themselves when
/// they had to be looked up or built.
struct GroupsForCall {
    key: u64,
    /// `None` when the key is the one the pass already holds, which is the common case: the pass
    /// keeps its set and the draw only hands over new dynamic offsets.
    groups: Option<Arc<CachedBindGroups>>,
    /// Set when nobody knows this combination and the table was not sent with it, so the draw has to
    /// be refused rather than built from bindings that describe a different pipeline.
    unknown: bool,
}

/// Works out what a draw's bindings fold to, fills the pass's offset lists, and returns the groups.
///
/// The fold replaces the `FxHasher` walk this used to be: same inputs, same meaning - a key that is
/// equal exactly when two draws need the same bind groups - but no hash function, and the dynamic
/// offsets are read straight out of the call's bindings instead of being collected into a vector
/// per set first.
fn bind_groups_for_call(
    wm: &WmRenderer,
    pipeline: &BlazePipeline,
    call: &DrawCall,
    offsets: &mut [[wgpu::DynamicOffset; MAX_DRAW_BINDINGS]; MAX_DRAW_SETS],
    offsets_len: &mut [usize; MAX_DRAW_SETS],
    held_key: u64,
) -> GroupsForCall {
    let alignment = wm.gpu.device.limits().min_uniform_buffer_offset_alignment as u64;
    let plan = &pipeline.plan;

    // A draw that carries a combination does not have to be folded: the JVM already resolved what
    // the bindings are, and this walk only has to produce the offsets `set_bind_group` needs. What
    // it must not do is decide anything the JVM's numbering did not cover, which is why the fold and
    // the buffer lookups behind it are skipped together.
    let numbering = call.combo != 0;

    let mut key = if numbering {
        COMBO_TAG | call.combo as u64
    } else {
        plan.salt
    };
    let mut slot = 0usize;

    // Numbered, and the table left behind: the number covers every binding and the offsets are all
    // zero. That holds exactly when the plan has no uniform binding, a uniform being the only
    // binding whose offset can travel with the draw - and it is the same condition the JVM numbers
    // such a draw under, so the two sides agreeing is what this reads. If they ever disagree the
    // draw is refused rather than bound with whatever offsets the last draw left in the pass.
    let table_needed = numbering && call.bindings_present == 0;

    if table_needed {
        if plan.has_uniforms {
            return GroupsForCall {
                key,
                groups: None,
                unknown: true,
            };
        }

        offsets_len.fill(0);
    }

    // Diagnostics: a draw that came numbered, and one whose number was the whole table. They say
    // whether the JVM is numbering combinations at all and how often the table was skipped - and
    // they are counted here rather than on the way in, because a draw refused below is one the JVM
    // sends again with the bindings and it would be counted twice.
    count_numbered();

    if table_needed {
        count_tableless();
    }

    // Nothing to walk when the number is the whole table: the loop below reads the call's bindings,
    // and those are the one thing a table-less draw does not carry.
    let sets = if table_needed {
        &[][..]
    } else {
        &plan.sets[..]
    };

    for (set, bindings_in_set) in sets.iter().enumerate() {
        let mut count = 0usize;

        for binding in bindings_in_set.iter() {
            let entry = call.bindings.get(slot).copied().unwrap_or(NO_BINDING);

            // The slot and the binding number, not the binding's name: the plan is already in the
            // key and a slot means the same binding under the same plan, so a string hash here was
            // per-draw work that could not tell apart two sets the rest of the key does not.
            if !numbering {
                key = fold(key, ((set as u64) << 32) | binding.binding as u64);
            }
            slot += 1;

            match &binding.resource {
                PlannedResource::Uniform { min_size } => {
                    let range = entry.offset..entry.offset + entry.length;

                    if numbering {
                        // The JVM's number already covers the resource and the length, and the offset
                        // too unless it travels with the draw - so the only question left here is
                        // whether this slot's offset travels, which needs the alignment, not the
                        // buffer. One that does not travel is baked into the bind group, and the
                        // number had to leave it out: it goes into the key here, which is what keeps
                        // two draws whose baked offsets differ from sharing one group.
                        if dynamic_offset(plan, binding, &range, alignment, min_size.is_some()) {
                            offsets[set][count] = range.start as wgpu::DynamicOffset;
                        } else {
                            key = fold(key, range.start);
                            offsets[set][count] = 0;
                        }

                        count += 1;
                        continue;
                    }

                    let (buffer, address) = call_buffer(&plan.name, binding, &entry);
                    let size = binding_size(buffer, &range, *min_size);

                    key = fold(key, address as u64);
                    key = fold(key, size);

                    // A dynamic offset is deliberately *not* part of the key: it is the one thing
                    // that may differ between two draws sharing a set, and sharing that set is the
                    // whole point of the offset. A baked offset is, because it lives inside the bind
                    // group. Either way what `set_bind_group` is handed is the offset the draw
                    // carries, so nothing has to be gathered for it.
                    if dynamic_offset(plan, binding, &range, alignment, min_size.is_some()) {
                        offsets[set][count] = range.start as wgpu::DynamicOffset;
                    } else {
                        key = fold(key, range.start);
                        offsets[set][count] = 0;
                    }

                    count += 1;
                }
                PlannedResource::Storage => {
                    if numbering {
                        continue;
                    }

                    let (buffer, address) = call_buffer(&plan.name, binding, &entry);
                    let range = entry.offset..entry.offset + entry.length;

                    key = fold(key, address as u64);
                    key = fold(key, binding_size(buffer, &range, None));
                    key = fold(key, range.start);
                }
                PlannedResource::Texture { .. } => {
                    if numbering {
                        continue;
                    }

                    let (_, address) = call_texture(&plan.name, binding, &entry);

                    key = fold(key, address as u64);
                }
                PlannedResource::Sampler => {
                    if numbering {
                        continue;
                    }

                    let (_, address) = call_sampler(&plan.name, binding, &entry);

                    key = fold(key, address as u64);
                }
            }
        }

        offsets_len[set] = count;
    }

    // The trace reads the call's bindings, so a table-less draw has nothing to trace: what it would
    // print is the table the JVM drew something else with.
    if !table_needed && crate::debug::trace_dynamic_offsets() && trace_filter_matches(&plan.name) {
        trace_call(plan, call, alignment, key);
    }

    if key == held_key {
        // Nothing a bind group is built from has moved: the pass keeps the set it has, and only the
        // offsets it just filled in travel with this draw.
        return GroupsForCall {
            key,
            groups: None,
            unknown: false,
        };
    }

    // Diagnostics: the `bind group cache` setting builds a fresh set for every draw, which is how
    // the cache itself can be ruled in or out as the cause of a rendering difference. Read live:
    // it is one relaxed load, and the switch takes effect on the next draw.
    let caching = crate::debug::bind_group_cache();

    if caching && let Some(cached) = take_cached_bind_groups(key) {
        return GroupsForCall {
            key,
            groups: Some(cached),
            unknown: false,
        };
    }

    // Numbered, and nobody knows this combination: the draw came without the bindings it was built
    // from, so there is nothing to build it from now. The caller re-sends the table and draws again.
    if numbering && call.bindings_present == 0 {
        return GroupsForCall {
            key,
            groups: None,
            unknown: true,
        };
    }

    count_cache_miss();

    let mut addresses: Vec<usize> = Vec::new();
    let mut slot = 0usize;

    let bind_groups = plan
        .sets
        .iter()
        .zip(&pipeline.bind_group_layouts)
        .map(|(bindings_in_set, layout)| {
            let entries = bindings_in_set
                .iter()
                .map(|binding| {
                    let entry = call.bindings.get(slot).copied().unwrap_or(NO_BINDING);
                    slot += 1;

                    match &binding.resource {
                        PlannedResource::Uniform { min_size } => {
                            let (buffer, address) = call_buffer(&plan.name, binding, &entry);
                            let range = entry.offset..entry.offset + entry.length;

                            addresses.push(address);

                            // A dynamic binding is created at offset zero, because the offset arrives
                            // with the draw; anything else keeps its offset here and gets a zero
                            // offset.
                            let offset = if dynamic_offset(
                                plan,
                                binding,
                                &range,
                                alignment,
                                min_size.is_some(),
                            ) {
                                0
                            } else {
                                range.start
                            };

                            wgpu::BindGroupEntry {
                                binding: binding.binding,
                                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                                    buffer,
                                    offset,
                                    size: BufferSize::new(binding_size(buffer, &range, *min_size)),
                                }),
                            }
                        }
                        PlannedResource::Storage => {
                            let (buffer, address) = call_buffer(&plan.name, binding, &entry);
                            let range = entry.offset..entry.offset + entry.length;

                            addresses.push(address);

                            wgpu::BindGroupEntry {
                                binding: binding.binding,
                                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                                    buffer,
                                    offset: range.start,
                                    size: BufferSize::new(binding_size(buffer, &range, None)),
                                }),
                            }
                        }
                        PlannedResource::Texture { .. } => {
                            let (view, address) = call_texture(&plan.name, binding, &entry);

                            addresses.push(address);

                            wgpu::BindGroupEntry {
                                binding: binding.binding,
                                resource: wgpu::BindingResource::TextureView(view),
                            }
                        }
                        PlannedResource::Sampler => {
                            let (sampler, address) = call_sampler(&plan.name, binding, &entry);

                            addresses.push(address);

                            wgpu::BindGroupEntry {
                                binding: binding.binding,
                                resource: wgpu::BindingResource::Sampler(sampler),
                            }
                        }
                    }
                })
                .collect::<Vec<_>>();

            wm.gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(&plan.name),
                layout,
                entries: &entries,
            })
        })
        .collect::<Vec<wgpu::BindGroup>>();

    LIVE_BIND_GROUP_COUNT.fetch_add(bind_groups.len() as u64, Ordering::Relaxed);
    count_bind_groups(bind_groups.len() as u64);

    let cached = Arc::new(CachedBindGroups {
        groups: bind_groups,
        addresses,
    });

    if caching {
        store_cached_bind_groups(key, cached.clone());
    }

    GroupsForCall {
        key,
        groups: Some(cached),
        unknown: false,
    }
}

/// Records one draw: the pipeline, the buffers, the bind groups and the draw itself.
///
/// This is the whole per-draw ABI: it used to be a pipeline bind, one bind per uniform, one per
/// sampler, one per buffer and then the draw, each with its own name lookup on the native side.
pub fn draw_call(wm: &WmRenderer, pass: &mut BlazeRenderPass, call: &DrawCall) -> bool {
    if call.pipeline.is_null() {
        panic!("wgpu-mc: a draw arrived with no pipeline bound");
    }

    // The slots the JVM filled in, against the array they live in. The plan is what says which slot
    // means what, so a count past the capacity would mean the two sides disagree about the table
    // rather than about one binding - a wrong draw either way, and not one to serve quietly.
    if call.bindings_len as usize > MAX_DRAW_BINDINGS {
        panic!(
            "wgpu-mc: a draw carries {} binding slots, more than the {MAX_DRAW_BINDINGS} this ABI \
             has room for",
            call.bindings_len
        );
    }

    // Safety: the pipeline pointer comes from `compile_render_pipeline` and the JVM keeps the
    // compiled pipeline alive for as long as any pass can draw with it.
    let pipeline = unsafe { &*call.pipeline };

    // The pass keeps one offset list per set, so a plan with more sets than that would index past
    // them. Like the binding count above, this is the two sides disagreeing about the table rather
    // than one wrong draw, and is not served quietly.
    if pipeline.plan.sets.len() > MAX_DRAW_SETS {
        panic!(
            "wgpu-mc: {} has {} bind group sets, more than the {MAX_DRAW_SETS} a pass keeps offsets \
             for",
            pipeline.plan.name,
            pipeline.plan.sets.len()
        );
    }

    // Diagnostics: one line per draw naming the texture every texture slot of this draw carries, in
    // draw order. The deduplicated "sampled X in slot Y" report says which textures a pipeline *has*
    // used; this says which one the draw that painted a given model used, which is the question a
    // model wearing another model's skin asks. Gated on the binding-resolution switch, because it is
    // a line per draw per texture slot.
    if crate::debug::binding_verbosity() {
        trace_draw_textures(pipeline, call);
    }

    // Field borrows rather than methods: `set_bind_group` wants the pass mutably while the groups
    // are read from the same struct.
    let BlazeRenderPass {
        pass: raw_pass,
        groups,
        key,
        offsets,
        offsets_len,
        pipeline: bound_pipeline,
        ..
    } = pass;

    if !std::ptr::eq(*bound_pipeline, call.pipeline) {
        raw_pass.set_pipeline(&pipeline.pipeline);
        *bound_pipeline = call.pipeline;

        count_pipeline_bind();

        // Diagnostics, and the only place a pipeline's name is touched on the draw path: the first
        // bind reports it and flips a flag on the pipeline, so every bind after that costs one
        // relaxed load. The pass trace is gated inside `trace_pipeline`.
        if crate::debug::logging() {
            log_pipeline_once(pipeline);
        }

        trace_pipeline(&pipeline.name);
    }

    for slot in 0..MAX_VERTEX_BUFFERS {
        if call.vertex_buffer_mask & (1 << slot) == 0 {
            continue;
        }

        let vertex_buffer = &call.vertex_buffers[slot];
        // Safety: a live buffer box the JVM holds for this pass, or the bit would not be set.
        let buffer = unsafe { &*(vertex_buffer.buffer as *const wgpu::Buffer) };

        raw_pass.set_vertex_buffer(
            slot as u32,
            buffer.slice(vertex_buffer.offset..vertex_buffer.offset + vertex_buffer.size),
        );
    }

    let outcome = bind_groups_for_call(
        wm,
        pipeline,
        call,
        offsets,
        offsets_len,
        if groups.is_some() { *key } else { 0 },
    );

    // Nobody knows this combination and the draw left the bindings behind: refuse it. Drawing would
    // bind groups built for whatever the JVM drew last, which is a wrong picture rather than a
    // missing one. The caller re-sends the bindings and draws again, and the pass is left as it was -
    // the pipeline and the vertex buffers above are idempotent to set a second time.
    if outcome.unknown {
        return false;
    }

    // Counted here rather than on the way in, because a refused draw is not one: the JVM sends the
    // bindings and calls again, and counting both would report twice the draws the frame made.
    count_draw();
    trace_draw();

    if let Some(built) = outcome.groups {
        *groups = Some(built);
        *key = outcome.key;
    }

    if let Some(bound) = groups.as_ref() {
        for (index, group) in bound.groups.iter().enumerate() {
            let count = offsets_len[index].min(MAX_DRAW_BINDINGS);

            raw_pass.set_bind_group(index as u32, group, &offsets[index][..count]);
        }
    }

    if call.indexed != 0 {
        // Safety: a live buffer box, as above.
        let buffer = unsafe { &*(call.index_buffer as *const wgpu::Buffer) };

        raw_pass.set_index_buffer(
            buffer.slice(..),
            if call.index_format == 2 {
                IndexFormat::Uint32
            } else {
                IndexFormat::Uint16
            },
        );
    }

    count_vertices((call.count as u64) * (call.instance_count as u64));

    let instances = 0..call.instance_count.max(1);

    draw_geometry(wm, raw_pass, pipeline, call, instances);

    true
}

/// Draws the geometry of a call, generating an index buffer for the topologies wgpu has no
/// equivalent of.
///
/// Lifted out of the two entry points this replaced, which is where the fan and quad rules were
/// worked out - see "Not every Minecraft topology is a triangle list" in the README for what each of
/// them got wrong before.
fn draw_geometry(
    wm: &WmRenderer,
    pass: &mut wgpu::RenderPass<'static>,
    pipeline: &BlazePipeline,
    call: &DrawCall,
    instances: std::ops::Range<u32>,
) {
    match pipeline.topology {
        PrimitiveTopology::TriangleFan => {
            // A fan is a triangle list over an index buffer built here, because wgpu has no fan
            // topology while Minecraft's indices for one are the sequential range rather than fan
            // triangles. That range starts at zero, so the vertices this draw refers to are
            // `first .. first + count` and the generated indices are relative to it.
            if call.count >= 3
                && let Some(indices) = fan_indices(&wm.gpu.device, call.count)
            {
                pass.set_index_buffer(indices.slice(..), IndexFormat::Uint32);

                let first_vertex = if call.indexed != 0 {
                    call.base_vertex + call.first as i32
                } else {
                    call.first as i32
                };

                pass.draw_indexed(0..3 * (call.count - 2), first_vertex, instances);
                return;
            }
        }
        // A run of quads drawn with no index buffer takes Minecraft's own quad pattern
        // (`i, i+1, i+2, i+2, i+3, i`). With one, the buffer Minecraft supplies already holds it.
        PrimitiveTopology::Quads if call.indexed == 0 => {
            if let Some((indices, index_count)) = quad_indices(&wm.gpu.device, call.count) {
                pass.set_index_buffer(indices.slice(..), IndexFormat::Uint32);
                pass.draw_indexed(0..index_count, call.first as i32, instances);
                return;
            }
        }
        _ => {}
    }

    if call.indexed != 0 {
        pass.draw_indexed(
            call.first..call.first + call.count,
            call.base_vertex,
            instances,
        );
    } else {
        pass.draw(call.first..call.first + call.count, instances);
    }
}

/// The size to bind, in bytes: what the shader reads, and never past the end of the buffer.
///
/// The JVM rounds a slice's length up to sixteen bytes, which is right for a uniform block and wrong
/// at the end of a buffer - a slice that ends at the last byte became a binding that ran four bytes
/// past it, which wgpu refuses. The size is clamped here, where the buffer is in hand, and it is the
/// shader's own block size when that is known rather than whatever the caller passed.
fn binding_size(buffer: &wgpu::Buffer, range: &Range<BufferAddress>, min_size: Option<u64>) -> u64 {
    let available = buffer.size().saturating_sub(range.start);
    let wanted = min_size.unwrap_or_else(|| range.end.saturating_sub(range.start));

    wanted.min(available).max(1)
}

/// Logs one draw's bindings: the plan, every binding in slot order, and the offset that goes with it.
///
/// Diagnostics. The offsets list is what `set_bind_group` gets, and wgpu pairs it with the layout's
/// dynamic bindings *in the order the bind group's entries were given*, so this line is the record
/// of what the shader will actually read.
fn trace_call(plan: &BindGroupPlan, call: &DrawCall, alignment: u64, key: u64) {
    static LINES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    // The cap is per run and generous, because the filter above is what keeps this readable: three
    // hundred lines is not enough to reach the world when the trace covers a whole run.
    if LINES.fetch_add(1, Ordering::Relaxed) >= 20_000 {
        return;
    }

    let mut line = String::new();
    let mut slot = 0usize;

    for (set, bindings_in_set) in plan.sets.iter().enumerate() {
        for binding in bindings_in_set.iter() {
            let entry = call.bindings.get(slot).copied().unwrap_or(NO_BINDING);
            slot += 1;

            let described = match &binding.resource {
                PlannedResource::Uniform { min_size } => {
                    let (buffer, address) = call_buffer(&plan.name, binding, &entry);
                    let range = entry.offset..entry.offset + entry.length;
                    let size = binding_size(buffer, &range, *min_size);
                    let dynamic =
                        dynamic_offset(plan, binding, &range, alignment, min_size.is_some());

                    format!(
                        "s{set}/b{} {} id={address:#x} start={} len={} size={} min={:?} {}",
                        binding.binding,
                        binding.name,
                        range.start,
                        range.end - range.start,
                        size,
                        min_size,
                        if dynamic { "DYNAMIC" } else { "baked" }
                    )
                }
                PlannedResource::Storage => {
                    let (_, address) = call_buffer(&plan.name, binding, &entry);
                    format!(
                        "s{set}/b{} {} storage id={address:#x} start={}",
                        binding.binding, binding.name, entry.offset
                    )
                }
                PlannedResource::Texture { .. } => {
                    let (_, address) = call_texture(&plan.name, binding, &entry);
                    format!(
                        "s{set}/b{} {} tex={address:#x}",
                        binding.binding, binding.name
                    )
                }
                PlannedResource::Sampler => {
                    let (_, address) = call_sampler(&plan.name, binding, &entry);
                    format!(
                        "s{set}/b{} {} sampler={address:#x}",
                        binding.binding, binding.name
                    )
                }
            };

            line.push_str(&described);
            line.push_str(" | ");
        }
    }

    info!(
        "wgpu-mc: draw {} key={key:x} alignment={alignment}: {line}",
        plan.name
    );
}

/// Whether a uniform binding can carry its offset dynamically.
///
/// Three things have to hold: the offset has to be a multiple of the alignment wgpu asks for, it has
/// to fit the `u32` a dynamic offset is, and the caller has to have left the feature on. An offset
/// that fails any of them is baked into the binding instead, with a zero dynamic offset, which
/// lands on the same range.
fn dynamic_offset(
    plan: &BindGroupPlan,
    binding: &PlannedBinding,
    range: &Range<BufferAddress>,
    alignment: u64,
    sized: bool,
) -> bool {
    static ONLY: std::sync::OnceLock<Option<Vec<String>>> = std::sync::OnceLock::new();

    // On unless the `dynamic offsets` debug setting (or the `wgpu-no-dynamic-offsets` marker)
    // turns it off - see the cache key. Measured in a world with it on: 327680 draws, 120 bind
    // groups built, 327560 cache hits and the frame rendered correctly, which is what the switch
    // was waiting for. The setting is read here rather than cached: it is one relaxed load, and
    // turning it off has to take effect on the next draw, not on the next launch.
    let enabled = crate::debug::dynamic_offsets();

    // Diagnostics: a file listing uniform names restricts the feature to those names, so a rendering
    // difference can be pinned on one binding without rebuilding anything.
    let only = ONLY.get_or_init(|| {
        std::fs::read_to_string("wgpu-dynamic-offset-names")
            .ok()
            .map(|names| {
                names
                    .split(',')
                    .map(|name| name.trim().to_string())
                    .filter(|name| !name.is_empty())
                    .collect::<Vec<_>>()
            })
    });

    let allowed = match only {
        Some(only) => only.iter().any(|name| name == &binding.name),
        None => true,
    };

    // `sized` says whether the shader's own block size is known from reflection. It improves the
    // layout - a binding with a declared minimum is checked at bind time rather than left to wgpu's
    // late check at draw time - but a dynamic offset does not depend on it, so it is not required.
    let _ = sized;

    enabled
        && allowed
        && plan.has_uniforms
        && matches!(binding.resource, PlannedResource::Uniform { .. })
        && range.start.is_multiple_of(alignment)
        && u32::try_from(range.start).is_ok()
}

#[repr(C)]
#[derive(Clone, Debug)]
pub struct VertexFormat {
    pub elements: Box<RawArray<VertexFormatElement>>,
    pub vertex_size: u64,
}

#[repr(u64)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum UniformType {
    TexelBuffer = 0,
    UBO = 1,
    Sampler = 2,
}

#[repr(C)]
#[derive(Clone, Debug)]
pub struct BindGroupEntryDescriptor {
    pub type_: UniformType,
    pub name: FfiStr,
    pub texture_format: GpuFormat,
}

#[repr(transparent)]
pub struct FfiStr {
    ptr: *const c_char,
}

impl Clone for FfiStr {
    fn clone(&self) -> Self {
        Self {
            ptr: CString::new(self.to_string()).unwrap().into_raw(),
        }
    }
}

/// Builds a string the ABI can hand back out, leaking the copy the way [`Clone`] does.
impl From<&str> for FfiStr {
    fn from(value: &str) -> Self {
        Self {
            ptr: CString::new(value).unwrap().into_raw(),
        }
    }
}

impl Deref for FfiStr {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        unsafe { CStr::from_ptr(self.ptr).to_str().unwrap() }
    }
}

impl Display for FfiStr {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(self)
    }
}

impl Debug for FfiStr {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(self)
    }
}

#[repr(C)]
#[derive(Clone, Debug)]
pub struct FragState {}

#[repr(C)]
#[derive(Debug, Clone)]
pub struct BlazeBindGroupLayout {
    pub entries: Box<RawArray<BindGroupEntryDescriptor>>,
}

#[repr(C)]
#[derive(Clone, Debug)]
pub struct BlazeColorTargetState {
    /// Null when the pipeline asks for no blending, which is what an empty
    /// `ColorTargetState#blendFunction` means. Minecraft's pipelines are explicit about this:
    /// opaque geometry leaves it empty, everything translucent names a `BlendFunction`.
    pub blend: Option<Box<BlazeBlendState>>,
    pub format: GpuFormat,
    /// `ColorTargetState#writeMask`: bit 0 red, bit 1 green, bit 2 blue, bit 3 alpha. Zero is
    /// meaningful - a pipeline can accumulate into the target without writing to it.
    pub write_mask: u32,
}

/// One blend factor, mirroring `com.mojang.blaze3d.platform.SourceFactor`/`DestFactor`.
///
/// Those two Java enums hold the same constants in the same order apart from `SRC_ALPHA_SATURATE`,
/// which only the source has, so a single enum covers all four positions. The numbers are this
/// ABI's own - Kotlin maps each Java constant to one of them by name, so they only have to agree
/// with `WmNative` on the other side.
#[repr(u64)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlendFactor {
    Zero = 0,
    One = 1,
    SrcColor = 2,
    OneMinusSrcColor = 3,
    DstColor = 4,
    OneMinusDstColor = 5,
    SrcAlpha = 6,
    OneMinusSrcAlpha = 7,
    DstAlpha = 8,
    OneMinusDstAlpha = 9,
    SrcAlphaSaturate = 10,
    Constant = 11,
    OneMinusConstant = 12,
}

/// The four factors of a `BlendFunction`, in the order `glBlendFuncSeparate` takes them.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BlazeBlendState {
    pub src_color: BlendFactor,
    pub dst_color: BlendFactor,
    pub src_alpha: BlendFactor,
    pub dst_alpha: BlendFactor,
}

impl BlendFactor {
    fn to_wgpu(self) -> wgpu::BlendFactor {
        match self {
            BlendFactor::Zero => wgpu::BlendFactor::Zero,
            BlendFactor::One => wgpu::BlendFactor::One,
            BlendFactor::SrcColor => wgpu::BlendFactor::Src,
            BlendFactor::OneMinusSrcColor => wgpu::BlendFactor::OneMinusSrc,
            BlendFactor::DstColor => wgpu::BlendFactor::Dst,
            BlendFactor::OneMinusDstColor => wgpu::BlendFactor::OneMinusDst,
            BlendFactor::SrcAlpha => wgpu::BlendFactor::SrcAlpha,
            BlendFactor::OneMinusSrcAlpha => wgpu::BlendFactor::OneMinusSrcAlpha,
            BlendFactor::DstAlpha => wgpu::BlendFactor::DstAlpha,
            BlendFactor::OneMinusDstAlpha => wgpu::BlendFactor::OneMinusDstAlpha,
            BlendFactor::SrcAlphaSaturate => wgpu::BlendFactor::SrcAlphaSaturated,
            // wgpu has one constant blend colour where GL has separate colour and alpha ones.
            // Minecraft's pipelines never ask for either, so the distinction is not modelled.
            BlendFactor::Constant => wgpu::BlendFactor::Constant,
            BlendFactor::OneMinusConstant => wgpu::BlendFactor::OneMinusConstant,
        }
    }
}

impl BlazeColorTargetState {
    /// The wgpu blend state for this target, or `None` for no blending at all.
    ///
    /// `BlendFunction` is always an add; Minecraft has no separate blend equation.
    pub fn to_wgpu_blend(&self) -> Option<wgpu::BlendState> {
        let blend = self.blend.as_ref()?;

        Some(wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: blend.src_color.to_wgpu(),
                dst_factor: blend.dst_color.to_wgpu(),
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: blend.src_alpha.to_wgpu(),
                dst_factor: blend.dst_alpha.to_wgpu(),
                operation: wgpu::BlendOperation::Add,
            },
        })
    }
}

#[repr(C)]
#[derive(Clone, Debug)]
pub struct BlazeDepthStencilState {
    pub compare_function: CompareFunction,
    /// Non-zero when the pipeline wants depth writes. Minecraft calls this `writeDepth`.
    pub active: u64,
    pub bias_constant: i32,
    pub bias_slope_scale: f32,
}

/// How a fragment's depth is compared against the depth buffer.
///
/// `CompareOp` on the Java side, renumbered rather than passed through as an ordinal so the two
/// sides never depend on declaration order.
#[repr(u64)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompareFunction {
    Never = 0,
    Less = 1,
    Equal = 2,
    LessEqual = 3,
    Greater = 4,
    NotEqual = 5,
    GreaterEqual = 6,
    Always = 7,
}

impl From<CompareFunction> for wgpu::CompareFunction {
    fn from(value: CompareFunction) -> Self {
        match value {
            CompareFunction::Never => wgpu::CompareFunction::Never,
            CompareFunction::Less => wgpu::CompareFunction::Less,
            CompareFunction::Equal => wgpu::CompareFunction::Equal,
            CompareFunction::LessEqual => wgpu::CompareFunction::LessEqual,
            CompareFunction::Greater => wgpu::CompareFunction::Greater,
            CompareFunction::NotEqual => wgpu::CompareFunction::NotEqual,
            CompareFunction::GreaterEqual => wgpu::CompareFunction::GreaterEqual,
            CompareFunction::Always => wgpu::CompareFunction::Always,
        }
    }
}

#[repr(u64)]
#[derive(Copy, Clone, Debug)]
pub enum PrimitiveTopology {
    Lines = 1,
    DebugLineStrip = 2,
    Points = 3,
    Tris = 4,
    TriangleStrip = 5,
    TriangleFan = 6,
    Quads = 7,
}

impl PrimitiveTopology {
    /// Whether a draw of this topology needs the fan expansion in `draw_indexed`.
    pub fn is_fan(self) -> bool {
        matches!(self, PrimitiveTopology::TriangleFan)
    }

    /// The wgpu topology that draws what Minecraft's `VertexFormat.Mode` asks for.
    ///
    /// Every one of these used to collapse onto `TriangleList`, which is right for exactly one of
    /// them. The two that matter:
    ///
    ///  - `Quads` really is a triangle list, and has to stay one: Minecraft's own index buffer for
    ///    the mode expands each quad into `i, i+1, i+2, i+2, i+3, i`, so the triangulation happens
    ///    on the CPU and the topology only has to agree with it. That is why the GUI and the
    ///    terrain kept working.
    ///  - `Lines` is not. Its index buffer is `i, i+1, i+2, i+3, i+2, i+1` - three line segments
    ///    per group of four vertices, because the vertex shader expands each pair into a quad
    ///    through `gl_VertexID % 2`. Drawn as triangles that is a pile of long thin triangles
    ///    across the screen, which is what the F3 overlay's 3D crosshair turned into.
    ///
    /// `TriangleFan` has no wgpu equivalent; `draw_indexed` expands it into a list with an index
    /// buffer of its own, so the topology here is only what the pipeline is created with.
    pub fn to_wgpu(self) -> wgpu::PrimitiveTopology {
        match self {
            PrimitiveTopology::Lines => wgpu::PrimitiveTopology::LineList,
            PrimitiveTopology::DebugLineStrip => wgpu::PrimitiveTopology::LineStrip,
            PrimitiveTopology::Points => wgpu::PrimitiveTopology::PointList,
            PrimitiveTopology::Tris | PrimitiveTopology::Quads | PrimitiveTopology::TriangleFan => {
                wgpu::PrimitiveTopology::TriangleList
            }
            PrimitiveTopology::TriangleStrip => wgpu::PrimitiveTopology::TriangleStrip,
        }
    }
}

#[repr(C)]
#[derive(Clone, Debug)]
pub struct RenderPipeline {
    pub name: FfiStr,
    pub bind_group_layouts: Box<RawArray<BlazeBindGroupLayout>>,
    pub color_target_states: Box<RawArray<BlazeColorTargetState>>,
    pub depth_stencil_state: Option<Box<BlazeDepthStencilState>>,
    pub vertex_formats: Box<RawArray<VertexFormat>>,
    pub vertex_shader: FfiStr,
    pub fragment_shader: FfiStr,
    pub directives: FfiStr,
    pub frag_state: Option<Box<FragState>>,
    pub primitive_topology: PrimitiveTopology,
    /// Non-zero when the pipeline wants back faces culled, i.e. `RenderPipeline#isCull`.
    ///
    /// Minecraft's GL backend answers that flag with `glEnable(GL_CULL_FACE)` - back faces,
    /// counter-clockwise front, which are the GL defaults - and the flag defaults to *true*, so a
    /// pipeline only turns culling off by asking. Leaving it out of the ABI meant every triangle
    /// was drawn twice; on blended geometry the two copies of the same quad are at the same depth,
    /// so which of them wins is not defined, which is what made leaves ghost and flicker.
    pub cull: u64,
}

#[repr(u64)]
#[allow(non_camel_case_types)]
#[derive(Copy, Clone, Debug)]
pub enum GpuFormat {
    None = 0,
    R8_UNORM = 1,
    R8_SNORM = 2,
    RG8_UNORM = 3,
    RG8_SNORM = 4,
    RGB8_UNORM = 5,
    RGB8_SNORM = 6,
    RGBA8_UNORM = 7,
    RGBA8_SNORM = 8,
    R16_UNORM = 9,
    R16_SNORM = 10,
    RG16_UNORM = 11,
    RG16_SNORM = 12,
    RGB16_UNORM = 13,
    RGB16_SNORM = 14,
    RGBA16_UNORM = 15,
    RGBA16_SNORM = 16,
    R8_UINT = 17,
    R8_SINT = 18,
    RG8_UINT = 19,
    RG8_SINT = 20,
    RGB8_UINT = 21,
    RGB8_SINT = 22,
    RGBA8_UINT = 23,
    RGBA8_SINT = 24,
    R16_UINT = 25,
    R16_SINT = 26,
    RG16_UINT = 27,
    RG16_SINT = 28,
    RGB16_UINT = 29,
    RGB16_SINT = 30,
    RGBA16_UINT = 31,
    RGBA16_SINT = 32,
    R32_UINT = 33,
    R32_SINT = 34,
    RG32_UINT = 35,
    RG32_SINT = 36,
    RGB32_UINT = 37,
    RGB32_SINT = 38,
    RGBA32_UINT = 39,
    RGBA32_SINT = 40,
    R16_FLOAT = 41,
    RG16_FLOAT = 42,
    RGB16_FLOAT = 43,
    RGBA16_FLOAT = 44,
    R32_FLOAT = 45,
    RG32_FLOAT = 46,
    RGB32_FLOAT = 47,
    RGBA32_FLOAT = 48,
    RGB10A2_UNORM = 49,
    RGB10A2_UINT = 50,
    RG11B10_FLOAT = 51,
    D32_FLOAT = 52,
    D32_FLOAT_S8_UINT = 53,
    D24_UNORM_S8_UINT = 54,
    D16_UNORM = 55,
    S8_UINT = 56,
}

impl GpuFormat {
    pub fn to_wgpu_texture_format(&self) -> wgpu::TextureFormat {
        match self {
            GpuFormat::RGBA8_UNORM => wgpu::TextureFormat::Rgba8Unorm,
            GpuFormat::R8_UNORM => wgpu::TextureFormat::R8Unorm,
            GpuFormat::D32_FLOAT => wgpu::TextureFormat::Depth32Float,
            _ => unimplemented!("{self:?}"),
        }
    }

    /// The wgpu vertex format for this `GpuFormat`, or `None` when wgpu has no equivalent.
    ///
    /// Three-component 8- and 16-bit formats are the case worth explaining. WebGPU only defines
    /// the `x2` and `x4` spellings at those component sizes, and Minecraft does use one of them -
    /// `VertexFormatElement::NORMAL` is three normalised signed bytes - so the `x4` spelling is
    /// substituted for the `x3` one. That is safe here for two reasons:
    ///
    ///  - the extra component is padding, not the next element's data. `VertexFormat::Builder`
    ///    refuses to build a format whose size is not a multiple of four, so a three-byte element
    ///    is always followed by at least one more byte inside the same vertex; Minecraft's own
    ///    formats that use `NORMAL` all call `padding(1)` explicitly. The caller checks that the
    ///    widened read still fits inside the vertex and logs it if it does not;
    ///  - wgpu accepts the wider attribute for a narrower shader input. For the vertex stage it
    ///    only compares the *scalar kind* of the two types, not the component count, so a `vec3`
    ///    input is served by a `x4` attribute and simply ignores the fourth component.
    ///
    /// The remaining arms are formats 26.1 cannot produce as a vertex element at all - its
    /// `VertexFormatElement::Type` is one of FLOAT/UBYTE/BYTE/USHORT/SHORT/UINT/INT - and exist so
    /// that the mapping is total instead of aborting the process on an unexpected value.
    pub fn to_wgpu_vertex_format(&self) -> Option<wgpu::VertexFormat> {
        use wgpu::VertexFormat as Vf;

        Some(match self {
            GpuFormat::R8_UNORM => Vf::Unorm8,
            GpuFormat::RG8_UNORM => Vf::Unorm8x2,
            GpuFormat::RGB8_UNORM => Vf::Unorm8x4,
            GpuFormat::RGBA8_UNORM => Vf::Unorm8x4,

            GpuFormat::R8_SNORM => Vf::Snorm8,
            GpuFormat::RG8_SNORM => Vf::Snorm8x2,
            GpuFormat::RGB8_SNORM => Vf::Snorm8x4,
            GpuFormat::RGBA8_SNORM => Vf::Snorm8x4,

            GpuFormat::R8_UINT => Vf::Uint8,
            GpuFormat::RG8_UINT => Vf::Uint8x2,
            GpuFormat::RGB8_UINT => Vf::Uint8x4,
            GpuFormat::RGBA8_UINT => Vf::Uint8x4,

            GpuFormat::R8_SINT => Vf::Sint8,
            GpuFormat::RG8_SINT => Vf::Sint8x2,
            GpuFormat::RGB8_SINT => Vf::Sint8x4,
            GpuFormat::RGBA8_SINT => Vf::Sint8x4,

            GpuFormat::R16_UNORM => Vf::Unorm16,
            GpuFormat::RG16_UNORM => Vf::Unorm16x2,
            GpuFormat::RGB16_UNORM => Vf::Unorm16x4,
            GpuFormat::RGBA16_UNORM => Vf::Unorm16x4,

            GpuFormat::R16_SNORM => Vf::Snorm16,
            GpuFormat::RG16_SNORM => Vf::Snorm16x2,
            GpuFormat::RGB16_SNORM => Vf::Snorm16x4,
            GpuFormat::RGBA16_SNORM => Vf::Snorm16x4,

            GpuFormat::R16_UINT => Vf::Uint16,
            GpuFormat::RG16_UINT => Vf::Uint16x2,
            GpuFormat::RGB16_UINT => Vf::Uint16x4,
            GpuFormat::RGBA16_UINT => Vf::Uint16x4,

            GpuFormat::R16_SINT => Vf::Sint16,
            GpuFormat::RG16_SINT => Vf::Sint16x2,
            GpuFormat::RGB16_SINT => Vf::Sint16x4,
            GpuFormat::RGBA16_SINT => Vf::Sint16x4,

            GpuFormat::R32_UINT => Vf::Uint32,
            GpuFormat::RG32_UINT => Vf::Uint32x2,
            GpuFormat::RGB32_UINT => Vf::Uint32x3,
            GpuFormat::RGBA32_UINT => Vf::Uint32x4,

            GpuFormat::R32_SINT => Vf::Sint32,
            GpuFormat::RG32_SINT => Vf::Sint32x2,
            GpuFormat::RGB32_SINT => Vf::Sint32x3,
            GpuFormat::RGBA32_SINT => Vf::Sint32x4,

            GpuFormat::R32_FLOAT => Vf::Float32,
            GpuFormat::RG32_FLOAT => Vf::Float32x2,
            GpuFormat::RGB32_FLOAT => Vf::Float32x3,
            GpuFormat::RGBA32_FLOAT => Vf::Float32x4,

            GpuFormat::R16_FLOAT => Vf::Float16,
            GpuFormat::RG16_FLOAT => Vf::Float16x2,
            GpuFormat::RGBA16_FLOAT => Vf::Float16x4,

            GpuFormat::RGB16_FLOAT
            | GpuFormat::RGB10A2_UNORM
            | GpuFormat::RGB10A2_UINT
            | GpuFormat::RG11B10_FLOAT
            | GpuFormat::D32_FLOAT
            | GpuFormat::D32_FLOAT_S8_UINT
            | GpuFormat::D24_UNORM_S8_UINT
            | GpuFormat::D16_UNORM
            | GpuFormat::S8_UINT
            | GpuFormat::None => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A slot that may travel is a uniform of a plan that has one, and nothing else.
    ///
    /// This is the rule the two sides of the combination ABI share: the JVM leaves such a slot's
    /// offset out of the number it mints, and a draw that arrives without its binding table is one
    /// whose plan marked no slot at all. If this ever answered yes for a storage buffer - whose
    /// offset is baked into the group, because DX12 has no offset for a storage descriptor - a
    /// table-less draw would bind whatever offset the last group was built with and nothing here
    /// would fold it back into the key.
    #[test]
    fn only_a_uniform_of_a_plan_that_has_one_may_carry_its_offset() {
        let uniform = PlannedResource::Uniform { min_size: Some(64) };
        let storage = PlannedResource::Storage;
        let texture = PlannedResource::Texture { cube: false };
        let sampler = PlannedResource::Sampler;

        assert!(may_travel(true, &uniform));
        assert!(!may_travel(false, &uniform));
        assert!(!may_travel(true, &storage));
        assert!(!may_travel(true, &texture));
        assert!(!may_travel(true, &sampler));

        // The flag the JVM reads is the plan's, and the condition the table-less path checks is the
        // same one: a plan that marks no slot is a plan with no uniform binding, so its offsets are
        // all zero and its draws are answered without the table.
        assert_eq!(PlanBinding::kind_of(&uniform), DRAW_BINDING_BUFFER);
        assert_eq!(PlanBinding::kind_of(&storage), DRAW_BINDING_BUFFER);
    }
}
