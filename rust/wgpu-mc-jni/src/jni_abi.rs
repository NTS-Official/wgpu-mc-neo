//! The C ABI of this crate, reachable a second way: over JNI.
//!
//! Everything in `device.rs` and `blaze.rs` that carries `#[unsafe(no_mangle)]` is an entry point the
//! mod calls through **`java.lang.foreign`** - a linker bound by name, reading the structs
//! `bindings.h` declares by byte offset. That is the right bridge on the Java the 26.1 mod runs on,
//! and it is not available on the one 1.21.1 runs on: `java.lang.foreign` is a *preview* API in Java
//! 21, and a mod cannot ask a launcher for `--enable-preview`. So on the `all-jni` branch every one of
//! those exports gets a second entry point, resolved by name the way JNI resolves anything.
//!
//! # What a wrapper is, and what it deliberately is not
//!
//! A wrapper does no marshalling of its own. Its whole job is to turn the numbers a JNI call carries
//! back into the arguments the export already takes:
//!
//! | the export takes | the wrapper takes | how it travels |
//! | --- | --- | --- |
//! | `&T` / `&mut T` / `Box<T>` / `*mut T` | `jlong` | the pointer itself, as a number |
//! | a descriptor (`&DrawCall`, `&RenderPipeline`, `&RawArray<T>`, ...) | `jlong` | a direct `ByteBuffer` the JVM fills, addressed with `MemoryUtil.memAddress` |
//! | `*const u8` / `*mut u8` | `jlong` | the same, for pixel and vertex bytes |
//! | `FfiStr` / `*const c_char` | `JString` | UTF-8, copied into a `CString` that lives for the call |
//! | `GpuFormat` | `jint` | the discriminant, as the C ABI already carries it |
//! | `bool` | `jboolean` | 0 or 1 |
//!
//! **Structs are not rebuilt here.** A descriptor's layout is `bindings.h`'s, and the JVM writes those
//! bytes into a direct buffer - LWJGL ships in the game and `MemoryUtil` is already how the mod
//! allocates. Having this side read fields out of a buffer and rebuild a Rust struct would be a second
//! description of the same layout, in a second language, with nothing checking them against each
//! other; passing the address keeps exactly one description, the one `cbindgen` emits.
//!
//! # Why the class name is not the one the exports are already on
//!
//! The 51 JNI entry points this crate already had all resolve against `dev.birb.wgpu.rust.WgpuNative`,
//! which is the mod's *gameplay* bridge - settings, palettes, sections, entities. These 64 are the
//! *renderer* bridge, and they resolve against `dev.birb.wgpu.rust.WmJni`, so that a run's stack trace
//! and a missing-symbol message say which half of the mod failed.
//!
//! # The one thing that does not survive the trip
//!
//! `acquire_next_texture` answers a `*mut SurfaceTexture` and `present_surface` consumes a
//! `Box<SurfaceTexture>`; `create_command_encoder` answers a `Box<CommandEncoderHandle>` and
//! `submit_command_encoder` consumes one. Both pass through here as a number and both are still the
//! same allocation on the same side - but a JVM that drops one without presenting or submitting it
//! leaks it, and nothing here can tell. That is the same contract the FFM bridge has.

use std::ffi::{CString, c_char};

use jni::JNIEnv;
use jni::objects::{JClass, JString};
use jni::sys::{jboolean, jdouble, jint, jlong};
use jni_fn::jni_fn;
use wgpu_mc::wgpu;

use crate::blaze::{
    BlazeDepthStencilState, BlazeRenderPass, BlazeRenderPassDescriptor, DrawCall, FfiStr,
    GpuFormat, PlanBinding, RawArray, RenderPipeline,
};
use crate::device::{BlazePipeline, CommandEncoderHandle};
use crate::indirect::BatchRecord;

/// A handle the JVM holds as a number, read back as a shared borrow.
///
/// # Safety
///
/// `handle` must be a pointer this crate handed out and must still be live, and nothing may hold a
/// `&mut` to the same value for the duration of the borrow.
unsafe fn shared<'a, T>(handle: jlong) -> &'a T {
    unsafe { &*(handle as *const T) }
}

/// A handle the JVM holds as a number, read back as an exclusive borrow.
///
/// # Safety
///
/// As [`shared`], and no other borrow of the same value may be alive.
unsafe fn exclusive<'a, T>(handle: jlong) -> &'a mut T {
    unsafe { &mut *(handle as *mut T) }
}

/// A handle that may be absent, where the C ABI spells absence as a null pointer.
///
/// # Safety
///
/// As [`shared`], for a `handle` that is either a live pointer or zero.
unsafe fn optional<'a, T>(handle: jlong) -> Option<&'a T> {
    if handle == 0 {
        None
    } else {
        Some(unsafe { shared(handle) })
    }
}

/// Hands a boxed value to the JVM as the pointer it now owns.
fn boxed<T>(value: Box<T>) -> jlong {
    Box::into_raw(value) as jlong
}

/// Takes a boxed value back, to drop it.
///
/// # Safety
///
/// `handle` must be a pointer [`boxed`] produced and must not be used again.
unsafe fn unbox<T>(handle: jlong) -> Box<T> {
    unsafe { Box::from_raw(handle as *mut T) }
}

/// A Java string as the C ABI's `FfiStr` wants it: NUL-terminated UTF-8, owned for the call.
///
/// `FfiStr` is a bare pointer - it borrows, it does not own - so the copy has to outlive the call that
/// reads it, which is what this type is for. Every wrapper that takes a name binds one to a local
/// before it calls, and drops it after.
struct CText(CString);

impl CText {
    fn new(env: &mut JNIEnv, value: &JString) -> Self {
        let text: String = env
            .get_string(value)
            .expect("the JVM handed this side a string it cannot read")
            .into();
        Self(
            CString::new(text)
                .expect("a Java string with a NUL in it is not a name the C ABI carries"),
        )
    }

    /// The same string as the C ABI's borrowed string type.
    fn ffi(&self) -> FfiStr {
        unsafe { FfiStr::from_raw(self.0.as_ptr()) }
    }

    /// The same string as the bare pointer the two `label` parameters are declared with.
    fn ptr(&self) -> *const c_char {
        self.0.as_ptr()
    }
}

/// Rebuilds a texture format from the discriminant the ABI carries.
///
/// The C ABI passes `GpuFormat` by value, which is to say as its discriminant, so this is the same
/// number the FFM bridge already sends - read back through a checked conversion rather than a
/// `transmute`, because the value arrives from the JVM and an out-of-range one is undefined behaviour
/// in a fieldless enum rather than a wrong texture.
///
/// A discriminant this build has never heard of becomes [`GpuFormat::None`], the variant the enum
/// already reserves for "no format": `create_texture` then fails where the format is used instead of
/// silently allocating something else.
fn gpu_format(discriminant: jint) -> GpuFormat {
    GpuFormat::from_repr(discriminant as u64).unwrap_or_else(|| {
        log::error!(
            "the JVM asked for texture format {discriminant}, which this build of the renderer does \
             not have; falling back to GpuFormat::None"
        );
        GpuFormat::None
    })
}

/// The class every entry point in this file resolves against.
///
/// Exposed so a test can check the wrappers against the exports they mirror without a JVM: a wrapper
/// written against the wrong class is a wrapper nothing will ever call. Test-only, because outside
/// `cfg(test)` the class name appears in the attributes below and nowhere else - a const nothing reads
/// is dead code, and `-D warnings` is the gate.
#[cfg(test)]
pub(crate) const WM_JNI: &str = "dev.birb.wgpu.rust.WmJni";
// ---------------------------------------------------------------------------------------------
// Surface
// ---------------------------------------------------------------------------------------------

/// Creates the renderer's surface for a window the game owns.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn createSurface(_env: JNIEnv, _class: JClass, wm: jlong, display: jlong, window: jlong) {
    unsafe { crate::device::create_surface(shared(wm), display as u64, window as u64) }
}

/// Reconfigures the swapchain for a new size, or a new present mode.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn configureSurface(
    _env: JNIEnv,
    _class: JClass,
    wm: jlong,
    width: jint,
    height: jint,
    present_mode: jint,
) {
    unsafe {
        crate::device::configure_surface(
            shared(wm),
            width as u32,
            height as u32,
            present_mode as u32,
        )
    }
}

/// Releases the surface, which is what a game closing its window does.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn dropSurface(_env: JNIEnv, _class: JClass, wm: jlong) {
    unsafe { crate::device::drop_surface(shared(wm)) }
}

/// Acquires the next swapchain image. The JVM owns the answer until it presents it.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn acquireNextTexture(_env: JNIEnv, _class: JClass, wm: jlong) -> jlong {
    unsafe { crate::device::acquire_next_texture(shared(wm)) as jlong }
}

/// Presents an acquired image, and takes the box back.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn presentSurface(_env: JNIEnv, _class: JClass, wm: jlong, surface_texture: jlong) {
    unsafe {
        crate::device::present_surface(shared(wm), unbox::<wgpu::SurfaceTexture>(surface_texture))
    }
}

/// Blits a texture view into the swapchain image and submits the frame.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn blitFromTexture(
    _env: JNIEnv,
    _class: JClass,
    wm: jlong,
    texture_view: jlong,
    surface_texture: jlong,
) {
    unsafe {
        crate::device::blit_from_texture(
            shared(wm),
            shared::<wgpu::TextureView>(texture_view),
            shared::<wgpu::SurfaceTexture>(surface_texture),
        )
    }
}

// ---------------------------------------------------------------------------------------------
// Command encoders and render passes
// ---------------------------------------------------------------------------------------------

/// Opens a command encoder. The JVM owns it until it submits or flushes it.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn createCommandEncoder(_env: JNIEnv, _class: JClass, wm: jlong) -> jlong {
    unsafe { boxed(crate::device::create_command_encoder(shared(wm))) }
}

/// Submits what an encoder has recorded, and takes the box back.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn submitCommandEncoder(_env: JNIEnv, _class: JClass, wm: jlong, encoder: jlong) {
    unsafe {
        crate::device::submit_command_encoder(shared(wm), unbox::<CommandEncoderHandle>(encoder))
    }
}

/// Submits what the shared encoder has recorded, without taking it back.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn flushEncoder(_env: JNIEnv, _class: JClass, wm: jlong, encoder: jlong) {
    unsafe { crate::device::flush_encoder(shared(wm), exclusive::<CommandEncoderHandle>(encoder)) }
}

/// Begins a render pass. `descriptor` addresses a `BlazeRenderPassDescriptor` in a direct buffer.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn createRenderPass(
    mut env: JNIEnv,
    _class: JClass,
    encoder: jlong,
    descriptor: jlong,
    name: JString,
) -> jlong {
    let name = CText::new(&mut env, &name);
    unsafe {
        boxed(crate::device::create_render_pass(
            exclusive::<CommandEncoderHandle>(encoder),
            shared::<BlazeRenderPassDescriptor>(descriptor),
            name.ffi(),
        ))
    }
}

/// Ends a render pass, and takes the box back.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn dropRenderPass(_env: JNIEnv, _class: JClass, pass: jlong) {
    unsafe { crate::device::drop_render_pass(unbox::<BlazeRenderPass>(pass)) }
}

/// Restricts drawing to a rectangle, in the coordinates `glScissor` uses.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn setScissorRect(
    _env: JNIEnv,
    _class: JClass,
    pass: jlong,
    x: jint,
    y: jint,
    width: jint,
    height: jint,
) {
    unsafe {
        crate::device::set_scissor_rect(
            exclusive::<BlazeRenderPass>(pass),
            x as u32,
            y as u32,
            width as u32,
            height as u32,
        )
    }
}

// ---------------------------------------------------------------------------------------------
// Clearing
// ---------------------------------------------------------------------------------------------

/// Clears a colour attachment to a packed colour.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn clearColorTexture(
    _env: JNIEnv,
    _class: JClass,
    encoder: jlong,
    texture: jlong,
    clear_color: jint,
) {
    unsafe {
        crate::device::clear_color_texture(
            exclusive::<CommandEncoderHandle>(encoder),
            shared::<wgpu::Texture>(texture),
            clear_color as u32,
        )
    }
}

/// Clears a depth attachment.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn clearDepthTexture(
    _env: JNIEnv,
    _class: JClass,
    encoder: jlong,
    texture: jlong,
    clear_depth: jdouble,
) {
    unsafe {
        crate::device::clear_depth_texture(
            exclusive::<CommandEncoderHandle>(encoder),
            shared::<wgpu::Texture>(texture),
            clear_depth,
        )
    }
}

/// Clears a colour and a depth attachment together, which is one pass rather than two.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn clearColorAndDepthTextures(
    _env: JNIEnv,
    _class: JClass,
    encoder: jlong,
    color_texture: jlong,
    clear_color: jint,
    depth_texture: jlong,
    clear_depth: jdouble,
) {
    unsafe {
        crate::device::clear_color_and_depth_textures(
            exclusive::<CommandEncoderHandle>(encoder),
            shared::<wgpu::Texture>(color_texture),
            clear_color as u32,
            shared::<wgpu::Texture>(depth_texture),
            clear_depth,
        )
    }
}

/// The same, with the colour half limited to a rectangle - a scissored `glClear`.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
#[allow(clippy::too_many_arguments)]
pub fn clearColorAndDepthTexturesRegion(
    _env: JNIEnv,
    _class: JClass,
    encoder: jlong,
    color_texture: jlong,
    clear_color: jint,
    depth_texture: jlong,
    clear_depth: jdouble,
    region_x: jint,
    region_y: jint,
    region_width: jint,
    region_height: jint,
) {
    unsafe {
        crate::device::clear_color_and_depth_textures_region(
            exclusive::<CommandEncoderHandle>(encoder),
            shared::<wgpu::Texture>(color_texture),
            clear_color as u32,
            shared::<wgpu::Texture>(depth_texture),
            clear_depth,
            region_x as u32,
            region_y as u32,
            region_width as u32,
            region_height as u32,
        )
    }
}
// ---------------------------------------------------------------------------------------------
// Pipelines
// ---------------------------------------------------------------------------------------------

/// Compiles a pipeline from a `RenderPipeline` the JVM filled in.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn compileRenderPipeline(_env: JNIEnv, _class: JClass, wm: jlong, descriptor: jlong) -> jlong {
    unsafe {
        boxed(crate::device::compile_render_pipeline(
            shared(wm),
            shared::<RenderPipeline>(descriptor),
        ))
    }
}

/// Compiles the other depth variant of a pipeline that has already been described.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn createPipelineVariant(
    _env: JNIEnv,
    _class: JClass,
    wm: jlong,
    source: jlong,
    depth_stencil: jlong,
) -> jlong {
    unsafe {
        boxed(crate::device::create_pipeline_variant(
            shared(wm),
            shared::<BlazePipeline>(source),
            optional::<BlazeDepthStencilState>(depth_stencil),
        ))
    }
}

/// Frees one compiled pipeline variant. Null-tolerant, because a variant may never have been made.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn dropRenderPipeline(_env: JNIEnv, _class: JClass, pipeline: jlong) {
    crate::device::drop_render_pipeline(pipeline as *mut BlazePipeline)
}

/// The plan's bindings in slot order, written into `out`.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn pipelineBindings(_env: JNIEnv, _class: JClass, pipeline: jlong, out: jlong) -> jint {
    unsafe {
        crate::blaze::pipeline_bindings(
            shared::<BlazePipeline>(pipeline),
            exclusive::<RawArray<PlanBinding>>(out),
        ) as jint
    }
}

/// Whether this device and backend can run batched draws, which decides how a terrain pipeline is
/// declared and so has to be known before one is compiled.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn batchingPossible(_env: JNIEnv, _class: JClass) -> jboolean {
    crate::device::batching_possible() as jboolean
}

// ---------------------------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------------------------

/// Records one draw, bindings and all. The answer says whether it was recorded or refused.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn drawCall(_env: JNIEnv, _class: JClass, wm: jlong, pass: jlong, call: jlong) -> jboolean {
    unsafe {
        crate::device::draw_call(
            shared(wm),
            exclusive::<BlazeRenderPass>(pass),
            shared::<DrawCall>(call),
        ) as jboolean
    }
}

/// Records a run of draws as one indirect call. Zero means "not as a batch"; draw them one at a time.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn drawCallBatch(
    _env: JNIEnv,
    _class: JClass,
    wm: jlong,
    pass: jlong,
    template: jlong,
    records: jlong,
) -> jint {
    unsafe {
        crate::device::draw_call_batch(
            shared(wm),
            exclusive::<BlazeRenderPass>(pass),
            shared::<DrawCall>(template),
            shared::<RawArray<BatchRecord>>(records),
        ) as jint
    }
}

/// Records the graph's terrain pass into the frame's own encoder.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn renderTerrainPass(
    _env: JNIEnv,
    _class: JClass,
    wm: jlong,
    color: jlong,
    depth: jlong,
    translucent: jboolean,
) -> jboolean {
    unsafe {
        crate::device::render_terrain_pass(
            shared(wm),
            shared::<wgpu::TextureView>(color),
            shared::<wgpu::TextureView>(depth),
            translucent != 0,
        ) as jboolean
    }
}

// ---------------------------------------------------------------------------------------------
// Textures, views and samplers
// ---------------------------------------------------------------------------------------------

/// Creates a texture with the mip chain the game asked for.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
#[allow(clippy::too_many_arguments)]
pub fn createTexture(
    mut env: JNIEnv,
    _class: JClass,
    wm: jlong,
    format_id: jint,
    width: jint,
    height: jint,
    depth_or_layers: jint,
    usage: jint,
    mip_levels: jint,
    name: JString,
) -> jlong {
    let name = CText::new(&mut env, &name);
    unsafe {
        boxed(crate::device::create_texture(
            shared(wm),
            gpu_format(format_id),
            width as u32,
            height as u32,
            depth_or_layers as u32,
            usage as u32,
            mip_levels as u32,
            name.ffi(),
        ))
    }
}

/// Drops a texture the JVM closed.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn dropTexture(_env: JNIEnv, _class: JClass, texture: jlong) {
    unsafe { crate::device::drop_texture(unbox::<wgpu::Texture>(texture)) }
}

/// Creates a view, optionally over a sub-range of the mip chain.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn createTextureView(
    _env: JNIEnv,
    _class: JClass,
    wm: jlong,
    texture: jlong,
    usage: jint,
    base_mip_level: jint,
    mip_levels: jint,
) -> jlong {
    unsafe {
        boxed(crate::device::create_texture_view(
            shared(wm),
            shared::<wgpu::Texture>(texture),
            usage as u32,
            base_mip_level as u32,
            mip_levels as u32,
        ))
    }
}

/// Drops a view.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn dropTextureView(_env: JNIEnv, _class: JClass, view: jlong) {
    unsafe { crate::device::drop_texture_view(unbox::<wgpu::TextureView>(view)) }
}

/// Uploads a rectangle of CPU bytes into a texture.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
#[allow(clippy::too_many_arguments)]
pub fn writeToTexture(
    _env: JNIEnv,
    _class: JClass,
    wm: jlong,
    destination: jlong,
    source: jlong,
    source_size: jlong,
    mip_level: jint,
    depth_or_layer: jint,
    dest_x: jint,
    dest_y: jint,
    width: jint,
    height: jint,
) {
    unsafe {
        crate::device::write_to_texture(
            shared(wm),
            shared::<wgpu::Texture>(destination),
            source as *const u8,
            source_size as u64,
            mip_level as u32,
            depth_or_layer as u32,
            dest_x as u32,
            dest_y as u32,
            width as u32,
            height as u32,
        )
    }
}

/// Copies a rectangle of a texture into a buffer.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
#[allow(clippy::too_many_arguments)]
pub fn copyTextureToBuffer(
    _env: JNIEnv,
    _class: JClass,
    wm: jlong,
    encoder: jlong,
    source: jlong,
    dest: jlong,
    offset: jlong,
    mip: jint,
    x: jint,
    y: jint,
    width: jint,
    height: jint,
) {
    unsafe {
        crate::device::copy_texture_to_buffer(
            shared(wm),
            exclusive::<CommandEncoderHandle>(encoder),
            shared::<wgpu::Texture>(source),
            shared::<wgpu::Buffer>(dest),
            offset as u64,
            mip as u32,
            x as u32,
            y as u32,
            width as u32,
            height as u32,
        )
    }
}

/// Copies a rectangle of one texture into another.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
#[allow(clippy::too_many_arguments)]
pub fn copyTextureToTexture(
    _env: JNIEnv,
    _class: JClass,
    encoder: jlong,
    source: jlong,
    destination: jlong,
    mip: jint,
    dest_x: jint,
    dest_y: jint,
    src_x: jint,
    src_y: jint,
    width: jint,
    height: jint,
) {
    unsafe {
        crate::device::copy_texture_to_texture(
            exclusive::<CommandEncoderHandle>(encoder),
            shared::<wgpu::Texture>(source),
            shared::<wgpu::Texture>(destination),
            mip as u32,
            dest_x as u32,
            dest_y as u32,
            src_x as u32,
            src_y as u32,
            width as u32,
            height as u32,
        )
    }
}

/// Copies a rectangle of a buffer into a texture.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
#[allow(clippy::too_many_arguments)]
pub fn copyBufferToTexture(
    _env: JNIEnv,
    _class: JClass,
    wm: jlong,
    encoder: jlong,
    buffer: jlong,
    buffer_start: jlong,
    buffer_end: jlong,
    source_x: jint,
    source_y: jint,
    source_width: jint,
    source_height: jint,
    destination: jlong,
    destination_x: jint,
    destination_y: jint,
    copy_width: jint,
    copy_height: jint,
    mip_level: jint,
    depth_or_array_layers: jint,
) {
    unsafe {
        crate::device::copy_buffer_to_texture(
            shared(wm),
            exclusive::<CommandEncoderHandle>(encoder),
            shared::<wgpu::Buffer>(buffer),
            buffer_start as u64,
            buffer_end as u64,
            source_x as u32,
            source_y as u32,
            source_width as u32,
            source_height as u32,
            shared::<wgpu::Texture>(destination),
            destination_x as u32,
            destination_y as u32,
            copy_width as u32,
            copy_height as u32,
            mip_level as u32,
            depth_or_array_layers as u32,
        )
    }
}

/// Creates a sampler, including the per-sprite LOD clamp the game asks for.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
#[allow(clippy::too_many_arguments)]
pub fn createSampler(
    _env: JNIEnv,
    _class: JClass,
    wm: jlong,
    address_mode_u: jint,
    address_mode_v: jint,
    min_filter: jint,
    mag_filter: jint,
    max_anisotropy: jint,
    max_lod: jdouble,
) -> jlong {
    unsafe {
        boxed(crate::device::create_sampler(
            shared(wm),
            address_mode_u as u32,
            address_mode_v as u32,
            min_filter as u32,
            mag_filter as u32,
            max_anisotropy as u32,
            max_lod,
        ))
    }
}

/// Drops a sampler.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn dropSampler(_env: JNIEnv, _class: JClass, sampler: jlong) {
    unsafe { crate::device::drop_sampler(unbox::<wgpu::Sampler>(sampler)) }
}

/// Hands the graph the game's own block atlas, for the faces whose sprite the game animates.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn bindGameBlockAtlas(_env: JNIEnv, _class: JClass, wm: jlong, texture: jlong) {
    unsafe { crate::device::bind_game_block_atlas(shared(wm), shared::<wgpu::Texture>(texture)) }
}

/// Hands the graph the game's lightmap.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn bindGameLightmap(_env: JNIEnv, _class: JClass, wm: jlong, texture: jlong) {
    unsafe { crate::device::bind_game_lightmap(shared(wm), shared::<wgpu::Texture>(texture)) }
}
// ---------------------------------------------------------------------------------------------
// Buffers
// ---------------------------------------------------------------------------------------------

/// Creates an unmapped buffer.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn createBuffer(
    mut env: JNIEnv,
    _class: JClass,
    wm: jlong,
    label: JString,
    usage: jint,
    size: jlong,
) -> jlong {
    let label = CText::new(&mut env, &label);
    unsafe {
        boxed(crate::device::create_buffer(
            shared(wm),
            label.ptr(),
            usage as u32,
            size as u64,
        ))
    }
}

/// Creates a buffer and fills it in one call.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn createBufferInit(
    mut env: JNIEnv,
    _class: JClass,
    wm: jlong,
    label: JString,
    usage: jint,
    data: jlong,
    size: jlong,
) -> jlong {
    let label = CText::new(&mut env, &label);
    unsafe {
        boxed(crate::device::create_buffer_init(
            shared(wm),
            label.ptr(),
            usage as u32,
            data as *mut u8,
            size as u64,
        ))
    }
}

/// A buffer handed to the JVM already mapped, for the JVM to fill in place.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn allocateGpuBufferMapped(
    _env: JNIEnv,
    _class: JClass,
    wm: jlong,
    size: jlong,
    usages: jlong,
) -> jlong {
    unsafe {
        boxed(crate::device::allocate_gpu_buffer_mapped(
            shared(wm),
            size as u64,
            usages as u64,
        ))
    }
}

/// Drops a buffer.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn dropBuffer(_env: JNIEnv, _class: JClass, buffer: jlong) {
    unsafe { crate::device::drop_buffer(unbox::<wgpu::Buffer>(buffer)) }
}

/// Copies `length` bytes from a staging pointer into a buffer.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn writeToBuffer(
    _env: JNIEnv,
    _class: JClass,
    wm: jlong,
    buffer: jlong,
    start: jlong,
    length: jlong,
    data: jlong,
) {
    unsafe {
        crate::device::write_to_buffer(
            shared(wm),
            shared::<wgpu::Buffer>(buffer),
            start as u64,
            length as u64,
            data as *const u8,
        )
    }
}

/// Writes a whole buffer, which is what a uniform upload with no staging does.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn writeBufferWith(
    _env: JNIEnv,
    _class: JClass,
    wm: jlong,
    buffer: jlong,
    data: jlong,
    len: jlong,
) {
    unsafe {
        crate::device::write_buffer_with(
            shared(wm),
            shared::<wgpu::Buffer>(buffer),
            data as *const u8,
            len as u64,
        )
    }
}

/// Copies a range of one buffer into another.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
#[allow(clippy::too_many_arguments)]
pub fn copyBufferToBuffer(
    _env: JNIEnv,
    _class: JClass,
    wm: jlong,
    encoder: jlong,
    src: jlong,
    dest: jlong,
    src_offset: jlong,
    dest_offset: jlong,
    length: jlong,
) {
    unsafe {
        crate::device::copy_buffer_to_buffer(
            shared(wm),
            exclusive::<CommandEncoderHandle>(encoder),
            shared::<wgpu::Buffer>(src),
            shared::<wgpu::Buffer>(dest),
            src_offset as u64,
            dest_offset as u64,
            length as u64,
        )
    }
}

/// Reads a range of a buffer back to the CPU, blocking until the GPU has produced it.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn readBuffer(
    _env: JNIEnv,
    _class: JClass,
    wm: jlong,
    buffer: jlong,
    offset: jlong,
    length: jlong,
    destination: jlong,
) -> jboolean {
    unsafe {
        crate::device::read_buffer(
            shared(wm),
            shared::<wgpu::Buffer>(buffer),
            offset as u64,
            length as u64,
            destination as *mut u8,
        ) as jboolean
    }
}

/// The wgpu usage flags a buffer was created with, for diagnostics.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn bufferUsages(_env: JNIEnv, _class: JClass, buffer: jlong) -> jlong {
    unsafe { crate::device::buffer_usages(shared::<wgpu::Buffer>(buffer)) as jlong }
}

// ---------------------------------------------------------------------------------------------
// Device queries
// ---------------------------------------------------------------------------------------------

/// The largest texture this device will make.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn maxTextureSize(_env: JNIEnv, _class: JClass, wm: jlong) -> jint {
    unsafe { crate::device::max_texture_size(shared(wm)) as jint }
}

/// The alignment a dynamic uniform offset has to be a multiple of.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn minUniformOffsetAlignment(_env: JNIEnv, _class: JClass, wm: jlong) -> jint {
    unsafe { crate::device::min_uniform_offset_alignment(shared(wm)) as jint }
}

// ---------------------------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------------------------

/// Logs how much work the renderer was handed since the last call, then resets the counters.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn logRenderStats(_env: JNIEnv, _class: JClass) {
    crate::device::log_render_stats()
}

/// Writes a texture out as raw RGBA, preceded by its width and height.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn dumpTextureRgba(
    mut env: JNIEnv,
    _class: JClass,
    wm: jlong,
    texture: jlong,
    path: JString,
    flip_y: jboolean,
) -> jboolean {
    let path = CText::new(&mut env, &path);
    unsafe {
        crate::device::dump_texture_rgba(
            shared(wm),
            shared::<wgpu::Texture>(texture),
            path.ffi(),
            flip_y != 0,
        ) as jboolean
    }
}

/// Reads a texture into memory the JVM owns, as raw RGBA rows.
///
/// The address is the game's own allocation - `NativeImage`'s pixels - and `capacity` is what the
/// caller says it holds, so nothing is written past it. This exists so `glGetTexImage` does not have
/// to go through a file: a readback that writes bytes to disk and reads them straight back is a
/// screenshot taking a dependency on a writable working directory.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn readTextureRgba(
    _env: JNIEnv,
    _class: JClass,
    wm: jlong,
    texture: jlong,
    address: jlong,
    capacity: jint,
    flip_y: jboolean,
) -> jboolean {
    unsafe {
        crate::device::read_texture_rgba(
            shared(wm),
            shared::<wgpu::Texture>(texture),
            address as *mut u8,
            capacity.max(0) as usize,
            flip_y != 0,
        ) as jboolean
    }
}

/// Writes the image a `SurfaceTexture` holds out as raw RGBA.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn dumpSurfaceTextureRgba(
    mut env: JNIEnv,
    _class: JClass,
    wm: jlong,
    surface_texture: jlong,
    path: JString,
) -> jboolean {
    let path = CText::new(&mut env, &path);
    unsafe {
        crate::device::dump_surface_texture_rgba(
            shared(wm),
            shared::<wgpu::SurfaceTexture>(surface_texture),
            path.ffi(),
        ) as jboolean
    }
}

/// Whether the graph can draw the terrain pass yet, building it if it cannot.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn terrainPassReady(_env: JNIEnv, _class: JClass, wm: jlong) -> jboolean {
    unsafe { crate::device::terrain_pass_ready(shared(wm)) as jboolean }
}

/// Whether the terrain arena can still be grown, which is whether a refused section is worth
/// rebuilding.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn terrainArenaCanGrow(_env: JNIEnv, _class: JClass) -> jboolean {
    crate::device::terrain_arena_can_grow() as jboolean
}

/// How many sections the arena holds.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn terrainArenaSections(_env: JNIEnv, _class: JClass, wm: jlong) -> jint {
    unsafe { crate::device::terrain_arena_sections(shared(wm)) as jint }
}

/// How many slots the arena's pool has.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn terrainArenaSlots(_env: JNIEnv, _class: JClass) -> jint {
    crate::device::terrain_arena_slots() as jint
}

/// How many of those slots are handed out.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn terrainArenaUsed(_env: JNIEnv, _class: JClass) -> jint {
    crate::device::terrain_arena_used() as jint
}

/// The largest number of slots any one section has taken.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn terrainArenaLargestSection(_env: JNIEnv, _class: JClass) -> jint {
    crate::device::terrain_arena_largest_section() as jint
}

/// How many sections the terrain pass has drawn since the renderer started.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn terrainSectionsDrawn(_env: JNIEnv, _class: JClass) -> jint {
    crate::device::terrain_sections_drawn() as jint
}

/// How many sections the arena has refused to hold, over the whole run.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn terrainSectionsRefused(_env: JNIEnv, _class: JClass) -> jint {
    crate::device::terrain_sections_refused() as jint
}

/// How many of those refusals were handed to the JVM, over the whole run.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn terrainSectionsRefusedReported(_env: JNIEnv, _class: JClass) -> jint {
    crate::device::terrain_sections_refused_reported() as jint
}

/// How many refusals were never handed over, over the whole run.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn terrainSectionsRefusedDropped(_env: JNIEnv, _class: JClass) -> jint {
    crate::device::terrain_sections_refused_dropped() as jint
}

/// How many fluid blocks the mesher has drawn, over every bake so far.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn terrainFluidBlocks(_env: JNIEnv, _class: JClass) -> jint {
    crate::device::terrain_fluid_blocks() as jint
}

/// How many fluid faces the mesher has drawn, over every bake so far.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn terrainFluidQuads(_env: JNIEnv, _class: JClass) -> jint {
    crate::device::terrain_fluid_quads() as jint
}

/// What the terrain pass drew since the last call, per layer, packed three to a word.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn terrainLayerCounts(_env: JNIEnv, _class: JClass, which: jint) -> jlong {
    crate::device::terrain_layer_counts(which) as jlong
}

/// Hands this crate the box the game's own view covers.
#[jni_fn("dev.birb.wgpu.rust.WmJni")]
pub fn terrainWorldBounds(
    _env: JNIEnv,
    _class: JClass,
    horizon: jint,
    min_section_y: jint,
    max_section_y: jint,
) {
    crate::device::terrain_world_bounds(horizon, min_section_y, max_section_y)
}
