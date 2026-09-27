use crate::blaze::{
    BindGroupPlan, BlazeDepthStencilState, BlazeRenderPass, BlazeRenderPassDescriptor, DrawCall,
    FfiStr, GpuFormat, PrimitiveTopology, RenderPipeline,
};
use crate::preprocessing::{ProcessedShaderResult, process_shaders};
use crate::settings::{GraphicsBackend, Settings};
use crate::{MinecraftResourceManagerAdapter, RENDERER};
use futures::executor::block_on;
use glsl::syntax::TypeSpecifierNonArray;
use jni::JNIEnv;
use jni::objects::JClass;
use jni::sys::{jint, jlong};
use jni_fn::jni_fn;
use log::{error, info, warn};
use once_cell::sync::OnceCell;
use parking_lot::Mutex;
use raw_window_handle::{RawDisplayHandle, RawWindowHandle};
use std::borrow::Cow;
use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::{CStr, c_char};
use std::num::NonZero;
use std::path::PathBuf;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use wgpu_mc::wgpu::util::{BufferInitDescriptor, DeviceExt};
use wgpu_mc::wgpu::{
    Color, CurrentSurfaceTexture, Extent3d, Limits, Operations, Origin3d, PresentMode,
    ShaderSource, SurfaceTexture, TexelCopyBufferInfo, TexelCopyBufferLayout, TexelCopyTextureInfo,
    TextureFormat, naga,
};
use wgpu_mc::{Gpu, WmRenderer, wgpu};

/// Present mode asked for through the C ABI by the JVM side.
///
/// [`PresentModeRequest::FromSettings`] is what `WgpuSurface` sends: the `vsync` setting owns the
/// present mode, and keeping the translation on this side means the options screen does not have
/// to know which modes a particular driver offers.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
enum PresentModeRequest {
    FromSettings = 0,
    Fifo = 1,
    Immediate = 2,
    Mailbox = 3,
}

impl PresentModeRequest {
    fn from_raw(raw: u32) -> Self {
        match raw {
            1 => Self::Fifo,
            2 => Self::Immediate,
            3 => Self::Mailbox,
            _ => Self::FromSettings,
        }
    }
}

/// The swapchain, once it has been configured.
///
/// The blitter is built for the format the surface actually accepted, so it has to live next to
/// the configuration instead of in a process-wide constant: a Vulkan driver and a DX12 driver do
/// not offer the same format list, and a blit whose destination format disagrees with the
/// swapchain fails validation.
struct SurfaceState {
    format: TextureFormat,
    present_mode: PresentMode,
    width: u32,
    height: u32,
    blitter: PresentBlit,
}

/// Turns the main render target over while copying it into the swapchain.
///
/// A straight copy is the wrong thing here, and `wgpu::util::TextureBlitter` is exactly that. This
/// backend emulates OpenGL's clip-space orientation in every vertex shader (see
/// `preprocessing::EmulateGlClipSpace`), so the main target holds the frame the way OpenGL would
/// have held it: the first texel row is the *bottom* of the picture, because OpenGL's window
/// origin is the bottom-left corner and a framebuffer attachment's row 0 is its origin row. A
/// swapchain image's row 0 is the *top* of the window, so presenting has to turn the image back
/// over. `blit.wgsl` from `wgpu::util` carries the opposite correction - "Invert y so the texture
/// is not upside down" - which is right for wgpu's own convention and wrong for the one the rest
/// of this backend has to live in.
struct PresentBlit {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
}

/// Full-screen quad, sampled with `v` running the other way.
///
/// `corner.y` of 0 lands on the *bottom* row of the destination - `corner * 2 - 1` puts it at NDC
/// y = -1, which every wgpu backend maps to the last framebuffer row - and reads source row 0
/// there, so the source's rows come out reversed exactly once.
const PRESENT_BLIT_WGSL: &str = r#"
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) tex_coords: vec2<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    var out: VertexOutput;
    let corner = vec2<f32>(f32((vertex_index << 1u) & 2u), f32(vertex_index & 2u));
    out.tex_coords = corner;
    out.position = vec4<f32>(corner * 2.0 - 1.0, 0.0, 1.0);
    return out;
}

@group(0) @binding(0) var source_texture: texture_2d<f32>;
@group(0) @binding(1) var source_sampler: sampler;

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(source_texture, source_sampler, in.tex_coords);
}
"#;

impl PresentBlit {
    fn new(device: &wgpu::Device, format: TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("wgpu-mc present blit"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(PRESENT_BLIT_WGSL)),
        });

        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("wgpu-mc present blit"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("wgpu-mc present blit"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("wgpu-mc present blit"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                // The quad comes out of `vertex_index`; there is nothing to fetch.
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(format.into())],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                strip_index_format: None,
                front_face: Default::default(),
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: Default::default(),
                conservative: false,
            },
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("wgpu-mc present blit"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        Self {
            pipeline,
            layout,
            sampler,
        }
    }

    fn copy(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        source: &wgpu::TextureView,
        destination: &wgpu::TextureView,
    ) {
        // Built per frame rather than cached. Caching it by the source view's *address* was tried and
        // is wrong: Minecraft closes the main target's view when it rebuilds the target - which is
        // what a resize does - and the allocator hands the same address to the new one, so the cache
        // answered with a bind group for a texture that no longer existed. The frame came out
        // stretched, because the old target was a different size. One bind group a frame is not worth
        // that; the saving here is the encoder, not the bind group.
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("wgpu-mc present blit"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(source),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("blit"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: destination,
                resolve_target: None,
                depth_slice: None,
                ops: Operations {
                    load: wgpu::LoadOp::Clear(Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });

        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.draw(0..4, 0..1);
    }
}

static SURFACE_STATE: Mutex<Option<SurfaceState>> = Mutex::new(None);

/// Chooses a non-sRGB 8-bit format when the surface offers one.
///
/// Minecraft's main render target already holds display-referred colour, so an sRGB swapchain
/// would encode it a second time and wash the image out. `Bgra8Unorm` is the swapchain format on
/// both DX12 and Vulkan; `Rgba8Unorm` covers drivers that only expose that ordering.
fn preferred_surface_format(formats: &[TextureFormat]) -> TextureFormat {
    const PREFERRED: [TextureFormat; 4] = [
        TextureFormat::Bgra8Unorm,
        TextureFormat::Rgba8Unorm,
        TextureFormat::Bgra8UnormSrgb,
        TextureFormat::Rgba8UnormSrgb,
    ];

    PREFERRED
        .iter()
        .copied()
        .find(|format| formats.contains(format))
        .or_else(|| formats.first().copied())
        .unwrap_or(TextureFormat::Bgra8Unorm)
}

/// Turns a request into a mode the surface actually supports.
fn resolve_present_mode(request: PresentModeRequest, supported: &[PresentMode]) -> PresentMode {
    let wants_vsync = match request {
        PresentModeRequest::Fifo => true,
        PresentModeRequest::Immediate | PresentModeRequest::Mailbox => false,
        PresentModeRequest::FromSettings => crate::SETTINGS
            .read()
            .as_ref()
            .map(|settings| settings.vsync.value)
            .unwrap_or(true),
    };

    if wants_vsync {
        // Fifo is the one present mode every backend is required to support.
        PresentMode::Fifo
    } else {
        // Mailbox is the tear-free immediate mode; Immediate is the tearing one. Fall back to
        // Fifo rather than asking for a mode the surface will reject.
        [
            PresentMode::Mailbox,
            PresentMode::Immediate,
            PresentMode::Fifo,
        ]
        .into_iter()
        .find(|mode| supported.contains(mode))
        .unwrap_or(PresentMode::Fifo)
    }
}

/// Translates the raw handles GLFW handed the JVM into a wgpu surface target.
unsafe fn surface_target(display: u64, window: u64) -> wgpu::SurfaceTargetUnsafe {
    #[cfg(windows)]
    {
        use raw_window_handle::{Win32WindowHandle, WindowsDisplayHandle};
        use std::num::NonZeroIsize;
        use winapi::shared::windef::HWND;
        use winapi::um::winuser::{GWLP_HINSTANCE, GetWindowLongPtrW};

        let _ = display;
        let hwnd: HWND = window as HWND;

        let mut win_handle = Win32WindowHandle::new(NonZeroIsize::new(hwnd as isize).unwrap());
        win_handle.hinstance =
            NonZeroIsize::new(unsafe { GetWindowLongPtrW(hwnd as _, GWLP_HINSTANCE) } as isize);

        wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: Some(RawDisplayHandle::Windows(WindowsDisplayHandle::new())),
            raw_window_handle: RawWindowHandle::Win32(win_handle),
        }
    }

    #[cfg(not(windows))]
    {
        use raw_window_handle::{XlibDisplayHandle, XlibWindowHandle};

        use std::ptr::NonNull;

        let handle = XlibDisplayHandle::new(NonNull::new(display as _), 0);

        wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: Some(RawDisplayHandle::Xlib(handle)),
            raw_window_handle: RawWindowHandle::Xlib(XlibWindowHandle::new(window as _)),
        }
    }
}

unsafe fn build_surface(
    instance: &wgpu::Instance,
    display: u64,
    window: u64,
) -> Result<wgpu::Surface<'static>, wgpu::CreateSurfaceError> {
    unsafe { instance.create_surface_unsafe(surface_target(display, window)) }
}

/// Applies a configuration to a surface that the caller already holds a reference to.
///
/// Kept separate from [`configure_surface`] so the swapchain can also be reconfigured from
/// [`acquire_next_texture`], which already holds the surface lock and would deadlock if it went
/// through the locking entry point.
fn configure_surface_inner(
    wm: &WmRenderer,
    surface: &wgpu::Surface<'static>,
    width: u32,
    height: u32,
    request: PresentModeRequest,
    force: bool,
) {
    if width == 0 || height == 0 {
        return;
    }

    let capabilities = surface.get_capabilities(&wm.gpu.adapter);
    let format = preferred_surface_format(&capabilities.formats);
    let present_mode = resolve_present_mode(request, &capabilities.present_modes);
    let alpha_mode = capabilities
        .alpha_modes
        .iter()
        .copied()
        .find(|mode| *mode == wgpu::CompositeAlphaMode::Opaque)
        .unwrap_or(wgpu::CompositeAlphaMode::Auto);

    // This early return is for the JVM, which asks on every frame that the window is a different
    // size and would otherwise rebuild the swapchain for no reason. It must not apply to the
    // recovery in `acquire_next_texture`: there the *same* configuration is exactly what is broken,
    // so skipping the configure made the recovery a no-op that logged "still no swapchain image
    // after reconfiguring" once per frame while the window showed nothing.
    let unchanged = {
        let state = SURFACE_STATE.lock();
        state.as_ref().is_some_and(|state| {
            state.width == width
                && state.height == height
                && state.format == format
                && state.present_mode == present_mode
        })
    };

    if unchanged && !force {
        return;
    }

    surface.configure(
        &wm.gpu.device,
        &wgpu::SurfaceConfiguration {
            // `COPY_SRC` is what lets the presented image be read back for diagnostics; surface
            // textures support it on both Vulkan and DX12.
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            format,
            width,
            height,
            present_mode,
            // Two frames in flight is what DXGI's flip model and Vulkan's default both assume;
            // 0 would hand the choice to the backend and make latency unpredictable.
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: Vec::new(),
        },
    );

    let blitter = PresentBlit::new(&wm.gpu.device, format);

    info!(
        "wgpu-mc: swapchain {width}x{height}, {format:?}, {present_mode:?}, alpha {alpha_mode:?}, \
         driver formats {:?}",
        &capabilities.formats[..capabilities.formats.len().min(4)]
    );

    *SURFACE_STATE.lock() = Some(SurfaceState {
        format,
        present_mode,
        width,
        height,
        blitter,
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn configure_surface(
    wm: &WmRenderer,
    width: u32,
    height: u32,
    present_mode: u32,
) {
    let lock = wm.gpu.surface.lock();
    let Some(surface) = lock.as_ref() else {
        warn!("wgpu-mc: configure_surface was called before a surface existed");
        return;
    };

    configure_surface_inner(
        wm,
        surface,
        width,
        height,
        PresentModeRequest::from_raw(present_mode),
        false,
    );
}

/// Re-resolves the present mode from the settings and reconfigures the swapchain if it changed.
///
/// This is what makes `vsync` a setting that does not need a restart: the mode is not a property of
/// the wgpu instance, only of the surface configuration, and `configure_surface_inner` compares the
/// present mode along with the size - so reconfiguring at the *same* size still applies a change,
/// while leaving a swapped-in value that matches the current one alone.
///
/// Called from `sendSettings`, i.e. when the options screen is applied. Does nothing when there is
/// no surface, which is the case for a renderer that was created without a window.
pub fn reapply_present_mode() {
    let Some(wm) = RENDERER.get() else {
        return;
    };

    // The size to configure is the one already in force; reading it before taking the surface lock
    // keeps the lock order the one `configure_surface` established (surface, then surface state).
    let size = {
        let state = SURFACE_STATE.lock();
        match state.as_ref() {
            Some(state) => (state.width, state.height),
            None => return,
        }
    };

    let lock = wm.gpu.surface.lock();
    let Some(surface) = lock.as_ref() else {
        return;
    };

    configure_surface_inner(
        wm,
        surface,
        size.0,
        size.1,
        PresentModeRequest::FromSettings,
        false,
    );
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drop_surface(wm: &WmRenderer) {
    wm.gpu.surface.lock().take();
    *SURFACE_STATE.lock() = None;
}

/// The backend the player picked, and the wgpu backend set it corresponds to.
fn selected_backend() -> GraphicsBackend {
    crate::SETTINGS
        .read()
        .as_ref()
        .map(Settings::graphics_backend)
        .unwrap_or_default()
}

/// Maps the setting onto the backend set the wgpu instance is created with.
///
/// The mapping lives here rather than as a `From` impl so that `settings.rs` stays free of any
/// wgpu dependency: it only describes what the options screen is allowed to offer.
fn wgpu_backends(backend: GraphicsBackend) -> wgpu::Backends {
    match backend {
        GraphicsBackend::Vulkan => wgpu::Backends::VULKAN,
        GraphicsBackend::DirectX12 => wgpu::Backends::DX12,
    }
}

/// The instance flags, with GPU-based validation behind the debug setting it is offered as.
///
/// Host-side validation and the debug utilities are always on - they are what makes a wgpu error
/// name the call that caused it, and they cost little. GPU-based validation is the driver's own
/// validation layer: it checks what the GPU is actually asked to do, and it is slow enough that it
/// is off unless a player asks for it. It used to be unconditional here, which charged every
/// launch for a development tool.
fn instance_flags() -> wgpu::InstanceFlags {
    let flags = wgpu::InstanceFlags::VALIDATION | wgpu::InstanceFlags::DEBUG;

    if crate::debug::gpu_based_validation() {
        flags | wgpu::InstanceFlags::GPU_BASED_VALIDATION
    } else {
        flags
    }
}

/// Builds the instance, adapter, device and queue for one specific backend.
///
/// Returns `None` instead of panicking when the backend cannot be brought up, because the
/// caller wants to try the other one before giving up, and because a panic would run the panic
/// hook, which exits the game before the player can reach the options screen.
fn try_create_renderer(
    env: &mut JNIEnv,
    backend: GraphicsBackend,
    display: u64,
    window: u64,
) -> Option<WmRenderer> {
    // Before the instance, and therefore before any D3D12 object exists: PIX's GPU capturer hooks
    // D3D12 as it is loaded, and a process that loads it after the device exists is one PIX refuses
    // to attach to. The switch is read here rather than on the draw path because this is a one-off.
    crate::pix::load_capturers(crate::debug::pix_capture());

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu_backends(backend),
        flags: instance_flags(),
        memory_budget_thresholds: Default::default(),
        backend_options: Default::default(),
        display: None,
        // flags: Default::default()
    });

    // The surface is created before the adapter is requested so that the adapter can be required
    // to actually support it. Without `compatible_surface` wgpu is free to hand back an adapter
    // that cannot present to this window at all, which on a hybrid-graphics machine - and on
    // DX12 in particular, where the output may hang off a different adapter than the fast one -
    // leaves a device that renders perfectly and never shows anything.
    let surface = if window == 0 {
        None
    } else {
        match unsafe { build_surface(&instance, display, window) } {
            Ok(surface) => Some(surface),
            Err(err) => {
                error!("wgpu-mc: {backend:?} could not create a surface for this window: {err}");
                return None;
            }
        }
    };

    let adapter = match block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference:
            wgpu::PowerPreference::from_env().unwrap_or(wgpu::PowerPreference::HighPerformance),
        force_fallback_adapter: false,
        compatible_surface: surface.as_ref(),
    })) {
        Ok(adapter) => adapter,
        Err(err) => {
            error!("wgpu-mc: no usable {backend:?} adapter for this window: {err}");
            return None;
        }
    };

    let adapter_info = adapter.get_info();
    info!(
        "wgpu-mc: {backend:?} adapter is {} ({:?}, {:?})",
        adapter_info.name, adapter_info.backend, adapter_info.device_type
    );

    // `PIPELINE_CACHE` is what lets the driver's own compilation results be carried into the next
    // launch. It is asked for when the adapter has it - Vulkan does, DX12 has no serialisable cache
    // at all - and the device is still created without it if not.
    let mut required_features =
        wgpu::Features::MAPPABLE_PRIMARY_BUFFERS | wgpu::Features::DEPTH_CLIP_CONTROL;

    if adapter.features().contains(wgpu::Features::PIPELINE_CACHE) {
        required_features |= wgpu::Features::PIPELINE_CACHE;
    }

    // Timestamp queries, for the `gpu timestamps` debug switch. Asked for whenever the adapter has
    // them - requesting a feature does not change how anything renders, only what may be asked of
    // the device later - and the switch decides whether any are written.
    if adapter
        .features()
        .contains(wgpu::Features::TIMESTAMP_QUERY | wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS)
    {
        required_features |=
            wgpu::Features::TIMESTAMP_QUERY | wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS;
    }

    // Immediates - push constants, spelled the way WebGPU spells them: the render graph's terrain pass
    // hands each section its position that way, and a pipeline layout that declares an immediate size
    // is refused outright without the feature ("Features ... are required but not enabled on the
    // device"). That refusal is a wgpu validation error, and a validation error on this path ends the
    // process, so the feature is asked for whenever the adapter has it - and a graph whose device does
    // not gets its immediate pipelines skipped rather than built (see `create_pipelines`).
    if adapter.features().contains(wgpu::Features::IMMEDIATES) {
        required_features |= wgpu::Features::IMMEDIATES;
    }

    let (device, queue) = match block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: None,
        required_features,
        required_limits: Limits {
            max_bind_groups: adapter.limits().max_bind_groups,
            ..adapter.limits()
        },
        experimental_features: Default::default(),
        memory_hints: Default::default(),
        trace: Default::default(),
    })) {
        Ok(pair) => pair,
        Err(err) => {
            error!("wgpu-mc: the {backend:?} device could not be created: {err}");
            return None;
        }
    };

    let pipeline_cache = create_pipeline_cache(&device, &adapter);

    let gpu = Gpu {
        instance,
        adapter,
        // Storing the surface here, rather than through `create_surface` afterwards, is what
        // makes the adapter and the swapchain come from the same place.
        surface: Mutex::new(surface.map(Arc::new)),
        device,
        queue,
        pipeline_cache,
    };

    let resource_provider = Arc::new(MinecraftResourceManagerAdapter {
        jvm: env.get_java_vm().unwrap(),
    });

    Some(WmRenderer::new(Arc::new(gpu), resource_provider))
}

/// Hands the finished renderer to the process-wide cell and returns the pointer the JVM holds.
///
/// The renderer lives in `RENDERER` rather than in a leaked `Box` because the Rust entry points
/// that are not addressed by pointer (`getBackend`, `bakeSection`, `reloadShaders`) read it from
/// there, and because a `OnceCell` in a `static` is never dropped, the pointer stays valid for
/// as long as the process runs.
fn register_renderer(wm: WmRenderer, framebuffer: (u32, u32)) -> jlong {
    // The renderer's own atlases - the block atlas among them - are created here, as the Fabric
    // module did before the port. Nothing is packed into them yet: the atlas fills as block models
    // are read (`block.rs` allocates a texture the first time a model names one), so this only has to
    // happen before the first bake, and this is the earliest point where there is a device to make
    // the textures with.
    wm.init();

    // The scene needs a framebuffer size and the renderer needs to exist before it can have one, so
    // it is created here, with the size the JVM read from the window it just made. A renderer created
    // without a window - or before the window has a size - gets its scene on the first presented
    // frame instead; see `blit_from_texture`.
    if framebuffer.0 != 0 && framebuffer.1 != 0 {
        wm.ensure_scene(wgpu::Extent3d {
            width: framebuffer.0,
            height: framebuffer.1,
            depth_or_array_layers: 1,
        });
    }

    if RENDERER.set(wm).is_err() {
        error!("wgpu-mc: a renderer has already been registered");
        return 0;
    }

    RENDERER.get().unwrap() as *const WmRenderer as jlong
}

/// Creates the renderer (wgpu instance, adapter, device, queue) and returns the pointer to the
/// `WmRenderer` the rest of the C ABI is addressed with, or 0 when no backend could be created.
///
/// Kept separate from the JNI wrappers below so the renderer can also be created from Rust code
/// and so every JVM-side declaration shares one implementation.
pub fn create_renderer(
    env: &mut JNIEnv,
    display: u64,
    window: u64,
    width: u32,
    height: u32,
) -> jlong {
    let requested = selected_backend();

    // The configured backend is tried first, then the other one. A backend that is valid in the
    // config but unusable on this machine - a DX12 config carried onto a machine whose driver
    // has no DX12 support, or a Vulkan config on a system without a Vulkan ICD - would otherwise
    // leave the game with no renderer at all and no way to reach the options screen that would
    // let the player change it back.
    for backend in [requested, requested.alternative()] {
        if !backend.is_available_here() {
            info!("wgpu-mc: {backend:?} is not available on this platform, skipping it");
            continue;
        }

        let Some(wm) = try_create_renderer(&mut *env, backend, display, window) else {
            continue;
        };

        if backend != requested {
            error!(
                "wgpu-mc: {requested:?} could not be created, so the renderer is running on \
                 {backend:?} instead. Pick {backend:?} on the Electrum options page (or edit \
                 \"backend\" in config/wgpu-mc-renderer.json) and restart to stop falling back."
            );
        } else {
            info!("wgpu-mc: renderer created through {backend:?}");
        }

        return register_renderer(wm, (width, height));
    }

    error!(
        "wgpu-mc: neither {requested:?} nor its alternative could be created; no graphics \
         backend is available"
    );
    0
}

/// JNI entry point used by the Java/Kotlin backend to obtain the `WmRenderer` pointer.
#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn createWmRenderer(mut env: JNIEnv, _: JClass) -> jlong {
    // No window, so no framebuffer size: the scene is made by the first presented frame.
    create_renderer(&mut env, 0, 0, 0, 0)
}

/// JNI entry point that also registers an existing window, so the adapter can be required to be
/// able to present to it. `display` and `window` are the raw handles GLFW reports.
#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn createWmRendererOnWindow(
    mut env: JNIEnv,
    _: JClass,
    display: jlong,
    window: jlong,
    width: jint,
    height: jint,
) -> jlong {
    create_renderer(
        &mut env,
        display as u64,
        window as u64,
        width as u32,
        height as u32,
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn create_surface(wm: &WmRenderer, display: u64, window: u64) {
    match unsafe { build_surface(&wm.gpu.instance, display, window) } {
        Ok(surface) => {
            *wm.gpu.surface.lock() = Some(Arc::new(surface));
            // The bookkeeping describes the surface that was just replaced, so drop it and let
            // the next frame configure the new swapchain from scratch.
            *SURFACE_STATE.lock() = None;
        }
        Err(err) => error!("wgpu-mc: could not create a surface for this window: {err}"),
    }
}

//     surface.configure(&device, &surface_config);
//
//     println!("configured");
//
//     let display = Display {
//         surface,
//         device,
//         queue,
//         config: RwLock::new(surface_config),
//         instance,
//         adapter,
//     };
//
//     let resource_provider = Arc::new(MinecraftResourceManagerAdapter {
//         jvm: env.get_java_vm().unwrap(),
//     });
//
//     let wm = WmRenderer::new(display, resource_provider);
//
//     wm.init();
//
//     drop(RENDERER.set(wm));
// }

/// What the JVM holds for one `GpuDevice#createCommandEncoder` call.
///
/// Nothing. Minecraft's `CommandEncoder` is not `AutoCloseable` and has no `close`, and it makes
/// three of them a frame - so a native encoder per object is a thousand command buffers a second
/// that nothing frees. Each one is a D3D12 command allocator and command list with the frame's
/// recording in it, and none of it is visible to the garbage collector, which is why the process
/// went from one gigabyte to twenty in a minute with nothing to collect. Recording order is what
/// matters and everything is recorded on the render thread in call order, so they all share one
/// encoder and the handle is an empty box.
pub struct CommandEncoderHandle;

/// The one native encoder, heap-allocated so its address is stable for the passes that borrow it.
static SHARED_ENCODER: Mutex<Option<usize>> = Mutex::new(None);

/// Submissions since the last `log_render_stats`, reported there.
static SUBMISSIONS: AtomicU64 = AtomicU64::new(0);

/// Presents since the last `log_render_stats`, reported beside the submissions.
///
/// The two numbers together are the question this path is judged by - "how many submissions does a
/// frame cost" - and neither is answerable without the other: a submission count alone cannot tell
/// one-per-frame apart from two, and the frame rate is not the same number as the present count.
static PRESENTS: AtomicU64 = AtomicU64::new(0);

/// The submission the frame being recorded ended with, if it has submitted yet.
static LAST_SUBMISSION: Mutex<Option<wgpu::SubmissionIndex>> = Mutex::new(None);

/// The submissions of the frames in flight, oldest first.
///
/// A present pushes the frame it is presenting and waits for whatever is left once
/// the rames in flight setting of them are outstanding, which is what the wait at the bottom of
/// `present_surface` uses.
static IN_FLIGHT_SUBMISSIONS: Mutex<std::collections::VecDeque<wgpu::SubmissionIndex>> =
    Mutex::new(std::collections::VecDeque::new());

/// Notes the submission a frame - or a readback - ended with, so the next present knows what to wait
/// for and how much is in flight.
fn remember_submission(index: wgpu::SubmissionIndex) {
    *LAST_SUBMISSION.lock() = Some(index);
}

fn new_encoder(wm: &WmRenderer) -> wgpu::CommandEncoder {
    wm.gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("<wm/mc command encoder>"),
        })
}

/// The shared encoder, created on first use if it does not exist yet.
///
/// The renderer is reachable globally, so this needs no argument and every entry point that records
/// something can simply ask for it.
fn shared_encoder() -> *mut wgpu::CommandEncoder {
    let mut slot = SHARED_ENCODER.lock();

    if let Some(address) = *slot {
        return address as *mut wgpu::CommandEncoder;
    }

    let Some(renderer) = RENDERER.get() else {
        panic!("wgpu-mc: a command encoder was asked for before the renderer existed");
    };

    let pointer = Box::into_raw(Box::new(new_encoder(renderer)));
    *slot = Some(pointer as usize);
    LIVE_ENCODER_COUNT.store(1, Ordering::Relaxed);

    pointer
}

/// Runs [body] against the shared encoder.
fn with_shared_encoder<R>(body: impl FnOnce(&mut wgpu::CommandEncoder) -> R) -> R {
    let pointer = shared_encoder();

    // Safety: every caller is called from the render thread, in the order Minecraft records, and
    // wgpu only allows one pass to be recording at a time - which is also what makes one encoder
    // enough for all of Minecraft's encoder objects.
    unsafe { body(&mut *pointer) }
}

/// Submits whatever the shared encoder has recorded and starts a new one.
///
/// This is the *only* submission point there is: the JVM side records clears, passes, copies and
/// uploads into this one encoder and asks for a submit from two places - before a readback, and,
/// through the blit below, before a present. Everything else used to flush after itself, which was
/// ten submissions a frame, and the frame's own boundaries are cleaner for it: the start timestamp
/// of a frame's GPU measurement now lands at the start of the frame rather than at whichever
/// mid-frame flush happened last.
///
/// A render pass borrows the encoder while it is recording it, so finishing the encoder here would
/// pull it out from under that borrow. The JVM side does not do that, and if it ever does, this
/// refuses and says so rather than corrupting the recording.
fn flush_shared_encoder(wm: &WmRenderer) {
    if LIVE_PASS_COUNT.load(Ordering::Relaxed) != 0 {
        error!(
            "wgpu-mc: refusing to submit while {} render pass(es) are open; the recording stays in \
             the encoder",
            LIVE_PASS_COUNT.load(Ordering::Relaxed)
        );
        return;
    }

    let pointer = shared_encoder();

    // Safety: as above - the render thread is the only one recording, and it is here.
    let finished = unsafe { std::mem::replace(&mut *pointer, new_encoder(wm)) };

    // The encoder that was just installed is where the next frame starts recording, so if the last
    // frame has already been presented, its start timestamp goes here - see `timing`.
    crate::timing::frame_begin(wm, unsafe { &mut *pointer });

    let submission = SUBMISSIONS.fetch_add(1, Ordering::Relaxed);
    let index = wm.gpu.queue.submit([finished.finish()]);
    remember_submission(index);

    // Everything that was staged is in the stream that just went out, so the ring can hand those
    // buffers out again - once that submission has completed, which is what the callback says.
    finish_staging(wm, submission);
}

/// A ring of staging buffers that uploads travel through instead of the queue.
///
/// `Queue::write_buffer` is applied *at* a submission, ahead of every command in it, which is why
/// re-writing bytes an already recorded draw reads has to force a submission of its own - see
/// `WRITE_HIGH_WATER`, and the entities that were invisible until it did. Putting the bytes in the
/// *stream* instead costs one `copy_buffer_to_buffer` per upload and no submission at all: a copy is
/// a command, so when it happens is where it was recorded.
///
/// The bytes reach the staging buffer the same way they used to reach their destination - a
/// `Queue::write_buffer` - but that is now safe for free: the staging region is one nothing has ever
/// read, so "applied first, before the frame's commands" is exactly where it belongs, and the copy
/// that consumes it is a command that comes after it. The buffers are *not* mapped: wgpu refuses a
/// still-mapped buffer as the source of a copy ("Buffer with 'wgpu-mc staging' label is still
/// mapped"), and it does not need to be, because the write goes through the queue either way.
///
/// The price is that a staging region may only be reused once the submission reading it has
/// finished, so there is a ring of them and a buffer is handed out again only after every submission
/// that could still be reading it has completed. `Queue::on_submitted_work_done` is what says so. A
/// ring with no room left falls back to the queue-write path, which is still correct - it just costs
/// the submission this exists to avoid - so the frame keeps rendering if the GPU ever falls behind.
///
/// This is where ~3,200 submissions a second went. Minecraft keeps one immediate vertex buffer per
/// pipeline and rewrites it at offset zero for every draw of a batch, so every one of those writes
/// was a rewrite below the mark.
struct Stage {
    buffer: wgpu::Buffer,
    /// The next free byte, always aligned to `COPY_BUFFER_ALIGNMENT`.
    cursor: u64,
    /// Whether copies recorded since the last submission read this buffer.
    dirty: bool,
    /// The submission index after which no command reads this buffer.
    busy_until: u64,
}

/// How large each staging buffer is, and how many of them the ring holds.
///
/// Four megabytes is a compromise between "a frame's uploads fit" and "the ring is not a texture
/// atlas of its own"; four buffers is enough for the GPU to be three submissions behind without the
/// ring running dry, which no frame is.
const STAGING_BYTES: u64 = 4 * 1024 * 1024;
const STAGING_BUFFERS: usize = 4;

static STAGING: Mutex<Option<Vec<Stage>>> = Mutex::new(None);

/// How many uploads went through the staging ring, and how much went with them.
static STAGED_UPLOADS: AtomicU64 = AtomicU64::new(0);
static STAGED_BYTES: AtomicU64 = AtomicU64::new(0);
/// How many uploads could not be staged and went through `Queue::write_buffer` instead.
static STAGING_FALLBACKS: AtomicU64 = AtomicU64::new(0);

/// How many of the submissions have completed, which is what frees a staging buffer.
static SUBMISSIONS_DONE: AtomicU64 = AtomicU64::new(0);

fn new_stage(device: &wgpu::Device) -> Stage {
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("wgpu-mc staging"),
        size: STAGING_BYTES,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });

    Stage {
        buffer,
        cursor: 0,
        dirty: false,
        busy_until: 0,
    }
}

/// Copies `bytes` into the staging ring and records the copy into [buffer] at `start`.
///
/// Returns false when the upload cannot be staged - no room in the ring, a buffer that cannot take a
/// copy, or a buffer so large it does not fit a staging buffer at all - and the caller falls back to
/// `Queue::write_buffer`. Never returns false for a *failed* upload: a refusal here is a decision
/// about how to upload, not about whether to.
fn stage_copy(wm: &WmRenderer, buffer: &wgpu::Buffer, start: u64, bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }

    // A destination without `COPY_DST` cannot be the target of a copy, and asking wgpu anyway would
    // be a validation error - which ends the process here. Minecraft's buffers all ask for it (every
    // mapped buffer does, and the immediate ones are `COPY_DST | VERTEX`), so this is the backstop.
    if !buffer.usage().contains(wgpu::BufferUsages::COPY_DST) {
        STAGING_FALLBACKS.fetch_add(1, Ordering::Relaxed);
        return false;
    }

    // The copy covers whole four-byte units, because `copy_buffer_to_buffer` has to. The destination
    // gets the whole range, so the padding is written as zeroes rather than left as whatever the
    // staging buffer held - which would make two runs of the same upload differ in those bytes.
    let wanted = (bytes.len() as u64 + 3) & !3;
    if wanted > STAGING_BYTES {
        STAGING_FALLBACKS.fetch_add(1, Ordering::Relaxed);
        return false;
    }

    let done = SUBMISSIONS_DONE.load(Ordering::Relaxed);
    let mut guard = STAGING.lock();
    let stages = guard.get_or_insert_with(|| {
        (0..STAGING_BUFFERS)
            .map(|_| new_stage(&wm.gpu.device))
            .collect()
    });

    let Some(index) = stages
        .iter()
        .position(|stage| stage.busy_until <= done && stage.cursor + wanted <= STAGING_BYTES)
    else {
        STAGING_FALLBACKS.fetch_add(1, Ordering::Relaxed);
        return false;
    };

    let stage = &mut stages[index];
    let offset = stage.cursor;
    stage.cursor += wanted;
    stage.dirty = true;

    let source = stage.buffer.clone();
    drop(guard);

    // The queue write lands at the next submission, ahead of its commands; the copy is one of those
    // commands and comes after it. Nothing has ever read this region, so there is no draw to order
    // against, and the write needs no submission of its own.
    wm.gpu.queue.write_buffer(&source, offset, bytes);

    let padding = (wanted - bytes.len() as u64) as usize;
    if padding > 0 {
        wm.gpu
            .queue
            .write_buffer(&source, offset + bytes.len() as u64, &[0u8; 3][..padding]);
    }

    with_shared_encoder(|encoder| {
        encoder.copy_buffer_to_buffer(&source, offset, buffer, start, wanted);
    });

    STAGED_UPLOADS.fetch_add(1, Ordering::Relaxed);
    STAGED_BYTES.fetch_add(bytes.len() as u64, Ordering::Relaxed);
    true
}

/// Hands the staging buffers back once the submission that read them has completed.
///
/// Called right after a successful submit, with that submission's index. Only a buffer with copies
/// waiting goes back: one that was filled after the submit keeps its cursor, because its copies are
/// in the *next* submission and handing it out again would overwrite what they read.
fn finish_staging(wm: &WmRenderer, submission: u64) {
    let mut guard = STAGING.lock();
    let Some(stages) = guard.as_mut() else {
        return;
    };

    let mut waiting = false;

    for stage in stages.iter_mut() {
        if stage.dirty {
            stage.dirty = false;
            stage.cursor = 0;
            stage.busy_until = submission;
            waiting = true;
        }
    }

    if !waiting {
        return;
    }

    // One callback per submission that carried staged bytes, and it only ever moves the counter
    // forwards: `on_submitted_work_done` fires when everything submitted so far has finished, so two
    // of these can arrive out of order.
    wm.gpu.queue.on_submitted_work_done(move || {
        SUBMISSIONS_DONE.fetch_max(submission, Ordering::Relaxed);
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn create_command_encoder(wm: &WmRenderer) -> Box<CommandEncoderHandle> {
    let mut slot = SHARED_ENCODER.lock();
    if slot.is_none() {
        *slot = Some(Box::into_raw(Box::new(new_encoder(wm))) as usize);
        LIVE_ENCODER_COUNT.store(1, Ordering::Relaxed);
    }

    Box::new(CommandEncoderHandle)
}

/// Creates a view over a sub-range of a texture's mip chain.
///
/// The mip range is not decoration. wgpu only accepts a view with **exactly one** mip level as a
/// render attachment, and the size it renders into - the default viewport and scissor - is that
/// level's size, not the texture's. 26.1 renders into one mip level at a time to build a sprite
/// atlas (`TextureAtlas#uploadInitialContents` blits every sprite into `mipViews[level]`), so a
/// view that silently covers the whole chain renders every mip level into mip 0 at a fraction of
/// the size, which is what turned the blocks atlas into a pile of shrunken copies in one corner.
///
/// `mip_levels` of 0 means "the rest of the chain", which is what the one-argument
/// `GpuDevice#createTextureView` asks for; the range is clamped to what the texture actually has,
/// because Minecraft asks for as many levels as the *atlas* has even for a texture that is too
/// small to hold them.
#[unsafe(no_mangle)]
pub extern "C" fn create_texture_view(
    wm: &WmRenderer,
    texture: &wgpu::Texture,
    usage: u32,
    base_mip_level: u32,
    mip_levels: u32,
) -> Box<wgpu::TextureView> {
    // A view request for a texture Minecraft has already closed is answered with a view over a
    // throwaway 1x1 texture: wgpu treats it as a validation error, and a validation error is fatal
    // here. The Kotlin side checks `Texture#isClosed` first, so this is the second line of defence
    // for the pointers that reach the ABI some other way.
    if !texture_is_alive(texture) {
        // Every field comes from the tombstone: the texture itself must not be read.
        let (width, height, format) =
            dead_texture_description(texture).unwrap_or((1, 1, wgpu::TextureFormat::Rgba8Unorm));
        let view = Box::new(placeholder_view(wm, format, width, height));
        // Counted like any other view: `drop_texture_view` decrements for every view the JVM
        // closes, and a view that was never counted makes the live count drift downwards.
        LIVE_VIEW_COUNT.fetch_add(1, Ordering::Relaxed);
        note_view_alive(&view, "<placeholder for a closed texture>".to_string());
        return view;
    }

    let available = texture.mip_level_count();
    let base = base_mip_level.min(available.saturating_sub(1));
    let count = if mip_levels == 0 {
        None
    } else {
        Some(mip_levels.min(available - base).max(1))
    };

    // Read before the view exists, so that the view can be named after the texture it was made
    // from: this label is the whole point of the registry when a draw later binds the address.
    let label = format!(
        "{} (mip {}{})",
        texture_label(texture),
        base,
        match count {
            Some(count) if count > 1 => format!("..{}", base + count - 1),
            _ => String::new(),
        }
    );

    let texture_view = texture.create_view(&wgpu::TextureViewDescriptor {
        label: None,
        format: Some(texture.format()),
        dimension: Some(if (usage & 16) != 0 {
            wgpu::TextureViewDimension::Cube
        } else {
            wgpu::TextureViewDimension::D2
        }),
        usage: None,
        aspect: Default::default(),
        base_mip_level: base,
        mip_level_count: count,
        base_array_layer: 0,
        array_layer_count: None,
    });

    LIVE_VIEW_COUNT.fetch_add(1, Ordering::Relaxed);

    let view = Box::new(texture_view);
    note_view_alive(&view, label);
    view
}

#[unsafe(no_mangle)]
pub extern "C" fn drop_render_pass(_: Box<BlazeRenderPass>) {
    LIVE_PASS_COUNT.fetch_sub(1, Ordering::Relaxed);
}

#[unsafe(no_mangle)]
pub extern "C" fn create_render_pass(
    _encoder: &mut CommandEncoderHandle,
    render_pass_descriptor: &BlazeRenderPassDescriptor,
    name: FfiStr,
) -> Box<BlazeRenderPass> {
    LIVE_PASS_COUNT.fetch_add(1, Ordering::Relaxed);

    // Bookkeeping for the previous pass, which is now closed as far as drawing goes.
    count_pass();

    // Diagnostics, and gated: a pass trace costs a `format!` of the target's address plus two lock
    // acquisitions per pass, for a list that only `log_render_stats` reads.
    if crate::debug::logging() {
        let mut current = CURRENT_TRACE.lock();
        if let Some(trace) = current.take() {
            let mut traces = TRACES.lock();
            // Sixteen passes is a few frames' worth, which is as much as the stat line prints.
            if traces.len() >= 16 {
                traces.remove(0);
            }
            traces.push(trace);
        }

        *current = Some(PassTrace {
            draws: 0,
            clear: render_pass_descriptor
                .attachments
                .iter()
                .any(|attachment| attachment.clear_value.is_some()),
            depth: render_pass_descriptor.depth_attachment.is_some(),
            target: match render_pass_descriptor.attachments.iter().next() {
                Some(attachment) => format!("{name} @ {:p}", &attachment.texture_view),
                None => format!("{name} @ <no attachment>"),
            },
            pipelines: Vec::new(),
        });
    }

    // Safety: the render thread is the only one recording, it records in call order, and wgpu
    // allows one pass at a time - which is what makes one encoder enough.
    let encoder = unsafe { &mut *shared_encoder() };

    let render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: None,
        color_attachments: &render_pass_descriptor
            .attachments
            .iter()
            .map(|attachment| {
                Some(wgpu::RenderPassColorAttachment {
                    view: attachment.texture_view,
                    depth_slice: None,
                    resolve_target: None,
                    // `None` is `OptionalInt.empty()` on the Java side: "no clear value", which is
                    // what OpenGL does when a pass does not clear. It is *not* "clear to zero" -
                    // `Operations::default()` would be exactly that, and clearing a target that
                    // already holds this frame's earlier passes is how a screen the renderer drew
                    // ends up presented empty.
                    ops: match attachment.clear_value {
                        None => wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                        Some(clear_color) => wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: clear_color[0] as f64,
                                g: clear_color[1] as f64,
                                b: clear_color[2] as f64,
                                a: clear_color[3] as f64,
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                    },
                })
            })
            .collect::<Vec<_>>(),
        depth_stencil_attachment: render_pass_descriptor.depth_attachment.map(|tex| {
            wgpu::RenderPassDepthStencilAttachment {
                view: tex.texture_view,
                depth_ops: Some(wgpu::Operations {
                    // A pass that asks for a depth clear has to get one. This used to load the
                    // depth buffer unconditionally, which silently dropped every `clearDepth` a
                    // caller passed, and left the frame at the mercy of whatever the depth texture
                    // happened to hold.
                    load: match tex.clear_value {
                        Some(clear_depth) => wgpu::LoadOp::Clear(*clear_depth as f32),
                        None => wgpu::LoadOp::Load,
                    },
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }
        }),
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });

    Box::new(BlazeRenderPass::new(render_pass.forget_lifetime()))
}

/// Records one draw, bindings and all.
///
/// The whole per-draw ABI: the call carries the pipeline, the vertex and index buffers, the draw
/// parameters and every binding by slot. The bind groups it needs live in the pass, which is where
/// they are reused and where they are freed - so no draw allocates and none frees.
///
/// The answer says whether the draw was recorded: a draw that names a binding combination nobody
/// has seen, and does not carry the bindings it was built from, is refused so that the JVM can send
/// them and ask again. See `DrawCall::combo`.
#[unsafe(no_mangle)]
pub extern "C" fn draw_call(wm: &WmRenderer, pass: &mut BlazeRenderPass, call: &DrawCall) -> bool {
    crate::blaze::draw_call(wm, pass, call)
}

/// The wgpu usage flags a Blaze3D usage mask asks for.
///
/// Every buffer this side creates goes through here, because a usage that only one entry point
/// derives is a usage that only one entry point has. `create_buffer_init` was missing the texel
/// buffer translation and `allocate_gpu_buffer_mapped` ignored the mask it was handed entirely, so
/// the same Minecraft usage mask produced three different wgpu buffers depending on which call
/// vanilla happened to make - and a bind group built for one of them is a validation error, which
/// ends the process here.
///
/// Two of the bits are not a straight translation:
///
/// - `COPY_DST` is set for everything Minecraft uploads into *and* for everything it maps itself,
///   because both arrive at [`write_to_buffer`] - which is `Queue::write_buffer`, a copy from the
///   CPU, and wgpu refuses that on a buffer without this usage. Vanilla asks for `USAGE_COPY_DST` on
///   the buffers it uploads into, but a mapped buffer only asks for `USAGE_MAP_WRITE`:
///   `CloudRenderer`'s face buffer is `USAGE_MAP_WRITE | USAGE_UNIFORM_TEXEL_BUFFER`, so the faces
///   were written on the CPU, never copied to the GPU, and the shader read the zeroes the buffer was
///   created with. Every face decoded to cell (0, 0) facing down - which is one square of cloud
///   above the player's head that drifts with the cloud offset and snaps back, with no cloud layer
///   anywhere else.
/// - `USAGE_UNIFORM_TEXEL_BUFFER` becomes `STORAGE`: the shaders that read a texel buffer go through
///   the SSBO shim in `preprocessing.rs`, so that is the usage the bind group asks for.
pub fn wgpu_buffer_usages(usage: u32) -> wgpu::BufferUsages {
    let mut flags = wgpu::BufferUsages::empty();
    flags.set(wgpu::BufferUsages::MAP_READ, usage & 1 != 0);
    flags.set(wgpu::BufferUsages::MAP_WRITE, usage & 2 != 0);
    flags.set(
        wgpu::BufferUsages::COPY_DST,
        usage & 8 != 0 || usage & 2 != 0,
    );
    flags.set(wgpu::BufferUsages::COPY_SRC, usage & 16 != 0);
    flags.set(wgpu::BufferUsages::VERTEX, usage & 32 != 0);
    flags.set(wgpu::BufferUsages::INDEX, usage & 64 != 0);
    flags.set(wgpu::BufferUsages::UNIFORM, usage & 128 != 0);
    flags.set(wgpu::BufferUsages::STORAGE, usage & 256 != 0);

    // Readable back for diagnostics, the same way `create_texture` always is. Only for buffers that
    // cannot be mapped: wgpu rejects `MAP_READ | COPY_SRC` outright, and a mappable buffer needs no
    // help being read.
    if usage & 1 == 0 {
        flags.insert(wgpu::BufferUsages::COPY_SRC);
    }

    flags
}

/// The wgpu usage flags a buffer was created with, as raw bits, for diagnostics.
///
/// A log line saying a bind group wanted `STORAGE` and the buffer only has `MAP_WRITE` is the whole
/// answer to "why does this draw read nothing", and the JVM side cannot see the flags it derived.
#[unsafe(no_mangle)]
pub extern "C" fn buffer_usages(buffer: &wgpu::Buffer) -> u64 {
    buffer.usage().bits() as u64
}

#[unsafe(no_mangle)]
pub extern "C" fn create_buffer(
    wm: &WmRenderer,
    label: *const c_char,
    usage: u32,
    size: u64,
) -> Box<wgpu::Buffer> {
    let label = unsafe { CStr::from_ptr(label) };

    let wgpu_usage_flags = wgpu_buffer_usages(usage);

    let buffer = wm.gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label.to_str().unwrap()),
        size: size as _,
        usage: wgpu_usage_flags,
        mapped_at_creation: false,
    });

    LIVE_BUFFER_COUNT.fetch_add(1, Ordering::Relaxed);
    LIVE_BUFFER_BYTES.fetch_add(size, Ordering::Relaxed);

    Box::new(buffer)
}

/// The highest byte written into each buffer since it was created, by buffer address.
///
/// A submission carries the frame's recorded commands, and a `Queue::write_buffer` is applied *at*
/// that submission - ahead of every command in it. So a write that lands on bytes an already recorded
/// draw reads changes what that draw sees, and the frame has to be submitted before the write. That
/// is what this tracks: a write *above* the mark is into bytes nothing has read yet - Minecraft
/// writes every dynamic uniform, vertex block and face mesh into a fresh slice of a ring buffer - and
/// can travel with the frame, while a write at or below it may be rewriting something in flight.
///
/// The case this exists for is `RenderSystem#setShaderLights`: the `Lighting` block is a small,
/// *fixed-offset* uniform that is re-written between draws, and with the whole frame in one
/// submission every draw in it saw the last value - the item and entity renderers read zeroed lights
/// and drew black models, which is what the GUI item atlas came out full of.
///
/// The marks survive a submission, and that is the whole point: after a submission, the writes that
/// were applied with it are still what the *next* draw of the same range would otherwise share a
/// submission with. `VertexFormat#uploadToBuffer` is the case that proved it - one vertex buffer per
/// pipeline, rewritten at offset zero for every draw in a batch - and with the marks cleared by each
/// forced submission only the *first* rewrite of a frame was ordered: the second and every later one
/// travelled with the frame, so every draw of the batch read the last one's geometry. Entities drawn
/// with each other's geometry are models on top of each other, and the item atlas drew every slot
/// with the last item's, which is what "the icons are scrambled" looked like.
static WRITE_HIGH_WATER: std::sync::Mutex<Option<std::collections::HashMap<usize, u64>>> =
    std::sync::Mutex::new(None);

/// Whether writing `[start, start + length)` into the buffer at `address` can change what an already
/// recorded command reads, and records the write.
///
/// Reads and writes are taken to be ordered by offset: every write into a buffer raises the mark, and
/// a write below it is one that the recorded frame may already be reading from. That is conservative
/// in exactly one direction - a *different* byte range below the mark forces a submission it did not
/// have to - and Minecraft's uploads share one offset space with their readers, so the conservative
/// answer is the safe one.
fn write_needs_a_submission(address: usize, start: u64, length: u64) -> bool {
    let mut marks = WRITE_HIGH_WATER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let marks = marks.get_or_insert_with(Default::default);

    let previous = marks.insert(address, start + length);
    matches!(previous, Some(mark) if start < mark)
}

/// Forgets a buffer's write mark, which its drop does: the address can be handed to a new buffer, and
/// a mark that outlives its buffer would make that one submit a frame for every upload.
fn forget_write_marks(address: usize) {
    if let Some(marks) = WRITE_HIGH_WATER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_mut()
    {
        marks.remove(&address);
    }
}

/// Copies `length` bytes from `data` into `buffer` at `start`.
///
/// # Safety
///
/// `data` must point at `length` readable bytes. The JVM side passes the address of a native staging
/// allocation it just filled, and this copies synchronously, so the pointer only has to outlive this
/// call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn write_to_buffer(
    wm: &WmRenderer,
    buffer: &wgpu::Buffer,
    start: u64,
    length: u64,
    data: *const u8,
) {
    // A missing usage would be a wgpu validation error, and a validation error ends the process
    // here - so a buffer that cannot take the write is called out instead of being handed to wgpu.
    // Silently dropping the write is what made the clouds invisible for as long as they were: the
    // bytes were written on the CPU and never arrived.
    if !buffer.usage().contains(wgpu::BufferUsages::COPY_DST) {
        log::error!(
            "wgpu-mc: refusing to write {} bytes at {} into a {} byte buffer created without \
             COPY_DST",
            length,
            start,
            buffer.size()
        );
        return;
    }

    // `Queue::write_buffer` needs both ends of the range to be a multiple of
    // `COPY_BUFFER_ALIGNMENT`, and wgpu answers an unaligned one with a validation error, which ends
    // the process. The JVM side rounds its uploads to 16 bytes for exactly this reason, so this is
    // the backstop for the paths that do not - and the write that is dropped is the last three bytes
    // of an upload the caller was told is aligned.
    if !start.is_multiple_of(wgpu::COPY_BUFFER_ALIGNMENT)
        || !length.is_multiple_of(wgpu::COPY_BUFFER_ALIGNMENT)
    {
        log::error!(
            "wgpu-mc: refusing to write {length} bytes at {start}, which is not a multiple of the \
             {} byte copy alignment",
            wgpu::COPY_BUFFER_ALIGNMENT
        );
        return;
    }

    // SAFETY: the caller guarantees `data` covers `length` bytes, per the contract above.
    let bytes = unsafe { std::slice::from_raw_parts(data, length as _) };

    // The bytes go into the staging ring and travel as a copy inside the frame's own encoder, which
    // is ordered by the command stream and needs no submission at all. Only a staging ring with no
    // room left falls through to the queue-write path below, which does need one.
    if stage_copy(wm, buffer, start, bytes) {
        return;
    }

    // A write that lands on bytes an already recorded draw reads has to be submitted *before* it:
    // `Queue::write_buffer` is applied at the next submission, ahead of every command in it, so
    // without this the frame's draws would all read this write's value. See `WRITE_HIGH_WATER`.
    if write_needs_a_submission(buffer as *const wgpu::Buffer as usize, start, length) {
        flush_shared_encoder(wm);
    }

    wm.gpu.queue.write_buffer(buffer, start, bytes);
}

/// Decodes a packed `ARGB` colour into wgpu's float colour, matching `net.minecraft.util.ARGB`.
fn argb_to_wgpu_color(argb: u32) -> wgpu::Color {
    let channel = |shift: u32| ((argb >> shift) & 0xFF) as f64 / 255.0;

    wgpu::Color {
        r: channel(16),
        g: channel(8),
        b: channel(0),
        a: channel(24),
    }
}

/// The textures this side has created and not yet dropped, by address.
///
/// Every view is created from a raw `*const wgpu::Texture`, and wgpu answers a freed one with a
/// validation error - which is fatal here, because a validation error runs the panic hook and the
/// panic hook ends the process. Minecraft closes a render target's textures when it rebuilds one,
/// and a clear or a view request can still arrive for one of them in the same frame, so the pointer
/// is checked before it is used. A skipped clear or a skipped view costs a frame; the alternative
/// costs the session.
///
/// The address that matters is the one the JVM holds, which is the `Box` this side hands out - not
/// the address of the local the box was built from. Registering the wrong one left every live
/// texture unlisted, and the allocator hands a freed `Box` straight back out again, so a brand new
/// texture was regularly mistaken for its predecessor and replaced by a 1x1 placeholder. That
/// placeholder then failed wgpu's scissor check ("Scissor Rect { x: 0, y: 0, w: 878, h: 504 } is not
/// contained in the render target (1, 1, 1)") and took the process down.
static LIVE_TEXTURES: Mutex<Option<std::collections::HashMap<usize, String>>> = Mutex::new(None);

/// The views this side has created and not yet dropped, by the address the JVM holds.
///
/// A view is what a draw binds, and a bind group is built from the raw pointer the JVM kept - so a
/// view that Minecraft closed while a pass still named it is a use-after-free whose bytes are
/// whatever the allocator put there next, which is usually *another view*. That is the shape of
/// "every mob wears some other mob's skin, and sometimes an item icon": the picture is not wrong
/// because the wrong texture was chosen by name, but because the pointer that named the right
/// texture now names a different one.
///
/// The label behind each address is what makes that readable: it is recorded while the view is
/// alive, so a report after the drop never touches the freed memory.
static LIVE_VIEWS: Mutex<Option<std::collections::HashMap<usize, String>>> = Mutex::new(None);

/// The addresses of views that have been dropped, so a later use of one is caught rather than read.
static DEAD_VIEWS: Mutex<Option<std::collections::HashSet<usize>>> = Mutex::new(None);

/// The order view tombstones were added in, so the oldest can be forgotten.
static DEAD_VIEW_ORDER: Mutex<Option<std::collections::VecDeque<usize>>> = Mutex::new(None);
const DEAD_VIEW_LIMIT: usize = 2048;

/// What a dropped texture was, kept so a use-after-free can be described without reading it.
struct DeadTexture {
    label: String,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
    /// What the texture cost, so the live total can be given back when it is dropped.
    bytes: u64,
}

/// How many textures, bytes of texture and views this side has handed out and not taken back.
///
/// Diagnostics, and the answer to "where did twenty gigabytes go": a texture stays alive as long as
/// any view of it does, so a view Minecraft never closes is a texture that is never freed.
static LIVE_TEXTURE_COUNT: AtomicU64 = AtomicU64::new(0);
static LIVE_TEXTURE_BYTES: AtomicU64 = AtomicU64::new(0);
static LIVE_VIEW_COUNT: AtomicU64 = AtomicU64::new(0);

/// The same, for everything else this side hands the JVM and expects back.
///
/// Every one of these is a `Box` the JVM holds a pointer to, and a missing `drop` on the far side
/// leaks it - and everything it holds - until the process ends. Command buffers and bind groups are
/// the expensive ones: a frame records thousands of passes, so a leak of one per pass is hundreds
/// of megabytes a second.
static LIVE_BUFFER_COUNT: AtomicU64 = AtomicU64::new(0);
static LIVE_BUFFER_BYTES: AtomicU64 = AtomicU64::new(0);
static LIVE_ENCODER_COUNT: AtomicU64 = AtomicU64::new(0);
static LIVE_PASS_COUNT: AtomicU64 = AtomicU64::new(0);
pub static LIVE_BIND_GROUP_COUNT: AtomicU64 = AtomicU64::new(0);
static LIVE_PIPELINE_COUNT: AtomicU64 = AtomicU64::new(0);

static DEAD_TEXTURES: Mutex<Option<std::collections::HashMap<usize, DeadTexture>>> =
    Mutex::new(None);

/// The order tombstones were added in, so the oldest can be forgotten once there are too many.
///
/// A tombstone is only there to catch a use of a texture in the frame or two after Minecraft closed
/// it, so keeping the most recent few hundred is as good as keeping all of them - and the map
/// otherwise grows for the whole session, one label string per texture ever dropped.
static DEAD_TEXTURE_ORDER: Mutex<Option<std::collections::VecDeque<usize>>> = Mutex::new(None);
const DEAD_TEXTURE_LIMIT: usize = 512;

fn texture_address(texture: &wgpu::Texture) -> usize {
    texture as *const wgpu::Texture as usize
}

/// Registers a texture the JVM is about to be handed, which must be called with the `Box`.
///
/// The label is kept here rather than read back when the texture is dropped: wgpu 29's `Texture`
/// has no label accessor, and the name is what makes a use-after-free report readable.
fn note_texture_alive(texture: &wgpu::Texture, label: &str) {
    let address = texture_address(texture);
    LIVE_TEXTURES
        .lock()
        .get_or_insert_with(std::collections::HashMap::new)
        .insert(address, label.to_string());
    // The address may be one a closed texture used to occupy, which is why the tombstone is cleared
    // here: the check must not condemn a brand new texture for its predecessor's sins.
    if let Some(dead) = DEAD_TEXTURES.lock().as_mut() {
        dead.remove(&address);
    }
}

/// Remembers a texture that is being dropped, while its fields can still be read.
fn note_texture_dead(texture: &wgpu::Texture) {
    let address = texture_address(texture);
    let label = LIVE_TEXTURES
        .lock()
        .as_mut()
        .and_then(|live| live.remove(&address))
        .unwrap_or_else(|| "<unlabelled>".to_string());

    let dead = DeadTexture {
        label,
        width: texture.width(),
        height: texture.height(),
        format: texture.format(),
        bytes: texture_bytes(
            texture.width(),
            texture.height(),
            texture.depth_or_array_layers(),
        ),
    };

    LIVE_TEXTURE_COUNT.fetch_sub(1, Ordering::Relaxed);
    LIVE_TEXTURE_BYTES.fetch_sub(dead.bytes, Ordering::Relaxed);

    DEAD_TEXTURES
        .lock()
        .get_or_insert_with(std::collections::HashMap::new)
        .insert(address, dead);

    // Forget the oldest tombstones once there are too many, so this cannot grow for a whole
    // session. The entries are still in the order deque if their map entry was already replaced, and
    // removing a key that is not there is a no-op.
    let mut order = DEAD_TEXTURE_ORDER.lock();
    let order = order.get_or_insert_with(std::collections::VecDeque::new);
    order.push_back(address);

    while order.len() > DEAD_TEXTURE_LIMIT {
        if let Some(oldest) = order.pop_front()
            && let Some(dead) = DEAD_TEXTURES.lock().as_mut()
        {
            dead.remove(&oldest);
        }
    }
}

/// The label of a live texture, which is what names a view in a use-after-close report.
fn texture_label(texture: &wgpu::Texture) -> String {
    LIVE_TEXTURES
        .lock()
        .as_ref()
        .and_then(|live| live.get(&texture_address(texture)).cloned())
        .unwrap_or_else(|| "<unlabelled>".to_string())
}

/// Registers a view under the address the JVM is about to be handed.
///
/// Called with the `Box`, like [`note_texture_alive`]: the allocator reuses a freed box's address
/// immediately, and a view registered under the address of the local it was built from is a view
/// that will never be found again.
fn note_view_alive(view: &wgpu::TextureView, label: String) {
    let address = view as *const wgpu::TextureView as usize;
    LIVE_VIEWS
        .lock()
        .get_or_insert_with(std::collections::HashMap::new)
        .insert(address, label);
    if let Some(dead) = DEAD_VIEWS.lock().as_mut() {
        dead.remove(&address);
    }
}

/// Remembers a view that is being dropped, while it is still there to be named.
fn note_view_dead(view: &wgpu::TextureView) {
    let address = view as *const wgpu::TextureView as usize;
    LIVE_VIEWS
        .lock()
        .as_mut()
        .and_then(|live| live.remove(&address));

    DEAD_VIEWS
        .lock()
        .get_or_insert_with(std::collections::HashSet::new)
        .insert(address);

    let mut order = DEAD_VIEW_ORDER.lock();
    let order = order.get_or_insert_with(std::collections::VecDeque::new);
    order.push_back(address);

    while order.len() > DEAD_VIEW_LIMIT {
        if let Some(oldest) = order.pop_front()
            && let Some(dead) = DEAD_VIEWS.lock().as_mut()
        {
            dead.remove(&oldest);
        }
    }
}

/// Whether a view address names a live view, a dropped one, or one this side never made.
///
/// A report is what this is for: a draw that binds a dropped view is the bug that mixes textures
/// between models, and it has to be found without dereferencing the address to ask - so a caller
/// added when a mix is being chased would be the first. Nothing calls it today.
#[allow(dead_code)]
pub fn view_is_alive(address: usize) -> bool {
    LIVE_VIEWS
        .lock()
        .as_ref()
        .is_some_and(|live| live.contains_key(&address))
}

/// Whether a view address names one this side has dropped.
pub fn view_is_dead(address: usize) -> bool {
    DEAD_VIEWS
        .lock()
        .as_ref()
        .is_some_and(|dead| dead.contains(&address))
}

/// How many dropped view addresses are still remembered, for the report that names one.
pub fn dead_view_count() -> usize {
    DEAD_VIEWS
        .lock()
        .as_ref()
        .map(|dead| dead.len())
        .unwrap_or(0)
}

/// The label a view address was registered under, if any.
pub fn view_label(address: usize) -> Option<String> {
    LIVE_VIEWS
        .lock()
        .as_ref()
        .and_then(|live| live.get(&address).cloned())
}

/// How many views are live, for the render-stats line.
pub fn live_view_labels() -> usize {
    LIVE_VIEWS
        .lock()
        .as_ref()
        .map(|live| live.len())
        .unwrap_or(0)
}
/// An estimate of what a texture costs, good enough to watch for growth: four bytes a texel.
fn texture_bytes(width: u32, height: u32, depth_or_layers: u32) -> u64 {
    (width as u64) * (height as u64) * (depth_or_layers.max(1) as u64) * 4
}

/// Whether [texture] is one this side still owns, logging when it is not.
///
/// A pointer that is neither live nor known dead is *accepted*: the swapchain's own texture is
/// created by wgpu rather than by `create_texture`, and refusing an unknown pointer would break
/// presenting the frame. Only an address this side saw dropped is refused.
fn texture_is_alive(texture: &wgpu::Texture) -> bool {
    let address = texture_address(texture);
    if LIVE_TEXTURES
        .lock()
        .as_ref()
        .is_some_and(|live| live.contains_key(&address))
    {
        return true;
    }

    let dead = DEAD_TEXTURES.lock();
    let Some(dead) = dead.as_ref().and_then(|dead| dead.get(&address)) else {
        return true;
    };

    // The label comes from the drop, not from the texture: this reference is to freed memory, so
    // even a label lookup would be reading a dangling pointer.
    static REPORTED: AtomicU64 = AtomicU64::new(0);
    if REPORTED.fetch_add(1, Ordering::Relaxed).is_multiple_of(120) {
        warn!(
            "wgpu-mc: the {}x{} {:?} texture '{}' was used after it was closed; skipping it",
            dead.width, dead.height, dead.format, dead.label
        );
    }
    false
}

/// The size and format a dropped texture had, which is what a stand-in for it needs.
///
/// Nothing about the texture itself may be read here: the reference is to freed memory.
fn dead_texture_description(texture: &wgpu::Texture) -> Option<(u32, u32, wgpu::TextureFormat)> {
    let dead = DEAD_TEXTURES.lock();
    dead.as_ref()
        .and_then(|dead| dead.get(&texture_address(texture)))
        .map(|dead| (dead.width, dead.height, dead.format))
}

/// A whole-texture view, which is all a clear needs.
fn clear_view(texture: &wgpu::Texture) -> wgpu::TextureView {
    texture.create_view(&wgpu::TextureViewDescriptor {
        label: Some("<wm/mc clear>"),
        format: Some(texture.format()),
        dimension: Some(wgpu::TextureViewDimension::D2),
        usage: None,
        aspect: Default::default(),
        base_mip_level: 0,
        mip_level_count: None,
        base_array_layer: 0,
        array_layer_count: None,
    })
}

/// A stand-in view for a texture that no longer exists.
///
/// A view request that reaches the ABI for a closed texture would be a validation error, which is
/// fatal here, so the caller gets a view of *something* of the right format and size instead. The
/// size has to match what the texture had: wgpu checks every scissor against the render target, and
/// a 1x1 stand-in turns a full-window scissor into a fatal error of its own. The texture behind the
/// view is dropped at the end of this function: `wgpu::TextureView` keeps the resource itself alive,
/// so the view stays valid.
fn placeholder_view(
    wm: &WmRenderer,
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
) -> wgpu::TextureView {
    let texture = wm.gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("<wgpu-mc/closed texture>"),
        size: wgpu::Extent3d {
            width: width.max(1),
            height: height.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });

    clear_view(&texture)
}

/// Runs a render pass whose only job is its load op, which is how a clear is expressed in wgpu.
///
/// `CommandEncoder::clear_texture` cannot stand in: it clears to zero, and Minecraft clears depth
/// to 1.0 while testing with `LESS_THAN_OR_EQUAL`, so a depth buffer left at zero rejects every
/// fragment in the frame.
///
/// This is not optional despite looking like it. `GameRenderer` clears the main colour and depth
/// textures this way before it draws anything, every single frame - dropping it left the frame at
/// whatever the texture already held, which is why the window rendered black.
fn clear_attachments(
    encoder: &mut wgpu::CommandEncoder,
    color_texture: Option<&wgpu::Texture>,
    clear_color: u32,
    depth_texture: Option<&wgpu::Texture>,
    clear_depth: f64,
) {
    let color_view = color_texture
        .filter(|texture| texture_is_alive(texture))
        .map(clear_view);
    let depth_view = depth_texture
        .filter(|texture| texture_is_alive(texture))
        .map(clear_view);

    // A pass with no attachments at all is a validation error - "at least one attachment of any
    // kind must be provided" - and a validation error ends the process here. Both textures being
    // closed means the render target that owned them is gone, so there is nothing to clear anyway.
    if color_view.is_none() && depth_view.is_none() {
        return;
    }

    let color_attachments: Vec<Option<wgpu::RenderPassColorAttachment>> = color_view
        .iter()
        .map(|view| {
            Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(argb_to_wgpu_color(clear_color)),
                    store: wgpu::StoreOp::Store,
                },
            })
        })
        .collect();

    let depth_stencil_attachment =
        depth_view
            .as_ref()
            .map(|view| wgpu::RenderPassDepthStencilAttachment {
                view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(clear_depth as f32),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            });

    // Dropping the pass is what records it.
    drop(encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("<wm/mc clear>"),
        color_attachments: &color_attachments,
        depth_stencil_attachment,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    }));
}

#[unsafe(no_mangle)]
pub extern "C" fn clear_color_texture(
    _encoder: &mut CommandEncoderHandle,
    texture: &wgpu::Texture,
    clear_color: u32,
) {
    with_shared_encoder(|encoder| {
        clear_attachments(encoder, Some(texture), clear_color, None, 0.0)
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn clear_depth_texture(
    _encoder: &mut CommandEncoderHandle,
    texture: &wgpu::Texture,
    clear_depth: f64,
) {
    with_shared_encoder(|encoder| clear_attachments(encoder, None, 0, Some(texture), clear_depth));
}

#[unsafe(no_mangle)]
pub extern "C" fn clear_color_and_depth_textures(
    _encoder: &mut CommandEncoderHandle,
    color_texture: &wgpu::Texture,
    clear_color: u32,
    depth_texture: &wgpu::Texture,
    clear_depth: f64,
) {
    with_shared_encoder(|encoder| {
        clear_attachments(
            encoder,
            Some(color_texture),
            clear_color,
            Some(depth_texture),
            clear_depth,
        );
    });
}

/// The region variant: the colour is cleared *in place*, the depth whole.
///
/// `GlCommandEncoder` restricts `glClear` with the scissor box, and a wgpu load op has no such
/// thing - it always covers the attachment. So this used to clear the whole texture, and the caller
/// is `GuiItemAtlas#drawToSlot`: it re-draws one stale 32x32 slot of the GUI item atlas and clears
/// that slot first, which wiped *every other item icon in the atlas*. The icons came back only for
/// the items drawn after the last clear of the frame, and re-drawing an item is what a stale slot is
/// - hovering an item, or an item changing - so hovering refilled one icon and emptied the rest.
///
/// The colour half is now a rect-limited write of the clear colour, which is what a scissored clear
/// is: `Queue::write_texture` copies into a rectangle of a texture, and the bytes are the clear
/// colour rather than zeroes, so a caller that clears to something other than transparent black gets
/// what it asked for. The depth half is still the whole texture, and that is deliberate rather than
/// a shortcut: the only caller is the item atlas, whose depth attachment is never sampled and never
/// read back - it exists so an item's own faces depth-test against each other while it is drawn into
/// its slot - so clearing all of it before drawing into one slot changes nothing that anything can
/// observe. A caller that needed a *region* of depth kept would need a scissored depth draw, which
/// is a pipeline of its own and is listed under *Known gaps*.
#[unsafe(no_mangle)]
pub extern "C" fn clear_color_and_depth_textures_region(
    _encoder: &mut CommandEncoderHandle,
    color_texture: &wgpu::Texture,
    clear_color: u32,
    depth_texture: &wgpu::Texture,
    clear_depth: f64,
    region_x: u32,
    region_y: u32,
    region_width: u32,
    region_height: u32,
) {
    let color_bytes = clear_color_bytes(clear_color, color_texture.format());

    match color_bytes {
        Some(bytes) => write_color_region(
            color_texture,
            bytes,
            region_x,
            region_y,
            region_width,
            region_height,
        ),
        None => {
            // A format whose texels are not four bytes, which nothing in 26.1 clears by region.
            // Widening is wrong in the way that cost the item icons, so it says so rather than
            // doing it quietly.
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                warn!(
                    "wgpu-mc: a region clear of a {:?} texture cannot be written in place; it \
                     clears the whole texture instead",
                    color_texture.format()
                );
            });

            with_shared_encoder(|encoder| {
                clear_attachments(encoder, Some(color_texture), clear_color, None, 1.0);
            });
        }
    }

    with_shared_encoder(|encoder| {
        clear_attachments(encoder, None, 0, Some(depth_texture), clear_depth)
    });
}

/// The clear colour as the bytes a texture of [format] holds, or `None` for a format this does not
/// know how to write.
///
/// Minecraft's clear colour is packed `ARGB`, and the two eight-bit formats in play differ in the
/// order of the first two channels - which is the difference between a texture written as `RGBA8`
/// and one written as `BGRA8`.
fn clear_color_bytes(clear_color: u32, format: wgpu::TextureFormat) -> Option<[u8; 4]> {
    let a = ((clear_color >> 24) & 0xFF) as u8;
    let r = ((clear_color >> 16) & 0xFF) as u8;
    let g = ((clear_color >> 8) & 0xFF) as u8;
    let b = (clear_color & 0xFF) as u8;

    match format {
        wgpu::TextureFormat::Rgba8Unorm | wgpu::TextureFormat::Rgba8UnormSrgb => Some([r, g, b, a]),
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb => Some([b, g, r, a]),
        _ => None,
    }
}

/// Writes [bytes] over a rectangle of [texture], which is a scissored clear.
///
/// `bytes_per_row` is the rectangle's width in bytes, which `Queue::write_texture` takes as it comes:
/// it stages the data internally, unlike a buffer-to-texture copy, whose rows have to be padded to
/// the 256 byte copy alignment.
fn write_color_region(
    texture: &wgpu::Texture,
    bytes: [u8; 4],
    region_x: u32,
    region_y: u32,
    region_width: u32,
    region_height: u32,
) {
    let Some(wm) = RENDERER.get() else {
        return;
    };

    if !texture_is_alive(texture) || region_width == 0 || region_height == 0 {
        return;
    }

    let row_bytes = region_width * bytes.len() as u32;
    let data = bytes.repeat((region_width * region_height) as usize);

    // Once per run, because this is the line that says the item atlas' slot clears are in place
    // rather than wiping the atlas, and a run that shows blank icons wants to know which of the two
    // it is. A log line, so it is the logging switch that turns it on.
    static REPORTED: std::sync::Once = std::sync::Once::new();
    if crate::debug::logging() {
        REPORTED.call_once(|| {
            info!(
                "wgpu-mc: cleared a {region_width}x{region_height} region at {region_x},{region_y} \
                 of a {:?} texture in place",
                texture.format()
            );
        });
    }

    wm.gpu.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: Origin3d {
                x: region_x,
                y: region_y,
                z: 0,
            },
            aspect: Default::default(),
        },
        &data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(row_bytes),
            rows_per_image: Some(region_height),
        },
        Extent3d {
            width: region_width,
            height: region_height,
            depth_or_array_layers: 1,
        },
    );
}

/// Values this large are not a texture; they are what a mis-read descriptor looks like. Refusing
/// them here keeps a mis-typed handle a log line instead of a fatal wgpu validation error, which
/// aborts the JVM.
const DUMP_SANITY_LIMIT: u32 = 1 << 16;

/// Writes a texture out as raw RGBA, preceded by its width and height as two little-endian `u32`s.
///
/// Diagnostics. Nothing in the renderer calls this; it exists so the frame the renderer produces
/// can be looked at directly, without going through the window compositor. That is the only way to
/// tell "the renderer drew nothing" apart from "the swapchain never showed what it drew", and the
/// two need completely different fixes.
///
/// `flip_y` writes the rows bottom-up. The main render target needs it: this backend hands
/// Minecraft's OpenGL clip space to wgpu unchanged except for the y negation in
/// `preprocessing::EmulateGlClipSpace`, so the target holds the frame the way OpenGL would have
/// held it, first row at the bottom, and the present blit turns it over on the way to the
/// swapchain. Dumping it unflipped would write the file upside down relative to the presented
/// frame, which is exactly the comparison these dumps exist for.
///
/// Requires the texture to carry `COPY_SRC`.
#[unsafe(no_mangle)]
pub extern "C" fn dump_texture_rgba(
    wm: &WmRenderer,
    texture: &wgpu::Texture,
    path: FfiStr,
    flip_y: bool,
) -> bool {
    let width = texture.width();
    let height = texture.height();
    let unpadded = width * 4;
    let padded =
        unpadded.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;

    if width == 0 || height == 0 || width > DUMP_SANITY_LIMIT || height > DUMP_SANITY_LIMIT {
        error!("wgpu-mc: refusing to dump a {width}x{height} texture to {path}");
        return false;
    }

    let buffer = wm.gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("<wm/mc dump>"),
        size: (padded * height) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = wm
        .gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });

    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: Origin3d::ZERO,
            aspect: Default::default(),
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(height),
            },
        },
        Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );

    wm.gpu.queue.submit([encoder.finish()]);

    let slice = buffer.slice(..);
    let (sender, receiver) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = sender.send(result);
    });

    if wm
        .gpu
        .device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .is_err()
    {
        error!("wgpu-mc: the texture dump could not be waited for");
        return false;
    }

    match receiver.recv() {
        Ok(Ok(())) => {}
        _ => {
            error!("wgpu-mc: the texture dump could not be mapped");
            return false;
        }
    }

    let data = slice.get_mapped_range();
    let mut out = Vec::with_capacity(8 + (unpadded * height) as usize);
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());

    for row in 0..height {
        let source_row = if flip_y { height - 1 - row } else { row };
        let start = (source_row * padded) as usize;
        out.extend_from_slice(&data[start..start + unpadded as usize]);
    }

    drop(data);
    buffer.unmap();

    match std::fs::write(&*path, out) {
        Ok(()) => {
            info!("wgpu-mc: wrote {width}x{height} to {path}");
            true
        }
        Err(err) => {
            error!("wgpu-mc: could not write {path}: {err}");
            false
        }
    }
}

/// Dumps the image a `SurfaceTexture` holds.
///
/// The JVM side only ever has the `wgpu::SurfaceTexture` the acquire handed it, so the field access
/// happens here rather than by handing that pointer to [`dump_texture_rgba`] and hoping the two
/// types share a layout.
///
/// No flipping: a swapchain image's first row is the top of the window, so what lands in the file
/// is the picture as it was shown.
#[unsafe(no_mangle)]
pub extern "C" fn dump_surface_texture_rgba(
    wm: &WmRenderer,
    surface_texture: &SurfaceTexture,
    path: FfiStr,
) -> bool {
    dump_texture_rgba(wm, &surface_texture.texture, path, false)
}

/// Submits everything recorded so far, so the work is on the queue.
///
/// Minecraft calls this when a pass closes and before it presents. Both the encoder and the size
/// come from this side, so there is nothing to configure here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn flush_encoder(wm: &WmRenderer, _encoder: &mut CommandEncoderHandle) {
    flush_shared_encoder(wm);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn copy_buffer_to_buffer(
    _wm: &WmRenderer,
    _encoder: &mut CommandEncoderHandle,
    src: &wgpu::Buffer,
    dest: &wgpu::Buffer,
    src_offset: u64,
    dest_offset: u64,
    length: u64,
) {
    with_shared_encoder(|encoder| {
        encoder.copy_buffer_to_buffer(src, src_offset as _, dest, dest_offset, Some(length as _));
    });
}
/// Copies a rectangle of a texture into a buffer.
///
/// wgpu requires `bytes_per_row` to be a multiple of `COPY_BYTES_PER_ROW_ALIGNMENT` (256), and
/// Minecraft asks for tightly packed rows: `Screenshot.takeScreenshot` reads a whole 854-wide
/// target in one call, whose rows are 3416 bytes. Handing that number over is a validation error,
/// and a validation error here is fatal - it runs the panic hook, which takes the whole JVM with
/// it, so pressing F2 used to end the game. The rows are therefore copied through a padded scratch
/// buffer and written back one row at a time, exactly the way [`dump_texture_rgba`] already did.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copy_texture_to_buffer(
    wm: &WmRenderer,
    _encoder: &mut CommandEncoderHandle,
    source: &wgpu::Texture,
    dest: &wgpu::Buffer,
    offset: u64,
    mip: u32,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
) {
    // The token stands in for the one encoder this side owns; see `CommandEncoderHandle`.
    let encoder = unsafe { &mut *shared_encoder() };

    let texel_size = source.format().block_copy_size(None).unwrap();
    let unpadded = width * texel_size;
    let padded =
        unpadded.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;

    if unpadded == padded {
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: source,
                mip_level: mip,
                origin: Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: dest,
                layout: TexelCopyBufferLayout {
                    offset,
                    bytes_per_row: Some(unpadded),
                    rows_per_image: Some(height),
                },
            },
            Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        return;
    }

    let scratch = wm.gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("<wm/mc readback>"),
        size: (padded * height) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });

    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: source,
            mip_level: mip,
            origin: Origin3d { x, y, z: 0 },
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &scratch,
            layout: TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(height),
            },
        },
        Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );

    for row in 0..height as u64 {
        encoder.copy_buffer_to_buffer(
            &scratch,
            row * padded as u64,
            dest,
            offset + row * unpadded as u64,
            unpadded as u64,
        );
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copy_texture_to_texture(
    _encoder: &mut CommandEncoderHandle,
    source: &wgpu::Texture,
    destination: &wgpu::Texture,
    mip: u32,
    dest_x: u32,
    dest_y: u32,
    src_x: u32,
    src_y: u32,
    width: u32,
    height: u32,
) {
    // The token stands in for the encoder this side owns; see `CommandEncoderHandle`.
    let encoder = unsafe { &mut *shared_encoder() };

    let src_info = wgpu::TexelCopyTextureInfo {
        texture: source,
        mip_level: mip,
        origin: Origin3d {
            x: src_x,
            y: src_y,
            z: 0,
        },
        aspect: wgpu::TextureAspect::All,
    };
    let dest_info = wgpu::TexelCopyTextureInfo {
        texture: destination,
        mip_level: mip,
        origin: Origin3d {
            x: dest_x,
            y: dest_y,
            z: 0,
        },
        aspect: wgpu::TextureAspect::All,
    };

    let extent = Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };
    encoder.copy_texture_to_texture(src_info, dest_info, extent);
}

/// Where the processed shaders are written, when the diagnostics ask for them.
///
/// Diagnostics. The GLSL that reaches naga is not the GLSL Minecraft ships: uniforms are annotated
/// with the binding the pipeline layout gave them, implicit blocks are added, samplers are split.
/// When a shader reads a uniform that never arrives - or arrives at the wrong binding - the
/// processed source is the only place that shows which of the two happened, and the answer decides
/// whether the fix belongs on the JVM side's binding calls or on this side's numbering.
///
/// Turned on by the `dump shaders` debug setting, or by a file named `wgpu-dump-shaders` in the run
/// directory - the marker came first, and a marker needs no launcher support.
fn shader_dump_directory() -> Option<PathBuf> {
    static DIRECTORY: OnceCell<Option<PathBuf>> = OnceCell::new();

    if !crate::debug::dump_shaders() {
        return None;
    }

    DIRECTORY
        .get_or_init(|| {
            let run_directory = crate::RUN_DIRECTORY.get()?;
            let directory = run_directory.join("wgpu-shaders");

            std::fs::create_dir_all(&directory).ok()?;
            info!(
                "wgpu-mc: writing processed shaders to {}",
                directory.display()
            );
            Some(directory)
        })
        .clone()
}

/// Writes one pipeline's processed shaders out, when [`shader_dump_directory`] says to.
fn dump_shaders(name: &str, vert: &str, frag: &str) {
    let Some(directory) = shader_dump_directory() else {
        return;
    };

    let stem = name.replace(|character: char| !character.is_ascii_alphanumeric(), "_");

    for (suffix, source) in [("vert", vert), ("frag", frag)] {
        let path = directory.join(format!("{stem}.{suffix}.glsl"));
        if let Err(error) = std::fs::write(&path, source) {
            warn!("wgpu-mc: could not write {}: {error}", path.display());
        }
    }
}

/// The block sizes the shaders declare, read out of the preprocessed GLSL.
///
/// This is what the plans' `min_binding_size` and the bindings' sizes are taken from, rather than
/// leaving wgpu to derive the range from whatever length the JVM passed - a length rounded up to
/// sixteen bytes, which runs past the end of a buffer whose slice ends at the last byte. wgpu
/// validates a binding without a declared minimum against the shader's block at draw time anyway, so
/// asking naga once per pipeline is cheaper than being told about it per draw, and it is the only
/// number that cannot be wrong about how large that block is.
fn reflected_block_sizes(vert: &str, frag: &str) -> HashMap<String, u64> {
    let mut sizes = HashMap::new();

    for (stage, source) in [
        (naga::ShaderStage::Vertex, vert),
        (naga::ShaderStage::Fragment, frag),
    ] {
        let mut frontend = naga::front::glsl::Frontend::default();
        let options = naga::front::glsl::Options {
            stage,
            defines: Default::default(),
        };

        let Ok(module) = frontend.parse(&options, source) else {
            // The shader goes to wgpu as it is; if naga will not parse it here, its error there
            // says more about why than a second copy of the message would.
            if crate::debug::trace_dynamic_offsets() {
                warn!(
                    "wgpu-mc: naga would not parse the preprocessed {stage:?} shader for reflection"
                );
            }
            continue;
        };

        for (_, variable) in module.global_variables.iter() {
            // A GLSL `layout(std140) uniform Block { ... };` declares no instance name, so naga
            // leaves `variable.name` empty and puts `Block` on the *type* instead. Minecraft's
            // uniforms are all of that shape, and reading only `variable.name` therefore reflected
            // nothing at all - every layout went out with `min_binding_size: None`, which is the
            // check the plan exists to be able to state.
            let name = variable
                .name
                .clone()
                .or_else(|| module.types[variable.ty].name.clone());

            let Some(name) = name else {
                if crate::debug::trace_dynamic_offsets() {
                    warn!(
                        "wgpu-mc: a {:?} global in the {stage:?} shader has no name to reflect against",
                        variable.space
                    );
                }
                continue;
            };

            let sized = matches!(
                variable.space,
                naga::AddressSpace::Uniform | naga::AddressSpace::Storage { .. }
            );

            if !sized {
                continue;
            }

            let size = module.types[variable.ty].inner.size(module.to_ctx());

            // A runtime-sized array has no size of its own, and must not be given one: wgpu checks
            // those bindings against the shader instead.
            if size == 0 {
                continue;
            }

            sizes.insert(name, size as u64);
        }
    }

    if crate::debug::trace_dynamic_offsets() {
        info!("wgpu-mc: reflected block sizes: {sizes:?}");
    }

    sizes
}

/// Index buffers that turn a triangle fan into a triangle list, one per index count.
///
/// wgpu has no fan topology, and Minecraft's fan draws use its *sequential* index buffer
/// (`0, 1, 2, ...`), so the triangles are `(0, 1, 2), (0, 2, 3), (0, 3, 4), ...` - a shape a
/// triangle list can express, just not with those indices. The buffer is built once per size and
/// reused: fans are a handful of draws per frame (the sky dome, the stars, the sun and moon).
static FAN_INDICES: Mutex<Option<HashMap<u32, wgpu::Buffer>>> = Mutex::new(None);

/// Index buffers that turn a run of quads into a triangle list, one per vertex count.
///
/// Minecraft's `VertexFormat.Mode.QUADS` is OpenGL's `GL_QUADS`, which wgpu does not have either.
/// An *indexed* quad draw carries Minecraft's own quad index buffer (`i, i+1, i+2, i+2, i+3, i`) and
/// needs nothing from this side, but the sky disc's non-indexed draws do not - see [`draw`].
static QUAD_INDICES: Mutex<Option<HashMap<u32, wgpu::Buffer>>> = Mutex::new(None);

/// How many sizes either index cache keeps, and what happens at the limit.
///
/// Both are keyed by a size, and sizes come from Minecraft's meshes: a handful in practice, but
/// nothing stops a pipeline from drawing a fan of every length from 3 to 10000. Past the limit the
/// cache is emptied rather than grown - the buffers are a few kilobytes and are rebuilt on demand,
/// and a cache that cannot grow is a cache that cannot leak.
const INDEX_CACHE_LIMIT: usize = 64;

pub(crate) fn fan_indices(device: &wgpu::Device, index_count: u32) -> Option<wgpu::Buffer> {
    if index_count < 3 {
        return None;
    }

    let mut cache = FAN_INDICES.lock();
    let cache = cache.get_or_insert_with(HashMap::new);

    if cache.len() >= INDEX_CACHE_LIMIT && !cache.contains_key(&index_count) {
        cache.clear();
    }

    Some(
        cache
            .entry(index_count)
            .or_insert_with(|| {
                let triangles = index_count - 2;
                let mut indices = Vec::with_capacity((triangles * 3) as usize);

                for triangle in 0..triangles {
                    indices.push(0u32);
                    indices.push(triangle + 1);
                    indices.push(triangle + 2);
                }

                device.create_buffer_init(&BufferInitDescriptor {
                    label: Some("wgpu-mc fan indices"),
                    contents: bytemuck_cast_slice(&indices),
                    usage: wgpu::BufferUsages::INDEX,
                })
            })
            .clone(),
    )
}

/// The index buffer that turns [vertex_count] quads into triangles, and how many indices it holds.
pub(crate) fn quad_indices(
    device: &wgpu::Device,
    vertex_count: u32,
) -> Option<(wgpu::Buffer, u32)> {
    let quads = vertex_count / 4;
    if quads == 0 {
        return None;
    }

    let mut cache = QUAD_INDICES.lock();
    let cache = cache.get_or_insert_with(HashMap::new);

    if cache.len() >= INDEX_CACHE_LIMIT && !cache.contains_key(&quads) {
        cache.clear();
    }

    let buffer = cache
        .entry(quads)
        .or_insert_with(|| {
            let mut indices = Vec::with_capacity((quads * 6) as usize);

            for quad in 0..quads {
                let first = quad * 4;
                indices.extend_from_slice(&[
                    first,
                    first + 1,
                    first + 2,
                    first + 2,
                    first + 3,
                    first,
                ]);
            }

            device.create_buffer_init(&BufferInitDescriptor {
                label: Some("wgpu-mc quad indices"),
                contents: bytemuck_cast_slice(&indices),
                usage: wgpu::BufferUsages::INDEX,
            })
        })
        .clone();

    Some((buffer, quads * 6))
}

/// Reinterprets a slice of `u32` as bytes, which is what `create_buffer_init` wants.
fn bytemuck_cast_slice(values: &[u32]) -> &[u8] {
    // Safety: `u32` has no padding and no invalid bit patterns, so any byte view of it is defined.
    unsafe {
        std::slice::from_raw_parts(values.as_ptr() as *const u8, std::mem::size_of_val(values))
    }
}

/// Logs a pipeline the first time it is bound, and never touches its name again.
///
/// Diagnostics, and gated by the caller: the state is one flag on the pipeline itself, where a set
/// of names behind a mutex used to be looked up - and the name copied into it - on every single
/// bind. A pipeline is compiled once and bound thousands of times, so the first bind is the only
/// one that has anything to say.
pub(crate) fn log_pipeline_once(pipeline: &BlazePipeline) {
    if pipeline.logged.swap(true, Ordering::Relaxed) {
        return;
    }

    info!("wgpu-mc: pipeline in use: {}", pipeline.name);
}

/// The counters [`log_render_stats`] reports, on the thread that counts.
///
/// Every one of these was a process-wide atomic, incremented once or twice per draw - a locked
/// instruction to keep a number that only the logging thread ever reads. They are plain cells now.
/// One block per thread, so a thread that starts recording shows up in the totals instead of
/// sharing them; [`RECORDING_THREADS`] is what says whether that has happened.
struct Counters {
    passes: Cell<u64>,
    /// Pipeline binds, counted where the pipeline is actually set: `draw_call` skips the bind when
    /// the pass already holds the pipeline, so this counts state changes rather than draws.
    pipelines: Cell<u64>,
    draws: Cell<u64>,
    vertices: Cell<u64>,
    empty_passes: Cell<u64>,
    /// Draws the pass before the current one recorded. `u64::MAX` means "no pass has closed yet",
    /// which is what the stat line prints as `n/a`.
    last_pass_draws: Cell<u64>,
    /// Draws recorded into the pass that is currently open, so an empty pass can be told apart
    /// from a pass that drew and produced nothing.
    current_pass_draws: Cell<u64>,
    bind_groups_created: Cell<u64>,
    cache_hits: Cell<u64>,
    cache_misses: Cell<u64>,
    /// Draws that arrived with the JVM's number for their bindings, and of those, the ones whose
    /// number was the whole table and were therefore answered without reading it.
    numbered: Cell<u64>,
    tableless: Cell<u64>,
}

/// One interval's worth of [`Counters`], with the cells swapped out.
struct RenderStats {
    passes: u64,
    pipelines: u64,
    draws: u64,
    vertices: u64,
    empty_passes: u64,
    last_pass_draws: u64,
    bind_groups_created: u64,
    cache_hits: u64,
    cache_misses: u64,
    numbered: u64,
    tableless: u64,
}

impl Counters {
    const fn new() -> Counters {
        Counters {
            passes: Cell::new(0),
            pipelines: Cell::new(0),
            draws: Cell::new(0),
            vertices: Cell::new(0),
            empty_passes: Cell::new(0),
            last_pass_draws: Cell::new(u64::MAX),
            current_pass_draws: Cell::new(0),
            bind_groups_created: Cell::new(0),
            cache_hits: Cell::new(0),
            cache_misses: Cell::new(0),
            numbered: Cell::new(0),
            tableless: Cell::new(0),
        }
    }

    /// Counts a new pass, and closes the books on the one before it.
    fn open_pass(&self) {
        self.passes.set(self.passes.get() + 1);

        let previous = self.current_pass_draws.replace(0);
        self.last_pass_draws.set(previous);

        if previous == 0 {
            self.empty_passes.set(self.empty_passes.get() + 1);
        }
    }

    /// Reads every counter out and resets it, which is what `log_render_stats` reports.
    fn take(&self) -> RenderStats {
        let mut stats = RenderStats {
            passes: 0,
            pipelines: 0,
            draws: 0,
            vertices: 0,
            empty_passes: 0,
            last_pass_draws: 0,
            bind_groups_created: 0,
            cache_hits: 0,
            cache_misses: 0,
            numbered: 0,
            tableless: 0,
        };

        macro_rules! take {
            ($field:ident) => {
                stats.$field = self.$field.replace(0)
            };
        }

        take!(passes);
        take!(pipelines);
        take!(draws);
        take!(vertices);
        take!(empty_passes);
        take!(bind_groups_created);
        take!(cache_hits);
        take!(cache_misses);
        take!(numbered);
        take!(tableless);

        // The two that are not plain totals: the last pass's draws stay until the next pass closes,
        // and the current pass keeps counting.
        stats.last_pass_draws = self.last_pass_draws.replace(u64::MAX);
        stats
    }
}

/// How many threads have counted anything, so a second recording thread is visible rather than
/// silently missing from the totals.
static RECORDING_THREADS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// This thread's counters.
    ///
    /// The cells are only ever touched by the thread that owns them, which is the whole point: the
    /// record path - a draw, its vertices, the bind groups it built - runs on the render thread and
    /// nowhere else.
    static COUNTERS: Counters = {
        RECORDING_THREADS.fetch_add(1, Ordering::Relaxed);
        Counters::new()
    };
}

fn with_counters<R>(f: impl FnOnce(&Counters) -> R) -> R {
    COUNTERS.with(f)
}

/// Counts the draw states a pass records, one call per kind of thing counted.
pub(crate) fn count_pass() {
    with_counters(Counters::open_pass);
}

pub(crate) fn count_pipeline_bind() {
    with_counters(|c| c.pipelines.set(c.pipelines.get() + 1));
}

pub(crate) fn count_draw() {
    with_counters(|c| {
        c.draws.set(c.draws.get() + 1);
        c.current_pass_draws.set(c.current_pass_draws.get() + 1);
    });
}

pub(crate) fn count_vertices(vertices: u64) {
    with_counters(|c| c.vertices.set(c.vertices.get() + vertices));
}

pub(crate) fn count_bind_groups(bind_groups: u64) {
    with_counters(|c| {
        c.bind_groups_created
            .set(c.bind_groups_created.get() + bind_groups)
    });
}

pub(crate) fn count_cache_hit() {
    with_counters(|c| c.cache_hits.set(c.cache_hits.get() + 1));
}

pub(crate) fn count_cache_miss() {
    with_counters(|c| c.cache_misses.set(c.cache_misses.get() + 1));
}

/// Counts a draw that came numbered: the JVM resolved its bindings to one integer.
pub(crate) fn count_numbered() {
    with_counters(|c| c.numbered.set(c.numbered.get() + 1));
}

/// Counts a numbered draw that needed no binding table at all, which is the number being the whole
/// identity of its bind groups.
pub(crate) fn count_tableless() {
    with_counters(|c| c.tableless.set(c.tableless.get() + 1));
}

/// What one render pass was handed: how many draws, whether it cleared, whether it has a depth
/// attachment, and which pipelines it bound.
///
/// Diagnostics. Minecraft draws a frame in a handful of passes - a panorama, a GUI, a world - and
/// which of them ends up visible is decided by their order and by the state they are opened with.
/// Without this the counters say "960 draws happened" and nothing about where they went.
struct PassTrace {
    draws: u64,
    clear: bool,
    depth: bool,
    /// Which texture the pass drew into, as the view's address: two passes into the same render
    /// target share it, and the blit that presents names the same address again.
    target: String,
    pipelines: Vec<String>,
}

static CURRENT_TRACE: Mutex<Option<PassTrace>> = Mutex::new(None);
static TRACES: Mutex<Vec<PassTrace>> = Mutex::new(Vec::new());

/// Records a draw against the pass that is open.
///
/// Behind the logging switch, and that is the point of the switch here: this takes a lock that
/// every draw in the frame would otherwise queue on, to add one to a number that is only read for
/// a log line.
pub(crate) fn trace_draw() {
    if !crate::debug::logging() {
        return;
    }

    if let Some(trace) = CURRENT_TRACE.lock().as_mut() {
        trace.draws += 1;
    }
}

/// Records a pipeline bind against the pass that is open, keeping the first few distinct names.
pub(crate) fn trace_pipeline(name: &str) {
    if !crate::debug::logging() {
        return;
    }

    if let Some(trace) = CURRENT_TRACE.lock().as_mut()
        && trace.pipelines.len() < 6
        && !trace.pipelines.iter().any(|seen| seen == name)
    {
        trace.pipelines.push(name.to_owned());
    }
}

/// Records something that is not a render pass - the blit that presents a frame - in the same
/// history, so the pass order and the present can be read off together.
fn trace_marker(name: &str) {
    if !crate::debug::logging() {
        return;
    }
    let mut traces = TRACES.lock();
    if traces.len() >= 16 {
        traces.remove(0);
    }

    traces.push(PassTrace {
        draws: 0,
        clear: false,
        depth: false,
        target: name.to_owned(),
        pipelines: Vec::new(),
    });
}

/// Logs how much work the renderer was handed since the last call, then resets the counters.
///
/// Diagnostics. A frame that presents correctly and still comes out empty is either a frame
/// nothing drew into or a frame whose draws produced nothing, and the counters are what tells the
/// two apart without a GPU debugger.
#[unsafe(no_mangle)]
pub extern "C" fn log_render_stats() {
    report_pipeline_cache_once();
    crate::timing::report();

    // The counters live on the thread that counts, and this is that thread: a draw is recorded on
    // the render thread, so its block is the whole of the last interval. `RECORDING_THREADS` is
    // what makes the assumption checkable, because a second recording thread would be counted in a
    // block nobody reads.
    let stats = with_counters(Counters::take);

    // Submissions are not one of the per-thread counters: there is one encoder for the whole
    // renderer, so this is the number the frame boundary costs - one per frame is the target, and
    // the diagnostics line is where "did that stay true" is answered. The present count is what
    // makes that checkable: at one submission a frame the two numbers are equal.
    let submissions = SUBMISSIONS.swap(0, Ordering::Relaxed);
    let presents = PRESENTS.swap(0, Ordering::Relaxed);

    let RenderStats {
        passes,
        pipelines,
        draws,
        vertices,
        empty_passes: empty,
        last_pass_draws: last,
        bind_groups_created: bind_groups,
        cache_hits: hits,
        cache_misses: misses,
        numbered,
        tableless,
    } = stats;

    if RECORDING_THREADS.load(Ordering::Relaxed) > 1 {
        static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::Relaxed) {
            warn!(
                "wgpu-mc: {} threads have recorded draws; these counters only see this one",
                RECORDING_THREADS.load(Ordering::Relaxed)
            );
        }
    }

    let last = match last {
        u64::MAX => "n/a".to_owned(),
        count => count.to_string(),
    };

    info!(
        "wgpu-mc: render stats: {passes} render passes ({empty} of them empty, last had {last} \
         draws), {pipelines} pipeline binds, {draws} draws ({bind_groups} bind groups built, \
         {hits} cache hits and {misses} misses), {vertices} vertices, {submissions} submissions for \
         {presents} presented frame(s); {numbered} of the draws came numbered ({tableless} of them \
         without a binding table); uploads: {} staged ({} MB), {} fell back to queue writes",
        STAGED_UPLOADS.swap(0, Ordering::Relaxed),
        STAGED_BYTES.swap(0, Ordering::Relaxed) / (1024 * 1024),
        STAGING_FALLBACKS.swap(0, Ordering::Relaxed),
    );

    // Read in one lock rather than two: a guard taken inside the `info!` argument list lives until
    // the end of the statement, so a second lock of the same mutex inside it deadlocks the render
    // thread - which is exactly what it did.
    let (quarantined_count, quarantined_bytes) = {
        let quarantined = QUARANTINED_BUFFERS.lock();
        (
            quarantined.len(),
            quarantined
                .iter()
                .map(|(buffer, _)| buffer.size())
                .sum::<u64>(),
        )
    };

    info!(
        "wgpu-mc: live resources: {} textures ({} MB), {} views ({} with a label), {} buffers ({} \
         MB, {} quarantined in {} MB), {} encoders, {} passes, {} bind groups, {} pipelines, {} \
         tombstones, {} fan + {} quad index buffers",
        LIVE_TEXTURE_COUNT.load(Ordering::Relaxed),
        LIVE_TEXTURE_BYTES.load(Ordering::Relaxed) / (1024 * 1024),
        LIVE_VIEW_COUNT.load(Ordering::Relaxed),
        live_view_labels(),
        LIVE_BUFFER_COUNT.load(Ordering::Relaxed),
        LIVE_BUFFER_BYTES.load(Ordering::Relaxed) / (1024 * 1024),
        quarantined_count,
        quarantined_bytes / (1024 * 1024),
        LIVE_ENCODER_COUNT.load(Ordering::Relaxed),
        LIVE_PASS_COUNT.load(Ordering::Relaxed),
        LIVE_BIND_GROUP_COUNT.load(Ordering::Relaxed),
        LIVE_PIPELINE_COUNT.load(Ordering::Relaxed),
        DEAD_TEXTURES
            .lock()
            .as_ref()
            .map(|dead| dead.len())
            .unwrap_or(0),
        FAN_INDICES
            .lock()
            .as_ref()
            .map(|cache| cache.len())
            .unwrap_or(0),
        QUAD_INDICES
            .lock()
            .as_ref()
            .map(|cache| cache.len())
            .unwrap_or(0),
    );

    let traces = std::mem::take(&mut *TRACES.lock());
    for trace in traces.iter().rev().take(6).rev() {
        info!(
            "wgpu-mc:   pass -> {}: {} draws, clear={}, depth={}, pipelines {:?}",
            trace.target, trace.draws, trace.clear, trace.depth, trace.pipelines
        );
    }
}

/// Restricts drawing to a rectangle of the render target.
///
/// The rectangle arrives in the coordinates OpenGL uses for `glScissor`: measured from the
/// target's origin row, which is the row `gl_Position.y = -H` writes to. That is the row this
/// backend renders into after `preprocessing::EmulateGlClipSpace`, so the rectangle is forwarded
/// unchanged - converting it would clip the wrong half of the screen. The two callers in 26.1
/// agree: `GuiRenderer#enableScissor` builds its rectangle as `windowHeight - bottom * guiScale`,
/// and `GuiItemAtlas` renders items with `textureSize - bottom`.
///
/// The JVM side has already intersected it with the target, because wgpu validates the rectangle
/// rather than clamping it the way OpenGL does.
#[unsafe(no_mangle)]
pub extern "C" fn set_scissor_rect(
    pass: &mut BlazeRenderPass,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
) {
    pass.pass_mut().set_scissor_rect(x, y, width, height);
}

pub struct BlazePipeline {
    pub(crate) pipeline: wgpu::RenderPipeline,
    /// The topology the pipeline was built with, which decides how a vertex run is grouped.
    pub topology: PrimitiveTopology,
    pub name: String,
    /// The one description of this pipeline's bindings; see [`crate::blaze::BindGroupPlan`].
    pub plan: BindGroupPlan,
    pub bind_group_layouts: Vec<wgpu::BindGroupLayout>,
    /// Diagnostics: whether this pipeline has been reported as in use.
    ///
    /// On the pipeline rather than in a set of names, so that the first bind can report it and
    /// every later bind costs one relaxed load - the name is not even read again. It used to be a
    /// `HashSet<String>` behind a mutex, which was locked and hashed on every bind of every draw.
    pub logged: AtomicBool,
    /// Everything the *other* depth variant is built from, so building it costs no shader work.
    recipe: PipelineRecipe,
}

/// A pipeline that has been described but not written for every depth state.
///
/// Everything in here is the half of `compile_render_pipeline` that does not depend on the depth
/// state: the preprocessed GLSL in its shader modules, the pipeline layout built from the plan, the
/// vertex buffers and the colour targets. Two pipelines are created from it - one for a pass with a
/// depth attachment, one for a pass without - and only the variant that something actually draws
/// with is ever created: a GUI pipeline is never used in a pass with depth, and vice versa.
///
/// The shader modules and the layout are handles, so cloning the recipe for the second variant
/// costs three reference count bumps rather than a second GLSL preprocessing pass and a second naga
/// reflection.
#[derive(Clone)]
struct PipelineRecipe {
    label: String,
    vertex_module: wgpu::ShaderModule,
    fragment_module: wgpu::ShaderModule,
    layout: wgpu::PipelineLayout,
    /// One entry per vertex buffer slot: its stride and its attributes, in shader location order.
    /// The attributes are owned here, where the first version of this borrowed them from a bump
    /// arena that did not outlive the call.
    vertex_buffers: Vec<VertexBufferAttributes>,
    color_targets: Vec<Option<wgpu::ColorTargetState>>,
    /// wgpu's own topology, not the ABI's: the ABI's enum is what `draw_call` groups vertices by.
    topology: wgpu::PrimitiveTopology,
    cull: bool,
    /// Whether the depth bias a pipeline asks for may reach wgpu at all: it is a polygon offset,
    /// and wgpu rejects it outright on line and point topologies.
    is_polygon: bool,
}

#[derive(Clone)]
struct VertexBufferAttributes {
    stride: u64,
    attributes: Vec<wgpu::VertexAttribute>,
}

impl PipelineRecipe {
    /// Writes the pipeline for one depth state, which is `None` for a pass without a depth
    /// attachment.
    fn create(
        &self,
        wm: &WmRenderer,
        depth_stencil: Option<&BlazeDepthStencilState>,
    ) -> wgpu::RenderPipeline {
        let vertex_buffers = self
            .vertex_buffers
            .iter()
            .map(|buffer| wgpu::VertexBufferLayout {
                array_stride: buffer.stride,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &buffer.attributes,
            })
            .collect::<Vec<_>>();

        wm.gpu
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(&self.label),
                layout: Some(&self.layout),
                vertex: wgpu::VertexState {
                    module: &self.vertex_module,
                    entry_point: Some("main"),
                    compilation_options: Default::default(),
                    buffers: &vertex_buffers,
                },
                primitive: wgpu::PrimitiveState {
                    topology: self.topology,
                    strip_index_format: None,
                    // `Cw`, *because* the vertex shaders flip `gl_Position.y` to emulate GL's clip
                    // space: that flip mirrors the winding, so a triangle OpenGL calls
                    // counter-clockwise - and therefore front-facing - comes out clockwise here.
                    // `Ccw` culled exactly the faces that should have been kept, which showed as the
                    // ground disappearing and the sky's dark lower half showing through it. Without
                    // the flip this would have to be `Ccw`. Culling was not forwarded at all before, so
                    // every quad was rasterized - and for a single-sided mesh, culling the wrong side
                    // removes the surface rather than revealing a back face.
                    front_face: wgpu::FrontFace::Cw,
                    cull_mode: if self.cull {
                        Some(wgpu::Face::Back)
                    } else {
                        None
                    },
                    unclipped_depth: false,
                    // `RenderPipeline#getPolygonMode` is not forwarded: the one pipeline that asks for
                    // `WIREFRAME` (`pipeline/wireframe`, the chunk-section debug view) draws filled
                    // here. wgpu would need `Features::POLYGON_MODE_LINE` for it.
                    polygon_mode: Default::default(),
                    conservative: false,
                },
                depth_stencil: depth_stencil.map(|state| wgpu::DepthStencilState {
                    format: wgpu::TextureFormat::Depth32Float,
                    // Both of these used to be constants - `Always` with writes forced on - which
                    // is not a depth test at all. Minecraft asks for `LESS_THAN_OR_EQUAL` on
                    // almost everything, `EQUAL` without writes for glint, and biased variants for
                    // the overlays that sit on top of the block they belong to.
                    depth_write_enabled: Some(state.active != 0),
                    depth_compare: Some(state.compare_function.into()),
                    stencil: Default::default(),
                    bias: wgpu::DepthBiasState {
                        // wgpu rejects a depth bias on a non-triangle topology - "Depth bias is not
                        // compatible with non-triangle topology LineList" - and it is right to:
                        // polygon offset applies to polygons. OpenGL ignores it for lines and
                        // points in the same way, so zeroing it here is what Minecraft gets there
                        // too, and `lines_depth_bias` asks for it only because the pipeline is
                        // shared with the quad-based outlines.
                        constant: if self.is_polygon {
                            state.bias_constant
                        } else {
                            0
                        },
                        slope_scale: if self.is_polygon {
                            state.bias_slope_scale
                        } else {
                            0.0
                        },
                        clamp: 0.0,
                    },
                }),
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &self.fragment_module,
                    entry_point: Some("main"),
                    compilation_options: Default::default(),
                    targets: &self.color_targets,
                }),
                multiview_mask: None,
                // The driver's own cache, when the backend has one: the second launch of the same
                // build hands back pipelines that were already compiled instead of compiling them
                // again. `None` on the backends that do not implement it.
                cache: wm.gpu.pipeline_cache.as_ref(),
            })
    }
}

/// Frees one compiled pipeline variant.
///
/// Null-tolerant, because the two depth variants of a pipeline are compiled lazily now: the JVM
/// holds a slot per variant and only one of them may ever have been created, so the same call frees
/// whichever exist.
#[unsafe(no_mangle)]
pub extern "C" fn drop_render_pipeline(pipeline: *mut BlazePipeline) {
    if pipeline.is_null() {
        return;
    }

    LIVE_PIPELINE_COUNT.fetch_sub(1, Ordering::Relaxed);

    // Safety: the pointer came from `compile_render_pipeline` or `create_pipeline_variant`, it is
    // freed exactly once (the JVM clears its slot), and nothing else holds a reference to it.
    let pipeline = unsafe { Box::from_raw(pipeline) };

    // Cached sets are keyed on this pipeline's plan, and a later pipeline can be allocated at the
    // same address - the allocator hands it straight back out.
    crate::blaze::invalidate_bind_group_cache(&*pipeline as *const BlazePipeline as usize);

    drop(pipeline);
}

/// Compiles the other depth variant of a pipeline that has already been described.
///
/// The first variant is built by [`compile_render_pipeline`], which does the shader work; this one
/// reuses it through the pipeline's recipe, so all that is left is the driver's own pipeline
/// creation - the half that pipeline caches accelerate. Called the first time a pass whose depth
/// attachment differs from the compiled variant's binds the pipeline.
#[unsafe(no_mangle)]
pub extern "C" fn create_pipeline_variant(
    wm: &WmRenderer,
    source: &BlazePipeline,
    depth_stencil: Option<&BlazeDepthStencilState>,
) -> Box<BlazePipeline> {
    LIVE_PIPELINE_COUNT.fetch_add(1, Ordering::Relaxed);

    Box::new(BlazePipeline {
        pipeline: source.recipe.create(wm, depth_stencil),
        topology: source.topology,
        name: source.name.clone(),
        // The plan is the pipeline's description of its bindings, and the depth state is not part
        // of it, so the two variants share it rather than each numbering the bindings again.
        plan: source.plan.clone(),
        bind_group_layouts: source.bind_group_layouts.clone(),
        logged: AtomicBool::new(false),
        recipe: source.recipe.clone(),
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn compile_render_pipeline(
    wm: &WmRenderer,
    render_pipeline_description: &RenderPipeline,
) -> Box<BlazePipeline> {
    LIVE_PIPELINE_COUNT.fetch_add(1, Ordering::Relaxed);

    let directives = format!("#version 440\n{}\n", render_pipeline_description.directives);

    // A list, not a map, because the key below has to be canonical and the same list is turned into
    // the map `process_shaders` wants.
    let vertex_stage_input_layout: Vec<(String, u32)> = render_pipeline_description
        .vertex_formats
        .iter()
        .scan(0, |location, format| {
            Some(
                format
                    .elements
                    .iter()
                    .map(|element| {
                        *location += 1;
                        (element.name.to_string(), *location - 1)
                    })
                    .collect::<Vec<(String, u32)>>(),
            )
        })
        .flatten()
        .collect();

    // One plan, three consumers: the numbers in it are what the GLSL annotations below are written
    // from, what the wgpu layouts are built from, and what the bind groups and their dynamic offsets
    // are built from at draw time. They used to be three separate walks over three descriptions.
    let mut plan = BindGroupPlan::number(render_pipeline_description);

    // Everything the translation depends on, in a form a hash can be taken of: the two shader
    // sources, the defines, the binding numbers the annotator writes into the GLSL, and the vertex
    // locations it declares. The plan and the layout are *derived* from the pipeline description, so
    // keying on them is keying on the description - and on nothing that changes between launches.
    let shader_locations = plan.shader_locations();
    let cache_key = crate::shader_cache::key(&[
        &render_pipeline_description.vertex_shader,
        &render_pipeline_description.fragment_shader,
        &directives,
        &crate::shader_cache::canonical(
            shader_locations
                .iter()
                .map(|(name, (set, binding))| (name.clone(), format!("{set}:{binding}"))),
        ),
        &crate::shader_cache::canonical(
            vertex_stage_input_layout
                .iter()
                .map(|(name, location)| (name.clone(), *location)),
        ),
        // The backend is part of the key even though the translation is backend-independent today:
        // what comes out of it is GLSL for naga, and a future shim that emits something else - HLSL,
        // or a different set of workarounds - must not be answered with this build's entry. The cost
        // is one re-translation per pipeline when the player switches backend, which happens once.
        wm.gpu.adapter.get_info().backend.to_str(),
    ]);

    // Which part of the key moved, when a pipeline that should have been cached is translated again.
    // The parts are short enough to compare by eye across two runs, and this is the only way to tell
    // "the cache is not working" apart from "this pipeline is new".
    if crate::debug::logging() {
        static REPORTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if REPORTED.fetch_add(1, Ordering::Relaxed) < 12 {
            info!(
                "wgpu-mc: {} key {cache_key}: v={:08x} f={:08x} d={:08x} l={:08x} i={:08x} b={:08x}",
                render_pipeline_description.name,
                crate::shader_cache::part(&render_pipeline_description.vertex_shader),
                crate::shader_cache::part(&render_pipeline_description.fragment_shader),
                crate::shader_cache::part(&directives),
                crate::shader_cache::part(&crate::shader_cache::canonical(
                    shader_locations
                        .iter()
                        .map(|(name, (set, binding))| (name.clone(), format!("{set}:{binding}"))),
                )),
                crate::shader_cache::part(&crate::shader_cache::canonical(
                    vertex_stage_input_layout
                        .iter()
                        .map(|(name, location)| (name.clone(), *location)),
                )),
                crate::shader_cache::part(wm.gpu.adapter.get_info().backend.to_str()),
            );
        }
    }

    let (vert_processed, frag_processed, sampler_types, implicit_uniforms, block_sizes) =
        match crate::shader_cache::load(&cache_key) {
            Some(cached) => {
                // The preprocessor only ever produces `sampler2D` or `samplerCube` for a combined
                // sampler - its own match over the type is exhaustive over those two - so the flag
                // the cache keeps turns back into exactly the type it was made from.
                let samplers = cached
                    .cube_samplers
                    .iter()
                    .map(|(name, cube)| {
                        (
                            name.clone(),
                            if *cube {
                                TypeSpecifierNonArray::SamplerCube
                            } else {
                                TypeSpecifierNonArray::Sampler2D
                            },
                        )
                    })
                    .collect();

                let block_sizes = cached.blocks();

                // Destructured rather than moved field by field: the two maps are built from the
                // borrowed entry, and a partial move would leave it unusable for them.
                let crate::shader_cache::Translation {
                    vert,
                    frag,
                    implicit_uniforms,
                    ..
                } = cached;

                (vert, frag, samplers, implicit_uniforms, block_sizes)
            }
            None => {
                let ProcessedShaderResult {
                    frag,
                    vert,
                    sampler_types,
                    implicit_uniforms,
                } = process_shaders(
                    &render_pipeline_description.vertex_shader,
                    &render_pipeline_description.fragment_shader,
                    &directives,
                    &shader_locations,
                    vertex_stage_input_layout.iter().cloned().collect(),
                );

                // The reflection pass parses both shaders again, through naga this time, so it is the
                // other half of what a cache hit saves - and it depends on nothing but the two
                // strings above, which is why it can be cached with them.
                let block_sizes = reflected_block_sizes(&vert, &frag);

                crate::shader_cache::store(
                    &cache_key,
                    &crate::shader_cache::Translation {
                        vert: vert.clone(),
                        frag: frag.clone(),
                        cube_samplers: sampler_types
                            .iter()
                            .map(|(name, ty)| {
                                (
                                    name.clone(),
                                    matches!(ty, TypeSpecifierNonArray::SamplerCube),
                                )
                            })
                            .collect(),
                        implicit_uniforms: implicit_uniforms.clone(),
                        block_sizes: block_sizes.iter().map(|(k, v)| (k.clone(), *v)).collect(),
                    },
                );

                (vert, frag, sampler_types, implicit_uniforms, block_sizes)
            }
        };

    crate::shader_cache::report_once();

    dump_shaders(
        &render_pipeline_description.name,
        &vert_processed,
        &frag_processed,
    );

    plan.add_implicit_uniforms(&implicit_uniforms);
    plan.apply_sampler_types(&sampler_types);
    plan.apply_block_sizes(&block_sizes);

    let vert_module = wm
        .gpu
        .device
        .create_shader_module(wgpu::ShaderModuleDescriptor {
            label: None,
            source: ShaderSource::Glsl {
                shader: Cow::Borrowed(&vert_processed),
                stage: naga::ShaderStage::Vertex,
                // Don't pass any defines, the shader is already preprocessed above
                defines: &[],
            },
        });

    let frag_module = wm
        .gpu
        .device
        .create_shader_module(wgpu::ShaderModuleDescriptor {
            label: None,
            source: ShaderSource::Glsl {
                shader: Cow::Borrowed(&frag_processed),
                stage: naga::ShaderStage::Fragment,
                defines: &[],
            },
        });

    let bind_group_layouts: Vec<wgpu::BindGroupLayout> = plan.create_layouts(&wm.gpu.device);
    let vertex_buffers = render_pipeline_description
        .vertex_formats
        .iter()
        // A format with no elements describes no attributes at all, and declaring a vertex buffer
        // for it would make slot 0 *mandatory* at draw time. Minecraft never binds one for those:
        // `LIGHTMAP`, the `POST_*` passes and `ANIMATE_SPRITE` all use `DefaultVertexFormat.EMPTY`
        // and synthesise the full-screen triangle from `gl_VertexIndex` in `core/screenquad`, then
        // draw with `renderPass.draw(0, 3)`. That is exactly the "requires vertex buffer 0 to be
        // set" failure.
        .filter(|vertex_format| !vertex_format.elements.is_empty())
        .scan(0, |shader_location, vertex_format| {
            Some(VertexBufferAttributes {
                stride: vertex_format.vertex_size,
                attributes: vertex_format
                    .elements
                    .iter()
                    .filter_map(|element| {
                        // Locations are numbered the way GL numbers attribute indices, so an
                        // element that cannot be expressed still consumes its location rather
                        // than shifting every element after it.
                        let location = *shader_location;
                        *shader_location += 1;

                        let Some(format) = element.format.to_wgpu_vertex_format() else {
                            error!(
                                "wgpu-mc: vertex element {} is {:?}, which has no wgpu vertex \
                                 format; the pipeline will not validate",
                                element.name, element.format
                            );
                            return None;
                        };

                        // Three-component 8- and 16-bit formats are widened to four components
                        // (see `GpuFormat::to_wgpu_vertex_format`), which reads one component
                        // past the element. Minecraft pads its vertex formats to a multiple of
                        // four bytes so that byte exists, but say so rather than relying on it
                        // silently.
                        let end = element.offset + format.size();
                        if end > vertex_format.vertex_size {
                            error!(
                                "wgpu-mc: vertex element {} ({:?}) needs bytes {}..{} but a \
                                 vertex is only {} bytes; it will read into the next vertex",
                                element.name,
                                element.format,
                                element.offset,
                                end,
                                vertex_format.vertex_size
                            );
                        }

                        Some(wgpu::VertexAttribute {
                            format,
                            offset: element.offset,
                            shader_location: location,
                        })
                    })
                    .collect::<Vec<wgpu::VertexAttribute>>(),
            })
        })
        .collect::<Vec<VertexBufferAttributes>>();

    let layout = wm
        .gpu
        .device
        .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &bind_group_layouts
                .iter()
                .map(Option::from)
                .collect::<Vec<_>>(),
            immediate_size: 0,
        });

    // Whether the depth bias a pipeline asks for is allowed to reach wgpu at all: it is a polygon
    // offset, and wgpu rejects it outright on line and point topologies.
    let is_polygon = matches!(
        render_pipeline_description.primitive_topology.to_wgpu(),
        wgpu::PrimitiveTopology::TriangleList | wgpu::PrimitiveTopology::TriangleStrip
    );

    // Everything the pipeline is made of except its depth state. Both variants are written from
    // this recipe, and only the one something draws with is written at all: the Kotlin side asks
    // for the variant a pass needs, and the first one it asks for is the only one compiled here.
    let recipe = PipelineRecipe {
        // Named after `RenderPipeline#getLocation`, so wgpu's own validation errors say which
        // pipeline they are about instead of showing an empty label.
        label: render_pipeline_description.name.to_string(),
        vertex_module: vert_module,
        fragment_module: frag_module,
        layout,
        vertex_buffers,
        color_targets: render_pipeline_description
            .color_target_states
            .iter()
            .map(|state| {
                Some(wgpu::ColorTargetState {
                    format: state.format.to_wgpu_texture_format(),
                    // Straight from the pipeline. This used to be a hardcoded
                    // `ALPHA_BLENDING` with every channel written, which is wrong for most of
                    // Minecraft's pipelines: the GUI and its background use `OVERLAY`
                    // (`src * alpha + dst`), `INVERT`, `ADDITIVE` and premultiplied alpha, and
                    // several write to a subset of the channels or to none at all.
                    blend: state.to_wgpu_blend(),
                    write_mask: wgpu::ColorWrites::from_bits_truncate(state.write_mask),
                })
            })
            .collect(),
        topology: render_pipeline_description.primitive_topology.to_wgpu(),
        cull: render_pipeline_description.cull != 0,
        is_polygon,
    };

    let pipeline = recipe.create(
        wm,
        render_pipeline_description.depth_stencil_state.as_deref(),
    );

    // The driver's compilation results are worth carrying into the next launch, and the only
    // moment they change is here.
    save_pipeline_cache(wm);

    Box::new(BlazePipeline {
        pipeline,
        topology: render_pipeline_description.primitive_topology,
        name: plan.name.clone(),
        plan,
        bind_group_layouts,
        logged: AtomicBool::new(false),
        recipe,
    })
}

/// How many pipelines are compiled between two writes of the pipeline cache.
///
/// The cache only grows when a pipeline is compiled, and the file is megabytes: writing it per
/// pipeline would cost more than the compilation it saves. Sixteen is a compromise - the start of a
/// session compiles far more than that, so a normal run writes the cache several times and the last
/// write is never more than a few pipelines stale.
const PIPELINE_CACHE_SAVE_EVERY: u64 = 16;

/// What this launch's pipeline cache turned out to be.
///
/// The cache is created during mod construction, which is before the logger is up, so which of
/// these three cases applies is recorded here and printed by the first `log_render_stats` - the
/// question "why is there no cache file" deserves an answer in the log.
static PIPELINE_CACHE_STATE: std::sync::atomic::AtomicU8 =
    std::sync::atomic::AtomicU8::new(STATE_UNKNOWN);

const STATE_UNKNOWN: u8 = 0;
/// The device has no `PIPELINE_CACHE` feature, so there is nothing to create.
const STATE_ABSENT: u8 = 1;
/// There is a cache, but the backend does not hand its contents back - DX12.
const STATE_PRIVATE: u8 = 2;
/// The cache is on disk, and is written as pipelines are compiled.
const STATE_PERSISTED: u8 = 3;

/// Says once which kind of pipeline cache this backend ended up with.
fn report_pipeline_cache_once() {
    static REPORTED: AtomicBool = AtomicBool::new(false);

    if REPORTED.swap(true, Ordering::Relaxed) {
        return;
    }

    let loaded = LOADED_PIPELINE_CACHE_BYTES.load(Ordering::Relaxed) as f64 / (1024.0 * 1024.0);
    let file = crate::RENDERER
        .get()
        .and_then(|wm| pipeline_cache_file(&wm.gpu.adapter));

    match PIPELINE_CACHE_STATE.load(Ordering::Relaxed) {
        STATE_ABSENT => info!(
            "wgpu-mc: this device has no pipeline cache; every launch compiles every pipeline"
        ),
        STATE_PRIVATE => info!(
            "wgpu-mc: this backend keeps its pipeline cache to itself; every launch compiles every \
             pipeline"
        ),
        STATE_PERSISTED => info!(
            "wgpu-mc: pipeline cache: started from {:.1} MB, written to {} every {} pipelines",
            loaded,
            file.map(|file| file.display().to_string())
                .unwrap_or_default(),
            PIPELINE_CACHE_SAVE_EVERY,
        ),
        // No pipeline was compiled before the stats line, which cannot happen in a real frame, but
        // the report is not worth a panic either.
        _ => {}
    }
}

/// What this launch's cache file held, so the first write can say whether it was used.
///
/// The load happens during mod construction, before the logger is up, so the line about *loading*
/// the cache would be lost; this is what carries that number to the first save, which happens once
/// the first pipelines have been compiled.
static LOADED_PIPELINE_CACHE_BYTES: AtomicU64 = AtomicU64::new(0);

/// Creates the device's pipeline cache, seeded with whatever the last run left behind.
///
/// What this buys is the driver's own pipeline compilation: without it, every launch compiles every
/// pipeline from scratch, which is the slowest part of a renderer's startup and the reason wgpu
/// exposes a cache to persist between runs at all.
///
/// Not every backend has one. Vulkan implements it with `vkPipelineCache`; DX12 has no serialisable
/// form and returns a cache that stores nothing, and `get_data` answers `None` for it, so nothing is
/// ever written on that path. See [`pipeline_cache_file`] for why the data is kept per adapter.
fn create_pipeline_cache(
    device: &wgpu::Device,
    adapter: &wgpu::Adapter,
) -> Option<wgpu::PipelineCache> {
    if !device.features().contains(wgpu::Features::PIPELINE_CACHE) {
        // The logger is not up yet at this point in a launch - the device is created during mod
        // construction - so the case is recorded here and printed by `report_pipeline_cache_once`.
        PIPELINE_CACHE_STATE.store(STATE_ABSENT, Ordering::Relaxed);
        return None;
    }

    let file = pipeline_cache_file(adapter)?;
    let data = std::fs::read(&file).ok().filter(|data| !data.is_empty());

    LOADED_PIPELINE_CACHE_BYTES.store(
        data.as_ref().map(|data| data.len() as u64).unwrap_or(0),
        Ordering::Relaxed,
    );

    match &data {
        Some(data) => log::info!(
            "wgpu-mc: pipeline cache: {:.1} MB loaded from {}",
            data.len() as f64 / (1024.0 * 1024.0),
            file.display()
        ),
        None => log::info!(
            "wgpu-mc: pipeline cache: nothing to load from {}",
            file.display()
        ),
    }

    // Safety: the data is what a previous `PipelineCache::get_data` wrote to this file for this
    // adapter, which is the condition wgpu documents. It cannot be *proved* across a file, which is
    // what `fallback: true` is for: data the driver will not use is dropped rather than rejected.
    Some(unsafe {
        device.create_pipeline_cache(&wgpu::PipelineCacheDescriptor {
            label: Some("wgpu-mc"),
            data: data.as_deref(),
            fallback: true,
        })
    })
}

/// Where this adapter's pipeline cache lives, as a file beside the game's other renderer state.
///
/// Keyed by adapter, because a cache from a different device is worse than none: the driver
/// validates every entry and throws away the ones that do not match. wgpu's own
/// [`wgpu::util::pipeline_cache_key`] answers only for Vulkan, so anything else gets a key from the
/// same three numbers it uses.
fn pipeline_cache_file(adapter: &wgpu::Adapter) -> Option<PathBuf> {
    let info = adapter.get_info();
    let key = wgpu::util::pipeline_cache_key(&info).unwrap_or_else(|| {
        format!(
            "wgpu_mc_pipeline_cache_{:?}_{:04x}_{:04x}",
            info.backend, info.vendor, info.device
        )
        .to_lowercase()
    });

    let directory = crate::RUN_DIRECTORY.get()?;
    Some(directory.join(format!("{key}.bin")))
}

/// Writes the pipeline cache out, at most once every [`PIPELINE_CACHE_SAVE_EVERY`] pipelines.
///
/// The data is only worth writing when it has grown, and it is megabytes: piping every pipeline
/// creation through a file write would cost more than the compilation it saves. The write is
/// atomic - a temporary file renamed over the real one - because a half-written cache is exactly
/// what the driver would refuse wholesale on the next launch.
fn save_pipeline_cache(wm: &WmRenderer) {
    static SINCE_LAST_SAVE: AtomicU64 = AtomicU64::new(0);

    let Some(cache) = wm.gpu.pipeline_cache.as_ref() else {
        PIPELINE_CACHE_STATE.store(STATE_ABSENT, Ordering::Relaxed);
        return;
    };

    if SINCE_LAST_SAVE.fetch_add(1, Ordering::Relaxed) + 1 < PIPELINE_CACHE_SAVE_EVERY {
        return;
    }

    SINCE_LAST_SAVE.store(0, Ordering::Relaxed);

    let Some(data) = cache.get_data() else {
        // A cache the backend keeps to itself: wgpu creates one for DX12, but there is no way to
        // read it back, so there is nothing to persist and no point asking again.
        PIPELINE_CACHE_STATE.store(STATE_PRIVATE, Ordering::Relaxed);
        return;
    };

    PIPELINE_CACHE_STATE.store(STATE_PERSISTED, Ordering::Relaxed);

    let Some(file) = pipeline_cache_file(&wm.gpu.adapter) else {
        return;
    };

    let temporary = file.with_extension("bin.tmp");

    if let Err(err) = std::fs::write(&temporary, &data) {
        error!(
            "wgpu-mc: could not write the pipeline cache to {}: {err}",
            temporary.display()
        );
        return;
    }

    // Renamed over the real file rather than written into it: a cache the process was killed in the
    // middle of writing is one the driver would reject wholesale on the next launch.
    if let Err(err) = std::fs::rename(&temporary, &file) {
        error!("wgpu-mc: could not replace {}: {err}", file.display());
        return;
    }

    info!(
        "wgpu-mc: pipeline cache: {:.1} MB written to {} (started from {:.1} MB)",
        data.len() as f64 / (1024.0 * 1024.0),
        file.display(),
        LOADED_PIPELINE_CACHE_BYTES.load(Ordering::Relaxed) as f64 / (1024.0 * 1024.0)
    );
}

/// Creates a sampler with the modes Blaze3D asked for.
///
/// This used to build one default sampler and ignore every argument, which is `ClampToEdge` on all
/// three axes, `Nearest` filtering and `lod_max_clamp` of zero - one sampler for the whole game, and
/// the wrong one for most of it. What it breaks is anything whose texture is *meant* to repeat:
///
/// - `WeatherEffectRenderer` draws one quad per rain column whose texture *v* runs from
///   `bottomY / 4` to `topY / 4` and relies on the wrap to cut it into streaks. Clamped, the last
///   texel row is stretched over the whole column, so rain and snow render as blue and white lines
///   falling out of the sky - the report was "rain is thin lines that stretch down forever";
/// - the enchantment glint scrolls its texture the same way, so it stops being a moving pattern;
/// - flowing water and lava scroll their atlas sprite by whole texture coordinates in the same way;
/// - `lod_max_clamp` of zero pins every sample to mip 0, which is why nothing was mip-filtered - and
///   that part is deliberately *kept*: see the `lod_max_clamp` comment inside.
///
/// The last argument is the `OptionalDouble` LOD limit Blaze3D passes, negative when it did not ask
/// for one, and `max_anisotropy` of zero means "no anisotropy" - wgpu wants at least one.
#[unsafe(no_mangle)]
pub extern "C" fn create_sampler(
    wm: &WmRenderer,
    address_mode_u: u32,
    address_mode_v: u32,
    min_filter: u32,
    mag_filter: u32,
    max_anisotropy: u32,
    _max_lod: f64,
) -> Box<wgpu::Sampler> {
    let min = filter_mode(min_filter);
    let mag = filter_mode(mag_filter);

    // Anisotropy above one is only legal with linear filtering on both ends - wgpu answers anything
    // else with a validation error, and a validation error ends the process here. Nothing in 26.1
    // asks for it today, but the sampler cache would hand it straight through if something did.
    let anisotropy = if min == wgpu::FilterMode::Linear && mag == wgpu::FilterMode::Linear {
        max_anisotropy.clamp(1, 16) as u16
    } else {
        1
    };

    Box::new(wm.gpu.device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("<wm/mc sampler>"),
        address_mode_u: address_mode(address_mode_u),
        address_mode_v: address_mode(address_mode_v),
        // No 3D texture is sampled by this renderer, so `w` follows `u` rather than being clamped
        // while the others wrap.
        address_mode_w: address_mode(address_mode_u),
        mag_filter: mag,
        min_filter: min,
        // Mips are deliberately not sampled yet, whatever the request says. Minecraft's atlases are
        // stitched with the padding mipmapping needs, but this side builds every mip level by
        // *rendering* the atlas into it - one `Animate <atlas>` pass per level - and a level that is
        // empty or half filled does not blur: it samples the neighbouring sprite, which is what
        // "the stair's side is missing and the pressure plate has the bed's texture on one face"
        // looks like. `lod_max_clamp` of zero keeps every sample on mip 0, which is the level the
        // sprite uploads write directly; the icons were correct with exactly this, and turning mips
        // on is a separate change that has to come with a check that each level is filled.
        mipmap_filter: wgpu::MipmapFilterMode::Nearest,
        lod_min_clamp: 0.0,
        lod_max_clamp: 0.0,
        compare: None,
        anisotropy_clamp: anisotropy,
        border_color: None,
    }))
}

/// `com.mojang.blaze3d.textures.AddressMode`: 0 is `REPEAT`, 1 is `CLAMP_TO_EDGE`.
fn address_mode(mode: u32) -> wgpu::AddressMode {
    match mode {
        1 => wgpu::AddressMode::ClampToEdge,
        _ => wgpu::AddressMode::Repeat,
    }
}

/// `com.mojang.blaze3d.textures.FilterMode`: 0 is `NEAREST`, 1 is `LINEAR`.
fn filter_mode(mode: u32) -> wgpu::FilterMode {
    match mode {
        1 => wgpu::FilterMode::Linear,
        _ => wgpu::FilterMode::Nearest,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn drop_sampler(sampler: Box<wgpu::Sampler>) {
    // A cached bind group may name this sampler, and the allocator hands the address out again.
    crate::blaze::invalidate_bind_group_cache(&*sampler as *const wgpu::Sampler as usize);
}

/// Copies a range of a buffer back to the CPU, blocking until the GPU has produced it.
///
/// This is the read half of `CommandEncoder#mapBuffer`. wgpu exposes no CPU-visible mapping of its
/// own - a readback has to be mapped asynchronously and waited for - so the JVM side hands over its
/// staging pointer and gets it filled here. Without this the staging buffer stayed zeroed, which is
/// why every screenshot came out black the moment the crash in front of it was fixed.
///
/// Blocking is deliberate: Blaze3D's `mapBuffer` returns a view that is already populated, and the
/// only caller (a screenshot) is a one-off at a point where stalling costs nothing.
///
/// A buffer that cannot be mapped at all - one without `MAP_READ`, which is every uniform buffer -
/// is read through a scratch buffer instead, which is what makes this usable as a diagnostics probe
/// for the uniforms a shader is being handed.
#[unsafe(no_mangle)]
pub extern "C" fn read_buffer(
    wm: &WmRenderer,
    buffer: &wgpu::Buffer,
    offset: u64,
    length: u64,
    destination: *mut u8,
) -> bool {
    let end = offset + length;
    if end > buffer.size() || length == 0 {
        error!(
            "wgpu-mc: refusing to read {length} bytes at {offset} of a {} byte buffer",
            buffer.size()
        );
        return false;
    }

    // A copy - `copy_buffer_to_buffer` below, and `map_async` as well - has to be a multiple of
    // `COPY_BUFFER_ALIGNMENT` at both ends, and the caller has no reason to know that: a diagnostics
    // readback asks for exactly the bytes it wants to look at, and the cloud mesh is `3 * faces`
    // bytes, which is a multiple of 4 only when the face count is. Handing that to wgpu is a
    // validation error, and a validation error ends the process - the client died twice on this, in
    // `copy_buffer_to_buffer` with `Copy size 29349 does not respect COPY_BUFFER_ALIGNMENT` and then
    // in `map_async` with `range_size 29349 must be multiple of 4`, which is a diagnostic taking the
    // game down instead of answering its question. Both ends are widened to alignment here, and the
    // caller still gets exactly the bytes it asked for: the padding is read, dropped, and never
    // reaches the destination.
    let aligned_start = offset & !(wgpu::COPY_BUFFER_ALIGNMENT - 1);
    let aligned_end = end
        .next_multiple_of(wgpu::COPY_BUFFER_ALIGNMENT)
        .min(buffer.size());
    let mapped_length = aligned_end - aligned_start;

    // Where the bytes the caller asked for start inside the aligned range.
    let alignment_padding = offset - aligned_start;

    let scratch;
    let mut from_scratch = false;
    let source = if buffer.usage().contains(wgpu::BufferUsages::MAP_READ) {
        // Minecraft's own readback buffers are mappable, and that is the only way they can be read:
        // `Screenshot` hands over a `MAP_READ | COPY_DST` buffer, which needs no scratch and - more
        // to the point - cannot have one, because wgpu rejects `MAP_READ | COPY_SRC` on the same
        // buffer. Mapping it here is safe: it is not mapped at this point, which is exactly why
        // Blaze3D is asking.
        buffer
    } else {
        // A buffer Minecraft writes through `mapBuffer` comes through here too, and that is safe:
        // this side never maps a wgpu buffer - a `mapBuffer` on the JVM side is a CPU staging buffer
        // and a `write_to_buffer` when it closes - so there is no mapping here to disturb. It used to
        // be refused, which turned every read-modify-write mapping into an empty read and left the
        // diagnostics unable to answer the one question they exist for: whether the bytes a mapped
        // write handed over are actually in the buffer.
        if !buffer.usage().contains(wgpu::BufferUsages::COPY_SRC) {
            error!("wgpu-mc: buffer {offset}..{end} has no COPY_SRC, so it cannot be read back");
            return false;
        }

        scratch = wm.gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("<wm/mc readback>"),
            size: mapped_length,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = wm
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        encoder.copy_buffer_to_buffer(buffer, aligned_start, &scratch, 0, mapped_length);
        wm.gpu.queue.submit([encoder.finish()]);

        from_scratch = true;
        &scratch
    };

    let slice = source.slice(if from_scratch {
        0..mapped_length
    } else {
        aligned_start..aligned_end
    });
    let (sender, receiver) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = sender.send(result);
    });

    if wm
        .gpu
        .device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .is_err()
    {
        error!("wgpu-mc: the buffer readback could not be waited for");
        return false;
    }

    if !matches!(receiver.recv(), Ok(Ok(()))) {
        error!("wgpu-mc: the buffer readback could not be mapped");
        return false;
    }

    {
        let data = slice.get_mapped_range();
        // The mapped range starts at the aligned copy, which may begin a few bytes before the range
        // the caller asked for; the padding is this side's and the caller never sees it.
        unsafe {
            std::ptr::copy_nonoverlapping(
                data.as_ptr().add(alignment_padding as usize),
                destination,
                length as usize,
            );
        }
    }

    source.unmap();
    true
}

#[unsafe(no_mangle)]
pub extern "C" fn write_buffer_with(
    wm: &WmRenderer,
    buffer: &wgpu::Buffer,
    data: *const u8,
    len: u64,
) {
    let mut view = wm
        .gpu
        .queue
        .write_buffer_with(buffer, 0, NonZero::new(len).unwrap())
        .unwrap();

    view.copy_from_slice(unsafe { std::slice::from_raw_parts(data, len as usize) });
}

/// A buffer that is handed to the JVM already mapped, for the JVM to fill in place.
///
/// `usages` was ignored here, and the two flags written instead were not enough for anything that
/// also gets bound: a caller asking for `USAGE_UNIFORM_TEXEL_BUFFER` got a buffer with no `STORAGE`,
/// which is a bind group validation error - and a validation error ends the process on this side.
/// `mapped_at_creation` requires `MAP_WRITE`, so that one is always set; `MAP_READ` is dropped when
/// the mask asks for `COPY_SRC`, because wgpu rejects that pair outright.
#[unsafe(no_mangle)]
pub extern "C" fn allocate_gpu_buffer_mapped(
    wm: &WmRenderer,
    size: u64,
    usages: u64,
) -> Box<wgpu::Buffer> {
    LIVE_BUFFER_COUNT.fetch_add(1, Ordering::Relaxed);
    LIVE_BUFFER_BYTES.fetch_add(size, Ordering::Relaxed);

    let mut usage = wgpu_buffer_usages(usages as u32) | wgpu::BufferUsages::MAP_WRITE;
    if !usage.contains(wgpu::BufferUsages::COPY_SRC) {
        usage.insert(wgpu::BufferUsages::MAP_READ);
    }

    Box::new(wm.gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size,
        usage,
        mapped_at_creation: true,
    }))
}

#[unsafe(no_mangle)]
pub extern "C" fn acquire_next_texture(wm: &WmRenderer) -> *mut SurfaceTexture {
    let lock = wm.gpu.surface.lock();
    let Some(surface) = lock.as_ref() else {
        warn!("wgpu-mc: acquire_next_texture was called before a surface existed");
        return ptr::null_mut();
    };

    match surface.get_current_texture() {
        CurrentSurfaceTexture::Success(texture) | CurrentSurfaceTexture::Suboptimal(texture) => {
            Box::into_raw(Box::new(texture))
        }
        CurrentSurfaceTexture::Outdated
        | CurrentSurfaceTexture::Lost
        | CurrentSurfaceTexture::Validation => {
            // The swapchain no longer matches the window: a resize, a move to another monitor, a
            // fullscreen toggle, a driver-side reset, or - for `Validation` - a configuration wgpu
            // will not accept another acquire from. DX12 reports this far more readily than Vulkan
            // does, and the JVM side only calls `configure_surface` when the window size changed,
            // so recover here instead of dropping every frame from here on.
            //
            // `Validation` used to be logged and dropped, which is what turned a stale swapchain
            // into a log line per frame for minutes on end - and frames nobody ever saw.
            static ACQUIRE_FAILURES: AtomicU64 = AtomicU64::new(0);
            let failures = ACQUIRE_FAILURES.fetch_add(1, Ordering::Relaxed);
            if failures.is_multiple_of(120) {
                info!(
                    "wgpu-mc: the swapchain went stale ({failures} frames so far), reconfiguring it"
                );
            }

            let (width, height, request) = {
                let state = SURFACE_STATE.lock();
                match state.as_ref() {
                    Some(state) => (
                        state.width,
                        state.height,
                        match state.present_mode {
                            PresentMode::Fifo => PresentModeRequest::Fifo,
                            PresentMode::Immediate => PresentModeRequest::Immediate,
                            _ => PresentModeRequest::Mailbox,
                        },
                    ),
                    // Never configured, so there is nothing to restore; the next frame will
                    // configure it through the normal path.
                    None => return ptr::null_mut(),
                }
            };

            // Forced, because the configuration being "the same" is the whole problem: the
            // swapchain behind it is what needs rebuilding.
            configure_surface_inner(wm, surface, width, height, request, true);

            match surface.get_current_texture() {
                CurrentSurfaceTexture::Success(texture)
                | CurrentSurfaceTexture::Suboptimal(texture) => {
                    ACQUIRE_FAILURES.store(0, Ordering::Relaxed);
                    Box::into_raw(Box::new(texture))
                }
                other => {
                    if failures.is_multiple_of(120) {
                        warn!("wgpu-mc: still no swapchain image after reconfiguring: {other:?}");
                    }
                    ptr::null_mut()
                }
            }
        }
        CurrentSurfaceTexture::Timeout | CurrentSurfaceTexture::Occluded => {
            // The frame is simply skipped; neither is an error.
            ptr::null_mut()
        }
    }
}

/// Presents the frame, waits until only the rames in flight setting's worth of frames are outstanding, and lets wgpu
/// reclaim what the submissions behind them were holding.
///
/// The poll is not optional. wgpu frees a submission's command buffers, staging memory and
/// destroyed resources when the device is polled and the GPU has caught up - and this side never
/// polled, so every submission of every frame stayed allocated. With ten submissions a frame and a
/// few hundred frames a second without vsync, that is the rest of the twenty gigabytes: the leak
/// looked like "the game asks for memory and never gives it back" because it was exactly that.
///
/// That poll used to be `PollType::Poll`, which never blocks: the CPU was free to record frame N+1,
/// N+2, ... while the GPU was still on frame N, and nothing bounded the distance between them. The
/// only thing that did was the swapchain running out of images, which is a stall that arrives
/// whenever the driver decides. Waiting for a *named* submission instead is the same poll with a
/// bound on it: the frame that leaves that many behind has to be finished before the CPU
/// records the next one.
#[unsafe(no_mangle)]
pub extern "C" fn present_surface(wm: &WmRenderer, surface_texture: Box<SurfaceTexture>) {
    surface_texture.present();
    PRESENTS.fetch_add(1, Ordering::Relaxed);

    // The frame that just presented is the frame this side is finished with, which is the one moment
    // a graph holding pipelines built for a cull mode or a depth test can be replaced without one
    // frame being drawn half with the old ones. Nothing happens unless a pipeline-state switch moved.
    crate::debug::rebuild_pipelines_if_stale(wm);

    // The `frames in flight` setting, read per present rather than cached: one relaxed lock read a
    // frame, and lowering it has to take effect now - a player would be lowering it because the
    // latency is what they are looking at.
    let limit = crate::SETTINGS
        .read()
        .as_ref()
        .map_or(2, |settings| settings.frames_in_flight());

    // The arena's range recycling follows the same number: a section's old ranges are parked for that
    // many frames, because that is the first frame whose submission this present has waited for, and a
    // range handed out before then can be written while the frame that still draws it is on the GPU.
    if let Some(scene) = wm.scene() {
        scene.section_storage.write().set_deferred_depth(limit);
    }

    // What is left once this frame is counted among those in flight - `None` until there are `limit`
    // of them, which is the warm-up of a run and the case where nothing was submitted at all.
    //
    // The loop is a loop because the limit can change: waiting for the newest of the submissions
    // that fell out is enough to cover all of the older ones, so only one wait is ever made.
    let finished_frame = {
        let mut in_flight = IN_FLIGHT_SUBMISSIONS.lock();

        if let Some(index) = LAST_SUBMISSION.lock().take() {
            in_flight.push_back(index);
        }

        let mut wait_for = None;
        while in_flight.len() >= limit {
            match in_flight.pop_front() {
                Some(index) => wait_for = Some(index),
                None => break,
            }
        }

        wait_for
    };

    let poll = match finished_frame {
        // A submission index wgpu has already seen; if its frame is done, this returns at once.
        Some(index) => wm.gpu.device.poll(wgpu::PollType::Wait {
            submission_index: Some(index),
            timeout: None,
        }),
        // Nothing to wait for: release whatever the GPU is finished with and carry on.
        None => wm.gpu.device.poll(wgpu::PollType::Poll),
    };

    if let Err(error) = poll {
        warn!("wgpu-mc: polling the device after a present failed: {error:?}");
    }

    // A frame has been presented: a finished frame's timestamps can be picked up, and the `pix
    // capture` switch is followed here - a capture starts and ends at a frame boundary.
    crate::timing::frame_presented(wm);
    crate::pix::tick();
}

#[unsafe(no_mangle)]
pub extern "C" fn copy_buffer_to_texture(
    wm: &WmRenderer,
    _encoder: &mut CommandEncoderHandle,
    buffer: &wgpu::Buffer,
    buffer_start: u64,
    _buffer_end: u64,
    source_x: u32,
    source_y: u32,
    source_width: u32,
    source_height: u32,
    destination: &wgpu::Texture,
    destination_x: u32,
    destination_y: u32,
    copy_width: u32,
    copy_height: u32,
    mip_level: u32,
    _depth_or_array_layers: u32,
) {
    if mip_level >= destination.mip_level_count() {
        return;
    }

    // The token stands in for the one encoder this side owns; see `CommandEncoderHandle`.
    let encoder = unsafe { &mut *shared_encoder() };

    let buffer_size = buffer.size();

    let source_width_u64 = source_width as u64;
    let source_height_u64 = source_height as u64;

    let texel_size = destination.format().block_copy_size(None).unwrap() as u64;

    let offset_texels = source_x as u64 + source_y as u64 * source_width_u64;

    let source_offset = buffer_start + offset_texels * texel_size;

    let src_row_bytes = source_width_u64 * texel_size;

    let aligned_row_bytes = src_row_bytes.next_multiple_of(256);

    let scratch = wm.gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: aligned_row_bytes * source_height_u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });

    let mut valid_rows = 0u32;

    for row in 0..source_height_u64 {
        let src_row_offset = source_offset + row * src_row_bytes;

        let dst_row_offset = row * aligned_row_bytes;

        let src_end = src_row_offset + src_row_bytes;
        if src_row_offset >= buffer_size || src_end > buffer_size {
            continue;
        }

        if !src_row_offset.is_multiple_of(4) || !src_row_bytes.is_multiple_of(4) {
            continue;
        }

        encoder.copy_buffer_to_buffer(
            buffer,
            src_row_offset,
            &scratch,
            dst_row_offset,
            src_row_bytes,
        );

        valid_rows += 1;
    }

    if valid_rows == 0 {
        return;
    }

    encoder.copy_buffer_to_texture(
        TexelCopyBufferInfo {
            buffer: &scratch,
            layout: TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(aligned_row_bytes as u32),
                rows_per_image: Some(valid_rows),
            },
        },
        TexelCopyTextureInfo {
            texture: destination,
            mip_level,
            origin: Origin3d {
                x: destination_x,
                y: destination_y,
                z: 0,
            },
            aspect: Default::default(),
        },
        Extent3d {
            width: copy_width,
            height: valid_rows.min(copy_height),
            depth_or_array_layers: 1,
        },
    );
}

/// Uploads a rectangle of CPU bytes into a texture.
///
/// `depth_or_layer` is which cubemap face (or array layer) to write into - an index, not a count.
/// Every caller that reaches this passes an index: `CubeMapTexture#doLoad` uploads the six faces
/// with six separate calls, `writeToTexture(texture, image, 0, i, ...)`, and the two-argument
/// convenience overload passes 0 for the ordinary 2D case. `GlCommandEncoder` reads it the same
/// way, as a face index into `GlConst.CUBEMAP_TARGETS`.
#[unsafe(no_mangle)]
pub extern "C" fn write_to_texture(
    wm: &WmRenderer,
    destination: &wgpu::Texture,
    source: *const u8,
    source_size: u64,
    mip_level: u32,
    depth_or_layer: u32,
    dest_x: u32,
    dest_y: u32,
    width: u32,
    height: u32,
) {
    // Levels past the end of the chain are dropped rather than sent to wgpu, which would raise a
    // validation error for a level that does not exist. `create_texture` clamps a texture's chain
    // to what its size can hold, and Minecraft uploads every level the atlas has, so this is
    // reached by, say, a 9x9 sprite in an atlas with five levels - not by a bug worth aborting on.
    if mip_level >= destination.mip_level_count() {
        return;
    }

    // A texture write is the buffer case with no way to be clever about it: `Queue::write_texture`
    // is applied at the next submission, ahead of the frame's commands, so a re-upload of a texture
    // an already recorded draw samples would change what that draw sees. There is no offset to
    // compare against - the write names a mip, a layer and a rectangle - so this submits first, and
    // uploads are rare enough (a resource load, a skin, a font) for that to cost nothing.
    flush_shared_encoder(wm);

    wm.gpu.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: destination,
            mip_level,
            origin: Origin3d {
                x: dest_x,
                y: dest_y,
                z: depth_or_layer,
            },
            aspect: Default::default(),
        },
        unsafe { std::slice::from_raw_parts(source, source_size as _) },
        TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(destination.format().block_copy_size(None).unwrap() * width),
            rows_per_image: Some(height),
        },
        Extent3d {
            width,
            height,
            // One layer per call. Passing `depth_or_layer` here instead of into `origin.z` made a
            // cubemap face upload claim `i` whole layers - which is what produced
            // "Copy at offset 0 for 8388608 bytes would end up overrunning the bounds of the
            // Source buffer of size 4194304" on face 2 - and made the ordinary 2D uploads ask for
            // zero layers, which wgpu silently ignores.
            depth_or_array_layers: 1,
        },
    );
}

/// Submits what the shared encoder has recorded. The handle is an empty box and is dropped here.
#[unsafe(no_mangle)]
pub extern "C" fn submit_command_encoder(wm: &WmRenderer, _encoder: Box<CommandEncoderHandle>) {
    flush_shared_encoder(wm);
}

/// Records the render graph's terrain pass: the sections the Rust baker meshed, drawn from the arena
/// the section feed fills.
///
/// The pass is recorded into the frame's own encoder, between two of Minecraft's, so that what it
/// draws lands in the same colour and depth attachments as the geometry around it - a terrain pass in
/// a submission of its own would be terrain nothing else in the frame could occlude, or that occludes
/// everything. `depth` is therefore the caller's depth texture rather than the scene's own: the JVM
/// side hands over the view the pass it is standing in for would have used.
///
/// Answers whether anything was drawn, which is false when there is no scene yet, no graph, or no
/// terrain pipeline in it - and a caller that ignores the answer draws a world with no ground in it
/// and no line in the log to say why.
#[unsafe(no_mangle)]
pub extern "C" fn render_terrain_pass(
    wm: &WmRenderer,
    color: &wgpu::TextureView,
    depth: &wgpu::TextureView,
) -> bool {
    // The graph opens a pass of its own, and wgpu allows one at a time: recording it inside one of
    // Minecraft's would leave the frame's recording in a state neither side can describe.
    let open = LIVE_PASS_COUNT.load(Ordering::Relaxed);
    if open != 0 {
        error!(
            "wgpu-mc: the terrain pass was asked for while {open} render pass(es) are open; it has to \
             be recorded between two of Minecraft's, not inside one"
        );
        return false;
    }

    let Some(scene) = wm.scene() else {
        return false;
    };

    if !ensure_terrain_pipeline(wm) {
        return false;
    }

    let graph = crate::RENDER_GRAPH.get().unwrap().lock();

    // The matrices travel in buffers the pass binds, so they are uploaded for this frame before the
    // pass reads them. The one the culler uses is the perspective times the view, which is what the
    // shader multiplies a section by.
    crate::application::upload_terrain_matrices(wm);

    let view_projection = {
        let matrices = crate::renderer::MATRICES.lock();
        multiply(&matrices.projection, &matrices.view)
    };

    // The camera's section, read back out of the matrix this frame is drawn with rather than taken from
    // a value sent beside it: the view matrix is `R * translate(-p)`, so `p = -R^T * t` and the section
    // follows. It is used for one thing - the arena's trim - and the JVM's own `setCameraSection` is the
    // same kind of hint, sent when this path is off so that the arena is still trimmed against a camera.
    // Nothing in the transform reads either of them.
    if let Some(scene) = wm.scene() {
        let matrices = crate::renderer::MATRICES.lock();
        let view = matrices.view;

        *scene.camera_section_pos.write() = derive_camera_section(&view);
    }

    // The model matrix's translation, which is where the camera is in the space a section's name is
    // relative to. The culler needs it for the same reason the shader does: without it, a section is
    // tested where its *name* points rather than where it is drawn.
    let model_translation = {
        let matrices = crate::renderer::MATRICES.lock();
        let model = matrices.terrain_transformation;
        [model[3][0], model[3][1], model[3][2]]
    };

    report_terrain_transform(scene, &view_projection, model_translation);

    with_shared_encoder(|encoder| {
        graph.render_with_mvp(
            wm,
            encoder,
            scene,
            color,
            Some(depth),
            [0.0, 0.0, 0.0],
            view_projection,
            model_translation,
        );
    });

    true
}

/// The section the camera is in, read out of the level's view matrix.
///
/// The matrix is `R * translate(-p)` (see the terrain pass's own side): its three-by-three part is the
/// camera's rotation and its translation column is `R * (-p)`, so the position is `-R^T * t` - one dot
/// product per axis. Only the arena's trim wants it: what a draw is placed by is the matrix itself, and
/// a trim hint that is a frame old trims a frame late, which costs nothing at all.
fn derive_camera_section(view: &[[f32; 4]; 4]) -> glam::IVec2 {
    let translation = glam::Vec3::new(view[3][0], view[3][1], view[3][2]);

    let axis = |column: usize| {
        glam::Vec3::new(view[column][0], view[column][1], view[column][2]).dot(translation)
    };

    let position = glam::Vec3::new(-axis(0), -axis(1), -axis(2));

    glam::ivec2(
        (position.x / 16.0).floor() as i32,
        (position.z / 16.0).floor() as i32,
    )
}

/// Diagnostics: what the shader is handed, and where it lands.
///
/// The terrain pass is a transform in three parts - the section the pass names, the model matrix the
/// JVM sends, and the view-projection - and a picture that is mirrored, shifted or flat says which of
/// them is wrong only if their numbers are in the log. So the line names the camera's section, the
/// first section the arena holds with its model translation, the y scale of the matrix the shader
/// multiplies by, and the clip position the section's centre comes out at. Once a second, and only
/// with the section diagnostics on.
fn report_terrain_transform(
    scene: &wgpu_mc::mc::Scene,
    view_projection: &[[f32; 4]; 4],
    model_translation: [f32; 3],
) {
    if !wgpu_mc::mc::chunk::DIAGNOSTIC_LOGGING.load(Ordering::Relaxed) {
        return;
    }

    static REPORTED: AtomicU64 = AtomicU64::new(0);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);

    if REPORTED.swap(now, Ordering::Relaxed) == now {
        return;
    }

    let camera = *scene.camera_section_pos.read();
    let sections = scene.section_storage.read();
    let Some((pos, _)) = sections.iter().next() else {
        return;
    };

    let rel = glam::ivec3(pos.x - camera.x, pos.y, pos.z - camera.y);
    let centre = glam::vec3(
        (rel.x as f32 + 0.5) * 16.0,
        (rel.y as f32 + 0.5) * 16.0,
        (rel.z as f32 + 0.5) * 16.0,
    );

    // The shader's own arithmetic, in the order it does it: the section's centre, the model matrix,
    // the view (which is the identity here) and the view-projection. The culler is handed the same
    // translation and builds the same box, which is what keeps "where it is drawn" and "what is
    // culled" one place.
    let moved = [
        centre.x + model_translation[0],
        centre.y + model_translation[1],
        centre.z + model_translation[2],
    ];
    let clip: [f32; 4] = std::array::from_fn(|row| {
        (0..4)
            .map(|k| view_projection[k][row] * if k == 3 { 1.0 } else { moved[k] })
            .sum()
    });

    log::info!(
        "wgpu-mc: terrain transform: camera section {camera:?}, first section {pos:?} (relative {rel:?}), \
         model translation {model_translation:?}, y scale {:+.4}, section centre at {moved:?}, clip {clip:?}",
        view_projection[1][1],
    );
}

/// `a * b` for two column-major 4x4 matrices, which is the order the shader multiplies in.
fn multiply(a: &[[f32; 4]; 4], b: &[[f32; 4]; 4]) -> [[f32; 4]; 4] {
    let mut out = [[0.0f32; 4]; 4];

    for column in 0..4 {
        for row in 0..4 {
            out[column][row] = (0..4).map(|k| a[k][row] * b[column][k]).sum();
        }
    }

    out
}

/// How many sections the terrain pass has drawn since the renderer started.
///
/// The JVM side asks before it dumps the terrain layer: the pass runs from the first frame, when the
/// section arena is still empty, and a dump of the first one is a picture of nothing. This is the
/// number that says the pass has had a world to draw.
#[unsafe(no_mangle)]
pub extern "C" fn terrain_sections_drawn() -> u32 {
    wgpu_mc::render::graph::terrain_sections_drawn().min(u32::MAX as u64) as u32
}

/// How many blocks the fluid mesher has seen holding a fluid, over every bake so far.
///
/// The JVM side logs this beside the terrain pass. Zero is a fluid that never reached the baker;
/// blocks with no faces is a fluid whose sprite is not in the atlas. See `chunk::FLUID_BLOCKS`.
#[unsafe(no_mangle)]
pub extern "C" fn terrain_fluid_blocks() -> u32 {
    wgpu_mc::mc::chunk::fluid_totals().0.min(u32::MAX as u64) as u32
}

/// How many sections the arena has refused to hold, over the whole run. See [terrain_fluid_blocks].
#[unsafe(no_mangle)]
pub extern "C" fn terrain_sections_refused() -> u32 {
    wgpu_mc::mc::chunk::sections_refused().min(u32::MAX as u64) as u32
}

/// How many of those refusals were handed to the JVM by `refusedSections`, over the whole run.
///
/// The other half of the return channel's diagnostic: the JVM counts what it drains, and the two
/// numbers have to agree - a refusal that never arrived is a section the JVM still believes was
/// baked, which nothing will offer again.
#[unsafe(no_mangle)]
pub extern "C" fn terrain_sections_refused_reported() -> u32 {
    wgpu_mc::mc::chunk::sections_refused_reported().min(u32::MAX as u64) as u32
}

/// How many refusals were never handed over, over the whole run: past the list's cap, or cleared by
/// a level change. `refused == reported + dropped` is the invariant; see [terrain_sections_refused].
#[unsafe(no_mangle)]
pub extern "C" fn terrain_sections_refused_dropped() -> u32 {
    wgpu_mc::mc::chunk::sections_refused_dropped().min(u32::MAX as u64) as u32
}

/// How many fluid faces the mesher has drawn, over every bake so far. See [terrain_fluid_blocks].
#[unsafe(no_mangle)]
pub extern "C" fn terrain_fluid_quads() -> u32 {
    wgpu_mc::mc::chunk::fluid_totals().1.min(u32::MAX as u64) as u32
}

/// Whether the graph can draw the terrain pass yet, building it if it cannot.
///
/// The JVM side asks this before it takes a pass away from Minecraft. The graph's terrain pipeline is
/// built from the block atlas, which a resource reload stitches on a background thread, so for the
/// first seconds of a session there is nothing to draw with - and a pass taken away then is a frame
/// with no ground in it rather than a frame drawn another way. See `ensure_terrain_pipeline`.
#[unsafe(no_mangle)]
pub extern "C" fn terrain_pass_ready(wm: &WmRenderer) -> bool {
    wm.scene().is_some() && ensure_terrain_pipeline(wm)
}

/// How many sections the arena holds, which is what says whether there is terrain to draw at all.
///
/// The JVM side asks this before it takes the solid layer's pass away from Minecraft: the graph draws
/// the arena's contents, so a pass taken over while the arena is still empty is a frame with no ground
/// in it - the same trade, and the same answer, as the pipeline check above. It is one lock and a length,
/// and it is asked per frame because it changes as the world is meshed.
#[unsafe(no_mangle)]
pub extern "C" fn terrain_arena_sections(wm: &WmRenderer) -> u32 {
    wm.scene()
        .map_or(0, |scene| scene.section_storage.read().len() as u32)
}

/// How many slots the arena's pool has, and how many of them are handed out.
///
/// The pair is the diagnosis a full arena needs and the one thing the refusal count cannot say: a
/// refusal means "this section did not fit", and these say whether that is because the arena is nearly
/// full, because it is fragmented, or because the section that did not fit is enormous. Read on the
/// terrain line, where a run that is refusing sections can be sized from what it reports rather than
/// from the constant `arena_slots` guesses with.
#[unsafe(no_mangle)]
pub extern "C" fn terrain_arena_slots() -> u32 {
    RENDERER
        .get()
        .and_then(|wm| wm.scene())
        .map_or(0, |scene| scene.section_storage.read().pool_slots())
}

/// See [terrain_arena_slots].
#[unsafe(no_mangle)]
pub extern "C" fn terrain_arena_used() -> u32 {
    RENDERER
        .get()
        .and_then(|wm| wm.scene())
        .map_or(0, |scene| scene.section_storage.read().used_slots())
}

/// The largest number of slots any one section has taken, which is what a pool has to fit one of.
///
/// See [terrain_arena_used]: this is the other half of "why did it not fit".
#[unsafe(no_mangle)]
pub extern "C" fn terrain_arena_largest_section() -> u32 {
    wgpu_mc::mc::chunk::largest_section_slots()
}

/// Whether the arena can still be grown, which is whether a refused section is worth rebuilding.
///
/// A refusal is undone by two things: the arena grows (see `WmRenderer::grow_arena`), and the JVM asks
/// the game to rebuild the section that did not fit. The second is only worth doing while the first is
/// still possible - at the device's own `max_buffer_size` there is no room to grow into, and a rebuild
/// would be refused exactly as the last one was, once per tick, for as long as the player stands there.
/// So the JVM asks this before it dirties anything.
#[unsafe(no_mangle)]
pub extern "C" fn terrain_arena_can_grow() -> bool {
    RENDERER
        .get()
        .and_then(|wm| wm.scene())
        .is_some_and(|scene| scene.section_storage.read().pool_slots() < scene.arena_cap_slots)
}

/// Whether the graph has a terrain pipeline, building it if it does not.
///
/// The graph is built by a shader reload, and a shader reload is gated on the renderer being up *and*
/// on a resource reload happening after that - a `--quickPlaySingleplayer` launch reaches its world
/// without either, and its reload has already gone by. So the pass is what asks for the graph, once a
/// second at most: the block atlas it samples is stitched on a background thread after a reload, and
/// until it exists the graph comes up without the pipeline (see `create_pipelines`), which is exactly
/// the case a later attempt fixes.
fn ensure_terrain_pipeline(wm: &WmRenderer) -> bool {
    let built = || {
        crate::RENDER_GRAPH
            .get()
            .is_some_and(|graph| graph.lock().pipelines.contains_key("terrain"))
    };

    if built() {
        return true;
    }

    static LAST_ATTEMPT: AtomicU64 = AtomicU64::new(0);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);

    if LAST_ATTEMPT.swap(now, Ordering::Relaxed) == now {
        return false;
    }

    crate::application::load_shaders(wm);

    if !built() {
        warn!(
            "wgpu-mc: the render graph has no terrain pipeline yet; is the block atlas registered? \
             the pass will ask again next second"
        );
    }

    built()
}

/// Blits the frame into the swapchain image, and submits it.
///
/// The blit is what turns the main target's clip space back over - see `PresentBlit` - and it is
/// recorded into the shared encoder rather than one of its own: the whole frame is in that encoder,
/// and this is the submission that carries it, right before the swapchain image is presented.
///
/// This is also where the scene is ticked, because it is the one point in a frame that is known to
/// happen exactly once and to have a framebuffer size behind it: the sections the baker finished
/// since the last frame are moved into the arena here, the ones the camera has left behind are
/// trimmed, and the depth texture is rebuilt if this frame is a different size from the last one. It
/// happens *before* the blit records itself, so the buffer writes belong to the frame they are for.
/// Says how far the arena should reach, in chunks, and sizes it to that.
///
/// The render distance is Minecraft's own - the setting the player moves on the video options
/// screen - and it is sent rather than guessed because the arena's job is to hold exactly what the
/// game is drawing: trim it tighter and sections are freed while their meshes are still on screen,
/// leave it wider and the arena grows with everything the player has ever walked past.
///
/// Once per change, not once per frame: the distance moves when the slider does, and the JVM's side
/// of this (see `sendCameraSection`) sends it whenever it differs from the last one it sent. The
/// report that matters is the first one, and it arrives when a world is joined - which is the moment
/// `LevelRenderer#setLevel` has just cleared the arena, so the arena is empty and the size takes.
/// That is where a session's arena is decided, buffer and all.
///
/// What a *later* report cannot do is resize: the arena is one range allocator over one buffer, and
/// neither can be moved under live sections. The arena keeps the size it was built with, the trim
/// radius still follows the new distance, and the log says which of the two happened. A report that
/// arrives before the scene exists is dropped for the same reason it is rare - the JVM counts it as
/// sent, and the arena keeps the default size it was created with, which is the large one.
#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn setRenderDistance(_env: JNIEnv, _class: JClass, chunks: jint) {
    let Some(wm) = RENDERER.get() else {
        return;
    };

    let Some(scene) = wm.scene() else {
        // No scene yet, so nothing to size - and this report is not sent again, because the JVM's
        // side of it fires on a change. The arena then keeps the size it was made with, which is a
        // placeholder for a world that has not reported one: a session in that state grows the arena
        // when a section does not fit (`WmRenderer::grow_arena`), so it converges on the size the world
        // needs rather than on a number guessed here.
        return;
    };

    let chunks = chunks.max(0);
    let previous = scene.section_storage.read().width();
    // Capped by the device, not by a constant of this renderer's: `arena_slots` asks for 1.9 GB at 64
    // chunks, which is a request for a buffer that may be larger than the device is willing to make -
    // and a buffer that cannot be created is a validation error on the first write, which ends the
    // process. `arena_cap_slots` is this device's own `max_buffer_size` in slots, read when the scene
    // was made; past it the arena stops growing as well (see `WmRenderer::grow_arena`).
    let wanted = wgpu_mc::mc::chunk::arena_slots(chunks as u32).min(scene.arena_cap_slots);

    // The pool *and* the buffer it lives in, in one call, and only while the arena is empty - see
    // `Scene::set_arena_slots`. The report arrives when a world is joined, which is the moment the
    // arena was just cleared for, so this is where a session's arena is sized: to what the game says
    // it is drawing, rather than to the largest render distance it might have been set to.
    let sized = scene.set_arena_slots(wm, wanted);

    scene.section_storage.write().set_width(chunks);

    if sized {
        info!(
            "wgpu-mc: the section arena holds {} slot(s), {} MB, for {chunks} chunk(s) of view",
            wanted,
            wanted as u64 * 4 / (1024 * 1024)
        );
    } else {
        // The report lost the race with the first bake, so the arena cannot be re-created under the
        // sections it is holding. It can still be *grown*, which is the same change made without moving
        // anything: every range keeps its offset, so the report is honoured by the next frame's growth
        // instead of being dropped - and without this, a session whose first report arrived a frame late
        // ran at the arena's placeholder size and grew to the right one by doubling, one buffer copy at
        // a time. Only upwards: a report that asks for less than the pool already has is not a reason to
        // move it.
        let pool = scene.section_storage.read().pool_slots();
        let target = wanted
            .max(pool.saturating_mul(2))
            .min(scene.arena_cap_slots);

        if target > pool {
            scene
                .pending_arena_growth
                .store(target, std::sync::atomic::Ordering::Relaxed);
        }

        if previous != chunks {
            warn!(
                "wgpu-mc: the render distance is now {chunks} chunk(s), which the arena cannot be \
                 re-created for while it holds sections; it holds {} slot(s) and will grow to {target} \
                 on the next frame",
                pool
            );
        }
    }
}

/// Says where the camera is, in sections, which is what the arena is trimmed against.
///
/// Called once per frame from the JVM, before the frame is presented. Sections are what the terrain
/// path is keyed by everywhere - the baker's positions, the arena's ranges, the graph pass's grid -
/// so this is the one coordinate the renderer needs from the camera, and it is sent rather than
/// derived because the renderer has no camera of its own: Minecraft's matrices arrive as matrices.
///
/// `x` and `z` only: the arena trims horizontally, because a vertical slice of the world is loaded
/// all at once. This is the signature the Fabric module's `setSectionPos` had, kept for the same
/// reason it had it.
#[jni_fn("dev.birb.wgpu.rust.WgpuNative")]
pub fn setCameraSection(_env: JNIEnv, _class: JClass, x: jint, z: jint) {
    let Some(wm) = RENDERER.get() else {
        return;
    };

    let Some(scene) = wm.scene() else {
        // No framebuffer yet, so no scene to trim: the position is dropped rather than kept, because
        // the JVM sends one per frame and the next one is a frame away.
        return;
    };

    *scene.camera_section_pos.write() = glam::ivec2(x, z);
}

/// Blits the frame into the swapchain image, and submits it.
///
/// The blit is what turns the main target's clip space back over - see `PresentBlit` - and it is
/// recorded into the shared encoder rather than one of its own: the whole frame is in that encoder,
/// and this is the submission that carries it, right before the swapchain image is presented.
///
/// This is also where the scene is ticked, because it is the one point in a frame that is known to
/// happen exactly once and to have a framebuffer size behind it: the sections the baker finished
/// since the last frame are moved into the arena here, the ones the camera has left behind are
/// trimmed, and the depth texture is rebuilt if this frame is a different size from the last one. It
/// happens *before* the blit records itself, so the buffer writes belong to the frame they are for.
#[unsafe(no_mangle)]
pub extern "C" fn blit_from_texture(
    wm: &WmRenderer,
    texture_view: &wgpu::TextureView,
    surface_texture: &SurfaceTexture,
) {
    trace_marker(&format!("blit {:p}", texture_view));
    // The destination view has to be created with the format the swapchain was configured with,
    // and the blitter has to have been built for that same format. Both come from the state
    // `configure_surface` left behind, because the format is negotiated with the driver and is
    // not necessarily the `Bgra8Unorm` this used to assume.
    {
        let state = SURFACE_STATE.lock();
        let Some(state) = state.as_ref() else {
            warn!("wgpu-mc: nothing to blit into yet, the swapchain has not been configured");
            return;
        };

        // The swapchain's own size, which is what the scene's depth texture has to match: this is the
        // size the frame is presented at, and the terrain pass renders into an attachment of it.
        if let Some(scene) = wm.ensure_scene(wgpu::Extent3d {
            width: state.width,
            height: state.height,
            depth_or_array_layers: 1,
        }) {
            wm.tick_scene(scene);
        }

        let view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor {
                label: None,
                format: Some(state.format),
                dimension: Some(wgpu::TextureViewDimension::D2),
                usage: Some(wgpu::TextureUsages::RENDER_ATTACHMENT),
                aspect: Default::default(),
                base_mip_level: 0,
                mip_level_count: None,
                base_array_layer: 0,
                array_layer_count: None,
            });

        // Recorded into the one encoder this side owns, after everything else the frame recorded,
        // and submitted here: this is the frame's single submission and the swapchain image is
        // presented straight after it. The blit has to be in the same submission as the passes that
        // drew the target, or the swapchain gets the image as it was before them.
        with_shared_encoder(|encoder| {
            state
                .blitter
                .copy(&wm.gpu.device, encoder, texture_view, &view);

            // The blit is the last thing a frame submits, so this is where its end timestamp goes.
            crate::timing::frame_end(encoder, wm);
        });
    };

    flush_shared_encoder(wm);
}

#[unsafe(no_mangle)]
pub extern "C" fn create_buffer_init(
    wm: &WmRenderer,
    label: *const c_char,
    usage: u32,
    data: *mut u8,
    size: u64,
) -> Box<wgpu::Buffer> {
    let label = unsafe { CStr::from_ptr(label) };
    let data = unsafe { std::slice::from_raw_parts(data, size as _) };

    let diff = data.len().next_multiple_of(16) - data.len();

    let padded_data: Vec<u8> = data
        .iter()
        .copied()
        .chain(std::iter::repeat_n(0, diff))
        .collect();

    let wgpu_usage_flags = wgpu_buffer_usages(usage);

    let buffer = wm.gpu.device.create_buffer_init(&BufferInitDescriptor {
        label: Some(label.to_str().unwrap()),
        usage: wgpu_usage_flags,
        contents: &padded_data,
    });

    LIVE_BUFFER_COUNT.fetch_add(1, Ordering::Relaxed);
    LIVE_BUFFER_BYTES.fetch_add(size, Ordering::Relaxed);

    Box::new(buffer)
}

/// Creates a texture with the mip chain Minecraft asked for.
///
/// The count is clamped to the largest chain the size can hold: Minecraft allocates a sprite's
/// scratch texture with the *atlas's* mip count, and a 9x9 sprite cannot have five levels. Asking
/// wgpu for them anyway is a validation error, so the extra levels are dropped here and
/// `write_to_texture` drops the uploads that would have filled them.
#[unsafe(no_mangle)]
pub extern "C" fn create_texture(
    wm: &WmRenderer,
    format_id: GpuFormat,
    width: u32,
    height: u32,
    depth_or_layers: u32,
    usage: u32,
    mip_levels: u32,
    name: FfiStr,
) -> Box<wgpu::Texture> {
    let mut wgpu_usage_flags = wgpu::TextureUsages::empty();

    wgpu_usage_flags.set(wgpu::TextureUsages::COPY_DST, usage & 1 != 0);
    // Always allowed, and it is what makes a texture readable back for diagnostics. Nothing in
    // Minecraft's own usage flags asks for it on most textures, so without this the only textures
    // that can be dumped are the ones that already opt in.
    wgpu_usage_flags.set(wgpu::TextureUsages::COPY_SRC, true);
    wgpu_usage_flags.set(wgpu::TextureUsages::TEXTURE_BINDING, usage & 4 != 0);
    wgpu_usage_flags.set(wgpu::TextureUsages::RENDER_ATTACHMENT, usage & 8 != 0);

    let format = format_id.to_wgpu_texture_format();
    let max_levels = 32 - width.max(height).max(1).leading_zeros();

    let texture = wm.gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some(&*name),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: depth_or_layers,
        },
        mip_level_count: mip_levels.clamp(1, max_levels),
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu_usage_flags,
        view_formats: &[],
    });

    // The box's address is the one the JVM is handed and the one `drop_texture` sees again, so the
    // bookkeeping has to be done after boxing - registering the local's address instead left every
    // live texture unrecognised, and the allocator reuses a freed box's address immediately.
    let texture = Box::new(texture);
    note_texture_alive(&texture, &name);

    LIVE_TEXTURE_COUNT.fetch_add(1, Ordering::Relaxed);
    LIVE_TEXTURE_BYTES.fetch_add(
        texture_bytes(width, height, depth_or_layers),
        Ordering::Relaxed,
    );

    texture
}

/// Drops a texture the JVM closed, remembering it so a later use can be refused.
#[unsafe(no_mangle)]
pub extern "C" fn drop_texture(texture: Box<wgpu::Texture>) {
    note_texture_dead(&texture);
}

#[unsafe(no_mangle)]
pub extern "C" fn drop_texture_view(view: Box<wgpu::TextureView>) {
    LIVE_VIEW_COUNT.fetch_sub(1, Ordering::Relaxed);

    // A cached bind group may name this view, and the allocator hands the address out again.
    crate::blaze::invalidate_bind_group_cache(&*view as *const wgpu::TextureView as usize);

    note_view_dead(&view);
}

/// Buffers Minecraft has closed, kept alive for a short while.
///
/// The same use-after-close that the texture registry exists for happens to buffers, and it is
/// fatal in a different way: `Queue::write_buffer` answers a destroyed buffer with "Buffer with
/// 'Cloud UTB #1' label is invalid", and a validation error ends the process. Minecraft closes a
/// buffer and then writes to it in the same frame - it does that every time the cloud uniform
/// texel buffer is rebuilt - so a short quarantine makes the write land in a buffer that still
/// exists. Nothing reads it afterwards, and the entry is dropped once it is old enough or once the
/// quarantine is over budget, which is what keeps this from being a leak.
static QUARANTINED_BUFFERS: Mutex<Vec<(Box<wgpu::Buffer>, std::time::Instant)>> =
    Mutex::new(Vec::new());

/// How long a closed buffer is kept alive, and how much of them.
///
/// The use-after-close this exists for is *within a frame*: Minecraft closes the cloud uniform
/// buffer and writes to it again before the frame is out. Two seconds and a thousand entries was
/// far more than that.
const BUFFER_QUARANTINE: std::time::Duration = std::time::Duration::from_millis(500);

/// The quarantine's budget, in bytes.
///
/// A count was the wrong unit here, because the entries are not a fixed size: the uniform rings are
/// megabytes each, so 128 of them is hundreds of megabytes of memory held for a use that is over by
/// the end of the frame. The budget is what those entries actually cost - the title screen's uniform
/// rings are a few hundred kilobytes each, and the ones that are megabytes are the ones that were
/// worth evicting early.
const BUFFER_QUARANTINE_BYTES: u64 = 32 * 1024 * 1024;

fn quarantine_buffer(buffer: Box<wgpu::Buffer>) {
    let now = std::time::Instant::now();
    let size = buffer.size();
    let mut quarantined = QUARANTINED_BUFFERS.lock();

    quarantined.retain(|(_, closed_at)| now.duration_since(*closed_at) < BUFFER_QUARANTINE);

    // Oldest first, until the newcomer fits. `is_empty` and not `< budget - size`: a single buffer
    // larger than the whole budget still gets its quarantine, because dropping it would mean the
    // write Minecraft is about to make hits a buffer that is already gone - which is fatal, where
    // holding one large buffer for half a second is not.
    let mut bytes: u64 = quarantined.iter().map(|(buffer, _)| buffer.size()).sum();

    while !quarantined.is_empty() && bytes + size > BUFFER_QUARANTINE_BYTES {
        let (evicted, _) = quarantined.remove(0);
        bytes = bytes.saturating_sub(evicted.size());
    }

    quarantined.push((buffer, now));
}

#[unsafe(no_mangle)]
pub extern "C" fn drop_buffer(buffer: Box<wgpu::Buffer>) {
    LIVE_BUFFER_COUNT.fetch_sub(1, Ordering::Relaxed);
    LIVE_BUFFER_BYTES.fetch_sub(buffer.size(), Ordering::Relaxed);

    // A cached bind group may name this buffer, and the allocator hands the address out again. The
    // quarantine keeps the box itself alive, but a set built for the buffer that used to be there
    // has to go.
    crate::blaze::invalidate_bind_group_cache(&*buffer as *const wgpu::Buffer as usize);

    // The write mark goes with it: the address will belong to a new buffer eventually, and a mark
    // that outlived this one would make every upload into its successor submit a frame.
    forget_write_marks(&*buffer as *const wgpu::Buffer as usize);

    quarantine_buffer(buffer);
}

#[unsafe(no_mangle)]
pub extern "C" fn max_texture_size(wm: &WmRenderer) -> u32 {
    wm.gpu.device.limits().max_texture_dimension_2d
}

#[unsafe(no_mangle)]
pub extern "C" fn min_uniform_offset_alignment(wm: &WmRenderer) -> u32 {
    wm.gpu.device.limits().min_uniform_buffer_offset_alignment
}
