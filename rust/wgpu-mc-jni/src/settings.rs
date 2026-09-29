#![allow(dead_code)]

use std::path::PathBuf;

use lazy_static::lazy_static;
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use strum::IntoEnumIterator;
use strum_macros::{EnumIter, IntoStaticStr};

use crate::RUN_DIRECTORY;

static RENDERER_CONFIG_JSON: OnceCell<PathBuf> = OnceCell::new();

/// Renderer config, relative to the game directory.
const CONFIG_PATH: &str = "config/wgpu-mc-renderer.json";

/// Where the config used to live, before this crate was built for more than Fabric.
const LEGACY_CONFIG_PATH: &str = "config/fabric/wgpu-mc-renderer.json";

/// Add your settings here. Only use the structs from this
/// file, like FloatSetting and IntSetting, then add an
/// appropriate field to SettingsInfo below, and a default
/// value in the Default impl for this.
#[derive(Serialize, Deserialize, Debug)]
#[non_exhaustive]
pub struct Settings {
    /// Every field is `#[serde(default)]` so that a config file written by an older build still
    /// loads. Without this, adding a setting would reset the whole file, because
    /// [`Settings::load_or_default`] falls back to the defaults when deserialization fails.
    #[serde(default)]
    pub backend: EnumSetting,
    #[serde(default)]
    pub vsync: BoolSetting,
    /// **How the game's window fills the screen**, which vanilla has only two answers to.
    ///
    /// `Exclusive` is what the game's own fullscreen is: GLFW's monitor mode, the swapchain owned by the
    /// display, which is a real mode switch. `Borderless` is a window covering the monitor with no
    /// decorations - the same pixels and none of the mode switch, which is what a player wants when they
    /// alt-tab. `Off` is a window.
    ///
    /// Applied by `DisplayMode` on the JVM side, which is what `Window#setMode` is redirected to; see
    /// that file for why the seam is there and not at `toggleFullScreen`. The answer is in this document
    /// because it is this side that decides what the game's fullscreen key does.
    ///
    /// **Named rather than `#[serde(default)]`, and this is the trap it avoids.** `EnumSetting`'s own
    /// `Default` is `selected: 0` - the *first* variant, which is `Exclusive` - so a bare
    /// `#[serde(default)]` here hands every fresh config exclusive fullscreen however the enum is ordered,
    /// and `#[default]` on the variant does nothing. That was measured: the config was deleted, the client
    /// started, and it took the display over. See `no_fullscreen_mode`.
    #[serde(default = "no_fullscreen_mode")]
    pub fullscreen_mode: EnumSetting,
    /// Whether the terrain is drawn from the Rust baker's meshes rather than from Minecraft's own.
    ///
    /// On by default, because the path is what the renderer is being built towards; the switch is here
    /// so that it can be turned off from inside the game - which is what the `wgpu-geo-terrain` marker
    /// file used to be, a file name a player had to know and a restart to change.
    #[serde(default)]
    pub terrain: BoolSetting,
    /// Whether the game's animated block textures move, drawn on the options screen's Quality page.
    ///
    /// The two values are the quality convention the graphics preset uses - `Fancy` is the animation
    /// on, `Fast` is off - and what it decides is which atlas a face samples when the sprite behind it
    /// is one the game animates: the game animates its block atlas by rendering each due frame into it,
    /// so a face drawn from *that* atlas moves for free, and a face drawn from the copy this side packs
    /// holds whatever frame it was copied at. See `UV_GAME_ATLAS`.
    ///
    /// Not a performance switch in disguise, and the schema says so rather than implying otherwise: the
    /// game renders those frames whether or not this side samples them, and the difference at the
    /// sampler is one binding. `Fast` is the picture this renderer drew before any of it existed.
    ///
    /// Applied by baking the block models again, because the answer is *in* the vertices: a face is
    /// baked with one atlas's coordinates and a flag saying which. See `debug::apply`, which asks the
    /// JVM for the re-bake, and `BlockCache.blockTexturesChanged`.
    ///
    /// The default is named rather than left to `#[serde(default)]`, which would take
    /// `EnumSetting::default()` - the *first* variant, `Fast`, i.e. the animation off. That is the trap
    /// this field shares with [`Settings::frames_in_flight`]: a serde default and the type's own default
    /// are two different things, and only one of them is the behaviour this setting is replacing.
    #[serde(default = "animated_textures_default")]
    pub animated_textures: EnumSetting,
    /// How many frames the CPU may record ahead of the GPU.
    ///
    /// One is a full stall on every frame; two hides the recording behind the GPU's own work, which
    /// is what a frame time is made of; three buys a little more slack at the cost of a frame of
    /// latency and one more swapchain image. The present waits for the frame that leaves this many
    /// behind - see `present_surface` - so lowering it takes effect on the next present, not on the
    /// next launch.
    #[serde(default = "two_frames_in_flight")]
    pub frames_in_flight: IntSetting,
    /// The two switches that change how the frame is rendered, under the `Optimization` heading.
    ///
    /// **Field order here is the row order on the options screen**, and the sections have to come out
    /// contiguous: the page reads its rows from *this* struct - the config's own order, through
    /// `getSettings` - and its headings from [`SettingsInfo`], so a setting whose two orders disagree
    /// is drawn under the heading that happens to precede it. That is exactly what happened when
    /// these two were declared after the debug switches but marked `Optimization`: the page drew
    /// `Debug`, then `Optimization` over half of it, then `Debug` again for the rest.
    #[serde(default)]
    pub bind_group_cache: BoolSetting,
    #[serde(default)]
    pub dynamic_offsets: BoolSetting,
    /// Everything below is a debug switch, offered under the options screen's `Debug` heading.
    /// They are the marker files this renderer grew while it was being written, with a place in
    /// the UI: the marker still works (see [`crate::debug`]), and the setting is what a player can
    /// reach without knowing a file name.
    ///
    /// The ones that are off unless asked for name [`off`] as their serde default, because
    /// `#[serde(default)]` alone would take `BoolSetting::default()`, which is `true`.
    #[serde(default = "off")]
    pub host_validation: BoolSetting,
    #[serde(default = "off")]
    pub gpu_based_validation: BoolSetting,
    /// Whether the instance is built with wgpu's `DEBUG` flag: debug information in shaders and
    /// objects. See [`SettingInfo::debug`] for where it sits on the options screen.
    #[serde(default = "on")]
    pub shader_debug_info: BoolSetting,
    /// Whether an indirect draw whose arguments are out of bounds is turned into a no-op rather than
    /// being undefined. See [`SettingInfo::debug`].
    #[serde(default = "on")]
    pub validate_indirect_calls: BoolSetting,
    /// Whether labels are passed to the backend at all. See [`SettingInfo::debug`].
    #[serde(default = "off")]
    pub discard_backend_labels: BoolSetting,
    /// Whether a driver whose major Vulkan compliance version is 0 may be chosen at all.
    #[serde(default = "off")]
    pub allow_noncompliant_adapter: BoolSetting,
    /// Whether the renderer writes its diagnostic log lines. See [`SettingInfo::debug`] and
    /// [`SettingInfo::optimization`] for how the switches are grouped on the options screen.
    #[serde(default = "off")]
    pub logging: BoolSetting,
    #[serde(default = "off")]
    pub diagnostics: BoolSetting,
    #[serde(default = "off")]
    pub trace_dynamic_offsets: BoolSetting,
    #[serde(default = "off")]
    pub binding_verbosity: BoolSetting,
    #[serde(default = "off")]
    pub dump_shaders: BoolSetting,
    #[serde(default = "off")]
    pub gpu_timestamps: BoolSetting,
    #[serde(default = "off")]
    pub pix_capture: BoolSetting,
    /// Whether the section feed is timed, phase by phase.
    ///
    /// The feed is the one path that runs on Minecraft's chunk-build threads, so "is this cheap" is a
    /// question with a number behind it: how long the light lookup, the block data and the call into
    /// the native side each take, per section rebuild. Off by default because it is a clock read per
    /// phase per section, and a session that wants the number is one that would rather have it than
    /// not.
    #[serde(default = "off")]
    pub section_timing: BoolSetting,
    /// Whether the renderer reports what its buffer uploads did - see [`SettingInfo::debug`].
    ///
    /// Its own switch rather than part of `logging`: these are per-upload lines, one of them reads
    /// the buffer back from the GPU to check the bytes arrived, and a session that wants the
    /// renderer's log lines does not necessarily want either. Off by default, and *not* turned on by
    /// the logging switch, for the same reason.
    #[serde(default = "off")]
    pub upload_report: BoolSetting,
    /// How many frames to write out, starting with the next one presented.
    ///
    /// Zero is off. A dump is asked for while looking at the frame that misbehaves - the world is
    /// reached after a different number of frames every run, so "frame 300" is no use - and *one* frame
    /// rarely settles a flicker: a handful in a row is what says whether the terrain is there on every
    /// frame or on every other one. This is the `wgpu-dump-now` marker as a setting, with a count
    /// instead of a file, and turning it down to zero and up again asks for another handful.
    #[serde(default)]
    pub dump_frames: IntSetting,
    /// The two that change the *picture* rather than what is said about it: every pipeline the render
    /// graph builds is drawn with back faces kept, or with the depth test asking the opposite
    /// question.
    ///
    /// They were written to tell "the winding is wrong" apart from "the depth test keeps the wrong
    /// end", on a picture that is inside out either way, and they are the `wgpu-terrain-no-cull` and
    /// `wgpu-terrain-greater-depth` marker files as switches. Both are read when a pipeline is built
    /// rather than as it draws - see `wgpu_mc::render::graph::set_pipeline_diagnostics` - so applying
    /// one rebuilds the graph's pipelines, which is the one debug switch here that costs a moment
    /// rather than nothing.
    #[serde(default = "off")]
    pub terrain_no_cull: BoolSetting,
    /// Whether the terrain pass honours the game's occlusion graph. See [Settings::terrain_occlusion].
    ///
    /// Defaulted rather than `off`: the behaviour it names is the behaviour the renderer already had, so
    /// a config written before this switch existed is not asking for anything different. `BoolSetting`'s
    /// own default is `true`, which is exactly what that needs.
    #[serde(default)]
    pub terrain_occlusion: BoolSetting,
    /// Whether the game's block atlas is sampled from its base mip level only. See
    /// [`Settings::atlas_base_mip_only`].
    #[serde(default = "off")]
    pub atlas_base_mip_only: BoolSetting,
    #[serde(default = "off")]
    pub terrain_greater_depth: BoolSetting,
    /// **How many mip levels the terrain shaders bias their block-atlas fetches by**, which is a
    /// diagnostic for "is the level of detail chosen correctly" rather than a picture setting. See
    /// [`Settings::atlas_lod_bias`].
    #[serde(default = "no_lod_bias")]
    pub atlas_lod_bias: FloatSetting,
    /// **Whether the game atlas' mip levels are blended**, which is the one field where its sampler
    /// differs from this renderer's own. See [Settings::game_atlas_blend_mips].
    #[serde(default = "off")]
    pub game_atlas_blend_mips: BoolSetting,
}

/// The default of [`Settings::atlas_lod_bias`]: **no bias at all**, which is the fetch a plain
/// `textureSample` would make.
///
/// The range is small on purpose. A bias is a *shift of the chosen level*, and the whole question this
/// exists to answer is whether the level is off by one or two - a range wide enough to reach the end of
/// the chain would answer "yes it moved" without answering "by how much". The ends are still reachable
/// enough to be obvious: minus four is four levels finer, which on a five-level chain is level 0 for
/// almost every surface, and plus four is the opposite.
fn no_lod_bias() -> FloatSetting {
    FloatSetting {
        min: -4.0,
        max: 4.0,
        step: 0.5,
        // **-4, and it is a calibration rather than a taste.** A player's runs settled it: at `-4` the
        // fluid shimmer is gone, at `0` it is there. That is a *level* the sampler is choosing wrongly and
        // a bias that cancels it, not a preference about sharpness.
        //
        // **What it is compensating for is not known**, and that is worth stating plainly rather than
        // dressed up. Every reading of the coordinate path says the level should already be right - the
        // sprite's UVs span its own texels, `atlas_base_mip_only` proves the sampler honours a clamp, and
        // the bias itself moves the picture - so the fault is somewhere between the coordinates and the
        // derivative that has not been found. A global shift is the *wrong* shape of fix either way,
        // because it moves every surface including the ones that are already correct, which is why the
        // default is a calibration to be replaced by a per-sprite clamp rather than the answer.
        value: -4.0,
    }
}

/// The default of a setting that is off unless a player asks for it.
///
/// `BoolSetting`'s own default is `true`, which is right for a switch that disables something and
/// wrong for a log.
fn off() -> BoolSetting {
    BoolSetting::of(false)
}

/// The default of a setting that is on unless a player turns it off.
///
/// `BoolSetting::default()` is already `true`, so this exists to say *which* `true` a setting means
/// rather than to change a value - a `#[serde(default)]` would read as "whatever the type does", and
/// the two instance flags that were hardcoded in `device.rs` are on because that is what the renderer
/// was building by hand, not because the type defaults that way.
fn on() -> BoolSetting {
    BoolSetting::of(true)
}

/// The default of `frames_in_flight`: one frame recorded ahead of the one being presented.
///
/// Written out rather than left to `IntSetting::default`, whose range is 0..100 - and a zero here
/// would mean waiting for a frame that was never submitted.
fn two_frames_in_flight() -> IntSetting {
    IntSetting::of(1, 3, 1, 2)
}

/// The default of `animated_textures`: the animation on, which is the game's own behaviour.
///
/// See the note on that field: `EnumSetting`'s own default is its first variant, which is `Fast`.
fn animated_textures_default() -> EnumSetting {
    EnumSetting::from_variant(AnimatedTextures::default())
}

/// The default of `fullscreen_mode`: **a window**, which is what `options.txt`'s `fullscreen: false` means
/// and what a fresh install should open with.
///
/// Explicit for the reason on the field: `EnumSetting`'s own default is its zero variant rather than the
/// enum's `#[default]`, so the two have to be said separately and only one of them is a behaviour.
fn no_fullscreen_mode() -> EnumSetting {
    EnumSetting::from_variant(FullscreenMode::default())
}

#[derive(Serialize)]
pub struct SettingsInfo {
    backend: EnumSettingInfo<GraphicsBackend>,
    vsync: SettingInfo,
    fullscreen_mode: EnumSettingInfo<FullscreenMode>,
    terrain: SettingInfo,
    /// On the Quality page rather than on the renderer's own, which is why the page skips it: one row
    /// in one place. See `OptionPages`' `DRAWN_ELSEWHERE`.
    animated_textures: EnumSettingInfo<AnimatedTextures>,
    frames_in_flight: SettingInfo,
    /// The two switches that change *how* the frame is rendered rather than what is reported about
    /// it. **This order has to match [`Settings`]'s**, because the two halves of a row come from the
    /// two documents: the row itself and its order from the settings, and the heading it sits under
    /// from this schema. See the note on [`Settings::bind_group_cache`].
    bind_group_cache: SettingInfo,
    dynamic_offsets: SettingInfo,
    host_validation: SettingInfo,
    gpu_based_validation: SettingInfo,
    shader_debug_info: SettingInfo,
    validate_indirect_calls: SettingInfo,
    discard_backend_labels: SettingInfo,
    allow_noncompliant_adapter: SettingInfo,
    logging: SettingInfo,
    diagnostics: SettingInfo,
    trace_dynamic_offsets: SettingInfo,
    binding_verbosity: SettingInfo,
    dump_shaders: SettingInfo,
    gpu_timestamps: SettingInfo,
    pix_capture: SettingInfo,
    section_timing: SettingInfo,
    upload_report: SettingInfo,
    dump_frames: SettingInfo,
    terrain_no_cull: SettingInfo,
    terrain_occlusion: SettingInfo,
    atlas_base_mip_only: SettingInfo,
    terrain_greater_depth: SettingInfo,
    atlas_lod_bias: SettingInfo,
    game_atlas_blend_mips: SettingInfo,
}

/// The section the options screen puts a setting under, when it is not one of the plain ones.
///
/// A name rather than an index, so the screen can decide how a section looks - it draws this one
/// as a sub-heading after a blank row - without this side knowing anything about layout.
const DEBUG_SECTION: &str = "Debug";

/// The section for the switches that decide how the frame is rendered.
///
/// They were debug switches because they were written to bisect a rendering bug, and they are not:
/// one caches bind groups between draws and the other stops baking offsets into them, both are on by
/// default, and turning either off is a performance decision - the diagnostic is the *frame time*.
/// Putting them under `Debug` made them look like something to turn on when something is wrong.
const OPTIMIZATION_SECTION: &str = "Optimization";

lazy_static! {
    pub static ref SETTINGS_INFO: SettingsInfo = SettingsInfo {
        backend: EnumSettingInfo::new(
            "Graphics API wgpu renders with. Vulkan is available on Windows and Linux, \
            DirectX 12 only on Windows. The two are not interchangeable at runtime: the wgpu \
            instance, the adapter and every resource below it are created for one backend and \
            live as long as the game does, so switching takes effect on the next launch.",
            true,
        ),
        vsync: SettingInfo {
            desc: "Whether or not to sync the framerate to the display's framerate.\
            May reduce screen tearing, on the cost of added latency. Takes effect as soon as it \
            is applied: the swapchain is reconfigured with the other present mode.",
            // Unlike `backend`, this is not a property of the wgpu instance: it only picks the
            // swapchain's present mode, and a surface can be reconfigured at any time. `sendSettings`
            // does exactly that, which is why this one is applied without a restart.
            needs_restart: false,
            section: None,
        },
        fullscreen_mode: EnumSettingInfo::new(
            "How the window fills the screen.\n\n\
            **Exclusive** is GLFW's monitor mode - a real display mode switch, and what the game's own \
            fullscreen has always been. **Borderless** covers the monitor with no decorations and does \
            not change the display mode, which is what to pick if you alt-tab. **Off** is a window.\n\n\
            The game's own fullscreen key (F11 by default) toggles between Off and whichever mode is \
            chosen here, so the key keeps working and the page decides what it returns to.\n\n\
            Takes effect as soon as it is applied: none of the three touches the device or the \
            swapchain's format, so there is nothing to restart for - the surface is reconfigured when \
            the window's size changes, which this does.",
            false,
        ),
        terrain: SettingInfo {
            desc: "Draw the terrain from the Rust baker's meshes instead of from Minecraft's own. \
            The sections are baked natively and drawn by the render graph in the pass Minecraft's own \
            solid layer would have used, so the depth buffer, the cutout and translucent layers and \
            everything drawn after them stay Minecraft's. Switching it on asks the level renderer to \
            rebuild its sections, because the graph can only draw what the baker has baked; switching \
            it off hands the terrain back on the next frame. This is what the `wgpu-geo-terrain` marker \
            file used to be.",
            needs_restart: false,
            section: None,
        },
        animated_textures: EnumSettingInfo::new(
            "Whether the block textures Minecraft animates - fire, lava, the campfire, the sea lantern, \
            every sprite with an `.mcmeta` that says so - move on the terrain this renderer draws. \
            Minecraft animates its block atlas by rendering each due frame into it, and `Quality` draws \
            those faces from that atlas, so they move exactly as they do in vanilla and nothing is \
            copied per frame. `Fast` bakes them against this renderer's own copy of the sprite instead, \
            which is a single frame: the picture this renderer drew before any of this existed. The two \
            are a fidelity choice rather than a speed one - the game renders those frames either way - \
            and switching takes effect after the block models are baked again, which is a second or two \
            and re-meshes the loaded sections.",
            false,
        ),
        frames_in_flight: SettingInfo {
            desc: "How many frames the CPU may record ahead of the GPU. One is a full stall on \
            every frame and two hides the recording behind the GPU's own work; three adds a frame \
            of latency for a little more slack. Applies to the next present, without a restart.",
            needs_restart: false,
            section: None,
        },
        host_validation: SettingInfo::debug(
            "Load the graphics backend's own validation layer: D3D12's debug layer, Vulkan's \
            validation layer, or GL's debug output. It is written by the graphics vendor rather than \
            by wgpu, it reports through the driver's debug output, and it catches what wgpu cannot see \
            - a barrier in the wrong place, a resource used before its GPU work finished, a \
            descriptor the driver disagrees about. It was unconditional here, so every player loaded \
            a vendor debug layer they were not reading; it is off by default now. `gpu based \
            validation` needs it. \
            **This does not switch wgpu's own validation on or off.** wgpu checks every call it is \
            given whatever this says - that is what an error naming the call comes from, and it is \
            what produces the `wgpu_core::validation` warnings in the log with this setting off. So \
            turning this off does not make an invalid call silent; it removes the vendor's second \
            opinion and its debug output. The wgpu instance is created with this flag, so switching \
            takes effect on the next launch.",
            true,
        ),
        gpu_based_validation: SettingInfo::debug(
            "Ask the backend's validation layer to check what the GPU is actually asked to do, rather \
            than only the commands that were recorded: it runs the same vendor layer as `host \
            validation` above, but on the GPU. That is what catches a mistake only the hardware can \
            see - a read of a resource whose earlier write has not landed, a shader reading past a \
            binding, a missing barrier between two passes. It is the slowest thing here by a wide \
            margin and it is a development tool, so it is off by default. \
            **It needs `host validation`**, because it is that same layer doing more; asking for this \
            alone turns that on with it rather than doing nothing. \
            The wgpu instance is created with this flag, so switching takes effect on the next \
            launch.",
            true,
        ),
        shader_debug_info: SettingInfo::debug(
            "Build the instance with wgpu's `DEBUG` flag: debug information in shaders and objects. \
            It does not validate anything and it does not cost anything per draw - what it decides is \
            whether the objects this renderer creates carry the information a graphics debugger reads \
            (RenderDoc, Nsight, PIX, `spirv-dis`). On by default, and worth turning off only if a \
            driver is measurably slowed by the extra metadata, because a capture without it names \
            things like `texture_47` instead of `wgpu-mc block atlas`. \
            The wgpu instance is created with this flag, so switching takes effect on the next \
            launch. `WGPU_DEBUG=0` still overrides it from outside the game.",
            true,
        ),
        validate_indirect_calls: SettingInfo::debug(
            "Check the arguments in an indirect draw buffer before issuing the draw, and turn the draw \
            into a no-op when they are out of bounds: an index range that does not fit the bound index \
            buffer, an instance range that does not fit an instance-stepped vertex buffer, and - the \
            one that matters here - a non-zero `first instance` on a device without indirect-first-\
            instance support. On by default and it should stay on: **without it, an out-of-bounds \
            indirect argument is undefined behaviour rather than an error**, and on D3D12 the built-in \
            `instance index` stops accounting for `first instance` at all, which would draw terrain \
            from the wrong offsets rather than fail. What it costs is a bounds check on a handful of \
            integers per indirect call, not per drawn section. \
            The wgpu instance is created with this flag, so switching takes effect on the next \
            launch. `WGPU_VALIDATION_INDIRECT_CALL=0` still overrides it from outside the game.",
            true,
        ),
        discard_backend_labels: SettingInfo::debug(
            "Do not pass the labels this renderer gives its objects down to the graphics backend. Every \
            buffer, texture, pipeline and bind group here is named, and a name is a string lookup and a \
            driver call each time one is created - measurable in a world that allocates as it loads, and \
            not measurable at all once it has settled. Off by default, because what the names buy is \
            that a driver's own validation error and a graphics debugger both say `wgpu-mc section arena` \
            instead of a handle, which is most of what makes either readable. \
            The wgpu instance is created with this flag, so switching takes effect on the next \
            launch. `WGPU_DISCARD_HAL_LABELS=1` still overrides it from outside the game.",
            true,
        ),
        allow_noncompliant_adapter: SettingInfo::debug(
            "Allow wgpu to offer an adapter whose driver does not meet the graphics API's own \
            requirements - in practice a Vulkan driver reporting a major compliance version of 0, which \
            wgpu otherwise refuses to use at all. Off by default, because the point of the requirement \
            is that such a driver may be broken in ways wgpu cannot see: it is an escape hatch for a \
            machine where nothing else is offered, not a setting to leave on. With it off and no \
            compliant adapter, the renderer reports that it could not create one and the game shows its \
            \"no supported graphics backend\" screen. \
            The wgpu instance is created with this flag, so switching takes effect on the next \
            launch. `WGPU_ALLOW_UNDERLYING_NONCOMPLIANT_ADAPTER=1` still overrides it from outside the \
            game.",
            true,
        ),
        logging: SettingInfo::debug(
            "Write the renderer's diagnostic log lines: each pipeline the first time it is used, \
            the plan its bindings resolve against, each render pass with its draw count, the draw \
            and submission counters once a second, the sprite-animation pass counter, and the \
            reports for the uploads and uniforms the renderer verifies as it goes. Off by default, \
            because it is a line per pipeline and a line per second rather than a line per frame - \
            and it is a *log* switch: the dumps below are a separate one, so a run can write a frame \
            out without filling the log with counters. This is the `wgpu-logging` marker as a \
            switch, and the `wgpu_mc.diagnostics` system property or `WGPU_MC_DIAGNOSTICS` \
            environment variable still turns it on from outside the game.",
            false,
        ),
        diagnostics: SettingInfo::debug(
            "Write the renderer's dumps out as files: the frame the game is showing, the textures \
            it is showing it with, and a sprite atlas after the frame or two it takes Minecraft to \
            compose one. The dump itself is asked for by a `wgpu-dump-now` file in the run \
            directory, because a dump is about one specific frame and the interesting one is rarely \
            the one a switch was flipped on at; this is the switch that lets the ask through, and \
            `wgpu-dump-frames` turns both on. What the renderer *says* while it does it is the \
            logging switch above, so a diagnostic session is usually both.",
            false,
        ),
        bind_group_cache: SettingInfo::optimization(
            "Reuse a bind group between draws that bind the same resources at different dynamic \
            offsets, instead of building one per draw. On by default, and worth about a \
            `wgpu::BindGroup` per draw when it is turned off - which is what the frame time in a \
            scene with many small draws is made of. Turning it off is also how the cache is ruled \
            in or out as the cause of a rendering difference. This is the `wgpu-no-bind-group-cache` \
            marker as a switch, and the marker still turns the cache off.",
            false,
        ),
        dynamic_offsets: SettingInfo::optimization(
            "Bind uniform buffers with an offset instead of baking the offset into the bind group. \
            On by default, and it is what makes the bind group cache worth having: Minecraft \
            re-binds a buffer at a new offset for almost every draw. Turning it off bakes the \
            offset again, which is what the renderer did before dynamic offsets existed - a bind group \
            per distinct offset, which in a world is thousands a frame, so off is for diagnosis \
            rather than for play. This is the `wgpu-no-dynamic-offsets` marker as a switch, and like \
            the marker it is read once: see the restart below.",
            true,
        ),
        trace_dynamic_offsets: SettingInfo::debug(
            "Log every draw's bindings - the plan, each binding in slot order, and the offset that \
            travels with it - and the key the bind group cache was asked for. Very loud: it is a \
            line per draw, so it is meant to be turned on for a few frames and read back. This is \
            the `wgpu-trace-dynamic-offsets` marker as a switch.",
            false,
        ),
        binding_verbosity: SettingInfo::debug(
            "Log how every binding name was resolved against a pipeline's binding plan, and what \
            the plan was left holding: a name that only matched through the shim's `_wm_texshim` / \
            `_wm_sampler` suffix, the names a plan can be bound under when one of them is not in \
            it, and the slots a pipeline change left empty. This is the detailed version of the \
            warning the renderer always prints when a binding is not in the plan at all, and it is \
            the switch to turn on when a shader reads nothing and the question is which name it \
            was looking for. This is the `wgpu-binding-log` marker as a switch.",
            false,
        ),
        dump_shaders: SettingInfo::debug(
            "Write the GLSL that reaches the shader compiler into `wgpu-shaders/`, after this \
            renderer's preprocessing. The source naga sees is not the source Minecraft ships - \
            uniforms are annotated with the binding the plan gave them, implicit blocks are added, \
            samplers are split - and it is the only place that shows which of those went wrong. \
            This is the `wgpu-dump-shaders` marker as a switch.",
            false,
        ),
        gpu_timestamps: SettingInfo::debug(
            "Measure how long each presented frame takes on the GPU, with timestamp queries at the \
            start of the frame's first submission and the end of its last one. The number is the \
            GPU's own clock, so it says what the driver actually spent - the frame's passes, not \
            the CPU time spent recording them - and it is reported with the render stats. Nothing \
            is measured while this is off: the queries are written into the frame's command stream, \
            so a disabled switch costs nothing at all.",
            false,
        ),
        pix_capture: SettingInfo::debug(
            "Load PIX's capture libraries into the game and let PIX inspect it: \
            `WinPixGpuCapturer.dll`, so PIX can attach for a GPU capture at all, and \
            `WinPixTimingCapturer.dll`, which programmatic timing captures run through. Both are \
            loaded from the newest PIX installation on the machine, before the D3D12 device is \
            created - which is why this needs a restart: a process that loads the GPU capturer \
            after its device exists is one PIX refuses to attach to. With it on, the game can be \
            attached to from PIX (or launched through it), and switching this off and on again \
            while it runs takes a `wgpu-mc-capture-N.wpix` timing capture of 600 frames. That \
            capture records through ETW providers, and only an elevated process may create sessions \
            for them, so the game itself has to run as an administrator - starting `gradlew` from an \
            administrator terminal is not enough, because Gradle reuses a daemon started without \
            elevation and the game, forked by that daemon, inherits its token: run `gradlew --stop` \
            first, or pass `--no-daemon`. Without it the capture is refused with E_ACCESSDENIED, and \
            the log says whether the process was elevated. A capture holds everything the capture \
            API can be asked for: GPU timing, CPU samples with call stacks at 4 kHz, and the memory \
            events - file IO, VirtualAlloc, HeapAlloc, custom allocator and page faults - whose \
            tables (`MemoryEventRanges`, `MemoryPairing`, `PageFaults`, `FileIORange`) are empty \
            without them. That is also what makes a capture big and slow: 1.7 to 2.7 GB for 600 \
            frames rather than a few hundred megabytes, and the frame rate during a capture drops to \
            a few frames per second while those events are recorded. Function names come from a PDB \
            the build writes beside the library (`rust/Cargo.toml` asks for line tables), which PIX \
            reads when it opens the capture - without it every native frame in the capture is an \
            address and its function information stays empty. `GPU resources`/API objects, memory \
            access sampling and kernel image information are options only PIX's own timing-capture \
            dialog has: they are not fields of the API's parameter struct, so a capture with them is \
            one taken from PIX's UI, which is what the loaded GPU capturer makes possible. \
            RivaTuner Statistics Server - MSI Afterburner's on-screen display - hooks D3D12 as well, \
            and with its `RTSSHooks64.dll` in the process the game crashes inside RTSS's own present \
            hook once these libraries are loaded, capture or no capture: the log says so, PIX's \
            libraries are left unloaded in that case, and a `wgpu-pix-with-rtss` file next to the \
            game overrules that. An RTSS profile for the `java.exe` the game runs as, with \
            Application detection level `None`, is what makes the switch work with RTSS installed. \
            Nothing happens at all on a machine without PIX: the log says which library was \
            missing.",
            true,
        ),
        upload_report: SettingInfo::debug(
            "Report what the renderer's buffer uploads did: what a mapped write put in the buffer, \
            whether those bytes are in it afterwards, and how much staging the writes went through. \
            Off by default, because one of the three reads the buffer back from the GPU, and this is \
            a line per upload rather than a line per second. This is the `wgpu-upload-report` marker \
            as a switch.",
            false,
        ),
        dump_frames: SettingInfo::debug(
            "Write out the next N frames the renderer presents, as raw files under `wgpu-frames`, and \
            zero for none. A dump is about the frame that is on screen - the world is reached after a \
            different number of frames every run, so a fixed frame number is no use - and one frame \
            rarely settles a flicker: a handful in a row is what says whether the terrain is there on \
            every frame or on every other one. Turning it to zero and up again asks for another \
            handful. This is the `wgpu-dump-now` marker as a setting, with a count instead of a file.",
            false,
        ),
        section_timing: SettingInfo::debug(
            "Time the section feed, phase by phase, and report the averages. The feed is what runs \
            on Minecraft's chunk-build threads - the light lookup, the block data and the call into \
            the native side - so this is the switch that answers \"what does one section rebuild \
            cost, and which part of it\" with three numbers instead of a guess. The averages appear \
            on the F3 screen while it is on, and the counts are per offer, so a quiet frame is one \
            with nothing to show. Off by default: it is a clock read per phase per section, and \
            closing it costs nothing at all. This is the `wgpu-section-timing` marker as a switch.",
            false,
        ),
        terrain_no_cull: SettingInfo::debug(
            "Draw every pipeline the render graph builds without back-face culling, so nothing \
            depends on the winding at all. This is a diagnostic for a picture that is inside out - \
            with both faces rasterized, \"the winding is wrong\" and \"the depth test keeps the wrong \
            end\" stop looking the same - and it is not a mode to play with: the second copy of every \
            quad is shaded with the far side's lighting, and on a translucent quad the two copies land \
            at the same depth, where which of them survives is not defined. It is read by every \
            pipeline the graph builds and not only by the terrain one, because that is where the \
            `wgpu-terrain-no-cull` marker it replaces was read; the name is the bug it was written \
            for. Applying it rebuilds the graph's pipelines, because a cull mode is part of the \
            pipeline rather than something a draw can change.",
            false,
        ),
        terrain_occlusion: SettingInfo::debug(
            "Honour the game's own occlusion graph - the list of sections `LevelRenderer` worked out \
            are on screen - and skip the ones it did not name. On is the faster and, while the list is \
            right, the correct answer; off draws every section the frustum contains.\n\n\
            This is a switch because that list is a **snapshot**, and how stale it is decides whether \
            trusting it costs a hole. `LevelRenderer.applyFrustum` is the only thing that refills it, \
            and it runs only when the camera has turned by more than two degrees or the occlusion \
            graph says something changed - so between those moments the list is whatever the last \
            refill produced. A section this side has baked and the game's list has not caught up with \
            is skipped by the terrain pass while the game's own mesh for it stays suppressed, and a \
            section neither side draws is a 16x16x16 hole. With this off there are no holes to \
            attribute and the difference says whether that is the mechanism.",
            true,
        ),
        atlas_base_mip_only: SettingInfo::debug(
            "Sample the game's own block atlas from its base mip level only, so a distant animated \
            sprite resolves to a single texel of it rather than a blend of mip levels.\n\n\
            Off, because the game animates the whole chain rather than one level of it: \
            `TextureAtlas#uploadAnimationFrames` walks every level and renders the current frame of \
            each animated sprite into each of them, through that level's own view and that level's own \
            uniform buffer. No level is staler than any other, so there is no stale level for a clamp \
            to avoid - and clamping to 0.0 does not make the animated textures fresher, it makes them \
            alias: one texel of a 16x16 sprite per pixel is the temporal noise a mip chain exists to \
            remove. It is a switch because the case for it is a picture rather than an argument, and a \
            run with it on against a run with it off is what settles it. Applying it rebuilds the \
            graph, because a sampler is built with the pipelines rather than per draw.",
            false,
        ),
        terrain_greater_depth: SettingInfo::debug(
            "Draw every pipeline the render graph builds with the depth test the opposite way round, \
            so the faces *behind* are the ones kept. The other half of the pair above: it is what \
            \"the depth values are the wrong way round\" would look like, which is the other reading \
            of a picture whose front faces are missing. Depth writes are unchanged - the test is what \
            moved - so the picture is the far side of everything the camera can see through, and the \
            sky and the HUD are drawn over it as usual. This is the `wgpu-terrain-greater-depth` \
            marker as a switch, and it too applies to every pipeline the graph builds. Applying it \
            rebuilds the graph's pipelines.",
            false,
        ),
        game_atlas_blend_mips: SettingInfo::debug(
            "Blend between mip levels when sampling the game's own block atlas. Off - which is what the \
            renderer ships - picks one level and writes it, so a scrolling animated sprite is never shown \
            as a mix of two of its moments. On blends them the way vanilla does \
            (`GL_LINEAR_MIPMAP_LINEAR`), which is smoother for a static texture and is where the \
            fluid shimmer comes from.\
\
\
            The reason this is a switch and not a decision: the two differ only for content that moves, \
            and which of the two a player prefers is a picture rather than an argument. It takes effect on \
            the next frame - the sampler is built with the graph, so applying it rebuilds the graph's \
            pipelines and nothing is baked.",
            false,
        ),
        atlas_lod_bias: SettingInfo::debug(
            "Shift the mip level the terrain shaders pick for the block atlases. `0` is no shift and \
            the fetch a plain `textureSample` would make; **positive samples a coarser level and \
            negative a finer one**, so a blurry picture that sharpens as this goes negative means the \
            chosen level was too coarse, and one that never changes means the level was never the \
            problem.\n\n\
            It is a measure rather than a picture setting. The level comes from the screen-space \
            derivative of the texture coordinates, and every reading of this renderer's coordinates \
            says that derivative should match the game's - so the number is here to disagree with the \
            reading if the picture does.\n\n\
            Read per draw, so it takes effect on the next frame: nothing is baked and no pipeline is \
            rebuilt. That is the point of it - the same value as a shader constant could not be told \
            apart from one that never reached the GPU.",
            false,
        ),
    };
    pub static ref SETTINGS_INFO_JSON: String = serde_json::to_string(&*SETTINGS_INFO).unwrap();
}

/// The graphics API the wgpu instance is created with.
///
/// Only the backends this renderer has actually been exercised on are listed. `wgpu::Backends`
/// knows about Metal and the WebGPU backends too, but neither can present to a GLFW window on
/// the platforms this mod ships for, so offering them would only produce a launch that fails
/// after the window is already up.
#[derive(EnumIter, IntoStaticStr, Eq, PartialEq, Clone, Copy, Debug, Default)]
pub enum GraphicsBackend {
    #[default]
    Vulkan,
    #[strum(serialize = "DirectX 12")]
    DirectX12,
}

impl GraphicsBackend {
    /// Whether this backend can run on the platform the game is currently on.
    pub fn is_available_here(self) -> bool {
        match self {
            GraphicsBackend::Vulkan => cfg!(any(windows, target_os = "linux")),
            GraphicsBackend::DirectX12 => cfg!(windows),
        }
    }

    /// The backend to try when this one cannot be created.
    ///
    /// With two variants this is simply the other one. It exists as a named operation so that
    /// adding a backend to the enum forces a decision here instead of silently making the
    /// fallback order depend on declaration order.
    pub fn alternative(self) -> Self {
        match self {
            GraphicsBackend::Vulkan => GraphicsBackend::DirectX12,
            GraphicsBackend::DirectX12 => GraphicsBackend::Vulkan,
        }
    }
}

impl Settings {
    /// Loads the settings from disk, or returns the defaults.
    pub fn load_or_default() -> Settings {
        let config_path = Self::config_path_get_or_init();
        let setting = if config_path.exists() {
            let contents = std::fs::read_to_string(config_path).unwrap_or_default();
            match serde_json::from_str(&contents) {
                Ok(settings) => settings,
                Err(err) => {
                    // Every field has a serde default, so this really is a malformed file rather
                    // than an older one. Falling back keeps the game launchable.
                    log::warn!("Couldn't read {config_path:?} ({err}); using the defaults");
                    Settings::default()
                }
            }
        } else {
            let default = Settings::default();
            default.write();
            default
        };
        log::info!("Loaded settings: {setting:?}");
        setting
    }

    /// Where the renderer config lives, under the game directory.
    ///
    /// The `fabric/` subdirectory is a leftover from when this crate only shipped as a Fabric
    /// mod; NeoForge never creates it, so a fresh install would fail to write the config at all.
    /// A config left there by an older build is still read, and is rewritten to the new location
    /// the next time the options are applied.
    fn config_path_get_or_init<'a>() -> &'a PathBuf {
        RENDERER_CONFIG_JSON.get_or_init(|| {
            let run_directory = RUN_DIRECTORY.get().unwrap();
            let legacy = run_directory.join(LEGACY_CONFIG_PATH);
            if legacy.exists() {
                return legacy;
            }
            run_directory.join(CONFIG_PATH)
        })
    }

    pub fn write(&self) -> bool {
        let config_path = Self::config_path_get_or_init();

        if let Some(parent) = config_path.parent()
            && let Err(err) = std::fs::create_dir_all(parent)
        {
            log::error!("Couldn't create {parent:?} for the renderer config: {err}");
            return false;
        }

        let str = serde_json::to_string_pretty(self).unwrap();
        // Failing to persist a setting must not take the game down with it: a panic here would
        // run the panic hook, which exits Minecraft.
        match std::fs::write(config_path, str) {
            Ok(()) => true,
            Err(err) => {
                log::error!("Couldn't write the renderer config to {config_path:?}: {err}");
                false
            }
        }
    }
}

impl Settings {
    /// How many frames may be in flight, clamped.
    ///
    /// The config is a text file and the schema's range is only advice to the options screen: a
    /// hand-edited `0` or `9` has to mean one frame or three, not "wait for a frame that will never
    /// be submitted" or "run a second ahead".
    pub fn frames_in_flight(&self) -> usize {
        self.frames_in_flight.value.clamp(1, 3) as usize
    }

    /// The graphics API to build the wgpu instance with, falling back to [`GraphicsBackend::default`]
    /// when the settings have not been loaded yet (the JVM sends the run directory during client
    /// setup, which happens before the window and therefore before `create_renderer`).
    pub fn graphics_backend(&self) -> GraphicsBackend {
        self.backend.get_variant()
    }

    /// Whether faces whose sprite the game animates are drawn from the game's own atlas.
    ///
    /// See [`Settings::animated_textures`]: `Fancy` (the default) is yes, `Fast` is the frozen picture
    /// this renderer drew before the animated-texture path existed.
    /// How the window fills the screen. See [FullscreenMode].
    ///
    /// Read by `DisplayMode` on the JVM side through `getFullscreenMode`, once per mode change rather
    /// than per frame - there is nothing on the draw path that depends on it.
    pub fn fullscreen_mode(&self) -> FullscreenMode {
        self.fullscreen_mode.get_variant::<FullscreenMode>()
    }

    pub fn animated_textures(&self) -> bool {
        self.animated_textures
            .get_variant::<AnimatedTextures>()
            .is_on()
    }

    /// Whether the game's block atlas is sampled from its base mip level only.
    ///
    /// See `wgpu_mc::render::graph::set_atlas_base_mip_only`, which is what applies it - and which
    /// carries the argument for why the default is off.
    pub fn atlas_base_mip_only(&self) -> bool {
        self.atlas_base_mip_only.value
    }

    /// How many mip levels the terrain shaders bias their block-atlas fetches by. See
    /// `Settings::atlas_lod_bias` for what the value is for; it is read per draw, so moving it takes
    /// effect on the next frame and nothing has to be rebuilt.
    pub fn atlas_lod_bias(&self) -> f32 {
        self.atlas_lod_bias.value as f32
    }

    /// **Whether the sampler for the game's own block atlas blends between mip levels.**
    ///
    /// alse - the default - gives MipmapFilterMode::Nearest: the level chosen from the screen-space
    /// derivative is written to directly, and the blend that Linear would do between two of them is
    /// skipped. The reason to want that, and the switch to test it against, are in
    /// `wgpu_mc::render::atlas::game_atlas_sampler`.
    pub fn game_atlas_blend_mips(&self) -> bool {
        self.game_atlas_blend_mips.value
    }
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            backend: EnumSetting::from_variant(GraphicsBackend::default()),
            vsync: BoolSetting::default(),
            fullscreen_mode: EnumSetting::from_variant(FullscreenMode::default()),
            // `BoolSetting::default()` is `true`, which is the right default for the terrain path: it is
            // what the renderer is being built towards, and the switch is here to turn it *off*.
            terrain: BoolSetting::default(),
            // `Fancy`: the animated textures move, which is what the game does and what a player who has
            // not been told about this row expects to see.
            animated_textures: EnumSetting::from_variant(AnimatedTextures::default()),
            frames_in_flight: two_frames_in_flight(),
            // The debug switches default to the behaviour the renderer had before they existed:
            // logging and tracing off, the bind group cache and dynamic offsets on, and both
            // validation layers off. Host-side validation and GPU-based validation used to be
            // unconditional here, which charged every launch for a development tool - and the outer
            // layer is the more expensive of the two per recorded call, so it is the one worth a
            // switch rather than a constant.
            host_validation: BoolSetting::of(false),
            gpu_based_validation: BoolSetting::of(false),
            // The instance flags that were constants in `device.rs` until they were settings. Two are
            // on because that is what the renderer was building by hand: `DEBUG` (it set the flag
            // directly) and `VALIDATION_INDIRECT_CALL` (it lost the flag by setting `DEBUG` alone
            // instead of `InstanceFlags::debugging()`). The other two were off because they were
            // never set at all.
            shader_debug_info: BoolSetting::of(true),
            validate_indirect_calls: BoolSetting::of(true),
            discard_backend_labels: BoolSetting::of(false),
            allow_noncompliant_adapter: BoolSetting::of(false),
            logging: BoolSetting::of(false),
            diagnostics: BoolSetting::of(false),
            bind_group_cache: BoolSetting::of(true),
            dynamic_offsets: BoolSetting::of(true),
            trace_dynamic_offsets: BoolSetting::of(false),
            binding_verbosity: BoolSetting::of(false),
            dump_shaders: BoolSetting::of(false),
            gpu_timestamps: BoolSetting::of(false),
            pix_capture: BoolSetting::of(false),
            section_timing: BoolSetting::of(false),
            upload_report: BoolSetting::of(false),
            dump_frames: IntSetting::of(0, 120, 1, 0),
            // The renderer culled back faces and used the ordinary depth test before these existed,
            // and both of them are diagnostics: the switch is here to turn one *on*.
            terrain_no_cull: BoolSetting::of(false),
            terrain_occlusion: BoolSetting::of(true),
            atlas_base_mip_only: BoolSetting::of(false),
            terrain_greater_depth: BoolSetting::of(false),
            atlas_lod_bias: no_lod_bias(),
            game_atlas_blend_mips: BoolSetting::of(true),
        }
    }
}

/// Every debug switch the renderer has, resolved the way [`crate::debug`] reads them.
///
/// A struct rather than six getters because the flags are always wanted together: they are applied
/// in one place (when the settings are loaded or sent) and read in another (the draw path).
///
/// **`PartialEq` without `Eq`, because one member is a float.** The renderer's own convention elsewhere
/// is that a rate or a fraction that never has to be a hash key is an integer, and the bias was written
/// as a float because that is what the shader takes - so this is the one struct here that cannot be
/// `Eq`. Comparing two of these with `==` is still legal and is all any caller does; what is given up is
/// being able to use one as a key.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DebugSettings {
    /// Whether wgpu's own validation runs in this process. See [`Settings::host_validation`].
    pub host_validation: bool,
    pub gpu_based_validation: bool,
    /// Whether shaders and objects are built with debug information. See
    /// [`Settings::shader_debug_info`].
    pub shader_debug_info: bool,
    /// Whether an indirect draw with out-of-bounds arguments is made a no-op. See
    /// [`Settings::validate_indirect_calls`].
    pub validate_indirect_calls: bool,
    /// Whether object labels are kept from the backend. See [`Settings::discard_backend_labels`].
    pub discard_backend_labels: bool,
    /// Whether a non-compliant driver may be chosen. See [`Settings::allow_noncompliant_adapter`].
    pub allow_noncompliant_adapter: bool,
    /// Whether the renderer's diagnostic *log lines* are written.
    pub logging: bool,
    /// Whether its *dumps* are written. The two are separate switches, so a run can take a frame
    /// without a line a second of counters, and the other way round.
    pub diagnostics: bool,
    pub bind_group_cache: bool,
    pub dynamic_offsets: bool,
    pub trace_dynamic_offsets: bool,
    pub binding_verbosity: bool,
    pub dump_shaders: bool,
    pub gpu_timestamps: bool,
    pub pix_capture: bool,
    /// Whether the section feed is timed - see [`Settings::section_timing`].
    pub section_timing: bool,
    /// Whether the terrain pass honours the game's occlusion graph rather than drawing every section
    /// the frustum contains. See [Settings::terrain_occlusion], which is the switch.
    pub terrain_occlusion: bool,
    /// Whether the game's block atlas is sampled from its base mip level only. See
    /// [`Settings::atlas_base_mip_only`].
    pub atlas_base_mip_only: bool,
    /// How many mip levels the terrain shaders bias their block-atlas fetches by. See
    /// `Settings::atlas_lod_bias`.
    pub atlas_lod_bias: f32,
    /// Whether the game atlas blends between mip levels. See [Settings::game_atlas_blend_mips].
    pub game_atlas_blend_mips: bool,
    /// Whether every pipeline the graph builds keeps its back faces. See
    /// [`Settings::terrain_no_cull`], which is the switch - and
    /// `wgpu_mc::render::graph::set_pipeline_diagnostics`, which is what applies it to a pipeline.
    pub terrain_no_cull: bool,
    /// Whether every pipeline the graph builds draws with the depth test the other way round. The
    /// other half of the pair above.
    pub terrain_greater_depth: bool,
}

impl Settings {
    pub fn debug(&self) -> DebugSettings {
        DebugSettings {
            host_validation: self.host_validation.value,
            gpu_based_validation: self.gpu_based_validation.value,
            shader_debug_info: self.shader_debug_info.value,
            validate_indirect_calls: self.validate_indirect_calls.value,
            discard_backend_labels: self.discard_backend_labels.value,
            allow_noncompliant_adapter: self.allow_noncompliant_adapter.value,
            logging: self.logging.value,
            diagnostics: self.diagnostics.value,
            bind_group_cache: self.bind_group_cache.value,
            dynamic_offsets: self.dynamic_offsets.value,
            trace_dynamic_offsets: self.trace_dynamic_offsets.value,
            binding_verbosity: self.binding_verbosity.value,
            dump_shaders: self.dump_shaders.value,
            gpu_timestamps: self.gpu_timestamps.value,
            pix_capture: self.pix_capture.value,
            section_timing: self.section_timing.value,
            terrain_no_cull: self.terrain_no_cull.value,
            terrain_occlusion: self.terrain_occlusion.value,
            atlas_base_mip_only: self.atlas_base_mip_only.value,
            atlas_lod_bias: self.atlas_lod_bias.value as f32,
            game_atlas_blend_mips: self.game_atlas_blend_mips.value,
            terrain_greater_depth: self.terrain_greater_depth.value,
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct SettingInfo {
    pub desc: &'static str,
    pub needs_restart: bool,
    /// Which section of the options screen this belongs to; absent for the plain ones.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section: Option<&'static str>,
}

impl SettingInfo {
    /// A setting that sits in the options screen's list without a heading of its own.
    pub const fn new(desc: &'static str, needs_restart: bool) -> SettingInfo {
        SettingInfo {
            desc,
            needs_restart,
            section: None,
        }
    }

    /// A setting under the options screen's `Debug` sub-heading.
    pub const fn debug(desc: &'static str, needs_restart: bool) -> SettingInfo {
        SettingInfo {
            desc,
            needs_restart,
            section: Some(DEBUG_SECTION),
        }
    }

    /// A setting under the options screen's `Optimization` sub-heading.
    pub const fn optimization(desc: &'static str, needs_restart: bool) -> SettingInfo {
        SettingInfo {
            desc,
            needs_restart,
            section: Some(OPTIMIZATION_SECTION),
        }
    }
}

/// T should only be a c-like enum (no fields on variants),
/// mostly because I'm not sure what will happen when you put in anything else.
#[derive(Serialize, Deserialize)]
pub struct EnumSettingInfo<T: IntoEnumIterator + Into<&'static str> + LanguageKey> {
    pub desc: &'static str,
    pub needs_restart: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section: Option<&'static str>,
    /// The display names, in the order the options screen cycles through them.
    variants: Vec<&'static str>,
    /// Where each of those names is translated, in the same order.
    ///
    /// A key rather than a translated string because the wording belongs to the language files; the
    /// display name above is what a language that has not heard of this setting falls back to.
    variant_keys: Vec<&'static str>,
    #[serde(skip_serializing)]
    _marker: std::marker::PhantomData<T>,
}

impl<T: IntoEnumIterator + Into<&'static str> + LanguageKey> EnumSettingInfo<T> {
    pub fn new(desc: &'static str, needs_restart: bool) -> EnumSettingInfo<T> {
        EnumSettingInfo {
            desc,
            needs_restart,
            section: None,
            variants: T::iter().map(|e| e.into()).collect(),
            variant_keys: T::iter().map(|e| e.lang_key()).collect(),
            _marker: Default::default(),
        }
    }
}

/// A setting value that is named in the mod's language file.
///
/// The two halves of a value's text travel together: the name the schema carries, which is what the
/// options screen shows when nothing translates it, and the key a translation is found under.
pub trait LanguageKey {
    fn lang_key(&self) -> &'static str;
}

impl LanguageKey for GraphicsBackend {
    fn lang_key(&self) -> &'static str {
        match self {
            GraphicsBackend::Vulkan => "wgpu_mc.option.backend.vulkan",
            GraphicsBackend::DirectX12 => "wgpu_mc.option.backend.directx12",
        }
    }
}

/// Whether the block textures the game animates move on the terrain this renderer draws.
///
/// The values are spelled the way the graphics preset spells its own - `Fast` and `Fancy` - because
/// this is a row on the same page and answers the same kind of question. `Fancy` is the animation:
/// see [`Settings::animated_textures`] for what the two actually do.
#[derive(EnumIter, IntoStaticStr, Eq, PartialEq, Clone, Copy, Debug, Default)]
pub enum AnimatedTextures {
    /// The animation is off. A face whose sprite the game animates is baked against this side's copy
    /// of that sprite, which is one frame of it, and holds that frame.
    #[strum(serialize = "Fast")]
    Fast,
    /// The animation is on, for the price of one more binding: such a face is baked with the game's own
    /// coordinates and drawn from the atlas the game is animating.
    #[default]
    #[strum(serialize = "Fancy")]
    Fancy,
}

impl AnimatedTextures {
    /// Whether the animation is the one this variant asks for.
    pub fn is_on(self) -> bool {
        matches!(self, AnimatedTextures::Fancy)
    }
}

impl LanguageKey for AnimatedTextures {
    fn lang_key(&self) -> &'static str {
        match self {
            AnimatedTextures::Fast => "wgpu_mc.option.animated_textures.fast",
            AnimatedTextures::Fancy => "wgpu_mc.option.animated_textures.fancy",
        }
    }
}

/// **How the game's window fills the screen**, which is three answers where the game has two.
///
/// The two the game has are `Off` and `Exclusive`: `Window#setMode` decides between
/// `GLFW.glfwSetWindowMonitor(handle, monitor, ..)` and `(handle, 0L, ..)`, and `options.txt`'s
/// `fullscreen` is a boolean. `Borderless` is the third, and it is not a variation on either: it is a
/// window that covers the monitor with its decorations off, so it changes no display mode and owns no
/// output - which is the whole reason to want it.
///
/// See `DisplayMode` on the JVM side for what each becomes in GLFW calls.
#[derive(EnumIter, IntoStaticStr, Eq, PartialEq, Clone, Copy, Debug, Default)]
pub enum FullscreenMode {
    /// GLFW's monitor mode: the display switches resolution, and the swapchain belongs to the output.
    /// This is what the game's own fullscreen is - reachable, but no longer what a fresh install gets.
    #[strum(serialize = "Exclusive")]
    Exclusive,
    /// A decorated-free window covering the monitor's whole bounds, moved to its origin. No mode
    /// switch, no exclusive ownership - the desktop stays as it is underneath.
    #[strum(serialize = "Borderless")]
    Borderless,
    /// A window, at the size and position it was last left at.
    ///
    /// **The default**, because the value this setting replaces was `options.txt`'s `fullscreen`, whose own
    /// default is `false` - a fresh install is a window. `#[serde(default)]` on the field takes
    /// `EnumSetting`'s zero variant, which is this one, so a config that has never named a mode opens
    /// windowed and nothing has to be written for it.
    #[default]
    #[strum(serialize = "Off")]
    Off,
}

impl FullscreenMode {
    /// Whether this mode is *any* kind of fullscreen, which is what the rest of the game is told.
    ///
    /// `Window#isFullscreen` is read in three places - the F11 handler writes the vanilla option from it,
    /// the pause menu draws a tick from it, and `Options` compares it to decide whether to call
    /// `toggleFullScreen` - and for all three, "covers the screen" is the question. Answering `false` for
    /// borderless would make the pause menu say windowed while the window covered the monitor.
    pub fn is_fullscreen(self) -> bool {
        !matches!(self, FullscreenMode::Off)
    }
}

impl LanguageKey for FullscreenMode {
    fn lang_key(&self) -> &'static str {
        match self {
            FullscreenMode::Exclusive => "wgpu_mc.option.fullscreen_mode.exclusive",
            FullscreenMode::Borderless => "wgpu_mc.option.fullscreen_mode.borderless",
            FullscreenMode::Off => "wgpu_mc.option.fullscreen_mode.off",
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename = "bool")]
pub struct BoolSetting {
    pub value: bool,
}

impl BoolSetting {
    /// A setting whose default is not `true`, which is what [`Default`] gives every bool.
    pub const fn of(value: bool) -> BoolSetting {
        BoolSetting { value }
    }
}

impl Default for BoolSetting {
    fn default() -> Self {
        BoolSetting { value: true }
    }
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename = "float")]
pub struct FloatSetting {
    min: f64,
    max: f64,
    step: f64,
    pub value: f64,
}

impl Default for FloatSetting {
    fn default() -> Self {
        FloatSetting {
            min: 70.0,
            max: 120.0,
            step: 2.5,
            value: 90.0,
        }
    }
}

impl FloatSetting {
    pub fn get_min(&self) -> f64 {
        self.min
    }

    pub fn get_step(&self) -> f64 {
        self.step
    }

    pub fn get_max(&self) -> f64 {
        self.max
    }
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename = "int")]
pub struct IntSetting {
    min: i32,
    max: i32,
    step: i32,
    pub value: i32,
}

impl IntSetting {
    /// A setting a player can move between `min` and `max` in `step`s.
    pub const fn of(min: i32, max: i32, step: i32, value: i32) -> Self {
        Self {
            min,
            max,
            step,
            value,
        }
    }
}

impl Default for IntSetting {
    fn default() -> Self {
        IntSetting {
            min: 0,
            max: 100,
            step: 1,
            value: 0,
        }
    }
}

impl IntSetting {
    pub fn get_min(&self) -> i32 {
        self.min
    }

    pub fn get_step(&self) -> i32 {
        self.step
    }

    pub fn get_max(&self) -> i32 {
        self.max
    }
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename = "enum")]
#[derive(Default)]
pub struct EnumSetting {
    pub selected: usize,
}

impl EnumSetting {
    pub fn from_variant<T: IntoEnumIterator + Eq>(variant: T) -> EnumSetting {
        EnumSetting {
            selected: T::iter().position(|item| item == variant).unwrap(),
        }
    }
    /// If you know which type the setting has, then just get the variant with this.
    ///
    /// A `selected` index that does not name a variant (a hand-edited config file, or a config
    /// written by a build that offered more variants) falls back to `T::default()` rather than
    /// panicking: a panic here would run the panic hook, which exits the game.
    pub fn get_variant<T: IntoEnumIterator + Default>(&self) -> T {
        T::iter().nth(self.selected).unwrap_or_default()
    }
}

/// The JSON shapes below are a contract with the options screen, which builds its widgets from
/// `SETTINGS_INFO_JSON` and sends the edited values back through `sendSettings`. They are the part
/// of the graphics-backend switch that can be checked without a GPU, so they are.
#[cfg(test)]
mod tests {
    use super::*;

    /// The config file as it looks without a `backend` key, i.e. one written before the setting
    /// existed. It has to keep loading, or adding the setting would reset everyone's options.
    const LEGACY_CONFIG: &str = r#"{
        "vsync": { "type": "bool", "value": false }
    }"#;

    /// The animated-texture switch: `Fancy` is the animation, `Fast` is the frozen picture.
    ///
    /// Worth its lines because the two names are borrowed from the graphics preset, where "Fast" is the
    /// *cheaper* option - and here the cheaper-sounding one draws a single frame, which is only cheaper
    /// in the sense that it is what the renderer did before the animated-texture path existed. Reversed,
    /// this row would turn the animation on for the player who asked for less of it.
    #[test]
    fn fast_is_the_frozen_picture_and_fancy_is_the_animation() {
        let mut settings = Settings::default();

        assert!(
            settings.animated_textures(),
            "the default is the game's own behaviour: the textures the game animates move"
        );

        settings.animated_textures = EnumSetting::from_variant(AnimatedTextures::Fast);
        assert!(!settings.animated_textures());

        settings.animated_textures = EnumSetting::from_variant(AnimatedTextures::Fancy);
        assert!(settings.animated_textures());
    }

    /// A config written before this setting existed draws the animation, because that is what its
    /// default says - and `#[serde(default)]` is what makes an older file keep every other value too.
    #[test]
    fn a_config_without_the_animated_texture_key_animates() {
        let settings: Settings = serde_json::from_str(LEGACY_CONFIG).expect("legacy config");

        assert!(settings.animated_textures());
    }

    #[test]
    fn a_config_without_a_backend_key_still_loads() {
        let settings: Settings = serde_json::from_str(LEGACY_CONFIG).expect("legacy config");

        assert_eq!(settings.graphics_backend(), GraphicsBackend::Vulkan);
        assert!(!settings.vsync.value, "the existing value must survive");

        // A config written before the debug switches existed has none of them, and every one of
        // them has to come back as its default rather than as `false` - `bind group cache` and
        // `dynamic offsets` default to *on*.
        let debug = settings.debug();
        assert!(debug.bind_group_cache, "an absent switch keeps its default");
        assert!(debug.dynamic_offsets, "an absent switch keeps its default");
        assert!(!debug.diagnostics);
        assert!(!debug.gpu_based_validation);
        assert!(!debug.terrain_no_cull, "the renderer culled back faces");
        assert!(
            !debug.terrain_greater_depth,
            "and the depth test kept the near faces"
        );
    }

    #[test]
    fn every_value_of_an_enum_setting_names_a_translation_key() {
        let info: serde_json::Value = serde_json::from_str(&SETTINGS_INFO_JSON).expect("schema");
        let backend = &info["backend"];

        let variants = backend["variants"].as_array().expect("variants");
        let keys = backend["variant_keys"].as_array().expect("variant_keys");

        assert_eq!(
            keys.len(),
            variants.len(),
            "one key per value: the options screen shows them in this order"
        );

        for key in keys {
            let key = key.as_str().expect("a string key");
            assert!(
                key.starts_with("wgpu_mc.option.backend."),
                "{key} is not a key of this mod's settings namespace"
            );
        }

        assert_eq!(
            keys[1],
            serde_json::json!("wgpu_mc.option.backend.directx12"),
            "the second value is DirectX 12, which is what the display name says"
        );
    }

    /// **The order of `fullscreen_mode`'s values is an ABI**, and this is the half of it that can be
    /// checked here.
    ///
    /// The JVM's `DisplayMode.Mode` enum is declared in the same order and the value crosses as an
    /// *index* (`WgpuNative.windowMode` returns `FullscreenMode as u8`), so reordering this enum silently
    /// reorders that one - and the failure would not be an error, it would be `Borderless` selecting
    /// exclusive fullscreen. The other half is a list of three names in `DisplayMode.Mode`, which has no
    /// test source set of its own to live in; this is the half that can be asserted, and it is the half
    /// that would be edited first.
    #[test]
    fn the_window_modes_are_in_the_order_the_jvm_reads_them_in() {
        let modes: Vec<&'static str> = FullscreenMode::iter().map(|mode| mode.into()).collect();

        assert_eq!(
            modes,
            vec!["Exclusive", "Borderless", "Off"],
            "the JVM's DisplayMode.Mode declares EXCLUSIVE, BORDERLESS, OFF in this order and a value \
             crosses as an index - see `windowMode`"
        );

        // And the first variant is the default, which is what `#[serde(default)]` on the field means:
        // `EnumSetting`'s own default is variant zero, and zero has to be the game's own fullscreen so
        // that a config written before this setting existed behaves as it did.
        // The default is a window, because the value this setting stands in for is `options.txt`'s
        // `fullscreen` - a boolean whose own default is false. A fresh install opens windowed.
        assert_eq!(
            FullscreenMode::default(),
            FullscreenMode::Off,
            "a config with no `fullscreen_mode` key must open a window, which is what the game's own \
             `fullscreen: false` means"
        );

        // Every variant answers the question the rest of the game asks, and only `Off` says no.
        assert!(FullscreenMode::Exclusive.is_fullscreen());
        assert!(
            FullscreenMode::Borderless.is_fullscreen(),
            "a borderless window covers the screen, so the pause menu and the F11 handler have to be \
             told it is fullscreen - otherwise the video settings screen offers to turn fullscreen on \
             for a window that already fills the display"
        );
        assert!(!FullscreenMode::Off.is_fullscreen());
    }

    #[test]
    fn the_debug_switches_are_offered_under_a_heading() {
        let info: serde_json::Value = serde_json::from_str(&SETTINGS_INFO_JSON).expect("schema");

        for name in [
            "host_validation",
            "gpu_based_validation",
            "shader_debug_info",
            "validate_indirect_calls",
            "discard_backend_labels",
            "allow_noncompliant_adapter",
            "logging",
            "diagnostics",
            "trace_dynamic_offsets",
            "binding_verbosity",
            "dump_shaders",
            "gpu_timestamps",
            "pix_capture",
            "section_timing",
            "upload_report",
            "terrain_no_cull",
            "terrain_greater_depth",
        ] {
            assert_eq!(
                info[name]["section"],
                serde_json::json!("Debug"),
                "{name} is a debug switch and belongs under the heading"
            );
        }

        // The two that decide how the frame is rendered rather than what is said about it have a
        // heading of their own, above the debug ones - they are on by default and turning one off is
        // a performance decision, not a diagnostic.
        for name in ["bind_group_cache", "dynamic_offsets"] {
            assert_eq!(
                info[name]["section"],
                serde_json::json!("Optimization"),
                "{name} is an optimisation and belongs under its own heading"
            );
        }

        // The two that are not debug switches carry no section, or the options screen would draw
        // the heading over them.
        assert!(info["backend"].get("section").is_none());
        assert!(info["vsync"].get("section").is_none());
    }

    /// The options screen draws a heading when a setting's section differs from the one before it,
    /// so the order the schema lists them in is the order the page shows - and a section whose
    /// settings are not contiguous would be drawn twice.
    ///
    /// The order is read out of the serialized text rather than out of a `serde_json::Value`, because
    /// a `Value`'s object is a sorted map: the field order is a property of this file's struct and of
    /// the JSON the JVM side parses with Gson, and the text is the only place a test can see it.
    #[test]
    fn the_sections_are_contiguous_and_optimizations_come_first() {
        let position = |name: &str| {
            SETTINGS_INFO_JSON
                .find(&format!("\"{name}\""))
                .unwrap_or_else(|| panic!("{name} is not in the schema at all"))
        };

        let plain = ["backend", "vsync"];
        let optimization = ["bind_group_cache", "dynamic_offsets"];
        let debug = [
            "gpu_based_validation",
            "logging",
            "diagnostics",
            "trace_dynamic_offsets",
            "binding_verbosity",
            "dump_shaders",
            "gpu_timestamps",
            "pix_capture",
            "terrain_no_cull",
            "terrain_occlusion",
            "atlas_base_mip_only",
            "terrain_greater_depth",
        ];

        for earlier in plain {
            for later in optimization.iter().chain(debug.iter()) {
                assert!(
                    position(earlier) < position(later),
                    "{earlier} is listed after {later}, so the page draws a heading over it"
                );
            }
        }

        // Every optimisation before every debug switch: that is what makes the two sections
        // contiguous and puts the optimisations above the diagnostics.
        for earlier in optimization {
            for later in debug {
                assert!(
                    position(earlier) < position(later),
                    "{earlier} is listed after {later}: the sections are not in order, and one of \
                     them is split in two"
                );
            }
        }
    }

    /// The page's rows come from the *settings* document and their headings from the *schema*, so
    /// the two field orders have to agree. They did not, once: `bind_group_cache` and
    /// `dynamic_offsets` were declared after the debug switches and marked `Optimization`, and the
    /// page drew `Debug`, then `Optimization` over half of it, then `Debug` again.
    #[test]
    fn the_config_and_the_schema_list_the_settings_in_the_same_order() {
        let config = serde_json::to_string(&Settings::default()).expect("settings");
        let info: serde_json::Value = serde_json::from_str(&SETTINGS_INFO_JSON).expect("schema");

        // Both documents are read as *text*, because a `serde_json::Value` sorts its keys: which
        // order the settings are in is exactly what this test is about.
        let order_in = |document: &str| {
            let mut positions: Vec<(&str, usize)> = NAME_LIST
                .iter()
                .map(|name| {
                    let at = document
                        .find(&format!("\"{name}\""))
                        .unwrap_or_else(|| panic!("{name} is in one document and not the other"));
                    (*name, at)
                })
                .collect();
            positions.sort_by_key(|(_, at)| *at);
            positions
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>()
        };

        assert_eq!(
            order_in(&config),
            order_in(&SETTINGS_INFO_JSON),
            "the settings document and the schema list the settings in different orders, so a row \
             is drawn under the heading of whichever section came before it"
        );

        // And the names really are the schema's own, so a setting added to one document and not the
        // other is caught here rather than by a missing row.
        assert_eq!(NAME_LIST.len(), info.as_object().expect("an object").len());
    }

    /// Every setting's name, which is the same in both documents. Kept as a list because the two
    /// documents' own key order is not readable through `serde_json::Value` - see the test above.
    const NAME_LIST: [&str; 30] = [
        "backend",
        "vsync",
        "fullscreen_mode",
        "terrain",
        "animated_textures",
        "frames_in_flight",
        "bind_group_cache",
        "dynamic_offsets",
        "host_validation",
        "gpu_based_validation",
        "shader_debug_info",
        "validate_indirect_calls",
        "discard_backend_labels",
        "allow_noncompliant_adapter",
        "logging",
        "diagnostics",
        "trace_dynamic_offsets",
        "binding_verbosity",
        "dump_shaders",
        "gpu_timestamps",
        "pix_capture",
        "section_timing",
        "upload_report",
        "dump_frames",
        "terrain_no_cull",
        "terrain_occlusion",
        "atlas_base_mip_only",
        "terrain_greater_depth",
        "atlas_lod_bias",
        "game_atlas_blend_mips",
    ];

    /// The options screen, pulled in for the one part of it that is a contract with this side: how
    /// it decides that a page holds the renderer's settings.
    ///
    /// No compiler checks that. `Page.name` is a `Component`, so comparing it to a literal is legal
    /// Kotlin and stays legal when the label moves into a language file - which is what happened:
    ///
    /// ```kotlin
    /// if (name.string == "Electrum") { … sendSettings(…) … }
    /// ```
    ///
    /// The page is called `Neolectrum` in every language, so the comparison stopped matching, the
    /// page applied itself to variables only the screen could see, and no setting on it was sent to
    /// the renderer or written to the config - a restart then put every one of them back. A page is
    /// found by what its rows hold now, and the checks below are the text of that: this is a review
    /// that cannot be forgotten rather than a test of behaviour.
    const OPTION_PAGES: &str =
        include_str!("../../../neoforge/src/main/kotlin/dev/birb/wgpu/gui/OptionPages.kt");

    #[test]
    fn the_renderer_settings_page_is_not_found_by_its_label() {
        // Comments are dropped, because both this file and `OptionPages.kt` quote the mistake on
        // purpose - the quote is what says what not to do again.
        let source = code_of(OPTION_PAGES);

        // The mistake was a *comparison*: `name.string == "Electrum"` decided which page owned the
        // renderer's settings. Reading a label to print it is fine - the apply log names the rows it
        // applied that way - so this looks for a comparison rather than for the field.
        assert!(
            !source.contains("name.string ==") && !source.contains("name.string !="),
            "a page must be identified by the settings its rows carry, not by its translated label"
        );

        assert!(
            source.contains("it.setting != null"),
            "the page holding the renderer's settings is the one whose rows carry a setting name"
        );

        assert!(
            source.contains("WgpuNative.sendSettings("),
            "and it is the page that hands them to the renderer, which is what persists them"
        );
    }

    /// Kotlin source with its comment lines removed, one per line so that a quoted mistake in a
    /// comment is not read as the mistake itself.
    fn code_of(source: &str) -> String {
        source
            .lines()
            .filter(|line| {
                let line = line.trim_start();
                !(line.starts_with("//") || line.starts_with('*') || line.starts_with("/*"))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The section feed, pulled in for the one invariant in it that is a hole in the world when broken.
    const RUST_CHUNK_BAKE: &str =
        include_str!("../../../neoforge/src/main/kotlin/dev/birb/wgpu/chunk/RustChunkBake.kt");

    /// The hook that starts a bake, pulled in because *what it records* is the same invariant from the
    /// other side: it must not write the answer a second time. See the assertions that use it.
    const RUST_CHUNK_BAKE_MIXIN: &str = include_str!(
        "../../../neoforge/src/main/java/dev/birb/wgpu/mixin/chunk/RustChunkBakeMixin.java"
    );

    /// The native side of the "is this side keeping up" question, pulled in for what it keys on.
    const TERRAIN_ARENA_CAPACITY: &str = include_str!("device.rs");

    /// **Every place this side forgets what it told Rust, it forgets that Rust is drawing it too.**
    ///
    /// Minecraft's own mesh for a section is dropped once Rust has been handed that section
    /// (`SectionCompilerMixin`, gated on the `rustHas` set), which is what stops the game meshing the
    /// whole world for a pass that has been taken over. The cost of getting it wrong is asymmetric and
    /// that is why this is a test rather than a review: a `rustHas` entry that outlives its `sent` entry
    /// is a section this side believes Rust is drawing while Rust has been told to forget it - and a
    /// section nothing draws has nothing to look at. There is no log line for it, because from both
    /// sides the section looks handled.
    ///
    /// So the two tables are checked against each other by *counting*: the source has to clear them the
    /// same number of times, and no `sent` entry may be removed without its `rustHas` twin. A new
    /// `sent.clear()` that nobody thought about fails here rather than in a world.
    #[test]
    fn forgetting_what_rust_was_told_forgets_that_rust_is_drawing_it() {
        let source = code_of(RUST_CHUNK_BAKE);

        let clears = |name: &str| source.matches(&format!("{name}.clear()")).count();

        assert_eq!(
            clears("sent"),
            clears("rustHas"),
            "`sent` is cleared {} time(s) and `rustHas` {} - the two are one fact: what Rust has been \
             told, and what of that it is drawing",
            clears("sent"),
            clears("rustHas"),
        );

        assert!(
            clears("sent") > 0,
            "the test is looking for the clears by name; if they were renamed it is checking nothing"
        );

        // The removals, which are the refusals: one section at a time, and the two go together here
        // rather than in blocks of their own.
        assert_eq!(
            source.matches("sent.remove(key)").count(),
            source.matches("rustHas.remove(key)").count(),
            "a section removed from `sent` is a section Rust may have refused, so it is one \
             Minecraft's mesh has to be the fallback for again",
        );

        assert!(
            source.contains("rustHas.remove(key)"),
            "and the refusal path is where that matters: the arena refusing a section is the one case \
             where Rust was told and is still not drawing",
        );
    }

    /// **A refused section is queued for a rebuild rather than dropped**, and the requests are throttled
    /// three ways.
    ///
    /// A refusal is the one thing that leaves a section permanently undrawn. The arena had no room, so
    /// nothing was baked; `sent` forgets the section so the *next* rebuild carries its blocks - and a
    /// rebuild only carries what changed, so the next rebuild is the thing that never comes. The native
    /// side hands each refusal over exactly once (`SectionStorage::refused` is a `mem::take`), so a
    /// refusal this side drops is a hole for the session, with no log line and nothing to look at.
    ///
    /// The drain used to drop them: it marked at most its budget dirty and forgot the rest. That is the
    /// shape this test exists to keep out, and it is why the assertions are about the *queue* rather than
    /// about the budget: what has to survive is the refusals, not the rate.
    ///
    /// Checked on the source because the logic is Kotlin and this is the crate that can see it - the same
    /// arrangement as the `sent`/`rustHas` test above, and for the same reason: the failure is a missing
    /// line, and a missing line is what a test can see.
    #[test]
    fn a_refused_section_is_queued_for_a_rebuild_and_not_dropped() {
        let source = code_of(RUST_CHUNK_BAKE);

        // A queue, and the drain adds to it rather than acting on the batch directly.
        assert!(
            source.contains("pendingRedirty[key] = System.nanoTime()"),
            "the refusal drain has to remember the sections it could not act on; without this a refusal \
             past the per-frame budget is a section nothing will ever rebuild"
        );

        assert!(
            source.contains("private val pendingRedirty = java.util.LinkedHashMap<Long, Long>()"),
            "a map keyed by position, because a section refused twice before it was retried is one \
             rebuild and not two - and because *when* it was queued is what says how long the hole it \
             made was on screen"
        );

        // **Single-section dirty, never the neighbours variant.** `setSectionDirtyWithNeighbors` dirties
        // the 26 around it as well, which turns one refusal into 27 rebuilds - and those neighbours were
        // not refused, so each of their rebuilds offers a payload and reserves a bake slot for nothing.
        assert!(
            source.contains("renderer.setSectionDirty("),
            "a refusal is one section, so it is marked dirty one section at a time"
        );

        assert!(
            !source.contains("setSectionDirtyWithNeighbors"),
            "marking a refused section dirty must not drag its 26 neighbours into the rebuild queue: \
             they were not refused, and one refusal would become 27 rebuilds"
        );

        // The backoff, which is what stops "queue full, mark dirty, offer again, still full" from being
        // a storm: less work while the bake pool is behind.
        assert!(
            source.contains("private fun backedUp()"),
            "the drain has to ask how deep the bake queue is before asking for more work"
        );

        assert!(
            source.contains("val budget = if (backedUp()) 1 else REDIRTY_PER_FRAME"),
            "**and the backoff has to shrink the budget rather than stop the drain.** This queue is fed \
             by nothing else, so a backoff that returned early would be a livelock: no rebuild is asked \
             for because the pool is full, and the pool stays full because no rebuild finishes. The \
             sections stranded that way are 16x16x16 holes with nothing drawing them - which is what the \
             first version of this backoff did."
        );

        // The other half of the same property, checked structurally rather than by the string above:
        // nothing may consult `backedUp` before the budget is computed, because that is the shape that
        // stops the drain instead of slowing it.
        let drain = source
            .split("fun redirtyDue()")
            .nth(1)
            .expect("`redirtyDue` is still there");
        let before_budget = drain
            .split("val budget")
            .next()
            .expect("the budget is still computed in it");

        assert!(
            !before_budget.contains("backedUp()"),
            "`backedUp()` is consulted before the budget is computed, which makes it an early return \
             waiting to happen - it must only choose the size of the budget"
        );

        assert!(
            source.contains("WgpuNative.queuedBakes()")
                && source.contains("WgpuNative.maxQueuedBakes()"),
            "the depth of the bake queue is read from the native side rather than guessed at, and so is \
             the ceiling it is a fraction of"
        );

        // And the budget, which is the third throttle.
        assert!(
            source.contains("REDIRTY_PER_FRAME"),
            "the requests are budgeted per frame"
        );

        // **A refusal is queued whatever the arena's state**, which is the gate that produced a hole
        // after everything else had been fixed.
        assert!(
            source.contains("if (pendingRedirty.size < PENDING_REDIRTY_LIMIT) {"),
            "the refusal drain must queue the rebuild without asking whether the arena can grow. It used \
             to read `if (canGrow && ...)`, and a key dropped there had already been removed from \
             `rustHas` - so Minecraft's mesh was no longer suppressed *and* nothing asked for that \
             section to be rebuilt: the same 16x16x16 hole by another route. The rebuild converges even \
             with a full arena, because the next bake is refused too and answers \"not taken\", which \
             leaves Minecraft's own mesh in place."
        );

        let forget = source
            .split("fun forgetRefused()")
            .nth(1)
            .expect("`forgetRefused` is still there");
        let before_queue = forget
            .split("pendingRedirty[key] = System.nanoTime()")
            .next()
            .expect("the refusal still queues a rebuild");

        assert!(
            !before_queue.contains("canGrow &&"),
            "`canGrow` is gating the queue again - see the note above for what that cost"
        );

        // **`bake`'s answer has to be the decision, not "it did not throw".**
        //
        // The mixin that drops Minecraft's mesh is handed this answer, and every refusal inside
        // `bakeNow` is a plain return: a full bake queue, a rejected payload, an arena at its limit.
        // While the answer was `true` for all of them, a section Rust refused was dropped from
        // Minecraft's mesh too - drawn by neither side, which is the hole.
        assert!(
            source.contains("tookThisSection.get()"),
            "`bake` must read back the decision recorded inside `bakeNow`, not report that the call \
             returned normally"
        );

        // **And the decision has to include what Rust accepted.** `send` can come back having taken
        // nothing - the bake queue was full and the task was dropped, or the records were refused - and
        // it was already reporting that while the decision ignored it. A section Rust refused then had
        // Minecraft's mesh dropped too, and nothing asks for it back: only an *arena* refusal reaches
        // the re-offer drain, not a dropped task.
        assert!(
            source.contains("noteTookSection(!firstLook[0] && !atCapacity() && accepted)"),
            "the decision to drop Minecraft's mesh must require that Rust accepted the payload; \
             otherwise a refused section is drawn by neither renderer"
        );

        assert!(
            source.contains("val centreTaken = !resync && (rejected and (1 shl CENTER)) == 0"),
            "`send` has to report what was accepted per section, which is the centre slot - a \
             neighbour's refusal only costs the next payload"
        );

        // And the capacity check itself, which is the part only the native side can answer.
        assert!(
            source.contains("!atCapacity()"),
            "the decision to drop Minecraft's mesh has to account for an arena that cannot grow: at the \
             device's buffer limit a refusal is permanent, so the game keeps its mesh for those sections"
        );

        assert!(
            source.contains("fun atCapacity()"),
            "and that decision needs the capacity answer from the native side, which is `atCapacity`"
        );

        // And what that native answer is keyed on, which a run corrected once already: an arena is a
        // pool of *contiguous* ranges, so one with room in total and none in the size class being asked
        // for refuses exactly as a full one does. Keying on "at the device's buffer limit" alone left 33
        // real refusals - in an arena reporting 80% full - with the guard dormant.
        let device = code_of(TERRAIN_ARENA_CAPACITY);
        assert!(
            device.contains("storage.refusals_waiting() > 0"),
            "the guard has to fire on *refusals*, not only on the pool being at the device's limit: a \
             fragmented pool refuses sections while reporting twenty per cent free, and `at_capacity` is \
             false through all of it"
        );

        // The mixin must not write the answer a second time - a coarser one would overwrite it.
        let mixin = code_of(RUST_CHUNK_BAKE_MIXIN);
        assert!(
            !mixin.contains("noteTookSection"),
            "the mixin writes `tookThisSection` as well, which overwrites the capacity check with \
             `bake`'s coarser answer. `bake` records it; the mixin only calls `bake`."
        );
    }

    /// The language files, pulled in so that editing one of them re-runs these tests.
    ///
    /// The options screen builds every key it asks for out of a setting's name - `wgpu_mc.option.`
    /// and, for the description, `.tooltip` - so a setting the language files have never heard of
    /// is not an error anywhere: it is a row with a name made out of the config key. That is worth
    /// a test rather than a review, because adding a setting is exactly when it happens.
    const EN_US: &str =
        include_str!("../../../neoforge/src/main/resources/assets/wgpu_mc/lang/en_us.json");
    const ZH_CN: &str =
        include_str!("../../../neoforge/src/main/resources/assets/wgpu_mc/lang/zh_cn.json");

    fn translations(json: &str) -> serde_json::Value {
        serde_json::from_str(json).expect("a language file")
    }

    /// The settings the schema offers, in the order the options screen shows them.
    fn setting_names(info: &serde_json::Value) -> Vec<String> {
        info.as_object()
            .expect("the schema is an object")
            .keys()
            .cloned()
            .collect()
    }

    #[test]
    fn every_setting_has_a_name_in_every_language() {
        let info: serde_json::Value = serde_json::from_str(&SETTINGS_INFO_JSON).expect("schema");
        let languages = [
            ("en_us", translations(EN_US)),
            ("zh_cn", translations(ZH_CN)),
        ];

        for (language, translations) in &languages {
            for setting in setting_names(&info) {
                let key = format!("wgpu_mc.option.{setting}");

                assert!(
                    translations.get(&key).is_some(),
                    "{language} has no name for `{setting}` ({key})"
                );
            }
        }
    }

    #[test]
    fn every_value_of_an_enum_setting_has_a_name_in_every_language() {
        let info: serde_json::Value = serde_json::from_str(&SETTINGS_INFO_JSON).expect("schema");
        let languages = [
            ("en_us", translations(EN_US)),
            ("zh_cn", translations(ZH_CN)),
        ];

        for (language, translations) in &languages {
            for setting in setting_names(&info) {
                let Some(keys) = info[&setting]["variant_keys"].as_array() else {
                    continue;
                };

                for key in keys {
                    let key = key.as_str().expect("a string key");

                    assert!(
                        translations.get(key).is_some(),
                        "{language} has no name for `{setting}` value {key}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_translation_only_has_keys_this_mod_owns() {
        // A key that names nothing is a typo that would show up as a missing translation somewhere
        // else - usually a whole section of the screen left in English - so the namespace is
        // checked from this side, where the settings are.
        for (language, translations) in [
            ("en_us", translations(EN_US)),
            ("zh_cn", translations(ZH_CN)),
        ] {
            for key in translations.as_object().expect("an object").keys() {
                assert!(
                    key.starts_with("wgpu_mc."),
                    "{language} translates {key}, which is not this mod's key"
                );
            }
        }
    }

    #[test]
    fn only_the_instance_flags_need_a_restart() {
        let info: serde_json::Value = serde_json::from_str(&SETTINGS_INFO_JSON).expect("schema");

        // The three that are decided when the wgpu *instance* is created, which is once per renderer and
        // cannot be revisited: an instance flag is not a field that can be written afterwards.
        //
        // `dynamic_offsets` is the one this test got wrong first, and it is not an instance flag: it was
        // declared live, on the reasoning that it is only read as a draw is recorded - but *what* it
        // decides is whether a uniform's offset is part of the number the JVM identifies a draw's bind
        // groups by, and a pass holding bind groups built under the other answer cannot follow it.
        // Flipping it in a running world ended the process. See `set_dynamic_offsets` in `debug.rs`.
        for name in [
            "host_validation",
            "gpu_based_validation",
            "pix_capture",
            "dynamic_offsets",
            "shader_debug_info",
            "validate_indirect_calls",
            "discard_backend_labels",
            "allow_noncompliant_adapter",
        ] {
            assert_eq!(
                info[name]["needs_restart"],
                serde_json::Value::Bool(true),
                "{name} cannot change under a frame that is already numbered"
            );
        }

        for name in [
            "logging",
            "diagnostics",
            "bind_group_cache",
            "trace_dynamic_offsets",
            "binding_verbosity",
            "dump_shaders",
            "gpu_timestamps",
            // These two are built into a pipeline rather than consulted as it draws, but a rebuild
            // of the graph's pipelines is still not a restart: applying one takes effect on the
            // frame after it is applied.
            "terrain_no_cull",
            "terrain_greater_depth",
        ] {
            assert_eq!(
                info[name]["needs_restart"],
                serde_json::Value::Bool(false),
                "{name} applies without a restart"
            );
        }
    }

    #[test]
    fn the_debug_defaults_are_what_the_renderer_did_before_they_existed() {
        let debug = Settings::default().debug();

        assert!(!debug.diagnostics, "logging was off");
        assert!(!debug.logging, "the diagnostic log was off");
        assert!(!debug.trace_dynamic_offsets, "tracing was off");
        assert!(!debug.binding_verbosity, "the binding log was off");
        assert!(!debug.dump_shaders, "shader dumps were off");
        assert!(!debug.gpu_timestamps, "nothing measured the GPU");
        assert!(!debug.pix_capture, "no capture was being taken");
        assert!(debug.bind_group_cache, "the cache was on");
        assert!(debug.dynamic_offsets, "dynamic offsets were on");
        assert!(
            !debug.gpu_based_validation,
            "GPU-based validation is a development tool, not a default"
        );
        // **The one default that is a departure rather than a restoration**, and named as such so that
        // nobody reads this test as a promise the renderer keeps validation on. It used to be
        // unconditional: every launch paid wgpu's validation for every recorded call, and the switch
        // exists because that is a development tool's cost. See `instance_flags` in `device.rs` for
        // what is lost with it off - a wgpu error naming the call that caused it.
        assert!(
            !debug.host_validation,
            "host-side validation is off by default now; it used to be unconditional"
        );
        // **The four instance flags that were constants in `device.rs`.** Two are on because that is
        // what the renderer was building by hand, and two are off because it never set them - so this
        // is a record of what changed as much as of what the defaults are.
        assert!(
            debug.shader_debug_info,
            "the DEBUG instance flag was unconditional, so it stays on by default"
        );
        assert!(
            debug.validate_indirect_calls,
            "indirect-call validation was on - `InstanceFlags::from_build_config` returns it in a \
             release build, and the renderer lost it by setting `DEBUG` alone"
        );
        assert!(
            !debug.discard_backend_labels,
            "labels were always passed to the backend"
        );
        assert!(
            !debug.allow_noncompliant_adapter,
            "a non-compliant driver was never offered"
        );
        assert!(
            !debug.terrain_no_cull && !debug.terrain_greater_depth,
            "both pipeline-state diagnostics are off: the renderer culled back faces and tested the \
             usual way round"
        );
    }

    #[test]
    fn the_selected_index_names_the_backend() {
        // Written as the options screen writes it: the field is set on a defaulted `Settings` rather
        // than built with struct-update syntax, because that is the shape Gson round-trips.
        #[allow(clippy::field_reassign_with_default)]
        let settings = {
            let mut settings = Settings::default();
            settings.backend = EnumSetting::from_variant(GraphicsBackend::DirectX12);
            settings
        };
        assert_eq!(settings.graphics_backend(), GraphicsBackend::DirectX12);

        // What Gson writes back after the options screen edited the enum.
        let round_tripped: Settings =
            serde_json::from_str(&serde_json::to_string(&settings).unwrap()).expect("round trip");
        assert_eq!(round_tripped.graphics_backend(), GraphicsBackend::DirectX12);
    }

    #[test]
    fn an_out_of_range_index_falls_back_instead_of_panicking() {
        let settings: Settings =
            serde_json::from_str(r#"{ "backend": { "type": "enum", "selected": 7 } }"#)
                .expect("config with a bogus index");

        assert_eq!(settings.graphics_backend(), GraphicsBackend::Vulkan);
    }

    #[test]
    fn the_schema_marks_the_backend_as_needing_a_restart() {
        let info: serde_json::Value = serde_json::from_str(&SETTINGS_INFO_JSON).expect("schema");
        let backend = &info["backend"];

        assert_eq!(backend["needs_restart"], serde_json::Value::Bool(true));
        assert_eq!(
            backend["variants"],
            serde_json::json!(["Vulkan", "DirectX 12"]),
            "the options screen renders these verbatim"
        );
    }
}
