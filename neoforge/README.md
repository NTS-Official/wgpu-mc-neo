 wgpu-mc NeoForge - Neolectrum

This module is the NeoForge port of the Fabric Electrum mod.

## Current Baseline

- Loader: NeoForge 26.1.2.109 (NeoGradle userdev 7.1.21)
- Minecraft: 26.1 (hotfix 2)
- Java: 25
- Mod loader metadata: `META-INF/neoforge.mods.toml`
- Native bridge: reuses `rust/wgpu-mc-jni`

## What Was Migrated For 26.1

Minecraft 26.1 is a large platform break for this mod, because it removed GL-level
control of the render backend. The concrete changes handled here:

### Toolchain

- `net.neoforged.gradle.userdev` 7.1.25 -> 7.1.21, NeoForge `21.1.228` -> `26.1.2.109`
- Java toolchain 21 -> 25 (Minecraft 26.1 moved to Java 25)
- Minecraft 26.1 is **no longer obfuscated**, so Parchment/mappings configuration was dropped
- NeoForge moved to four-part versions (`<minecraft>.<hotfix>.<release>`)
- The module is now actually wired into `settings.gradle`; previously it was not included
  in the build at all and referenced `neoforge_*` properties that did not exist

### Renames applied (26.1 uses official/Yarn-style names)

| 1.21.1 (Mojmap) | 26.1 |
| --- | --- |
| `net.minecraft.resources.ResourceLocation` | `net.minecraft.resources.Identifier` |
| `net.minecraft.client.gui.GuiGraphics` | `net.minecraft.client.gui.GuiGraphicsExtractor` |
| `net.minecraft.util.FastColor.ARGB32` | `net.minecraft.util.ARGB` |
| `net.minecraft.client.renderer.LightTexture` | `net.minecraft.client.renderer.Lightmap` |
| `net.minecraft.client.GraphicsStatus` | `net.minecraft.client.GraphicsPreset` |
| `net.minecraft.client.ParticleStatus` | `net.minecraft.server.level.ParticleStatus` |
| `net.minecraft.client.renderer.entity.ItemRenderer` | removed (use `ItemInHandRenderer`) |
| `SectionRenderDispatcher$RenderSection#origin` | `#renderOrigin` |
| `SectionRenderDispatcher$SectionTaskResult` | `$RenderSection$CompileTask$SectionTaskResult` |
| `PalettedContainer$Configuration` | `net.minecraft.world.level.chunk.Configuration` |
| `PalettedContainer$Strategy` | `PalettedContainerFactory` |
| `LayerLightEngine` | `LayerLightEventListener` + `BlockLightEngine` / `SkyLightEngine` |
| `SkyLightSectionStorage$SkyData` | `SkyLightSectionStorage$SkyDataLayerStorageMap` |
| `RegisterClientReloadListenersEvent` | `AddClientReloadListenersEvent` |

### Behavioural API changes

- `Screen#render` -> `Screen#extractRenderState`, `Screen#renderBackground` -> `#extractBackground`
- `GuiGraphics#drawString` -> `GuiGraphicsExtractor#text` / `#textWithWordWrap`
- `Gui#renderVignette` -> `#extractVignette`
- `DebugScreenOverlay#getSystemInformation` was removed; the F3 lines are now collected in
  `DebugHUDMixin` and the renderer's own line in `DebugEntrySystemSpecsMixin`, which appends it to
  vanilla's system block
- `BlockColors#getColor(...)` -> the `BlockTintSource` pipeline (`getTintSource` + `colorInWorld`)
- Mouse handling now uses `MouseButtonEvent` records instead of `(double, double, int)`
- `Window#window` is private; the GLFW handle is `Window#handle()`
- `Minecraft#resizeDisplay` -> `#resizeGui`
- `GameRenderer#bobView` now takes a `CameraRenderState`
- `VertexConsumer` gained `setColor(int)` and `setLineWidth(float)`
- `AddClientReloadListenersEvent` registers via `addListener(Identifier, listener)`

## The 26.1 render backend

26.1 replaced Blaze3D's GL-level seam with a backend abstraction, so the port now implements it
the same way the Fabric module does.

`dev.birb.wgpu.mixin.core.WgpuBackendSelectionMixin` swaps the `GlBackend` instance that
`Minecraft` builds into its backend list for `WgpuBackend`. From there the whole renderer comes
up on wgpu:

| Blaze3D interface | Implementation |
| --- | --- |
| `GpuBackend` | `backend/WgpuBackend.kt` |
| `GpuDeviceBackend` | `backend/WgpuDevice.kt` |
| `CommandEncoderBackend` | `backend/WgpuCommandEncoder.kt` |
| `RenderPassBackend` | `backend/WgpuRenderPass.java` |
| `GpuTexture` / `GpuTextureView` | `backend/WgpuTexture.kt` |
| `GpuBuffer` | `backend/WgpuBuffer.kt` |
| `GpuSampler` | `backend/WgpuSampler.kt` |
| `CompiledRenderPipeline` | `backend/WgpuCompiledRenderPipeline.kt` |

Everything except the render pass is Kotlin. `WgpuRenderPass` stays in Java because
`RenderPassBackend` declares `<T> void drawMultipleIndexed(...)` with an unbounded type parameter,
which Kotlin cannot override; the mixins are Java as well.

### Native bridge

The backend talks to `rust/wgpu-mc-jni` two ways, both addressing the *same* `WmRenderer`:

- **JNI** creates the renderer and returns its pointer.
  `WgpuNative.createWmRendererOnWindow(display, window)` is the entry point the backend uses: the
  window handles are passed in so Rust can create the surface *before* it asks for an adapter, and
  therefore require the adapter to support it. `createWmRenderer()` still exists and creates a
  renderer with no surface.
- **C ABI** (`rust/WmNative.kt`) is the FFI layer for everything after that. The Fabric module
  generates it with the `jextract` Gradle plugin; here it is bound by hand in one file with the
  struct layouts and field offsets written out next to the `bindings.h` declarations they mirror.

Both bridges are checked against the Rust side by `cargo test` - see "Neither compiler checks the
two bridges, so a test does".

### How presenting works in 26.1

26.1 has no `GpuSurfaceBackend` (that arrives in 26.2). `Minecraft` calls
`mainRenderTarget.blitToScreen()`, which routes to `CommandEncoderBackend.presentTexture`. That is
where `WgpuSurface` acquires the swapchain image, blits the colour view into it and presents.
`GpuDeviceBackend.presentFrame` is a no-op safety net, because a wgpu surface cannot be presented
with `glfwSwapBuffers`.

The swapchain format, present mode and alpha mode are negotiated from
`Surface::get_capabilities` rather than assumed. The format is a non-sRGB 8-bit one where the
driver offers it (`Bgra8Unorm`, else `Rgba8Unorm`), because Minecraft's main render target already
holds display-referred colour and an sRGB swapchain would encode it twice. The blit is built for
whatever format won, so the two cannot disagree; it is this backend's own `PresentBlit` rather than
`wgpu::util::TextureBlitter`, because it also has to turn the image over - see "Widget backgrounds
were missing because render targets are the other way up".
`desired_maximum_frame_latency` is 2, which is what DXGI's flip model and Vulkan's default both
expect.

A swapchain that goes stale is recovered in Rust: `acquire_next_texture` reconfigures and retries
once on `Outdated`/`Lost` instead of dropping the frame forever. DX12 reports those after a
monitor change or a fullscreen toggle far more readily than Vulkan does, and the Kotlin side only
reconfigures on a size change, so this cannot be left to the caller.

## Choosing the graphics backend

wgpu renders through one API per `Instance`. Two are wired up: **Vulkan** (the default) and
**DirectX 12**, which is Windows-only. The setting is owned by the native renderer, so both the
options screen and the config file edit the same value.

- **In game**: Video Options -> the **Electrum** tab -> `backend`.
- **On disk**: `config/wgpu-mc-renderer.json` in the game directory:

  ```json
  "backend": { "type": "enum", "selected": 0 }
  ```

  `selected` indexes the variant list in declaration order, so `0` is Vulkan and `1` is
  DirectX 12. `config/fabric/wgpu-mc-renderer.json` is still read if it exists, because that is
  where the config used to live and NeoForge never creates the `fabric/` directory.

The options screen is generated from the schema Rust serialises (`getSettingsStructure`), so the rows
on the Electrum page are exactly the settings the renderer has - `backend`, `vsync`, and the debug
switches under their own heading - and adding one in `settings.rs` is enough to make it appear. The
three placeholder settings the schema used to ship (`test_enum`, `test_float`, `test_int`) are gone,
along with the `TestEnumSetting` enum they needed; a config file that still has them keeps loading,
because every field is `#[serde(default)]` and unknown keys are ignored.

### Which settings need a restart, and which do not

`backend` does, and nothing can change that: the wgpu instance is created for exactly one backend,
and the adapter, device, queue and every texture and buffer below them are built from it - none of
that can be replaced while the game runs. It is declared `needs_restart: true` in
`rust/wgpu-mc-jni/src/settings.rs`, and three things in the UI follow from that flag:

- the setting's tooltip gains a red `* Requires restart` line;
- as soon as a restart-only setting is edited, a red *"Restart Minecraft to apply the marked
  settings"* notice appears above the Apply button, so it is visible without hovering;
- after Apply the notice stays up in the past tense, because otherwise the only signal that the
  restart is still pending would vanish at the moment the change was committed.

Applied settings are written to disk by the Rust side (`sendSettings`), which is what makes the
restart actually pick the new backend up.

**`dynamic_offsets` now asks for one too, and it is the case that shows why the flag has to be the
honest one.** It was declared `needs_restart: false` and was live - the switch decides whether a
uniform offset travels with the draw or is baked into the bind group it is bound with, and the JVM
side mints a *number* for a set of bindings which is what the bind group cache is keyed by. Which
of those two a binding is decides whether the offset is part of that number, so flipping the switch
between two draws of one frame leaves the pass holding bind groups built for the other rule - and
the game died the first time anyone flipped it in a running world. The switch is now latched at the
first read (`set_dynamic_offsets` in `rust/wgpu-mc-jni/src/debug.rs`, which also warns and ignores a
later change) *and* declared `needs_restart: true`, so the UI says so before anyone tries. Off is
also the slow setting - a bind group per distinct offset rather than per binding set - which is why
the tooltip says it is for diagnosis rather than for play.

**`vsync` does not need one, and no longer asks for it.** It only chooses the swapchain's presentmode, and a surface can be reconfigured whenever the game likes - this backend already does it on
every resize and whenever the swapchain goes stale. `sendSettings` therefore re-resolves the mode
from the settings it has just stored and reconfigures the surface if it changed
(`reapply_present_mode` in `device.rs`); `configure_surface_inner` compares the present mode along
with the size, so reconfiguring at an unchanged size still applies the new one. In a world, a
toggle moves the swapchain between `Fifo` and `Mailbox` with no restart and no dropped frame the
player can see:

```
wgpu-mc: swapchain 2560x1334, Bgra8Unorm, Mailbox, alpha Opaque, …
wgpu-mc: swapchain 2560x1334, Bgra8Unorm, Fifo,    alpha Opaque, …   ← vsync turned on
```

**Minecraft's own VSync option is deliberately inert here, hidden, and kept in step.** On the
OpenGL backend it is `glfwSwapInterval`, whose only equivalent on this side is the present mode -
which the renderer's own setting owns, because that is the one the player sees in the Electrum tab
and the one that applies without a restart. Letting both drive it would mean two owners of one
value: Minecraft calls `GpuDevice#setVsync` from `Window#updateVsync`, which runs at startup
(`Minecraft`'s constructor) and on every fullscreen toggle, so the vanilla toggle would silently
undo the player's choice on the next launch. So:

- `WgpuSurface.setVsync` accepts the call and does nothing with it;
- the vanilla toggle is not in the Video Settings list any more, rather than sitting there changing
  nothing;
- its *value* is still synced from ours - on apply, and once when the device is created - because it
  is not private to this mod. The F3 overlay prints "vsync" from `options.enableVsync()`
  (`DebugEntryFps`), and other mods read it to know whether frames are being synced; a stale value
  there would make both lie. The sync runs on the render thread, at device creation: changing the
  option runs Minecraft's own consumer, which asserts it is on that thread, and a mod-loading
  worker is not.

### Apply sent nothing, because the page was recognised by its label

An edit on the Electrum page is persisted by sending the whole page to the renderer as one JSON
document (`sendSettings`), which is also what applies the settings that can be applied live. Which
page does that used to be decided *inside* `Page.apply()` by comparing the page's name to a literal:

```kotlin
if (name.string == "Electrum") { … sendSettings(…) … }   // before
```

`Page.name` is a `Component`, and once the page labels moved into the language files it resolves to
the text of `wgpu_mc.page.electrum` - which is `Neolectrum`, in every language, and has been since
before the localisation work. The comparison therefore stopped matching, `apply()` fell through to
the branch meant for the vanilla pages, and every row on the page applied itself to a variable that
only the screen can see. Nothing was sent, nothing was written, and the failure was invisible in
both directions:

- no error, because the fall-through branch is a legitimate one for a page without renderer
  settings;
- no *pending* edit left behind either - `Option.apply()` had committed the value on this side, so
  the Apply button turned back into Close and the screen looked like it had saved something.

The next launch then read the old value out of `config/wgpu-mc-renderer.json`, which is exactly what
"the setting does not apply, and restarting puts it back" looks like from the outside. Every setting
on the page was affected, `backend` included - a switch that needs a restart anyway, and so hid the
bug for as long as the page's label happened to be the literal `Electrum`.

A page holds the renderer's settings exactly when one of its rows carries a setting name, and the
renderer is the side that named them, so that is what decides the branch now. It cannot go stale
when a label is renamed or translated. `cargo test` reads `OptionPages.kt` with `include_str!` and
fails if a page is recognised by `name.string` again
(`the_renderer_settings_page_is_not_found_by_its_label`, in `settings.rs`) - the same shape as the
tests that keep the language files and the two native bridges in step, and the only kind of test
this can have: the branch is Kotlin, and the failure was silent rather than loud.

The same launch also says what it loaded, because the renderer's own line about it is dropped: the
config is read during mod construction, and `env_logger` is installed later, by `setPanicHook` - so
`Loaded settings: …` went to a logger that did not exist yet. `WgpuMcModClient` reads the same
document back once the bridge is up:

```
wgpu-mc renderer settings as loaded: {"backend":{"type":"enum","selected":1},"vsync":{…}}
```

### The options are localised, and a missing translation is a test failure

The settings screen is built from the renderer's schema, which names a setting (`vsync`) and
describes it in English. Neither of those is what a player reads. The screen turns the name into the
key `wgpu_mc.option.<name>` - and the description into `wgpu_mc.option.<name>.tooltip` - and looks
both up in `assets/wgpu_mc/lang/`, so the wording lives where a resource pack or a translator can
reach it:

| Key | Text (en_us / zh_cn) |
| --- | --- |
| `wgpu_mc.screen.video_options` | Video Options / 视频设置 |
| `wgpu_mc.page.general`, `.electrum`, `.quality` | General, Electrum, Quality / 常规, Electrum, 画质 |
| `wgpu_mc.button.apply`, `.close`, `.undo` | Apply, Close, Undo / 应用, 关闭, 撤销 |
| `wgpu_mc.section.debug` | Debug / 调试 |
| `wgpu_mc.tooltip.requires_restart` | `* Requires restart` / `* 需要重启` |
| `wgpu_mc.notice.restart_pending`, `.restart_applied` | the two red restart notices |
| `wgpu_mc.option.<setting>` | the setting's name |
| `wgpu_mc.option.<setting>.tooltip` | the setting's description |
| `wgpu_mc.option.backend.vulkan`, `.directx12` | the values of an enum setting |

Two things make this survive a setting being added. The first is that a name no language file has is
still shown as *something*: the screen asks for `translatableWithFallback`, so an untranslated name
falls back to the schema key made readable (`bind_group_cache` as "Bind group cache") and an
untranslated description to the English the renderer sent with it. The second is
`cargo test`: three tests in `settings.rs` pull both language files in with `include_str!` - so
editing one re-runs them - and check that every setting in the schema has a name in `en_us` *and*
`zh_cn`, that every value of an enum setting does, and that neither file defines a key outside this
mod's namespace. Adding a setting without a translation fails the build's tests rather than quietly
shipping a row called `dump_shaders`.

The renderer's own spelling is kept beside the translated one (`Option.setting` next to
`Option.name`), because three places still need it: the JSON sent back to the renderer is a config
file and is keyed by `vsync`, not by `wgpu_mc.option.vsync`; the schema lookup that puts a setting
under its heading uses it; and so does the check that keeps vanilla's own vsync option in step. The
values of an enum setting travel as a key *and* a display name for the same reason: the key is what
a translation is found under, the display name is what a language that has never heard of the
setting - or a language file that is missing the key - falls back to.

en_us carries the names and the screen's wording; the descriptions are the renderer's English and
reach it as fallbacks, so only a language that wants to say something different needs a
`*.tooltip` entry. That is why `zh_cn.json` has all eight of them and `en_us.json` has none.

Vanilla's half of the screen (General and Quality) was already localised - the option names are
vanilla keys such as `options.renderDistance` - with two exceptions that are now fixed:

- the GUI scale row's "Auto" is vanilla's own `options.guiScale.auto` rather than a literal;
- the graphics preset row asked for `options.graphics`, which **26.1 renamed**. The old key is in
  `assets/minecraft/lang/deprecated.json`'s `removed` list, and `DeprecatedTranslationsInfo`
  *strips* removed keys from every language file as it loads them - so the key resolved to nothing
  in every language and the row was drawn labelled with the key itself:

  ```
  视频设置 → 画质:   options.graphics      高品质      ← before
                     预设                  高品质      ← after (`options.graphics.preset`)
  ```

  A key that resolves to nothing is drawn as itself, and there is no compile error on either side
  of the lookup, so the screen names them now: `OptionPages` walks every name, description and
  heading it built once per session, checks each `TranslatableContents` against `I18n.exists`, and
  logs the ones that are missing - skipping any that carry a fallback, since falling back is what
  this mod's own descriptions do on purpose. That line is what found the `options.graphics` bug:

  ```
  wgpu: 1 key(s) the options screen asks for are not in the loaded language, so those rows show
        the key itself: options.graphics
  ```

The F3 overlay's `Render backend:` line is deliberately still English, because vanilla's debug
overlay is English throughout.

### A vanilla option is adjusted through the option, not through a range this side guessed

The General and Quality pages are vanilla options, and three of them could not be adjusted at all.
Each row was a slider over a range written out here by hand, and a range written by hand is a guess
about somebody else's setting:

- `framerateLimit` is stored as `1..26` and shown as `10..260`, so vanilla's slider only ever
  produces multiples of ten. This side offered `5..260` in fives, so a click at the far left asked
  for **5** - and `OptionInstance#set` answered `Illegal option value 5 for 最大帧率` in the log,
  put the option back to its initial value (120) and left the row showing the 5 that had been asked
  for, with the Apply button still lit;
- `simulationDistance` goes to 32 on a machine with the memory for it (the heap size decides), and
  this side capped it at 16;
- `guiScale`'s maximum depends on the size of the window (`ClampingLazyMaxIntRange`), which no
  constant can express.

The option itself knows all of it, so it is asked. `OptionInstance.SliderableValueSet` - where its
slider mapping lives - is package-private in `net.minecraft.client`, so `IntSlider` asks the public
question instead: `validateValue` over `0..1024`, which answers with exactly the values the option
accepts, and the step falls out of them. A click between two accepted values snaps to the nearer one,
so the frame-rate row now offers 10, 20, ... 260 and nothing else.

Applying had a second way to lose a change, and it was the interesting one:

```
wgpu: applied 1 video option(s): 模拟距离=30      ← the row was applied …
wgpu: applied 1 video option(s): 预设=FANCY       ← … and then the preset row was, too
options.txt: simulationDistance:12                 ← GraphicsPreset.FANCY sets it back to 12
```

Editing any individual option is what makes the *graphics preset* row differ from its setting -
every one of those options calls `setGraphicsPresetToCustom` - so "the value differs from the
setting" was true for the preset as well, and the page applied it: `GraphicsPreset.FANCY.apply` sets
a dozen options, the one that had just been edited among them. A row is now applied when the
**player** edited it (`Option.edited`, set by the widgets, cleared by apply and undo) rather than
when its value happens to differ, and every other row is read back afterwards so the screen goes on
showing what the game has.

Two smaller pieces belong with that. `Options#save` is called on apply and on close - which is what
`OptionsSubScreen#removed` does, and nothing here did it, so a change lived in memory until the game
exited *cleanly*; this game is usually killed, and the next launch came back with the old value. And
each apply says what it did, because the alternative is a screen that looks like it saved something
while the log disagrees:

```
wgpu: applied 1 video option(s): 模拟距离=30
```

One thing here is not a bug, so that nobody goes looking for it: `simulationDistance` takes effect
when the world is loaded, not while it is running. The integrated server reads the option once, when
it is constructed (`IntegratedServer`), and vanilla has no live path for it either - the value is
what the *next* load of that world uses.

### The page is as wide as the display, not as the window

The page's width limit was the constant `2000` in pixels, which is one particular monitor's width
written down: on a smaller display the rows ran past the edge of the window, and on a wider one they
stopped short of the space there was. It is now the monitor's own width times `0.809`, rounded down
(`OptionPageScreen.WIDTH_SHARE_OF_MONITOR`), which is the same page relative to any display.

Three details are deliberate:

- **The monitor, not the window.** Laying the page out from the window makes every row a different
  width while the window's edge is dragged, and a window that is half the screen gets a page that is
  half the size on a display with room for all of it. The caller still clamps the result to the
  window in GUI units, so a window smaller than its display is the bound that applies.
- **The monitor's widest video mode, not the one it is running at.** A display driven below its own
  maximum can still show the page at full size, and GLFW lists a monitor's modes sorted by colour
  depth first - so the maximum is taken rather than assumed to be the first or the last entry.
- **It follows the window.** `findBestMonitor` walks GLFW's monitor list and compares rectangles, and
  the limit is read from `alignX`, once per widget - so the monitor is only re-resolved when the
  window's position says it moved, and a page that is open when the window is dragged to another
  display is laid out again for that display. A window with no monitor to name keeps the fallback
  width and says so once.

The resolved limit is logged when it changes, which is also how the layout can be checked without a
screenshot:

```
wgpu: the video options page is limited to 2071 pixels on a 2560 pixel wide monitor
```

### Falling back

If the configured backend cannot be created, the other one is tried before giving up, and the log
says so at error level. A backend named in the config but unusable on the machine - a DX12 config
carried over, or a driver with no Vulkan ICD - would otherwise leave the game with no renderer at
all and no way to reach the options screen that would let the player change it back. If neither
backend comes up, `createDevice` throws `BackendCreationException`, which turns into Minecraft's
own "No supported graphics backend was found" screen rather than a crash later on.

To see which backend the *running* renderer ended up on, open the F3 overlay: the `Render backend`
line reports the adapter wgpu selected, which is not necessarily the one that was requested.

### What the F3 overlay says about the graphics card
Vanilla's system block (`DebugEntrySystemSpecs`) asks a `GpuDevice` for a vendor, a renderer name, a
backend name and a version. On the OpenGL backend those four answers are `GL_VENDOR`, `GL_RENDERER`,
"OpenGL" and `GL_VERSION` - the graphics driver naming itself - and the port answered "wgpu" and
"wgpu-mc" for all four, which made the block useless for exactly the kind of report this mod needs.

`WgpuDevice` now answers with the adapter's own `AdapterInfo` (over the `WgpuNative.getAdapterInfo`
JNI call), so the block looks like it does on GL:

| Accessor | Source | Example |
| --- | --- | --- |
| `getVendor()` | the PCI vendor id, named | `NVIDIA` |
| `getRenderer()` | `AdapterInfo#name` | `NVIDIA GeForce RTX 5070 Laptop GPU` |
| `getBackendName()` | the API behind wgpu | `Vulkan`, `DirectX 12` |
| `getVersion()` | `driver` + `driver_info` | `NVIDIA 617.14`, or `32.0.16.1714` on DX12 |

The fourth line joins `driver` and `driver_info` because the two backends fill them in differently:
Vulkan reports the driver's name and version, DX12 puts the version in `driver` and leaves
`driver_info` empty. Either way the F3 overlay names the installed graphics driver, which is the
thing a rendering bug gets reported against.

The wgpu wording did not disappear, it moved: `DebugEntrySystemSpecsMixin` appends
`Render backend: wgpu 29.0.3 (vulkan)` *into the same group*, so it is drawn directly under the
vanilla lines instead of at the bottom of the column. In a full F3 column that block reads:

```
Java: 25.0.3
CPU: 32x Intel(R) Core(TM) i9-14900HX
Display: 854x480 (NVIDIA)
NVIDIA GeForce RTX 5070 Laptop GPU
Vulkan NVIDIA 617.14
Render backend: wgpu 29.0.3 (vulkan)
```

which is what `wgpu: F3 system block:` prints in the log when the diagnostics are on - the block
sits below the profiler section and is usually off the bottom of the window, so a screenshot is not
a way to check it.

### Prerequisites and platform notes

- DX12 needs a D3D12-capable adapter and a shader compiler. wgpu uses DXC when `dxcompiler.dll`
  is present and falls back to FXC otherwise, so a missing DXC costs shader model 5.1 features
  rather than the backend.
- Vulkan needs a working ICD. `WGPU_POWER_PREF` overrides the high-performance adapter default,
  which matters on hybrid-graphics laptops where the two backends may enumerate the same GPU
  differently.

### The Rust side has a lint gate, and it is the one CI runs

Nothing Rust-side counts as finished until it passes the same two commands
`.github/workflows/rust-check.yml` runs - from `rust/`, on the toolchain `rust-toolchain.toml` pins
(`nightly-2026-09-26`, so a workstation and the runner agree):

```sh
cargo fmt --all
cargo clippy --all-targets -- -D warnings
```

`fmt` is not cosmetic here: rustfmt is free to reflow between nightlies and clippy to grow lints, which is
why the toolchain is a *dated* nightly rather than `nightly` - the pin is what makes a red job say something
about the commit instead of about the day. `--all-targets` is the part that catches test code, which is where
the lints in this tree have actually come from (a `doc_lazy_continuation` in a new doc comment and two
bindings a rewritten loop stopped using).

Two things the gate does *not* cover, and both are noise rather than debt: this tree's vendored GLSL
preprocessor (`rust/cyntax`, its own workspace, reached as a path dependency) has six clippy warnings and
four of its own, and `cargo clippy -- -D warnings` only applies that flag to the workspace members - so they
print and do not fail. The `unused dependency` lines under them are cargo's manifest lint, not clippy's, and
`-D warnings` does not reach those either.

## What has actually been run

Everything below was observed by launching `:wgpu-mc-neoforge:runClient`, not inferred:

- The backend mixin applies and `Minecraft` calls `WgpuBackend.createDevice` from its constructor.
- `wgpu_mc_jni.dll` loads, the renderer comes up on Vulkan, and the log line is
  `wgpu-mc backend initialised through wgpu 29.0.3 (vulkan)`. The crash report's `Backend API`
  field reads `wgpu 29`, and the process has `vulkan-1.dll`, `igvk64.dll` and `RTSSVkLayer64.dll`
  mapped in.
- The window is created with `GLFW_NO_API`: `Window` receives the `WgpuBackend` as its
  constructor argument, so `setWindowHints` never asks for a GL context.
- Resource reloading runs to completion, every pipeline the game precompiles is accepted, and the
  renderer draws whole screens and presents them, on **both** Vulkan and DirectX 12: the title
  screen (panorama, logo, splash text, every button with its background, both corner strings), the
  vanilla options screen, and the video options screen on top of a loaded world, which exercises
  the sprite atlases, the blurred background and the GUI item atlas at once. The renderer's own
  dumps were compared against the swapchain image and match byte for byte.
- The in-game HUD and the world it is drawn over come from the same run: terrain, sky and the
  scenery behind the options panel are drawn by this backend.
- Uploaded textures were checked byte for byte against the resource pack's own files: the uploaded
  panorama cubemap faces match `panorama_1.png` with `mad=0.00` and no channel permutation. The
  title screen's sky is blue and its cherry blossoms are pink in the presented frame, not orange and
  purple.
- The F3 overlay's system block names the installed graphics driver (`NVIDIA GeForce RTX 5070
  Laptop GPU` over `Vulkan NVIDIA 617.14`, observed on both Vulkan and DirectX 12) with the wgpu
  line under it, and the adapter it reports agrees with the `backend` setting. A run on the DX12
  backend prints, from the mixin that reads vanilla's own group back:

  ```
  Java: 25.0.3
  CPU: 32x Intel(R) Core(TM) i9-14900HX
  Display: 2560x1334 (NVIDIA)
  NVIDIA GeForce RTX 5070 Laptop GPU
  DirectX 12 32.0.16.1714
  Render backend: wgpu 29.0.3 (dx12)
  ```

- A world was created and played: terrain, sky, clouds, the block atlas, the held item and the
  hotbar all drawn, over 5280 presented frames with `acquired=true` on every one of them. The
  window was resized three times before entering the world and maximised to 2560x1334 with the
  world loaded, which is the path that rebuilds a render target and used to end the process.
- The packaged jar was also started the way a player starts it: a scratch instance whose `mods`
  folder holds `wgpu_mc-<version>-all.jar` and nothing else, with no `config/fml.toml`, so FML's
  early loading screen is on - which is what a fresh instance has. It reached the title screen,
  created a world, presented frames, and exited cleanly; the same jar was then run beside
  KotlinForForge 6.3.0 with the same result. This is the run that covers both fixes above, because
  neither the missing stdlib nor the early loading screen is reachable from `runClient`.

### Reading a frame without a GPU debugger

When a frame comes out wrong there is nothing to attach a debugger to, and the two halves of the
problem - "the renderer drew nothing" and "the renderer drew something nobody presented" - look
identical from outside the process. `dev.birb.wgpu.backend.Diagnostics` is the switch that makes
the backend answer those questions, and it is a setting now: **Video Options → Electrum → Debug →
Diagnostics**, applied on the next frame.

```powershell
# or, with no launcher support and no trip through the menus
New-Item neoforge/runs/client/wgpu-dump-frames
```

With it on, every `runClient` writes, into `neoforge/runs/client/wgpu-frames/`:

| File | What it is |
| --- | --- |
| `frame-<n>-source.raw` | the main colour target at present *n*, turned the right way up |
| `frame-<n>-surface.raw` | the swapchain image that was actually presented |
| `tex-<label>-layer<l>.raw` | a texture right after it was uploaded |
| `atlas-<label>.raw` | a sprite atlas, right after the pass that builds it was submitted |

The frames are 2, 300, 900, 1800, 3000, 4200, 5400, 6600, 7800 and 9000, spread out because the
first ones are the loading splash and a world is only reached a minute or two in. A dump can also be
asked for at any moment, which is what makes "look at the frame I am looking at right now" possible:

```powershell
New-Item neoforge/runs/client/wgpu-dump-now   # dumps the next presented frame, then deletes itself
```

Each file is `u32 width`, `u32 height`, then RGBA8 rows; the
swapchain image is `Bgra8Unorm`, so its bytes are BGRA. A render target is written out turned over,
because that is how the backend holds it - see the clip-space section below - so `frame-<n>-source`
and `frame-<n>-surface` are directly comparable and are byte-identical apart from that swizzle.

An atlas has to be dumped through the pass that builds it rather than through an upload, and only
after the pass has been submitted; the first pass for a label is the one that counts, because that
is the level-0 pass and everything after it is a mip level. A `tex-` dump is the only way to tell a
texture that arrived wrong apart from a shader that samples it wrong: compare it against the
resource pack's own PNG under every channel permutation and flip, and exactly one combination
should come out at `mad=0.00` with the identity permutation among the channels - see "A colour
coming out of a texture is a texture problem". `frame-<n>-source.raw` is flipped row-wise on the
way out, because a render target holds the frame the OpenGL way round.

The same switch turns on the per-pass trace
(`wgpu-mc: pass -> <label>: N draws, clear=..., depth=..., pipelines [...]`), the one-shot
pipeline and render-target descriptions, the present/stats counters, and the line that found the
twenty-gigabyte leak:

```
wgpu-mc: live resources: 767 textures (74 MB), 786 views, 47 buffers (0 MB, 5 quarantined),
         1 encoders, 0 passes, 0 bind groups, 0 builders
```

`-Dwgpu_mc.diagnostics=true` and `WGPU_MC_DIAGNOSTICS=1` both turn the dumps on, and so does the
**Diagnostics** option on the Neolectrum page. The option is the one to use: it is visible, it is
saved with the rest of the settings, and it can be turned off again.

### The debug switches are settings, and the marker files are gone
Every diagnostic in this renderer grew as a marker file - a file dropped next to the game - which is
the right shape for a switch that has to work without a launcher and the wrong one for a player who
wants a single frame dumped. They are options on the **Neolectrum** page now, under two headings the
schema names - **Optimization** first, then **Debug** - separated from the backend and vsync by a blank
row, and **the files no longer do anything**:

> A run that still has `wgpu-logging` or `wgpu-dump-frames` in its directory takes the setting's
> answer, because the setting is the only source.

That is the point of the move rather than a side effect of it. The files were also a trap: four of the
switches were spelled as the **negation** of the setting they shadowed (`wgpu-no-bind-group-cache`,
`wgpu-no-dynamic-offsets`), so the file was resolved as `setting && !marker` - which means a file
*laying around* overrode the option and the option could not override the file. A file nothing in the
game can show you, that has no way to be turned off from inside the game, and that wins over the setting
is not a fallback; it is a second source of truth with no interface.

| Setting | Section | Default | Applies |
| --- | --- | --- | --- |
| Bind group cache | Optimization | on | next frame |
| Dynamic offsets | Optimization | on | next frame |
| GPU-based validation | Debug | off | next launch - it is a `wgpu::InstanceFlags` bit, and the instance is created once |
| Logging | Debug | off | next frame |
| Diagnostics | Debug | off | next frame |
| Trace dynamic offsets | Debug | off | next frame |
| Dump shaders | Debug | off | next frame |
| Binding resolution log | Debug | off | next frame |
| GPU timestamps | Debug | off | next frame |
| PIX capture | Debug | off | next frame - see "The GPU's own clock, and a PIX capture" |
| Terrain without back-face culling | Debug | off | next frame, and it rebuilds the graph's pipelines |
| Terrain with the depth test reversed | Debug | off | the same - it is the other half of that pair |

**Logging and Diagnostics are two switches, not one.** They were one, which meant a run that wanted a
frame written to disk also got a line per pipeline, a line per pass and a line a second of counters -
and, worse, several of those lines had no switch at all and were printed in every run: the sprite
animation counter ("`N` sprite animation passes in the last second") and the reports that verify a
mapped write arrived. `Logging` is what writes lines, `Diagnostics` is what writes files, and the
schema says which is which. Measured with both off, none of it runs: no counters, no per-pipeline
lines, no pass dump, no frame or atlas dump. `Diagnostics` is not a leftover name - it is the switch
the frame and texture dumps have always hung on, and it still does.

**Bind group cache and dynamic offsets moved under `Optimization`** because they are not diagnostics.
Both are on by default, turning either off costs a `wgpu::BindGroup` per draw, and the diagnostic that
comes out of them is the *frame time* - they were written to bisect a rendering bug, which is why they
lived under `Debug`, and that is exactly the wrong shelf for a switch a player is meant to leave alone.

**A row's order comes from the settings document and its heading from the schema, so the two field
orders have to agree.** They did not at first: the two optimisations were declared after the debug
switches in `Settings` and marked `Optimization` in `SettingsInfo`, and the page drew `Debug`, then
`Optimization` over the last two rows of it, then `Debug` again - one section, two headings, and an
"Optimization" that looked like it belonged to the debug switches below it. The fix is the field order
in `Settings` (which is the config file's order, and what `getSettings` serialises for the page),
`the_config_and_the_schema_list_the_settings_in_the_same_order` is what keeps the two in step, and
`the_sections_are_contiguous_and_optimizations_come_first` is what keeps `Optimization` above `Debug`
rather than in the middle of it. Both read the serialised *text*: a `serde_json::Value` sorts its
keys, which is the one thing about these documents the tests are asking about.

The section is what made the settings list taller than the window at the GUI scales a small window
allows, so the list scrolls with the wheel now instead of running under the Apply button - which is
also what the General page's last row needed, and it is why a row is only drawn when it fits whole.

The **tooltip is sized and placed the same way**, and for the same reason. It used to be as wide as
the row it belonged to and always started at that row's lower edge, so hovering one of the last rows
put it under the Apply button and off the bottom of the window, with the rest of the description
simply gone - and the descriptions are what the debug switches are documented in. It is now laid out
from its own content (`TooltipWidget`): the width is what the text asks for, capped by the list's
width and by 320 GUI units, the height follows the wrapped text, and it is drawn *below* its row when
there is room for it and *above* it when there is not. What it may cover is the band the rows
themselves live in - `confineTo` is given the top of the button row as its bottom - and the text is
clipped to the box, so a description too tall for that band ends at the box's edge rather than over
the buttons. The one line that never gives way is the red `* Requires restart`, which is drawn at the
box's bottom edge and has the paragraph clipped above it:

```
                                      ┌──────────────────────────────────────────────┐
   图形后端   DirectX 12    ← hovered │ wgpu 使用的图形 API。Vulkan 在 Windows 和 …   │
                                      │ … 所以切换要等到下次启动才生效。             │
                                      │ * 需要重启                                   │
                                      └──────────────────────────────────────────────┘
                                    ↑ stops here; 关闭 / 应用 / 撤销 are below and stay visible
```

A row with no description now draws no box at all: `Option.tooltip` is never null - an option without
one carries an empty component - so the vanilla pages used to show an empty rectangle under the row
the mouse was over.

`GPU-based validation` is the one that changed behaviour rather than moving: the instance used to be
created with it unconditionally, so every launch paid for a driver validation layer that only a
renderer being debugged wants. Host-side `VALIDATION` and `DEBUG` stay on always - they are what
makes a wgpu error name the call that caused it.

The schema says which settings belong to which heading (`"section": "Optimization"` or `"Debug"`), so
the options screen draws both headings without knowing what any of them do, and the Rust side resolves
them into atomics when the settings are loaded or applied: the draw path reads a flag, never a config
file or a lock. **The marker files those settings used to shadow are gone**, including the two that were
spelled as the *off* switch - see the section above for why a file that wins over the setting is worse
than no file at all.

The last two are the exception that proves the rule, because a draw path is not where they are read.
A cull mode and a depth compare are built *into* a pipeline, so `apply` hands them to the crate that
creates the pipelines (`set_pipeline_diagnostics`, in `wgpu-mc`'s graph) and that call reports whether
either of them moved - and if it did, the graph is built again at the end of the frame that follows,
from `present_surface`. That is the one point a frame is known to be finished with the graph: rebuilt
between two of a frame's passes, half of that frame would be drawn one way and half the other. Both
apply to every pipeline the graph builds rather than to the terrain one alone, because that is where
the markers were read when they were files - the names are the bug they were written for.

That is what made the gating worth doing at all. Three of these were *unconditional* work on the hot
path before:

- `log_pipeline_once` locked a mutex and hashed a name into a `HashSet<String>` on **every pipeline
  bind**. It is one `AtomicBool` on the `BlazePipeline` now, and the name is not read again after
  the first bind reports it;
- `trace_draw` locked the pass trace on **every draw**, to increment a number only a log line reads;
- `trace_pipeline` did the same on every pipeline bind, and the per-pass trace formatted the target
  address on every pass.

All three are behind the logging flag, so a normal run pays one relaxed load per draw for them
- and the counters behind `log_render_stats` are thread-local `Cell<u64>`s rather than process-wide
atomics, aggregated when the stat line is written. `LIVE_*` stays atomic, because those are
decremented by the cleaner thread, and a second recording thread would be a real bug: the counter
block is per thread, so `log_render_stats` logs a warning once if more than one thread ever counts.

The counters read the same whether the switch is on or off, which is the point: they are a cell
increment, not a diagnostic.

### The GPU's own clock, and a PIX capture

Two of the debug switches measure or record the GPU itself rather than the renderer's own work.

**`GPU timestamps`** measures how long each presented frame takes *on the GPU*, which is the one
number no amount of CPU instrumentation can produce. It writes a timestamp at the start of the
frame's first submission and another at the end of its last one - the start goes into the fresh
encoder the frame's first flush leaves behind, the end into the blit that closes the frame - and
reads the pair back a couple of frames later, once the mapping of its readback buffer completes. The
render thread never waits for the GPU: the results arrive through `map_async` when they arrive:

```
wgpu-mc: gpu frame time: 5.22 ms average over 9240 frames (last 4.50 ms, worst 3741.82 ms)
```

The `worst` is a startup frame - the loading screen to a world, where the CPU stalls between two
submissions that a timestamp pair brackets - while the average is what a frame really costs (5.2 ms
of GPU work while the frame rate was around 120, DX12, vsync off). The queries need
`Features::TIMESTAMP_QUERY | TIMESTAMP_QUERY_INSIDE_ENCODERS`; both are requested when the adapter
has them, and with the switch off not a single timestamp is written, so a disabled switch costs
nothing.

The first version of this crashed the game, which is worth writing down: a resolve's destination
offset has to be aligned to `QUERY_RESOLVE_BUFFER_ALIGNMENT`, and the frames' pairs were 16 bytes
apart. wgpu reported *"Resolve buffer offset has to be aligned to QUERY_RESOLVE_BUFFER_ALIGNMENT"*,
and a validation error on the render thread ends the process. Each frame now gets its own 256-byte
slice of the resolve buffer, of which the first 16 bytes are used.

**`PIX capture support`** loads PIX's own libraries into the game, which is what both ways of using
PIX need - attaching to the process *and* taking a programmatic capture:

- `WinPixGpuCapturer.dll`, so PIX can attach at all. It hooks D3D12 as it is loaded, and PIX refuses
  to attach to a process that loads it *after* its device exists: *"the process has not loaded
  WinPixGpuCapturer.dll"* is the message PIX gives, and it is why this switch needs a restart and
  why the load happens before `wgpu::Instance::new`. With it loaded, PIX draws its own HUD over the
  game (`GPU: CaptureTaken 0, Frame Time 8 ms`), which is how a screenshot proves it is live.
- `WinPixTimingCapturer.dll`, which a programmatic timing capture runs through.

For the capture itself the switch calls `PIXBeginCapture(PIX_CAPTURE_TIMING, ...)` - the documented
call, and the reason the parameters struct is written out by hand in `rust/wgpu-mc-jni/src/pix.rs`:
`pix3.h` is not part of any SDK this crate builds against, so the layout is spelled out from
Microsoft's documentation rather than included. Everything is looked for in the newest PIX
installation, and nothing is linked against:

- `WinPixTimingCapturer.dll`, from PIX's own installation (`C:\Program Files\Microsoft PIX\<version>`,
  newest version first); a timing capture needs the capturer *in the process*, which is what
  `PIXLoadLatestWinPixTimingCapturerLibrary()` does in `pix3.h` - a header function, so the search
  is written out here;
- the event runtime, where `PIXBeginCapture` itself lives: `WinPixEventRuntime.dll` beside the game
  or on `PATH` if it is there, otherwise PIX's own `WinPixEventRuntime_OneCore.dll`, whose
  `PIXBeginCapture2` is documented as equivalent.

Nothing is linked against, because most machines have no PIX and a missing import would stop the
mod loading at all. On a machine with PIX installed the log says what it found, and what PIX said:

```
wgpu-mc: PIX: C:\Program Files\Microsoft PIX\2603.25\WinPixGpuCapturer.dll loaded, so PIX can attach for a GPU capture
wgpu-mc: PIX: C:\Program Files\Microsoft PIX\2603.25\WinPixTimingCapturer.dll loaded, so programmatic timing captures can start
wgpu-mc: PIX: using C:\Program Files\Microsoft PIX\2603.25\WinPixEventRuntime_OneCore.dll
[WinPixServices]: Starting
Starting ETW session PixSysMonSession.…
wgpu-mc: PIX refused to start a timing capture (HRESULT 0x80070005). A programmatic timing capture
         needs the game to run elevated (PIX asks for administrator for timing captures), PIX's
         WinPixTimingCapturer.dll loaded in the process, and no capture already running
```

`0x80070005` is `E_ACCESSDENIED`, and it is the one requirement this side cannot satisfy for you:
PIX's documentation asks for the game to run **as Administrator** for a programmatic timing capture,
because the capture records through ETW providers and creating those sessions is refused without it.

**Starting `gradlew` from an administrator terminal does not make the game elevated.** Gradle reuses
a daemon that is already running, elevation is not part of the criteria it picks one by, and the game
is *forked by the daemon* - so it inherits the daemon's token. A run that looked like this, on the
machine this was written on, is what "I ran it as administrator and it still refuses" looks like:

```
pid 136512  java.exe  not elevated   GradleDaemon 9.4.1          started 10:49
pid 122192  java.exe  ELEVATED       (the elevated terminal's Gradle client)
pid 135472  java.exe  not elevated   parent = 136512   ← the game
```

The elevated client asked the *unelevated* daemon for the build, and the daemon forked the game. Both
of these fix it:

- `gradlew --stop` first, then `gradlew runClient` from the administrator terminal, so a new - and
  elevated - daemon is started; or
- `gradlew --no-daemon runClient`, which runs the build in a JVM forked by the elevated client.

The mod now answers the question itself, because the HRESULT alone cannot: `GetTokenInformation`
(`TokenElevation`) is asked once, the answer is part of the refusal message, and an unelevated launch
with the switch on says so before a capture is ever attempted:

```
wgpu-mc: PIX: this process is not running elevated, so the timing capture the `pix capture` switch
         takes will be refused … so run `gradlew --stop` first, or pass `--no-daemon`.
wgpu-mc: PIX refused to start a timing capture (HRESULT 0x80070005, E_ACCESSDENIED). This process is
         NOT elevated, and a timing capture needs administrator: …
```

Everything else in the sequence works without elevation: PIX's service starts, the ETW session is
attempted, and PIX can attach to the process for a GPU capture - the switch buys that on an
unelevated run too. What it cannot buy there is the programmatic timing capture. On success the log
reads:

```
wgpu-mc: PIX timing capture started, written to ...\wgpu-mc-capture-1.wpix
wgpu-mc: PIX timing capture stopped
wgpu-mc: PIX timing capture reached its 600 frames and stopped
```

The lines about the capturers are printed from the **first frame** rather than where they happen:
the loading is part of creating the device, and this crate's logger is only installed by the JVM
afterwards, so a line logged there would never be seen.

Two things about the call itself are worth knowing. It runs on a **thread of its own**: PIX's runtime
sets up COM as it loads, and from the render thread - whose apartment is already set - every call
answered `RPC_E_CHANGED_MODE` (0x80010106); a fresh thread has no apartment yet. And a capture is
**bounded to 600 frames** (about ten seconds), because a programmatic capture runs until it is
stopped and fills tooling memory measured in gigabytes; turning the switch back on takes the next
capture into the next numbered file. The begin/end pairing, the wide-string file name,
`flags=0x1` (the `PIX_CAPTURE_TIMING` bit) and `discard=0` were all checked against a stand-in
runtime that writes down what it was asked to do.

A capture is not only the GPU's clock. Every switch the capture API has is on:

| What it records | `TimingCaptureParameters` field | Table it fills in the capture |
| --- | --- | --- |
| GPU work and PIX GPU events | `CaptureGpuTiming` | `ApiQueueExecution`, `GpuApiMarkerRange`, `GpuWorkRange` |
| CPU samples, 4 kHz | `CaptureCpuSamples`, `CpuSamplesPerSecond` | `CpuSample` |
| Call stacks | `CaptureCallstacks` | `StackEvents`, `Stacks`, `ContextSwitchRange` |
| Win32 and DirectStorage file IO | `CaptureFileIO` | `FileIORange`, `FileInfo` |
| VirtualAlloc/VirtualFree | `CaptureVirtualAllocEvents` | `MemoryEventRanges`, `MemoryPairing` |
| HeapAlloc/HeapFree | `CaptureHeapAllocEvents` | `MemoryEventRanges`, `MemoryPairing` |
| Custom allocator events | `CapturePixMemEvents` | (nothing here: the renderer does not use PIX's allocator) |
| Page faults | `CapturePageFaultEvents` | `PageFaults` |

The first three were on from the start; the rest are what turned a capture with **no memory data at
all** into one with it. That is measurable rather than a claim - a `.wpix` is a **SQLite database**,
so its tables can simply be counted:

```
$ node -e 'const {DatabaseSync}=require("node:sqlite");const db=new DatabaseSync(process.argv[1],{readOnly:true});for(const t of ["MemoryEventRanges","MemoryPairing","PageFaults","FileIORange","ApiObjects","SymbolStrings"])console.log(t,db.prepare(`select count(*) n from "${t}"`).get().n)' runs/client/wgpu-mc-capture-1.wpix

                     capture-1               capture-2
                     (in a world,            (at the title screen,
                      memory events off)      memory events on)
MemoryEventRanges         0                     5397
MemoryPairing             0                   413569
PageFaults                0                   673866
FileIORange               0                      252
ApiObjects                0                        0      ← not an API option: see below
SymbolStrings             0                        0      ← needs a PDB: see below
```

The two captures are not the same scene - the first was taken in a world, the second during loading -
so the comparison says nothing about, say, `CpuSample`; it is about the tables that were empty
because nothing had asked for them, and the second capture is the quieter scene of the two.

`MemoryUsageSamples` itself stays empty in the file the game writes: it is *derived* by PIX from
those memory events (`MoveMemoryUsageSamples` in `pixstorage.dll`) when the capture is opened.

**Three of the things an instrumented capture can show are not reachable programmatically.** PIX's
timing-capture dialog has more options than its own capture API: `GPU resources` (the `ApiObjects`
tables - D3D12 resources, heaps, pipeline states, residency, demoted allocations), `Memory Access
Sampling`, `Kernel image information`, `Callstacks for non-title processes`, `Generate .etl file`,
`CLR data`. The names live in `Microsoft.PIX.UI.dll` and in no runtime, and `PixCaptureParameters`
has no field for any of them - it was checked field by field against
`Include/WinPixEventRuntime/pix3.h` from the newest WinPixEventRuntime Microsoft publishes
(1.0.240308001) and against Microsoft's GDK reference for the same struct. `pixtool
take-new-timing-capture` offers the same subset as the API, and `pixtool open-capture` refuses a
timing capture outright ("not a Windows GPU capture"). So a capture with those in it is one taken
from PIX's UI - which is exactly what the switch's installed `WinPixGpuCapturer.dll` makes possible.

**Function names need a PDB, and a release build had none.** The capture's `FunctionInformation`,
`SymbolStrings`, `ModuleSymbols`, `SourceFile` and `SourceLine` tables are filled when PIX resolves
symbols, and there was nothing to resolve: `cargo build --release` wrote a PDB with no source files
in it (6 MB, not one `.rs` name in it), so every native frame was an address. Three changes:

- `rust/Cargo.toml` asks for `debug = "line-tables-only"` in the release profile - line tables, no
  locals, nothing added to the code at runtime, and the PDB grows to ~47 MB;
- `copyNatives` copies that PDB into the mod's resources, and `WgpuNative` extracts it next to the
  library it extracts - both into the launcher's natives directory, because a debugger looks for
  symbols *beside the module* rather than in the mod's jar, and because that directory is already on
  `java.library.path`. The PDB travels inside the jar as well now: a packaged build is exactly the one
  somebody profiles when a player reports a frame that took 40 ms, and the alternative was asking
  them for a second download;
- PIX still needs a symbol path for everything else (the JDK's `java.exe`, the driver, Windows):
  *Configure PIX → Debug symbols*, or `_NT_SYMBOL_PATH=srv*C:\symbols*https://msdl.microsoft.com/download/symbols`.

Two things about the API itself came out of this and are worth writing down, because both produced a
wrong answer first:

- **`SUCCEEDED`, not `S_FALSE`.** `pix3.h` documents `S_FALSE` from `PIXBeginCapture`, and that is
  what older PIX returned; PIX 2603 returns plain `S_OK`. The code tested for `S_FALSE` alone, so a
  capture that *had* started was logged as `PIX refused … (HRESULT 0x00000000)` and left running -
  the frame counter that was supposed to stop it had already handed over. Both calls now ask the
  question the header's `SUCCEEDED(hr)` macro asks, which is a sign test.
- **Stopping a capture is not a flag flip.** `PIXEndCapture` writes the capture out, and with the
  memory events on that is gigabytes: it took longer than the ten seconds the call used to wait for,
  and the timeout left the capture running. It now runs on its own thread and is not waited for; a
  capture ends by itself at 600 frames either way, and the log says which of the two happened.

The cost is real and worth knowing before turning the switch on to profile frame times: 600 frames
of a world is now **1.7-2.7 GB** instead of 265 MB, the frame rate during a capture drops to a few
frames per second (the events are recorded synchronously), and PIX itself drops events it cannot keep
up with (`DroppedData` in the capture: 67k-240k rows). That is why the switch is off by default and
why `CAPTURE_FRAMES` is a constant rather than a setting - 600 frames of *play* is what a timing
capture is for, and a capture taken at three frames per second says more about the capture than about
the renderer.

### RTSS and PIX both hook D3D12, and together they crash the game

MSI Afterburner's on-screen display is RivaTuner Statistics Server, which works by injecting
`RTSSHooks64.dll` into the game and detouring `IDXGISwapChain::Present`, `ExecuteCommandLists` and
friends. PIX's `WinPixGpuCapturer.dll` hooks the same runtime. With both in the process, the game has
been taken down with the render thread inside the present path:

```
#  EXCEPTION_ACCESS_VIOLATION (0xc0000005) at pc=…, tid=110920
# Problematic frame:
# C  [D3D12Core.dll+0x11fd5]
Native frames: (J=a compiled Java code, …)
C  [D3D12Core.dll+0x11fd5]
C  [D3D12Core.dll+0x112bd]
C  [RTSSHooks64.dll+0x71a86]          ← the fault is reached through RTSS's hook
C  [d3d11.dll+0x495d2]
C  [RTSSHooks64.dll+0x4beee]
C  [wgpu_mc_jni.dll+0x5a3dbb]
J  dev.birb.wgpu.backend.WgpuSurface.blitAndPresent(…)     ← our present
```

`siginfo: … reading address 0x0000000000000733` is a pointer that is not an object at all, and the
module list of that same crash holds both hook libraries at once - `RTSSHooks64.dll` from
`C:\Program Files (x86)\RivaTuner Statistics Server` and `WinPixGpuCapturer.dll`,
`WinPixTimingCapturer.dll`, `PixStorage.dll` and `WinPixSysMonController.dll` from PIX. RTSS's own
`Profiles\Config` says how it does the D3D12 half: it caches the *private offsets* of
`IDXGISwapChain1::m_pCommandQueue` and of `ID3D12CommandQueue::ExecuteCommandLists` per Windows
version, and drives the overlay through them. A capturer that wraps the D3D12 objects is exactly what
those cached offsets do not survive.

None of that is this mod's code and none of it can be fixed from here, so what this side does is
refuse to load the capturer when the hook module is in its own process. A warning would leave a game
that dies a second after the first frame to be diagnosed by whoever hits it; the switch instead does
nothing, and says why:

```
wgpu-mc: PIX: RTSSHooks64.dll is loaded into this process, so RivaTuner Statistics Server - MSI
         Afterburner's on-screen display - is already hooking D3D12. PIX hooks the same runtime, and
         the two hook chains crash this game inside RTSS's present hook … whether or not a capture is
         running. PIX's libraries are therefore NOT loaded this launch and the `pix capture` switch
         does nothing. Add the `java.exe` this game runs as to RTSS's profile list and set its
         Application detection level to None, or quit RTSS/Afterburner, then start the game again.
         To load PIX anyway … create a file named `wgpu-pix-with-rtss` next to the game.
```

The exclusion is per application, so the OSD can stay on everywhere else: in RTSS, add the
`java.exe` the game runs as (`…\GraalVM\JDKs\25.1\bin\java.exe` in a dev run) and set its
*Application detection level* to **None**. Closing RTSS and Afterburner does the same thing more
bluntly. The `wgpu-pix-with-rtss` marker is the way back if a later RTSS or PIX makes the two
coexist: it is the same shape as every other override in this renderer, a file rather than a rebuild.
A crash during a capture also leaves PIX's own spool files beside the capture
(`wgpu-mc-capture-N.wpix-wal`, `.wpix-shm`, hundreds of megabytes); they are safe to delete, and the
`.wpix` next to them is what PIX can open.

### A shader may declare more than its pipeline provides

Five pipelines were failing to compile because a shader declared something the pipeline's own
description did not mention. On the GL side that is legal and the game relies on it:

- `GlProgram::setupUniforms` enumerates the uniform **blocks the shader declares** with
  `glGetActiveUniformBlockName` and gives `Projection`, `Lighting`, `Fog` and `Globals` a binding
  of their own, so `RenderSystem.bindDefaultUniforms` finds them even when the pipeline never asked
  for them. `core/terrain` imports `globals.glsl` and reads `CameraBlockPos`, while
  `TERRAIN_SNIPPET` only lists `Projection` and `ChunkSection`.
- Attribute locations are bound from the pipeline's vertex format, and an attribute with no binding
  reads the default generic value. `core/rendertype_crumbling.vsh` declares `in vec3 Normal` while
  `CRUMBLING` draws with `DefaultVertexFormat.BLOCK`, which has no Normal - and the shader never
  reads it.

wgpu has neither behaviour, so the Rust preprocessing shim now reproduces the observable result:

- `add_implicit_uniforms` walks the blocks the shaders declare and gives the ones the pipeline left
  out a binding, appended after everything it did declare. `with_implicit_uniforms` appends matching
  entries to the pipeline's own descriptor, which is what puts them in the pipeline layout *and* in
  every bind group built from it, so `bindDefaultUniforms` fills them by name.
- `drop_unprovided_inputs` removes an `in` declaration nothing will provide, before locations are
  numbered. Leaving it in place is what produced `Multiple bindings at location 0 are present`.
- Both were `.expect`s that aborted the process. What is left of that - a uniform or a shimmed
  sampler the pipeline cannot bind at all - now logs and gets a binding that cannot collide, so the
  shader still compiles and wgpu reports the unmatched binding by number.

### The first jar had no Kotlin in it

Dropping the exported jar into an instance did not start the game. Three runs stopped in the same
place - immediately after the mixin that runs on `Main.main` had been applied - and none of them put
an error in `debug.log`, because a class-loading failure is reported on stderr and a launcher does
not put stderr in that file. What it was is not in doubt: the mod is Kotlin, nothing on a player's
classpath carries the runtime (FML and NeoForge are Java, Minecraft ships no Kotlin), and the first
Kotlin class the game touches is that mixin's handler. The run after KotlinForForge 6.3.0 was added
is the confirmation: the same jar went on to the title screen.

A development run cannot show this, because the Kotlin Gradle plugin puts the stdlib on the *run*
classpath - `runClient` has had it since the first build, and only the published jar did not.
`neoforge/build.gradle.kts` now sends the stdlib through `jarJar`, which embeds it at
`META-INF/jarjar/kotlin-stdlib-<version>.jar` beside the `metadata.json` FML's jar-in-jar loader
reads, so the artifact to install is the one with the `-all` classifier:

```
neoforge/build/libs/wgpu_mc-<version>-all.jar
```

The plain jar is the input to that task and still cannot start on its own. KotlinForForge ships a
stdlib too, and the run above had both on the classpath, so an instance that has it keeps working.

### The update file, and the version it has to be keyed on

`neoforge.mods.toml` has carried an `updateJSONURL` since the mod was ported, pointing at
`neoforge/updates.json` - and that file was empty, which NeoForge reads as *no versions at all*. It is
filled in now, and the one thing about it worth writing down is **which Minecraft version the entries are
keyed on**, because the [docs](https://docs.neoforged.net/docs/misc/updatechecker/) describe the format and
not the key.

`VersionChecker#process` is the answer, and it is short:

```java
var mcVersion = FMLLoader.getCurrent().getVersionInfo().mcVersion();
String rec = promos.get(mcVersion + "-recommended");
String lat = promos.get(mcVersion + "-latest");
ComparableVersion current = new ComparableVersion(mod.getVersion().toString());
```

`VersionInfo.mcVersion` is not the NeoForge version and not the base game version: `FMLLoader` overwrites
it with the version of the **`minecraft` mod file** it discovered, which for a 26.1 hotfix carries the
hotfix - `26.1.2`, not `26.1`. The client's own log says so in as many words:

```text
Found valid mod file neoforge-26.1.2.109.jar with {minecraft} mods - versions {26.1.2}
        Minecraft 26.1.2 (minecraft)
```

So the file keys `promos` and its changelog lists on `26.1.2` (and, since the jar supports the whole
`[26.1, 26.1.999)` line, on `26.1.1` and `26.1` as well - a user on a hotfix asks about that hotfix). A
file keyed on the wrong string is not an error anywhere: the checker finds no `promos` entry, reports
`BETA`, draws no icon, and the mod looks like one that simply has no updates. `packaging.rs` in the JNI
crate reads both files and asserts that the entry the game will ask about names the version
`gradle.properties` builds - a version bump that forgets this file is otherwise silent.

`recommended` is deliberately absent. It means "the latest *stable* version", the checker reports `BETA`
when only `latest` is present, and this mod has no stable releases - `2612.0.7-alpha` - so claiming one
would be a lie told to the mod list. The moment there is a stable build, adding
`"<mcversion>-recommended"` is what turns `BETA_OUTDATED` into `OUTDATED`.

The file only takes effect once it is **pushed to `master`**: the URL is the raw GitHub one, so a local
edit is invisible to every player until then (and `raw.githubusercontent.com` caches for a few minutes,
which is worth remembering when testing a change to it).

### NeoForge hands the game a window that was made for OpenGL

With the runtime in the jar the game got further and then aborted, and this one is not a Java
exception - there is nothing to catch:

```
FATAL ERROR in native method: Thread[#3,Render thread,5,main]: No context is current or a function
that is not available in the current context was called. The JVM will abort execution.
    at org.lwjgl.opengl.GL11C.glIsEnabled(Native Method)
    at net.neoforged.fml.earlydisplay.render.GlState.readFromOpenGL(GlState.java:129)
    at net.neoforged.fml.earlydisplay.render.LoadingScreenRenderer.renderToScreen(LoadingScreenRenderer.java:230)
    at net.neoforged.fml.earlydisplay.DisplayWindow.periodicTick(DisplayWindow.java:525)
    at net.neoforged.neoforge.client.loading.ClientModLoader.finish(ClientModLoader.java:66)
    at net.minecraft.client.Minecraft.<init>(Minecraft.java:695)
    at net.minecraft.client.main.Main.main(Main.java:231)
```

FML creates a GLFW window *with an OpenGL context* before a single mod is loaded, draws the splash
screen into it, and later hands that same window to the game: `Window#createGlfwWindow` asks
`EarlyLoadingScreenController.current()` for it instead of calling `glfwCreateWindow`. Adopting it is
fine for wgpu - the surface is attached to that handle and the device comes up normally - but the
loading screen keeps a repaint tick installed across the hand over, and NeoForge drives that tick from
the render thread while the last of mod loading runs. The tick is OpenGL, the context it needs is the
one the loading screen created on its own thread, and in this mod the render thread never has it (nor
`GLCapabilities`, which is what `LoadingScreenRenderer#close` complains about once the tick is out of
the way) - so the first GL call aborts the JVM.

Whether the early screen exists at all is decided by FML from `config/fml.toml` *before* any mod is
loaded, so a mod cannot switch it off: a fresh instance has `earlyWindowControl = true`, and all the
mod can do is defuse the screen it is handed. `dev.birb.wgpu.backend.EarlyWindow` does that in two
steps - from the head of `Main.main` (`mixin/core/EarlyWindowMixin`) and from the hand-over call
itself (`mixin/render/WindowMixin`):

1. take the window over (`takeOverGlfwWindow`, which stops the loading screen's own render loop) and
   close it. A closed loading screen is skipped by the tick above, so no GL call is left for the
   render thread to make, and closing shuts down the thread pool that would otherwise keep the
   process alive after the game exits. Closing destroys GL objects, so it runs only while the
   window's context is current here *and* LWJGL has capabilities for this thread.
2. answer the hand-over question with "no", so the game falls through to `glfwCreateWindow` and gets
   this mod's `GLFW_NO_API` window - the window a development run has. The splash is hidden at that
   moment rather than at step 1, so it stays on screen for the mod loading it exists to report on.

Development runs still do not exercise any of this: `configureEarlyWindow` writes
`earlyWindowControl = false` into `runs/client/config/fml.toml` before every `runClient`, which is why
the abort first appeared in a player's instance instead of here. Deleting that key from
`runs/client/config/fml.toml` is what makes a dev run take the same path as a player's.

### Getting past the constructor took three fixes

All three were latent bugs that had never been reached, because the mod used to die while the mod
list was still being constructed:

1. **`WgpuNative`'s own initialisation order.** The object loads the native library from an `init`
   block declared above `NATIVE_RESOURCE_ROOTS`, and Kotlin runs an object's initializers in
   declaration order, so the resource-root list was still `null` when the loader searched it. The
   list is now a file-level declaration, where the ordering cannot be got wrong.
2. **`loadWm` called two JNI functions that do not exist.** `CoreLib.init()` asked Rust to install
   LWJGL's allocator, but that shim is commented out in `rust/wgpu-mc-jni/src/alloc.rs`, and
   `setClassLoader` has no Rust counterpart at all. `UnsatisfiedLinkError` is an `Error`, so the
   `catch (e: Exception)` around the call did not see it and the game died in `Minecraft`'s
   constructor. `loadWm` now only loads the library, exactly like the Fabric module's
   `WgpuNative.loadWm`, and `CoreLib` is gone. **Anything added there needs a matching `#[jni_fn]`
   on the Rust side, or the first touch of `WgpuNative` kills the game.**
3. **`DebugHUDMixin` contributed a public static method to its target.** Mixin rejects that
   outright. The lines it collected were never rendered anyway, because 26.1 builds the F3 line
   list as a local and passes it to the private `extractLines`. The mixin now appends to that list
   through `@ModifyArg`, which is both legal and makes the diagnostics actually appear.

### Why the game rendered black, and then pink

The last two things between "the renderer comes up" and "the game is playable" were both state
translations that looked right and were not, and both were found by dumping the renderer's own
output rather than by reading code.

1. **A render pass with no clear value cleared anyway.** `BlazeAttachmentDescriptor::clear_value`
   is null when Minecraft asks for no clear - that is `OptionalInt.empty()` - and OpenGL loads the
   attachment in that case. The port used `wgpu::Operations::default()` for it, and wgpu's
   `Default` for `Operations` is `LoadOp::Clear(V::default())`, i.e. *clear to transparent black*.
   Every pass without an explicit clear therefore wiped the target first, which is why the title
   screen presented as its bare panorama: the panorama pass ran last and erased the GUI pass before
   it. It is now `LoadOp::Load`, and the depth attachment honours the clear value it is given
   instead of always loading (`clearDepth` used to be dropped on the floor).
2. **`NativeImage` pixels were uploaded big-endian.** `NativeImage#pixels` packs a pixel per int as
   **ABGR**; the four bytes of an `Rgba8Unorm` texel are R, G, B, A, which is that int written
   *little*-endian. `ByteBuffer` is big-endian unless told otherwise, so every texture arrived as
   `(A, B, G, R)`: alpha in red, green and blue swapped. That is the washed-out pink the whole game
   was drawn in - red pinned at 255 everywhere, because the alpha channel is 255 nearly everywhere.
   One `.order(ByteOrder.LITTLE_ENDIAN)` fixes every texture in the game at once.
3. **"No depth state" was turned into a depth test.** A pipeline whose `depthStencilState` is null
   means the depth test is off, which is how every GUI pipeline is declared - the title screen, the
   HUD, the debug overlay. The port filled that in with `LESS_THAN_OR_EQUAL` *and* depth writes on,
   so the GUI was depth-tested against whatever the panorama had just written and lost. wgpu will
   not accept a pipeline with no depth-stencil state in a pass that has a depth attachment, so "off"
   is now spelt `Always` with writes disabled, which is the same thing.

The bytes and the depth state are also why the two symptoms looked unrelated: the pink was in the
textures, the missing GUI was in the pipeline state.

### A colour coming out of a texture is a texture problem

The dump above is what settled it, twice.

**First round: alpha in red.** `tex-minecraft-textures-gui-title-background-panorama-layer0.raw` held
the panorama's face 1 rotated by 180°, with `r` equal to the source's alpha and `g`/`b` swapped -
which is a *channel permutation*, not a shading bug, and no amount of looking at blends, uniforms or
projections would have found it. Matching the dumped bytes against the PNG's own pixels (24
candidate permutations x two flips) reported `mad=0.00` for exactly one of them, and that is what
named the bug: the upload wrote the image's pixel ints big-endian, so an `(R, G, B, A)` texel came
out `(A, B, G, R)`.

**Second round: red and blue swapped.** The fix for the first round - write those ints
little-endian - was still one channel pair off, and it put the sky in orange with the blue GUI
icons in red. The trap is that 26.1 has two pixel representations and they are one conversion apart:

- `NativeImage` stores **ABGR** ints and `GlCommandEncoder` uploads the image's own buffer, so
  `glTexSubImage2D(GL_RGBA, GL_UNSIGNED_BYTE, pointer)` reads an ABGR int's little-endian bytes,
  which are `R, G, B, A` - the layout an `Rgba8Unorm` texel wants;
- `NativeImage#getPixels()` is **not** that buffer. It copies and converts every element through
  `ARGB.fromABGR`, so it hands back ARGB, and an ARGB int's little-endian bytes are `B, G, R, A`.

The upload used `getPixels()` and then trusted the endianness, which is what swapped red and blue.
It now takes the four channels apart explicitly (`R` is bits 16-23, `G` 8-15, `B` 0-7, `A` 24-31)
and writes them in order, so no int representation can be mistaken for the byte layout again. The
same measurement confirms it: the uploaded face against `panorama_1.png` reports
`perm=0123 flipY mad=0.00` - the identity permutation - where it used to report `perm=2103`. The
`flipY` is the cube face's own orientation and not a colour matter, and the same file also explains
why this reached everything: every block, item, GUI, font and panorama texture in the game is
uploaded through that one call.

The other upload entry point, the `ByteBuffer` overload, is passed through unchanged and that is
correct: `UnihexProvider` is its only caller in 26.1, it fills the buffer with `0xFFFFFFFF` and `0`
(white and transparent, where no channel order can go wrong), and OpenGL reads the very same four
bytes per texel as `GL_RGBA`.

### Widget backgrounds were missing because render targets are the other way up

The title screen drew its logo, its splash text, every button *label* and both corner strings - and
no button backgrounds at all. The labels and the logo come from textures that were uploaded, so the
one thing they have in common is what the missing part is not: the backgrounds come from
`minecraft:widget/button`, a sprite in `minecraft:textures/atlas/gui.png`, and that atlas is not
uploaded. 26.1 *renders* it: `TextureAtlas#uploadInitialContents` draws every static sprite through
`animate_sprite_blit` into `mipViews[level]`, one pass per mip level, and the GUI later samples the
result with `u = x / width, v = y / height`.

That works on OpenGL and not here, because OpenGL and every other API disagree about which end of a
render target clip-space `y = +1` is. In OpenGL a framebuffer's first texel row is its window origin
row - the bottom-left corner - so `y = +1`, the top of the projection, lands on the *last* texel row.
Vulkan, D3D and Metal all put it on the *first*, and wgpu normalises them, so the sprite quads were
drawn into the atlas mirrored. Minecraft then sampled the rectangle it had packed the sprite into and
found empty atlas, which draws nothing at all.

Nothing notices while the target is only presented - the picture comes out the same either way up -
and that is why this survived the earlier rounds. It shows the moment a render target is *sampled
with Minecraft's own texture coordinates*, and in 26.1 that is every sprite atlas, the GUI item
atlas and the picture-in-picture renderers.

The fix is one statement appended to `main` in every translated vertex shader
(`preprocessing::EmulateGlClipSpace`), on top of the depth-range patch that was already there:

```glsl
gl_Position.y = -gl_Position.y;
```

That is what a translation layer does for the same reason, and it needs one compensation: the
present blit now turns the image over on its way to the swapchain (`PresentBlit` in `device.rs`,
replacing `wgpu::util::TextureBlitter`). It also puts `gl_FrontFacing` back the way Minecraft
expects it, because front-facing is counter-clockwise in *window* coordinates in OpenGL and
counter-clockwise in *framebuffer* coordinates here, and mirroring the clip space is what
reconciles the two.

Three things were wrong around the same area and were found while looking for it:

1. **A texture's mip chain was created with one level.** `create_texture` hardcoded
   `mip_level_count: 1` and ignored the count Minecraft asked for, so `blocks.png` (2048x2048, five
   levels) had nowhere to put levels 1-4.
2. **A texture view ignored the mip range it was asked for.** `base_mip_level` was 0 and
   `mip_level_count` was "the rest" for every view. wgpu only accepts a view with *exactly one* mip
   level as a render attachment, and the level a view starts at is the size the pass renders at, so
   the five `Animate blocks.png` passes all rendered into mip 0 - each one a shrunken copy of the
   whole atlas, on top of the last.
3. **`write_to_texture` dropped every upload with `mip_level != 0`.** That was a guard against
   levels that did not exist, and with (1) fixed it is a bounds check instead: Minecraft uploads a
   sprite's whole chain one level at a time, and a 9x9 sprite in a five-level atlas legitimately has
   fewer levels than the atlas does.

**How it was found**, because none of this is visible in a log: with the frame dump on, the GUI
atlas is written out too (`atlas-minecraft-textures-atlas-gui-png.raw`), and matching
`widget/button` - 200x20 pixels of vanilla resource pack - against it reported its *last* row at
y = 1022 with the rows running upwards, i.e. the sprite mirrored about the middle of the atlas.
Flipping the dumped atlas made it look like a normal `gui.png` again. After the fix the same match
reports the sprite's *first* row at (723, 1), rows running downwards, which is the coordinate the
stitcher packed it at.

### The atlas animation was cancelled, and is not any more

Animated sprites did not animate: water, lava, fire, the clock and the compass all held their first
frame. That was deliberate, and it was a leftover from before the render path above worked - a mixin
cancelled `TextureAtlas#cycleAnimationFrames` outright:

```java
@Inject(method = "cycleAnimationFrames", cancellable = true, at = @At("HEAD"))
private void dontTickAnimatedSprites(CallbackInfo ci) { ci.cancel(); }
```

The mixin is gone. `cycleAnimationFrames` re-renders the dirty frames of every animated sprite
through the same `Animate <atlas>` pass that builds the atlas in the first place
(`SpriteContents.AnimationState#drawToAtlas`), so once that pass renders correctly - one mip level
per pass, the right way up - there is nothing left for the cancel to work around.

It is worth knowing that these passes are the *only* thing that makes an animated texture change:
`TextureAtlas#uploadAnimationFrames` runs them, and if it does not, the atlas simply keeps the frame
it was built with. The renderer therefore counts them, once a second, with the diagnostics on:

```
wgpu: 130 sprite animation passes in the last second
```

Zero there after the resource load means the animation is not running, which is a diagnosis rather
than a guess - the passes are cheap (one quad per dirty sprite per mip) and ~130 a second is a world
running at ~150 frames a second with the blocks, items and GUI atlases all animating.

### The scissor rectangle is OpenGL's, and is forwarded as it is

`RenderPassBackend#enableScissor` used to be a no-op on the grounds that 26.1 never calls it. It
does: a title screen that walks into the options screen logs two rectangles (`0 120 854 262` and
`0 98 2562 1137`), and `GuiItemAtlas` sets one around every item it renders into its atlas. Both
callers build the rectangle the OpenGL way - `GuiRenderer#enableScissor` as
`windowHeight - bottom * guiScale`, `GuiItemAtlas` as `textureSize - bottom` - which is measured
from the target's origin row, the row this backend renders into. So it is forwarded unchanged, and
only clamped: OpenGL clips a scissor box that reaches outside the framebuffer and wgpu *rejects*
one, and the rectangle a scaled GUI hands over can reach past the edge (`2562` wide in an `854` wide
window).

### A scissored clear is not a load op, and the item atlas is why that matters

`glClear` is restricted by the scissor box. A wgpu load op is not: it always covers the attachment.
`clear_color_and_depth_textures_region` was therefore implemented by clearing the whole texture and
warning about it once, on the reasoning that the only caller redraws one stale slot of the GUI item
atlas and the cost is the other cached slots "until they are allocated again".

They are not allocated again. `GuiItemAtlas#drawToSlot` clears the 32x32 slot it is about to redraw,
so every redraw wiped every *other* item icon in the atlas, and the icons came back only for the
items drawn after the last clear of that frame. What makes a slot stale is an item whose render state
changed or whose slot was reassigned - opening the inventory, hovering a different item, an item
being used - so the report was "almost every item icon in the inventory is gone, hovering one brings
that one back and takes others with it, and the hotbar and the held item do the same". Player heads
are items, which is why some of the player textures went with them.

The colour half is now a rect-limited write of the clear colour, which is what a scissored clear is:

- `Queue::write_texture` copies into a rectangle of a texture, and the bytes written are the clear
  colour, so a caller that clears to something other than transparent black gets what it asked for
  (`clear_color_bytes` maps Minecraft's packed `ARGB` onto the texture's own channel order, and a
  format it does not know how to write is refused loudly rather than widened quietly);
- `bytes_per_row` is the rectangle's width in bytes, which `Queue::write_texture` accepts - unlike a
  buffer-to-texture copy, whose rows have to be padded to the 256 byte copy alignment;
- the item atlas' texture is created with `USAGE_COPY_DST` (26.1 asks for usage `13`), which is what
  makes the write legal at all.

The depth half is *still* the whole texture, deliberately: the item atlas' depth attachment is never
sampled and never read back - it exists so an item's own faces depth-test against each other while it
is drawn into its slot - so clearing all of it before drawing into one slot changes nothing anything
can observe. A caller that needed a region of depth *kept* would need a scissored depth draw, which
is a pipeline of its own, and that part stays under *Known gaps*.

The first region clear of a run is logged, because "the icons are blank" has two explanations and the
line says which one is in play:

```
wgpu-mc: cleared a 32x32 region at 224,160 of a Rgba8Unorm texture in place
```

### Reading the settings before they are needed

`sendRunDirectory` loads `config/wgpu-mc-renderer.json` and was called from
`FMLClientSetupEvent`, which fires *after* `Minecraft`'s constructor has already created the
window, the wgpu instance, the adapter and the swapchain. The `backend` setting was therefore read
after the decision it controls had been made, and switching to DirectX 12 did nothing at all. The
mod constructor now reads it - `FMLPaths.GAMEDIR` is available that early - and `sendRunDirectory`
became idempotent, because `OnceCell::set` on the second call used to `unwrap()` into a panic, and
the panic hook exits the game.

### A shader that came back with the wrong operator

The world drew its sky, its clouds, its sun and its fog and then a *white void* where the terrain
should be. Every input the terrain shader asks for turned out to be correct - the block atlas bound
to `Sampler0`, the lightmap to `Sampler2`, `Globals` carrying `ScreenSize` 854x480, `ChunkSection`
carrying a sane `ModelViewMat` - and the answer was in the *shader source we hand to naga*:

```glsl
rgssColorLow *= 0.25;   // terrain.fsh, as Minecraft ships it
rgssColorLow -= 0.25;   // what came out of our preprocessor
```

`rgssColorLow` is the sum of four rotated-grid samples, so averaging it is a multiplication. Turned
into a subtraction, every RGSS-filtered surface - the user's `Filtering: RGSS` setting - comes out
at *four times* its colour minus a quarter, which clips to white. That is the whole symptom: terrain
that is drawn, in the right place, with the right texture, and then blown out.

The blame is `cyntax`'s lexer, which had a copy-paste error in its two-character punctuators:

```rust
span!(range, '*') if matches!(self.chars.peek(), Some(span!('='))) => {
    PreprocessingToken::Punctuator(Punctuator::MinusEqual),   // *= parsed as -=
```

`*=` was the only operator affected - `+=`, `-=`, `/=`, `%=`, `&=`, `|=`, `^=`, `<<=`, `>>=` all
survived - which is why nothing else in the game looked wrong. The crate is now **vendored** at
`rust/cyntax` with the one-line fix, instead of being pinned to a revision of a fork, and
`preprocessing.rs` carries three tests that run every compound assignment through the preprocessor
and assert it comes back unchanged. They fail against the old revision.

### Not every Minecraft topology is a triangle list

`PrimitiveTopology` used to be translated with `match … { _ => TriangleList }`, which is right for
exactly one of the two shapes that matter:

- `QUADS` *is* a triangle list and has to stay one - Minecraft's index buffer for the mode expands
  each quad into `i, i+1, i+2, i+2, i+3, i` on the CPU - so the GUI and the terrain kept working and
  hid the bug.
- `LINES` is not. Its index buffer is `i, i+1, i+2, i+3, i+2, i+1`, three line segments per group of
  four vertices, because `rendertype_lines.vsh` expands each pair into a quad through
  `gl_VertexID % 2`. Drawn as triangles, the F3 overlay's 3D crosshair became long thin triangles
  across the whole screen - the "pointer stretched along the diagonal" - and so did every hitbox and
  chunk border.

`TRIANGLE_FAN` has no wgpu equivalent at all, so `draw_indexed` expands it: Minecraft's fan indices
are the sequential range, and the triangles are `(0,1,2), (0,2,3), …`, so a cached index buffer is
built per size and bound for that draw. The same change had to zero the depth bias for line and
point topologies, because wgpu rejects it there - `Depth bias is not compatible with non-triangle
topology LineList` - and OpenGL ignores polygon offset for lines too.

### Two ways a screenshot could not work

Pressing F2 ended the process. `Screenshot.takeScreenshot` reads a whole 854-wide target in one
`copy_texture_to_buffer` call, this side passed `bytes_per_row = width * 4` straight through, and
wgpu requires a multiple of 256:

```
wgpu error: Validation Error
Bytes per row does not respect `COPY_BYTES_PER_ROW_ALIGNMENT`
```

A validation error is fatal here - it runs the panic hook - so the fix is the one `dump_texture_rgba`
had always used: copy through a padded scratch buffer and write the rows back one at a time. With
that fixed the screenshot was *black*, because the read half of `mapBuffer` had never been
implemented: the staging buffer was allocated, handed to Blaze3D and freed without ever being filled
from the GPU. `read_buffer` now maps (through a scratch buffer of this side's own, so a buffer
Minecraft maps itself is left alone), waits for the GPU and fills it.

Two more diagnostics came out of the same dig, both behind the **Diagnostics** option:

- a file named `wgpu-dump-shaders` in the run directory writes every pipeline's *processed* GLSL to
  `wgpu-shaders/`, which is the only way to see the binding numbers and the operators a shader was
  actually compiled with;
- the first bind of each sampler logs *which texture* it got (`pass Section layers for opaque binds
  Sampler0 to minecraft:textures/atlas/blocks.png`), and the first bind of each uniform reads its
  bytes back and logs them as floats and ints. Buffers now carry `COPY_SRC` for exactly this - the
  same thing textures already did - except the mappable ones, where wgpu rejects the combination.

A panic also writes itself to `wgpu-panic.txt` in the run directory now. A native crash takes
whatever stderr still had buffered with it, which is why the first screenshot crash left a crash
report with no message anywhere in the log.

### Five crashes that were all one mistake about pointers

The session that first reached a world alive died twice within a minute of getting there, each time
on a wgpu validation error, and neither death was Minecraft's fault. The liveness registry that
decides whether a texture may still be used was keyed on `&wgpu::Texture` - and `create_texture`
registered the address of its own *local*, while the pointer the JVM is handed is the address of the
`Box` built from it. Every live texture was therefore unlisted, and the allocator hands a freed box's
address straight back out, so a brand new texture was regularly recognised as its dead predecessor:

- the "after it was closed" warning fired on healthy textures, and the texture was replaced by the
  placeholder the registry exists to hand out;
- the placeholder was 1x1, so the pass that drew into it with the real target's scissor died on
  `Scissor Rect { x: 0, y: 0, w: 878, h: 504 } is not contained in the render target (1, 1, 1)`;
- a clear whose colour *and* depth texture were both "closed" built a render pass with no
  attachments at all: `No color attachments or depth attachments were provided`;
- the swapchain recovery called `configure_surface_inner`, which returned early because the
  configuration was *unchanged* - and the unchanged configuration is what was broken. It logged
  `still no swapchain image after reconfiguring: Validation` once per frame for 1633 frames while
  the window showed nothing;
- the buffer side had no registry at all, so the same use-after-close ended the run as
  `In Queue::write_buffer - Buffer with 'Cloud UTB #1' label is invalid`. Minecraft closes that
  uniform texel buffer and writes to it in the same frame.

Each one is fixed where it belongs. The registry is keyed on the boxed address, which is what
`drop_texture` sees again, so a live texture is never mistaken for a dropped one; the tombstone
remembers the label, size and format it was dropped with, because none of those may be read out of a
freed texture; a placeholder is the size the real texture had, since wgpu checks every scissor
against the render target; a clear with nothing left to clear returns instead of building an empty
pass; the swapchain recovery reconfigures *forcibly*; and a closed buffer is quarantined for half a
second - or until the quarantine is over its byte budget - before it is really dropped, which makes
the cloud buffer's write land in a buffer that still exists.

What the guards are for - Minecraft rebuilding a render target on a resize and something in the same
frame still holding the old textures - has not gone away, and a genuine use-after-close now logs the
label, size and format it was refused for. In a full run of the fixed build there were none.

### The leak was one command encoder per Minecraft command encoder

The game grew from one gigabyte to twenty in a minute and took the machine with it, from the very
first frames and with no gameplay involved. The counters that eventually found it - live textures,
views, buffers, passes, bind groups and encoders, printed every 120 frames - showed everything flat
except one line:

```
live resources: 767 textures (74 MB), 786 views, 47 buffers (0 MB, 5 quarantined), 1 encoders, ...
```

The number before that fix was 24000 and climbing by about 960 a second. `CommandEncoder` in
Blaze3D is **not** `AutoCloseable` and has no `close`: it is a plain object that the garbage
collector takes away, which is free in the OpenGL backend, whose encoder owns nothing. Here every
one of them owned a `wgpu::CommandEncoder` - a D3D12 command allocator and command list holding the
frame's recording - and none of that is visible to the Java heap, so nothing ever collected it:

- three encoders a frame (`Minecraft#runTick` makes one per frame, and the renderer more),
- a few hundred frames a second, because this backend runs without vsync in the menus,
- about 200 KB of command buffer each, which is the 200 MB/s the process was growing at.

Every handle now shares **one** native encoder, and the pointer the JVM holds is an empty box. The
recording order is what matters and everything is recorded on the render thread in call order, so
one encoder is enough; `flush_encoder` submits what has been recorded and starts a new one, exactly
as before. A cleaner that gave the encoder back when the object was collected was written first and
is not enough on its own: the collector has no reason to run, because the memory it cannot see is
the memory that matters.

Two other things came out of the same dig: `present_surface` now polls the device, because wgpu
frees a submission's command buffers and staging memory when it is polled and the GPU has caught up,
and the diagnostics that read a uniform back are rationed to four a second. The readback flood was
doing 4722 readbacks for the blocks atlas alone - Minecraft binds one uniform *per sprite* while it
animates an atlas, and keying the "report this again" set by offset instead of by name made every
sprite a fresh readback, each one allocating, submitting and waiting.

### One submission a frame, and only where something reads the result

One encoder was the first half of that fix and this is the second: an encoder that everything shares
is no use if everything also *submits* it. `flush_encoder` used to be called after every clear, every
pass close, every upload, every copy and the blit, which is ten submissions a frame at two hundred
frames a second - ten command buffers, ten sets of GPU-side allocations and ten chances for the
driver to serialise.

Clears, passes, copies, blits and texture uploads are now recorded and left in the encoder; the
submission points that remain are the ones where something is about to *look* at the result:

- **before a present**, inside the native blit - which is also where the frame's GPU timestamp ends,
  so the frame boundary and the submission boundary are the same thing. That is what makes the
  measurement mean "this frame" rather than "the last segment of this frame", which is what it
  measured while a frame was split into ten submissions;
- **before a readback**: `copyTextureToBuffer`'s callback (26.1's screenshots go through it) and a
  pass-target dump. A copy whose result is read on the CPU has to have been submitted at all;
- **before a write that lands on bytes an already recorded draw reads** - see below.

**A queue write is not a command.** `Queue::write_buffer` and `Queue::write_texture` are applied *at*
a submission, ahead of every command in it, so with the whole frame in one submission every draw in
that frame reads the *last* value written - and that is not a theoretical hazard, it is what being
clever about this cost:

- `RenderSystem#setShaderLights` writes the `Lighting` block between draws. It is a small,
  *fixed-offset* uniform - not a ring slice like `DynamicTransforms` - so every item and entity in
  the frame was lit by whatever the last lighting setup of the frame was, which is usually all zeroes.
  Items and mobs drew **black**;
- the GUI item atlas is the tell: `GuiItemAtlas` renders every item icon of a frame into it, and the
  dump came out full of black cubes. Its icons are what the inventory shows, so the report was "the
  item icons are gone, and the hand and the player look wrong" - and none of *that* was the item
  atlas' own fault.

So writes are ordered against the recording now, by offset. Every write into a buffer raises a
per-buffer mark, and a submission forgets every mark; a write *above* the mark goes into bytes nothing
has read yet - Minecraft's ring buffers, one fresh slice per write - and travels with the frame, while
a write *at or below* it may be rewriting something in flight and submits what has been recorded
first. Texture writes submit first every time: they name a mip, a layer and a rectangle rather than an
offset, so there is nothing to compare against, and a re-upload mid-frame is rare enough (a resource
load, a skin, a font) for that to cost nothing.

The result is one submission a frame plus one for each in-place uniform rewrite, which the render
stats line says out loud:

```
wgpu-mc: render stats: 3255 render passes (0 of them empty, last had 1 draws), 6195 pipeline binds,
227125 draws (...), 486992493 vertices, 963 submissions
```

963 submissions for 120 frames is eight a frame rather than one, and it is eight *because the frame
wants eight*: the lighting, fog, projection and globals blocks are each re-written in place as the
frame moves between passes. Getting rid of those means recording them into the command stream as a
copy from a staging ring instead of a queue write - an in-stream copy can sit between two draws,
which a queue write cannot - and that is the work left here rather than a switch.

A pass borrows the encoder while it is open, so `flush_shared_encoder` refuses to submit while
`LIVE_PASS_COUNT` is non-zero and says so at error level: finishing an encoder out from under an open
pass is not a thing to do quietly, and the count being non-zero there would be a bug in the caller
rather than a timing accident.

### Every sampler was the same sampler

`create_sampler` took no arguments and built one default `wgpu::Sampler` for the whole game: all
three axes `ClampToEdge`, `Nearest` filtering, and `lod_max_clamp` of zero - which is "mip 0 only".
Blaze3D asks for the address modes and filters it wants, the JVM side remembered them so its accessors
could report them, and the native side dropped them on the floor.

Everything whose texture is *meant* to repeat was wrong:

- `WeatherEffectRenderer` draws one quad per rain column and lets the texture's `v` run from
  `bottomY / 4` to `topY / 4`, relying on the wrap to cut that into falling streaks. Clamped, the last
  texel row is stretched down the whole column: **rain and snow rendered as blue and white lines
  falling out of the sky**, which is exactly the shape of the report;
- the enchantment glint scrolls its texture the same way, so it stopped being a moving pattern;
- flowing water and lava scroll a sprite by whole texture coordinates in the same way;
- `lod_max_clamp` of zero pinned every sample to mip 0, which is why nothing was ever mip-filtered.

The modes now travel across the ABI as the numbers Blaze3D uses for them - `AddressMode` 0 is
`REPEAT`, `FilterMode` 1 is `LINEAR` - and so do the filters, with one deliberate exception: **mips
are still not sampled**, whatever the request says. `lod_max_clamp` stays at zero and the mipmap
filter stays `Nearest`, because this side builds every atlas mip level by *rendering* the atlas into
it - one `Animate <atlas>` pass per level - and a level that is empty or half filled does not blur, it
samples the *neighbouring sprite*. That is the difference between a correct icon and one wearing the
sprite packed next to it: the report was "the stair's side is missing and one face of the pressure
plate has the bed's texture", and it went away when mips went back off. Turning them on is a change
of its own and has to come with a check that every level is filled.

Anisotropy above one is only applied when both filters are linear, which is what wgpu requires rather
than something this side may decide: it answers anything else with a validation error, and a
validation error ends the process.

### One plan for a pipeline's bindings

Three things have to agree about where a binding lives: the wgpu `BindGroupLayout` that the pipeline
is built against, the `layout(binding = N)` annotations written into the GLSL, and the entries and
dynamic offsets of the bind group handed to `set_bind_group` at draw time. They used to be derived
separately - by a walk of the pipeline descriptor, by a map built for the shader rewriter, and by
another walk in `finalize_binding_builder` - and they disagreed, which is how a bind group came to
carry an offset list in one order while its layout expected another.

`blaze::BindGroupPlan` is now the single description, computed once per pipeline variant:

- `BindGroupPlan::number` numbers the bindings from the pipeline description, one per buffer and two
  per combined sampler. The GLSL annotations are written from `shader_locations()`, which is that
  numbering; the wgpu layouts come from `create_layouts()`; the bind group entries and the dynamic
  offsets come from walking `sets`, so the offsets list cannot be in a different order from the
  layout's dynamic bindings - it *is* that order.
- The uniform blocks a shader declares but the pipeline never listed are appended to the plan at the
  binding numbers the shader rewriter reserved for them. That replaced `with_implicit_uniforms`,
  which cloned the whole descriptor and leaked a `CString` per entry per pipeline; the descriptor
  copy is gone with it.
- `min_binding_size` is no longer left unset. `reflected_block_sizes` parses the preprocessed GLSL
  with naga and reads each uniform block's size out of it - the same view wgpu validates against - so
  a layout declares what the shader reads and the binding is checked when it is created rather than
  being left to wgpu's late per-draw check.

The defects the same review turned up:

- **A dynamic offset is a `u32`** and the slice offsets are `u64`, so an offset past four gigabytes
  would have been truncated into a binding that pointed somewhere else. An offset that does not fit
  is now baked into the binding instead of being handed over as a dynamic one.
- **`roundToward(length, 16)` ran past the end of the buffer.** The JVM rounds a slice's length up to
  the sixteen bytes a uniform range wants, and a slice whose last byte was the buffer's last byte
  became a binding four bytes too large, which wgpu refuses. Both sides clamp now: the JVM stops the
  rounding at the buffer's end, and `binding_size` clamps again where the buffer is in hand, so the
  size is never past the end and never smaller than the shader's block.
- **A storage buffer was given a dynamic offset** it cannot have - DX12 has no offset for a storage
  descriptor - while the layout did not declare one. That was the "BindGroup expects 4 dynamic
  offsets. However 5 dynamic offsets were provided" that ended a run. Only uniform bindings are
  dynamic now, and the offsets list is built from the plan's uniform bindings alone.

**Dynamic offsets are on.** The plan gives the order, the offsets are built in it, the alignment and
the `u32` are checked per binding, and an offset that fails either check is baked into the binding
instead with a zero offset, which lands on the same range. What held the switch off was that a
multi-binding layout drew *wrong* with it - which turned out to be the bind group cache keyed on the
builder's map slot rather than on the resource, so it answered with a set built for a different
texture; see "The cache was keyed on the wrong address". With that fixed, a world renders correctly
with 327560 of 327680 draws served from the cache. `wgpu-no-dynamic-offsets` turns the feature off,
and `wgpu-dynamic-offset-names` (a comma-separated list) restricts it to some uniforms, which is how
one binding at a time can be ruled in or out.

### A binding that is not in the plan now says so

`writeBinding` and `writeSampled` used to `return` when a name was not in the pipeline's plan, and
that silence cost a week: the sound the log made was the sound of everything working. The native side
does not let it pass - `blaze.rs` panics with `nothing bound in slot 'DynamicTransforms', which the
plan declares as a buffer`, which is a hard failure and a dead process - but a process that ends with
a slot name and no explanation is not a diagnosis either. Three things changed:

- **A name that is not in the plan is a warning, once per (pipeline, name)**, with the names the plan
  *can* be bound under printed alongside: `minecraft:pipeline/gui was bound a buffer under the name
  'Globals', which is not in its binding plan ... The plan's 2 binding(s) are: 0:DynamicTransforms,
  1:Projection`. A plan is built from what the shader declares, so the expected hits are the default
  uniforms `RenderSystem#bindDefaultUniforms` binds into every pass - `Globals`, `Lighting`, `Fog` -
  and the ones that matter are a shader that declares the binding under a *different spelling*.
- **`PlanBindings.of` has a suffix-aware fallback.** The shader preprocessing splits a combined
  sampler into `Sampler0_wm_texshim` and `Sampler0_wm_sampler`, and a lookup that misses is retried
  in both directions: a shim-spelled request falls back to the declared name, and a declared name to
  either half. Two hits are combined into a *pair* when one of them is a texture slot and the other a
  sampler slot, because half a sampler is worse than none of it. Every hit is logged once per name,
  under the binding-resolution switch.
- **After a pipeline change re-emits what the pass held, the slots left empty are named** - pipeline,
  slot, and the binding that slot holds - which is what turns "this draw is missing an uniform" into
  "this draw is missing `DynamicTransforms`, and the plan's names are these".

All three are the `binding_verbosity` switch on the options screen (`Debug`), default off, and they
also follow `diagnostics`: the fallback hits and the empty-slot listing are the verbose output, while
the warning is always printed because it is a binding that went nowhere.

It was verified by making every binding in the game take the fallback path for one run - the direct
lookup commented out - which resolved 26 bindings through their suffixes, reconstructed both halves
of every sampler pair, printed `has no binding named Sampler0_wm_texshim; it resolved through the
shader shim suffix to Sampler0 + Sampler0_wm_sampler`, and rendered the title screen correctly. With
the direct lookup in place the fallback has no hits at all, which is what a safety net should look
like: the run that needs it is the run where a plan and a caller disagree about a name.

### The sky came out as one forty-five degree wedge

`SkyRenderer#buildSkyDisc` writes a centre vertex and nine rim vertices forty-five degrees apart,
draws them with `RenderPass#draw(0, 10)`, and binds a `TRIANGLE_FAN` pipeline. wgpu has no fan
topology, and this side only expanded fans for *indexed* draws - which is where Minecraft's
sequential fan index buffer goes. The disc, drawn with no index buffer at all, was forwarded as a
plain triangle list: wgpu paired the vertices up sequentially, so only `(0, 1, 2)` was a real fan
triangle and the rest of the sky was missing. What that looks like in a world is a blue wedge 45°
wide on a white background, which a player reasonably describes as "there is no fog in that
direction" - the sky is where the fog gradient is.

Non-indexed draws are now expanded by topology as well: a fan becomes `(0, 1, 2), (0, 2, 3), ...`
over a generated index buffer, and a run of quads becomes Minecraft's own quad pattern
(`i, i+1, i+2, i+2, i+3, i`). The frame dump made this obvious, which is the whole point of having
one: `node .dsh-tmp/raw2png.mjs` on `frame-N-source.raw` showed the wedge directly.

### A texel of `CloudFaces` is one byte, not one `int`

The clouds were drawn but wrong: a handful of faces near the camera instead of a layer, which reads
as "only the chunk I am standing in has clouds". `CloudRenderer#encodeFace` writes **three bytes**
per face - `cellX >> 1`, `cellZ >> 1`, and a direction-and-flags byte - and
`rendertype_clouds.vsh` fetches `texelFetch(CloudFaces, face * 3 + n)`, so the buffer's texel format
is R8I: one texel is one byte. This side's texel-buffer shim declared the SSBO as `int[]` and read
`inner[index]`, which takes *four* bytes per fetch, so every field after the first came out of the
wrong place and the decoded cell coordinates were nonsense. The shim now declares a `uint[]` and
extracts the byte:

```glsl
ivec4(((int((CloudFaces.inner[uint(index) >> 2u] >> ((uint(index) & 3u) << 3u)) & 0xFFu) ^ 0x80) - 0x80), 0, 0, 1)
```

**And the byte is signed, which was the second half of the same bug.** The first version of this
fetch zero-extended it, on the reasoning that every use of a fetched value in that shader is a mask
or a shift - which is true of the *flags* byte and false of the coordinates: a cell west or north of
the camera has a negative coordinate, so `cellX >> 1` is negative, and zero-extending `-3` gives
`509`. Every one of those cells was drawn about twenty times further away than it should have been,
which put them outside the fog. What was left was the two-by-two cells around the player, drifting
with the cloud offset and snapping back whenever the centre cell changed - the report was "one square
of cloud above my head that bounces", and it is exactly what half a cloud layer drawn 6 km away looks
like. `(byte ^ 0x80) - 0x80` is the sign extension, chosen over a shift pair because it does not
depend on how a backend shifts a signed value.

The tests encode Minecraft's byte layout, decode it back - with negative coordinates now, which the
first version's test never did - and check that the four-byte read the shim used to do gives a
different answer, so a future change to the layout has to fail a test rather than a screenshot.

### The cloud layer was one square, and the reason was `COPY_DST`

Fixing the byte layout was not enough, because the shader was not reading the faces at all. A mapped
write - `mapBuffer` on the JVM side, `write_to_buffer` on this one - is `Queue::write_buffer`, a copy
from the CPU, and wgpu refuses that on a buffer created without `COPY_DST`. Vanilla asks for
`USAGE_COPY_DST` on the buffers it uploads into, but **not** on the ones it maps itself, and the cloud
face buffer is one of those (`USAGE_MAP_WRITE | USAGE_UNIFORM_TEXEL_BUFFER`): 181,824 bytes of faces
were written on the CPU every time the mesh was rebuilt, never copied to the GPU, and the shader read
the zeroes the buffer was created with. Every face decoded to cell `(0, 0)` facing down - one square
of cloud above the player's head, drifting with the cloud offset, and nothing else in the sky. The
buffer creation now asks for `COPY_DST` for anything `MAP_WRITE` as well, since that is how this side
uploads.

Two things came out of it that are worth keeping:

- `write_to_buffer` refuses to write into a buffer without `COPY_DST` and says so at error level. A
  wgpu validation error would end the process instead, and the *silent* version of this - a write
  that never happened - cost an afternoon: the clouds were drawn, from zeroes, with nothing in the
  log to say why. `read_buffer` also stopped refusing buffers Minecraft maps itself, because this
  side's mappings are CPU staging buffers rather than wgpu mappings, so there was nothing to disturb.
- With diagnostics on, a mapped write to a `Cloud*` buffer is **read back and compared** with what was
  sent, once a second. It compares the whole written range rather than a prefix, counts the non-zero
  bytes on both sides, and names the entry point and the wgpu usage flags the buffer ended up with:
  `all 30099 written bytes arrived in Cloud UTB #1 (30099 of them non-zero); created via createBuffer,
  blaze usage MAP_WRITE|UNIFORM_TEXEL_BUFFER, wgpu usage MAP_WRITE|COPY_DST|STORAGE|COPY_SRC`. A mesh
  that arrives with its second half missing passes a 16-byte check, and the second half is what draws
  the rest of the layer.
- The same report decodes the buffer as **faces**, which is what the shader will make of it:
  `Cloud UTB #1 holds 10033 faces: x -86..86, z -86..86, 0 beyond 120 cells, 48 marked inside;
  directions down=9985 north=8 south=6 west=17 east=17; first [0,0 down inside] ...`. A mesh of real
  cells and a mesh of zeroes look identical in a screenshot - both are one square of cloud above the
  player - and this is the line that tells them apart without a GPU debugger.
- A draw that reads further than the write reached warns, once a second: `a draw reads 9987 faces
  (29961 bytes) from Cloud UTB #2 but only 120 bytes were uploaded into it`. The texel buffer is bound
  as a storage buffer here, so the draw's range and the upload's range are two numbers this side has
  and can compare, and "the shader is reading stale faces" stops being invisible.

### The same usage mask made three different buffers

The `COPY_DST` fix above was made in `create_buffer`, and `create_buffer_init` and
`allocate_gpu_buffer_mapped` had their own copies of the same translation. They had drifted:
`create_buffer_init` never translated `USAGE_UNIFORM_TEXEL_BUFFER` to `STORAGE`, so a texel buffer
created with initial contents was a buffer no texel-buffer bind group could bind, and
`allocate_gpu_buffer_mapped` ignored the usage mask it was handed altogether and created
`MAP_READ | MAP_WRITE`. Any of those is a wgpu validation error - which ends the process here - and
the bug only hides as long as vanilla happens not to take that path. There is now one function,
`wgpu_buffer_usages`, and all three entry points call it; the mapped one adds `MAP_WRITE` on top,
because `mapped_at_creation` requires it, and drops `MAP_READ` when the mask asks for `COPY_SRC`,
because wgpu rejects that pair.

The JVM side can ask what a buffer ended up with (`buffer_usages`), which is what puts the flags into
the log lines above: the mask the JVM passes is not the mask the buffer has.

### A bounds check naga will not compile

An out-of-range `texelFetch` is undefined in GL, and until the sign extension was fixed this side's
texel-buffer shim was reading one: the fetch clamps its index in the natural way,

```glsl
ivec4(((uint(index) >> 2u) < uint(CloudFaces.inner.length()) ? byte : 0), 0, 0, 1)
```

and that shader does not compile. naga 29's GLSL front end lowers `.length()` on a runtime-sized
array member to a `Load` of the array rather than a pointer to it, and its validator answers
`InvalidPointerType` - in an expression, in a local, in a comparison, and in an `if` condition alike;
all four were tried. A shader module that fails validation is a wgpu error, which is fatal here, so
the first run with the clamp in it died in `create_shader_module` with the process's own crash log.

The clamp is left out, and it costs nothing: **WebGPU requires robust buffer access**, so an
out-of-bounds read of a storage buffer reads zero rather than the word after the mesh, which is what
the clamp was there to guarantee. `a_length_bounds_check_is_still_impossible` runs naga over the
idiom and fails if a later version starts accepting it, so the clamp goes back in when it can. The
test that would have caught the crash - `the_rewritten_texel_fetch_survives_naga` - runs the
rewritten fetch through naga's front end and validator, because "this is valid GLSL" and "naga
compiles this" are two different claims and only the second one keeps the game running.

### The mesh is rebuilt when the cell changes, and that is what vanilla does

`CloudOffset` is written into the `CloudInfo` uniform every frame, but the face buffer is only
rebuilt when the *cell* the camera is over changes, when the camera moves above or below the layer,
when the cloud status changes, or when the renderer asks for it - `CloudRenderer#render` compares
`cellX`/`cellZ` against the previous frame's, and `MappableRingBuffer#rotate` moves to the next of
three buffers for the new mesh. A cell is 12 blocks and the drift covers one in 400 ticks, so at a
walk that is a rebuild every few seconds and a stationary camera rebuilds every twenty, all of it
intended: the offset moves the layer smoothly and the mesh only has to be laid out again when a whole
cell has been crossed. What would *not* be intended is a frame drawn with a mesh from a different
cell, and that is what the draw-range warning and the face decode above exist to catch.

### The cloud ring was rebuilt every frame, because the buffer reported the wrong size

`COPY_DST` made the faces arrive, and the faces were a layer - the diagnostics read the buffer back,
decoded it, and printed `Cloud UTB #1 holds 9783 faces: x -85..85, z -85..85`. The sky stayed empty
anyway, one square of cloud over the player's head at best, flickering.

`GpuBuffer#size` is not a description, it is an *interface*: `CloudRenderer#render` compares
`this.utb.currentBuffer().size()` against the `utbSize` it computed, and rebuilds the ring of three
face buffers whenever they differ:

```java
if (this.utb == null || this.utb.currentBuffer().size() != utbSize) { ... this.utb = new MappableRingBuffer(...); }
```

wgpu needs buffer sizes to be a multiple of 16, and this side rounded every size *up* before handing
it to `GpuBuffer` - so the vanilla `utbSize` of 181,818 was reported back as 181,824, the comparison
never matched, and **the ring was closed and recreated on every frame of every cloud draw**. A
recreated ring is three freshly created, zeroed buffers, and on a frame where the cell did not change
nothing is written into them: the draw bound a buffer of zeroes, every face decoded to cell `(0, 0)`
facing down, and the whole layer collapsed into one square that drifted with the cloud offset. On the
frames where a rebuild did happen, the mesh was written into the new buffer and the layer appeared -
which is the flicker.

The rounded size now goes to wgpu and the size Minecraft asked for is what the buffer reports. The
diagnostic that names it exists because a buffer created sixty times a second is not visible in a
screenshot: `created buffer Cloud UTB #0 (181818 bytes asked for, 181824 bytes allocated)`, once for
the run instead of three times a frame.

Two smaller alignment bugs came out of the same change, both of which used to end the process with a
wgpu validation error rather than a wrong picture:

- **A readback has to be aligned at both ends.** `read_buffer` copies through a scratch buffer, and
  the diagnostic that verifies a mapped write asks for exactly the bytes that were written - which is
  `3 * faces`, i.e. a multiple of four only when the face count is. `Copy size 29349 does not respect
  COPY_BUFFER_ALIGNMENT` took the client down, and once that was fixed, `map_async` answered
  `range_size 29349 must be multiple of 4`. The copy is widened to alignment - offsets included - and
  the caller still gets exactly the bytes it asked for.
- **A bind group's buffer binding size has to be aligned too.** `setUniform` rounds a slice's length
  up to 16 and then clamps it to what is left of the buffer, and the clamp is what put the unrounded
  181,818 back: `Effective buffer binding size 181818 for storage buffers is expected to align to 4`.
  Rounding *down* to the four-byte alignment is what satisfies both a storage binding and a uniform
  block, and it can never run past the end of the buffer, which is what the clamp was for.

### What this side hands out, and what brings it back

Every one of these is a `Box` whose address the JVM holds, and a missing `drop` on the far side
leaks it and everything it owns. Blaze3D is not much help: `CommandEncoder` and
`CompiledRenderPipeline` are neither of them `AutoCloseable`, and `RenderPipeline` has no `close`
either, so two of these lifetimes had to be invented rather than implemented.

| Native object | Held by | Released by | Notes |
| --- | --- | --- | --- |
| `wgpu::Texture` | `WgpuTexture` | `close` → `drop_texture` | Minecraft closes its textures; the tombstone keeps the label for a later use-after-close |
| `wgpu::TextureView` | `WgpuTextureView` | `close` → `drop_texture_view` | |
| `wgpu::Buffer` | `WgpuBuffer` | `close` → `drop_buffer` | kept alive for two seconds afterwards, because Minecraft writes to the cloud buffer after closing it |
| `wgpu::Sampler` | `WgpuSampler` | `close` → `drop_sampler` | |
| `wgpu::CommandEncoder` | every `CommandEncoder` handle | nothing: there is one for the session | Minecraft makes three a frame and never frees any |
| `BlazePipeline` ×2 | `WgpuCompiledRenderPipeline` | `clearCaches`, or a cleaner → `drop_pipeline` | one per depth state; `clearCaches` is what Minecraft calls on a resource reload |
| `wgpu::BindGroup`s | one set per plan per pipeline, owned by the pass or the cache | when the pass closes, or when the cache evicts the least recently used set past 256 | see below |
| `wgpu::RenderPass` | `WgpuRenderPass` | `close` → `drop_render_pass` | with the draw call buffer, which goes back to the thread that owns it |

The caches are all bounded, and they report their size every 120 frames with the diagnostics on
(`… 184 pipelines, 486 tombstones, 0 fan + 0 quad index buffers`):

| Cache | Bound | Behaviour |
| --- | --- | --- |
| compiled pipelines | Minecraft's own pipeline set, one depth variant at a time | reused per `RenderPipeline`; freed on a reload; the other depth variant is compiled when a pass first needs it - see "One depth variant, compiled when a pass asks for it" |
| shader sources | the shader variants in use | reused per `(id, stage, defines)`; cleared with the pipeline cache |
| driver pipeline cache | one file per adapter, ~1 MB | wgpu's `PipelineCache`, written by Vulkan; nothing to persist on DX12 |
| dead-texture tombstones | 512, oldest first | a tombstone only has to outlive the frame that closed the texture |
| placeholder textures | one per format, grown on demand | the stand-in for a closed texture; a window resize used to leave a full-size texture behind at every size it had ever been |
| fan and quad index buffers | 64 sizes, then emptied | rebuilt on demand; the buffers are a few kilobytes |
| quarantined buffers | 500 ms or 32 MB, oldest evicted first | a byte budget rather than a count: the entries are megabytes each, and a count of them was hundreds of megabytes |
| `CommandEncoder` | 1 | see above |
| diagnostics sets | once per name, or one entry per second | the readback budget keeps its "already warned" second in a field, not in a set that grows once a second for the whole session |
| interned names | one per name the game asks for | see "A name is encoded once" below; the set follows the loaded assets |
| draw call buffers | one per pass open at a time, per thread | pooled rather than allocated; see "One downcall per draw" |

A resource reload is the test for the pipeline half: three of them in a row leave the count at 184,
where it used to climb by 184 each time.

### Bind groups are built per draw, and the cache that was meant to stop it

A bind group was built for every draw and freed as soon as the draw call returned, which the
counters make plain:

```
render stats: … 244945 draws (244945 bind groups built), …
```

That is bounded - the live count is zero whenever it is sampled, and `BindGroups_`'s `Drop` is what
decrements it - but it is one `wgpu::BindGroup` per draw, sixty to eighty thousand a second at the
title screen. The obvious fix is to reuse the last set while nothing has been re-bound, so the pass
now owns its set and rebuilds it only when something changes. Then the measurement that says whether
that was worth anything:

```
wgpu: 81025 draws, 81025 of them had to rebuild their bind groups
```

Every draw. Minecraft re-binds at least one uniform for every draw it makes - a chunk section's
matrix, an entity's transform, a GUI element's pose - and a bind group bakes the buffer *offset*
into itself, so no two of them are alike. The reuse path is therefore correct and never taken, which
is what **dynamic offsets** are for: one bind group per pipeline, with the moving offset passed at
bind time. `min_uniform_offset_alignment` is exported for it and Minecraft's uniform ring already
aligns its slices to it.

The offset was then left out of the cache key, so that a set could be shared between draws that
differ only in it, and the reuse arrived - 99.96% of draws served from the cache - **with the frame
disassembled**: the world came out as a checkerboard of dark tiles, the hotbar was a cyan smear, and
every slot of a sprite atlas held the same sprite. See "The cache was keyed on the wrong address"
for what that turned out to be.

The same pass found the buffer quarantine holding far more than it needs: the use-after-close it
guards against happens *within a frame*, so two seconds and a thousand entries became half a second
and a hundred and twenty-eight, and the title screen's 162 quarantined buffers - 676 MB live between
them, because Minecraft's uniform rings are megabytes each - became 57.

A count was still the wrong unit, because the entries are not one size. It is a **byte budget** now:
half a second or 32 MB, oldest evicted first, and the stat line says how much is held rather than
only how many. In the runs since, a world holds 100-odd quarantined buffers in 5-25 MB, where the
same count could have been hundreds of megabytes. A single buffer larger than the budget still gets
its quarantine - dropping it would mean the write Minecraft is about to make hits a buffer that is
already gone, which is the fatal case this exists for - so the budget is a target, not a hard cap.

### The cache was keyed on the wrong address

The bind group cache is keyed by a hash of everything a set is built from, and for a buffer that was
`buffer as *const wgpu::Buffer` - the address of the `wgpu::Buffer` *the builder's map holds*, not of
the buffer. It is a clone of the handle Rust was handed, stored under the binding's name, so its
address is the address of that map slot: one per name, reused by every draw that binds that name,
**whatever it binds to it**.

That is fine while a name always means the same resource. It is not fine for the sprite atlas, where
Minecraft binds a different texture view to `Sprite` for every sprite, or for the uniform buffers it
hands out one per draw: the key stayed the same, so the cache answered with the set built for the
*first* one - a set holding the first sprite's view and the first draw's buffers - and every draw
after it sampled the wrong texture. Which is exactly what the picture showed: one sprite copied into
every slot of the atlas, terrain tiles drawn from a single wrong uniform, a hotbar that was one
stretched quad.

The old key escaped this in baked mode by accident: the uniform offset was part of it, and Minecraft
hands out a fresh (buffer, offset) pair almost every draw, so the keys almost never repeated - 0.6%
of draws were cache hits, and those few were the ones that could go wrong. Leaving the offset out of
the key, which is the whole point of dynamic offsets, turned that rare collision into the common
case.

The identity is now the pointer the JVM handed over - the address of the box Rust gave it, which
lives exactly as long as the resource does. It is also, and this is the point, the address
`invalidate_bind_group_cache` is called with when something is freed, so the cache and the
invalidation finally agree about what "the same buffer" means. Two smaller things came with it:

- `reflected_block_sizes` read `variable.name` out of naga, which is empty for a GLSL
  `layout(std140) uniform Block {...};` - the block's name is on the *type* - so it reflected nothing
  at all and every layout went out with `min_binding_size: None`. With the fallback in place the
  layouts declare the size the shader reads, which is what the plan was written to be able to do.
- The trace behind `wgpu-trace-dynamic-offsets` now prints what the plan decided per binding
  (identity, offset, size, dynamic or baked), which is how this was found at all.

Measured in a world afterwards, with dynamic offsets on and the frame correct:

```
render stats: 327680 draws (120 bind groups built, 327560 cache hits and 120 misses)
```

One hundred and twenty bind groups built for a third of a million draws, where it used to be one
per draw. The switches: `wgpu-no-dynamic-offsets` bakes the offsets instead, `wgpu-no-bind-group-cache`
builds a set per draw, and `wgpu-dynamic-offset-names` restricts the feature to a list of uniforms.

### One downcall per draw, and bindings by slot

The cache above made the bind groups rare; it did not make the *draw path* short. Every binding was
still its own downcall - `bind_render_pipeline_to_pass`, `bind_buffer`, `bind_texture_and_sampler`,
`set_index_buffer`, `set_vertex_buffer`, then `draw` or `draw_indexed` - and behind each one:

- the JVM allocated a `Box` for the pass's binding builder and one per binding resource, and the
  binding resource owned its name, so every binding copied a `String` into native memory;
- the native side hashed that name into a map per binding per draw, and `drop_bind_groups` freed
  the previous set on the way;
- the pass's bind groups lived behind a `Mutex`, the cache key was built with `DefaultHasher`
  (SipHash, chosen for HashDoS resistance in a `HashMap` that never sees untrusted input), and the
  cache's LRU counter was a global `fetch_add` - two atomics and a lock acquisition per draw, all
  to protect a structure only the render thread could reach.

None of that was measurable in a profile until it was gone, which is the usual argument for not
having written it. What replaced it:

**The plan is read once.** `pipeline_bindings` fills a caller-supplied array with one entry per
binding - the name, the declared name, the set, the binding number and the kind - and the JVM turns
that into a slot table per compiled pipeline (`PlanBindings`). From then on a binding is an index:
`bindTexture("Sprite", …)` resolves to two slots once and is written into them, and nothing on the
draw path looks a name up.

**A draw is one call.** `draw_call` carries a `DrawCall`: the pipeline, up to eight vertex buffers
with a presence mask, the index buffer and its format, up to 32 bindings by slot, and the four draw
parameters. The JVM writes into a buffer it owns as things are bound and fills in the parameters at
the draw, so the native side learns the whole state of the draw at once and applies it - pipeline,
vertex buffers, bind groups, index buffer, draw - in that order.

**The pass owns its bind groups.** `BlazeRenderPass` keeps the set it last bound, the key it was
built for, and the dynamic offsets per group. A draw that hashes to the key the pass already holds
reuses the set and hands over new offsets, which is the common case and involves no cache lookup at
all; the cache is consulted only when the key changes, and it answers with an `Arc` the pass keeps.
`drop_bind_groups` is gone: the last reference to a set is released when the pass closes or the
cache evicts it. Both cache maps are now thread-local `RefCell`s, keyed with `FxHasher`, and the
tick counter that finds the least recently used entry is a plain field. What still needs an atomic
is invalidation from *another* thread - the cleaner that frees a collected pipeline cannot name
entries in a cache it does not own - so it bumps an epoch and the next lookup on the render thread
drops everything.

**A name is encoded once.** `NativeNames` interns the UTF-8 of every name into one `Arena.global()`
instead of an `Arena.ofConfined()` per call, which is what a buffer or texture label used to cost.
It reports its size every 4096 names, because the argument for an unbounded intern table is an
argument about where the names come from: they are labels Minecraft wrote by hand or derived from
asset names, and the few that carry a number carry a bounded one (`"… animation frame 3"`,
`"UberBuffer solid 7"`).

**The draw call buffer is pooled.** A pass wrote into `arena.allocate(DrawCall)` from an arena it
then closed - an arena, an allocation and a close per pass, tens of times a frame. A closed pass now
returns its buffer to its thread's pool and the next pass takes it back, clearing it first, since a
reused buffer otherwise still holds the previous pass's bindings, vertex buffer mask and index
buffer. Nested passes each take their own, so the pool holds one buffer per pass open at a time.

Measured in a world, with the diagnostics on:

```
render stats: 2160 render passes (0 of them empty, last had 1 draws), 2760 pipeline binds, 385320
draws (120 bind groups built, 2760 cache hits and 120 misses), 850193400 vertices
live resources: 774 textures (77 MB), 793 views, 98 buffers (418 MB, 12 quarantined), 1 encoders,
0 passes, 76 bind groups, 196 pipelines, 412 tombstones, 1 fan + 0 quad index buffers
```

The `pipeline binds` counter is the one that shows what the pass keeping its state is worth: 2760
binds for 385320 draws, one per hundred and forty draws, where every draw used to bind its pipeline.
The `cache hits` number *fell* from 327560 to ~2700 for the same reason - a draw that reuses the set
its pass already holds does not consult the cache at all - and 120 bind groups are still built per
interval, one per plan per pipeline. Nothing in the renderer frees a bind group per draw any more,
which is what the stable bind group count across a resource reload (`F3+T`) is: the cache drops, the
pipelines are freed, and both come back to the same size.

### One depth variant, compiled when a pass asks for it

A pipeline needs two `wgpu::RenderPipeline`s: a pass with a depth attachment cannot use one without
a depth-stencil state, and a pass without one cannot use a pipeline that has it. This side built
both, for every pipeline, the moment Minecraft precompiled it - and Minecraft precompiles *every*
pipeline it ships with, in `ShaderManager#apply`, whether or not a frame ever draws with it.

What made that expensive is not the driver's pipeline creation alone. `compile_render_pipeline`
preprocesses the GLSL, reflects it with naga, creates the shader modules and numbers the plan, and
all of that is the same for both variants; only `depth_stencil` differs. Doing it twice per pipeline
doubled the slowest part of startup for a variant that half the pipelines never used.

So a compiled pipeline is now one variant, plus a `PipelineRecipe`:

- `compile_render_pipeline` builds the recipe - shader modules, pipeline layout, vertex buffers,
  colour targets, topology, cull - and writes **one** pipeline from it. The JVM asks for the variant
  the binding pass needs (`precompilePipeline` asks for the one without depth state, since it does
  not know); the recipe is kept on the pipeline, so the shader work is over.
- The other variant is written by `create_pipeline_variant(pipeline, depth_state)` the first time a
  pass of the other kind draws with it: a `render_pipeline` from the modules and layout already in
  hand, and nothing else. That is also the call the pipeline cache accelerates.
- `drop_render_pipeline` takes a *nullable* pointer, because only one of the two slots may ever have
  been filled: freeing a compiled pipeline frees whichever variants exist.
- The plan is shared by both variants rather than numbered twice, and the ABI exposes a
  `variant_keys`-style single entry point rather than two descriptor compiles.

```
live resources: ... 85 bind groups, 115 pipelines, 500 tombstones, ...     ← 98 pipelines × 1 variant
                                                                              + the depth variants
                                                                              actually drawn with
```

**The driver's own cache is persistent.** `RenderPipelineDescriptor::cache` was `None`; it is now a
`wgpu::PipelineCache` created with the device, seeded from a file beside the config
(`wgpu_pipeline_cache_vulkan_<vendor>_<device>.bin`, the key wgpu's own `pipeline_cache_key`
suggests) and rewritten every 16 pipelines through a temporary file and a rename. Where it helps is
where the driver lets it: Vulkan implements it as `vkPipelineCache`, and DX12 has no serialisable
form at all - its adapter does not even offer the `PIPELINE_CACHE` feature - so on DX12 there is no
cache and no file, which the log says once instead of leaving a missing file unexplained:

```
wgpu-mc: pipeline cache: 1.2 MB written to ...\wgpu_pipeline_cache_vulkan_4318_11544.bin (started from 1.2 MB)
wgpu-mc: this device has no pipeline cache; every launch compiles every pipeline     ← the DX12 run
```

The two lines are one report: `report_pipeline_cache_once` runs from the first `log_render_stats`,
because the cache is created during mod construction, before the logger is up. The Vulkan numbers
above are a second launch - the first one wrote 1.29 MB, and the next one started from it and grew
no further, which is what "the driver reused its compilation" looks like from here.

The counter that says whether the laziness worked is `live resources`' pipeline count: 115 for a
world session, where compiling both variants of every precompiled pipeline put it at 196.

### Neither compiler checks the two bridges, so a test does

`WgpuNative.kt` names the native methods it wants, `WmNative.kt` names the C entry points and the
byte offsets of every struct field, and Rust names what it actually exports. Neither language knows
about the other's list, and every way the two can disagree is a *runtime* failure rather than a
compile error:

- an `external fun` with no `#[jni_fn]` throws `UnsatisfiedLinkError`, which is an `Error` - not
  something `catch (e: Exception)` sees - the first time that code path runs, which may be minutes
  into a session;
- a `handle("name", ...)` naming a symbol the cdylib does not export throws from
  `SymbolLookup#find` at whatever moment the handle is first touched;
- a field offset that drifted reads whatever happens to live at that byte instead, which shows up as
  a wrong colour or a wrong blend factor, not as an error.

`rust/wgpu-mc-jni/src/abi_tests.rs` therefore reads both Kotlin files, with `include_str!` (so
editing either one re-runs the test in `cargo test`), and checks them against this crate:

- every `external fun` in `WgpuNative.kt` has a `#[jni_fn]` here, with the same JVM argument count
  (Rust's two leading parameters are the `JNIEnv` and the class, so they do not count);
- every `handle("name", ...)` in `WmNative.kt` is a `#[no_mangle] extern "C"` export with the same
  argument count, and no export is left unbound;
- every `MemoryLayout.structLayout(...)` in `WmNative.kt` puts its fields where `offset_of!` puts
  them and is the size `size_of` reports - trailing `paddingLayout` entries included, which is how
  the 24-byte `BlazeColorTargetState` is spelled out - and the named `*_OFFSET` constants agree with
  the layout they describe;
- every `GpuFormat`, `UniformType`, `BlendFactor`, `CompareFunction` and `PrimitiveTopology` number
  is the one the Rust enum assigns, matched variant by variant, so adding a variant to either side
  fails the test rather than the frame.

The first run of that check found four things, all now deleted:

- **21 `external fun`s that nothing implemented.** They were the GPU-object half of the old JNI
  layer - `createTexture`, `createBuffer`, `createCommandEncoder`, `createBufferInit`,
  `dropTexture`, `dropBuffer`, `presentTexture`, `submitEncoders`, `getMaxTextureSize`,
  `getMinUniformAlignment`, `getRenderPassCommandSize`, `render`, `setCamera`, `setMatrix`'s
  neighbours `getMouseX`/`getMouseY`/`setCursorPosition`/`setCursorMode`, `setAllocator`,
  `updateWindowTitle`, `getTextureId`, `destroyPaletteStorage` - and Rust stopped exporting them
  when the C ABI took over that job. One of them was live: `WgpuTextureManager#getTextureId` called
  `WgpuNative.getTextureId`, so the class went with it. The file's own doc comment claimed these
  "mirror functions that still exist on the Rust side", which was exactly the assumption that made
  them dangerous.
- **Six `#[jni_fn]` implementations that nothing declared**, left over from the design where Rust
  owned the world: `setSectionPos`, `reloadStorage`, `bindRenderEffectsData`, `setLightmapID`,
  `clearEntities`, `setEntityInstanceBuffer`. Nothing in the crate called them either, so they could
  not run at all.
- **`extract_directives`**, a `#[no_mangle]` GLSL-directive dump from before the JNI layer, which
  nothing called and nothing bound.

What is left is 26 JNI declarations and 52 C-ABI bindings, each with an implementation on the other
side, and the test keeps it that way. Ten of the 26 are still only reachable from the old
Rust-renders-the-world path (`bakeSection`, the palette calls, `setMatrix`, `cacheBlockStates`,
`setWorldRenderState`, `createWmRenderer`): they resolve, and nothing in the backend calls them,
because chunk baking and skinning happen on the JVM side now. `cacheBlockStates` is the exception -
`TitleScreenMixin` starts a thread with `WgpuNative::cacheBlockStates` so the block-state colours are
baked while the title screen is up.

### Nothing ever culled a back face

Leaves and every other translucent surface had black ghosts in them and flickered while the camera
moved. Leaves are single-sided quads: in fancy graphics Minecraft meshes all six faces of every
leaf block, including the ones between two leaf blocks, and leaves it to `glCullFace(GL_BACK)` to
throw away whichever half of them faces away from the camera. This side never turned culling on.
The ABI had no field for it - 26.1's `RenderPipeline#isCull` was simply not forwarded - and both
pipeline-creation sites hardcoded `cull_mode: None`, so every quad was rasterized front *and* back.
On an opaque quad the second copy is hidden behind the first; on a blended one the two land at the
same depth, where which of them survives the depth test is not defined, and the back copy is shaded
with the far side's lighting. That is the ghosting, and the flicker is the same coin landing
differently as the view moves.

`cull` is now a field of the ABI's `RenderPipeline` (offset 80, `PIPELINE_CULL`), filled from
`RenderPipeline#isCull()` - which defaults to *true*, so a pipeline only gives up culling by asking,
exactly as `GlCommandEncoder` answers it with `GlStateManager._enableCull()`.

The winding is the part worth writing down, because getting it backwards deletes the world instead
of fixing it. The vertex shaders flip `gl_Position.y` to emulate GL's clip space (see
`EmulateGlClipSpace` in `rust/wgpu-mc-jni/src/preprocessing.rs`), and that mirror also mirrors the
winding: a triangle OpenGL calls counter-clockwise - and therefore front-facing - arrives at wgpu
clockwise. So the pairing that reproduces `glFrontFace(GL_CCW)` + `glCullFace(GL_BACK)` is
`front_face: wgpu::FrontFace::Cw` with `cull_mode: Some(wgpu::Face::Back)`.

`Ccw` was tried first, on the reasoning that "counter-clockwise in framebuffer coordinates" was
what the flip restored. It is not, and the way it fails is worth recognising: a single-sided quad
has no back face to fall back on, so culling the front ones removes the surface entirely. The world
came out as a pale blue sky with the sky box's dark lower half where the ground should have been,
the block outline still drawn around nothing, and the held item in the corner. The frame dump
(`wgpu-dump-now`, then `node .dsh-tmp/raw2png.mjs <frame>.raw bgra`) showed it as a scanned row of
`184,210,255` where the terrain used to be.

**One thing this does not forward is `PolygonMode`.** The single pipeline that asks for `WIREFRAME`
(`pipeline/wireframe`, the chunk-section debug view) still draws filled, because wgpu needs
`Features::POLYGON_MODE_LINE` for it and this device does not ask for that feature.

### The write marks were forgotten by the submission that needed them

Entities rendered as nothing but their own hitboxes - ten mobs standing in a row on screen, ten white
wireframes and empty grass behind them - while the terrain, the HUD and the held item were all
correct. Every layer underneath said the draws were fine: the per-draw trace showed `entity_cutout`
binding `creeper.png` for the creeper and `zombie.png` for the zombie, the native side saw the same
views in the same slots in the same order for 500,000 draws, the skins were uploaded (a dump of each
one holds a picture), and the shadow pass bound `shadow.png` in all 7,776 of its draws.

What was wrong was *when* the vertex data arrived. `VertexFormat#uploadToBuffer` keeps **one** vertex
buffer per pipeline and reuses it for every immediate draw of that pipeline, rewriting it at offset
zero with `CommandEncoder#writeToBuffer`. This backend answers that call with `Queue::write_buffer` -
which wgpu applies *at* a submission, ahead of every command in it. So a rewrite has to be ordered
against the draws already recorded that read the old bytes, and that is what the write high-water
marks are for: a write at or below a buffer's mark submits the frame first, so the old content is
consumed before the new content lands.

The bug was one line: `forget_write_marks()` ran inside `flush_shared_encoder`, on the reasoning that
"nothing is left in flight" after a submission. That holds for the writes *in* that submission and
forgets the one thing that still matters - the rewrite that forced it, which is sitting in the queue
waiting for the *next* submission. With the marks cleared, only the **first** rewrite of a frame was
ordered: the second rewrite saw a mark of zero, decided its bytes were untouched, and travelled with
the frame, where wgpu applied it before every command in it. Every draw of the batch then read the
**last** write's geometry. Entities were drawn with each other's vertices - models on top of each
other, or a model with another model's shape - and the GUI item atlas drew all of its slots with the
last item's geometry, which is what "the icons are scrambled, a pressure plate has a bed's face"
looked like. It also explains the shadow complaint: a shadow quad carrying an item's geometry is a
shadow *shaped* like an item.

The marks now survive a submission and are dropped with the buffer they describe (`drop_buffer`),
which is what makes the queue-write path correct - and it is now the *fallback* path, because the
cost of it was a submission per rewrite: ~3,200 a second in a scene with 150,000 draws, since
Minecraft keeps one immediate vertex buffer per pipeline and rewrites it at offset zero for every
draw of a batch.

### The uploads travel in the command stream now

A queue write is applied *at* a submission, so it has to be ordered by when the submission happens. A
copy is a *command*, so it is ordered by where it was recorded - which is the property the uploads
wanted all along. `write_to_buffer` now stages its bytes in a ring of four 4 MiB buffers and records a
`copy_buffer_to_buffer` into the frame's own encoder:

- the bytes reach the staging buffer through the same `Queue::write_buffer`, which is free here: the
  staging region is one nothing has ever read, so "applied first, before the frame's commands" is
  exactly where it belongs, and the copy that consumes it is a command that comes after it;
- the copy lands in the command stream in recording order, so a draw recorded *after* it sees the new
  bytes and a draw recorded *before* it sees the old ones - the same guarantee the forced submissions
  bought, at no submission at all;
- a staging region may only be reused once the submission reading it has finished, which is what the
  ring is for: a buffer is handed out again only after every submission that could still be reading it
  has completed, and `Queue::on_submitted_work_done` is what says so. Four buffers is three
  submissions of slack.
- the buffers are **not** mapped. The first version created them with `mapped_at_creation`, and wgpu
  ended the process with "Buffer with 'wgpu-mc staging' label is still mapped" - a mapped buffer may
  not be the source of a copy. It does not need to be, because the write goes through the queue.

Measured in the same scene: **3,201 submissions a second became 120** (two a frame, the frame's own
and the readbacks), 13,000 uploads and 17 MB a second, and no fallback to the queue-write path. The
ring running dry is the one case that still falls back, and the counters in the render-stats line say
so - `uploads: N staged (M MB), K fell back to queue writes` - which is what a scene with a larger
upload rate than four buffers can absorb would show.

`Queue::write_texture` still submits before it writes, for the reason the buffer path used to: a
texture write names a mip, a layer and a rectangle rather than a byte range, so there is no offset to
compare against a mark. Texture uploads are rare (a resource load, a skin, a font) and cost two
submissions a frame between them, so the same treatment is a `Known gaps` item rather than a fix that
has to land with this one.

### A marker file that puts a scene in front of the renderer

The corruption above could not be reproduced in a run that had nothing in it, so a marker-gated hook
put the missing thing into the world: `wgpu-reload-resources` calls `Minecraft#reloadResourcePacks`
twenty seconds into the run, because "after I refreshed" is F3+T, and every atlas and skin a reload
rebuilds is a texture that was closed and re-created while the renderer held pointers to it.

Two more hooks were written for the same run and have been **removed again**, because they changed the
game rather than merely observing it: one filled the hotbar with nine items on join (26.1 draws every
item icon through `GuiItemAtlas`, so an empty hotbar means that atlas is never used at all - "the icons
are wrong" was unreproducible without items to draw) and one spawned ten species with ten distinct
skins in front of the player and put the ring back whenever `/kill @e` cleared it. They are worth
writing down as a technique, not as code to keep: a run that starts with an empty inventory and no
mobs nearby cannot show a bug in either, and the fix that matters was found with them. What is left of
them in the world is nine items in the hotbar of the run directory's copy of the world, and the marker
files are gone, so nothing puts them back.

The two per-draw traces stay, and they are what closed the binding layer as a suspect: `wgpu-trace-plan`
names a pipeline family (with the `binding_verbosity` option turning
them on), and they print the texture every slot of every draw carries on both sides of the ABI, in draw
order, from the JVM's label registry and from the native side's view registry.

### The sun and the moon were both up

Fly high enough and the sky shows the sun and the moon at once. 26.1 draws both bodies
unconditionally - `SkyRenderer#renderSunMoonAndStars` rotates one to the sun's angle and the other to
the moon's - and leaves the one that has set to be hidden by the world: the sky pass paints the
hemisphere *above* the horizon and the ground covers the rest. Above the ground there is nothing left
to do the hiding, so the body that should have set is simply still there. Vanilla has the same hole;
nothing in the sky pass closes it.

`SkyRendererMixin` skips the two draws when the body they would place is under the horizon. The angle
is not a parameter of `renderSun`/`renderMoon` - the caller has already applied it to the pose stack -
so the test is on the rotation itself: a body is a quad at `(0, 100, 0)` in model space and the matrix
is a rotation about X by that body's angle, whose `m11` is the angle's cosine. Positive is above the
horizon, negative is below it, and the sky's transform carries no camera pitch to confuse the two. The
threshold is `-0.1` rather than zero because the sun's quad is thirty units across at a distance of a
hundred: cutting a body off when its *centre* crosses the horizon would make it vanish with a third of
its disc still up, which is a snap the sunset does not have.

The sunrise and sunset glow is a separate pass (`renderSunriseAndSunset`) and stays: it is what the
horizon looks like at dawn, not a body in the sky. The stars were already conditional on
`starBrightness`, and the dark disc below the horizon is still vanilla's - it is drawn when the eye is
under the world's horizon height, which this does not touch.

`wgpu: the moon is below the horizon, so it is not drawn` - and the same for the sun - is logged on
each transition, so a run says which body is being skipped rather than leaving it to a screenshot. It
alternates sun, moon, sun over a day, which is the whole of the test.

### The arena filled up, and the world stopped changing

This is the one that looked like a modelling bug and was not. The report was a huge mushroom that was
not drawn at all, and - the half that gave it away - breaking a block did not change the terrain, with
`F3+T` no better. Nothing about that says "rotation": it says the geometry on screen stopped being
replaced. The terrain line from that run says why, in two numbers side by side:

```
… 231 section(s) refused by the arena, … ; 231 refused section(s) drained
```

`231` sections the section feed had baked, handed to Rust, and been told there was no room for. The
arena is a fixed pool of `u32` slots inside one buffer, and it was sized from a guess about what a
section costs - `SLOTS_PER_SECTION = 8_000`, 32 KB, "a few hundred quads". One quad is **twenty-two**
slots (four vertices of four words, then six indices), so 8 000 is 364 quads, and the shell of a single
full cube is 1 536 quads: 33 792 slots, four times the estimate, before the cutout layer draws one
plant. At 16 chunks - what that run was set to - the pool came out at 32.9M slots, 131 MB, and a world
of ordinary surface geometry fills that in seconds. Hence the other number: the first refusals arrived
about four seconds after joining, and the count kept climbing to the end of the session.

What a refusal *does* is what made it look like something else entirely. `SectionStorage::allocate`
leaves a section it cannot replace exactly as it was, deliberately - stale geometry is a wrong picture,
no geometry is a hole - so a refused section keeps drawing its old mesh and the next rebuild of it is
refused as well. The world stops updating, section by section, and the only visible symptom is that
blocks do not change when you break them. The mushroom was a section that never got a mesh at all.

Three things were wrong behind that, and one of them made it permanent:

- **The refusal leaked the pool.** A section is several layers, each of which allocates a vertex range
  and an index range. When a later layer did not fit, the layers that *had* fit were dropped along with
  the section - and their ranges were never given back, because the allocator still counted them as
  handed out. Every refusal cost the pool the geometry that did fit, so a session that refused a section
  here and there ended up refusing everything. `SectionStorage::refuse` gives them back now.
- **The estimate was four times too small.** See above; `SLOTS_PER_SECTION` is 16 000 with the
  arithmetic written next to it, and the placeholder the arena is created with (`ARENA_SLOTS`) is now
  the *small* one on purpose - the game reports its render distance before anything is baked, and that
  report is what sizes the pool. It is **20 000** since the water was taken over, which is a fifth more
  and the same ratio vanilla gives its translucent section buffer; see "The water was baked and never
  drawn" for why.
- **Nothing grew the arena, and nothing retried the section.** A fixed pool and a render distance that
  is a slider do not have to agree, and the two ends of that disagreement were both missing. The arena
  now grows: `RangeAllocator::grow_to` extends the pool at the *end*, so every range already handed out
  keeps its offset and growing is one `copy_buffer_to_buffer` into a bigger buffer plus a swap of the
  bind group the terrain pass reads - no re-mesh, and the frame already recorded keeps the old buffer
  until it is done with it. `COPY_SRC` on the arena's usage flags is what makes that copy legal. And the
  JVM side, which was already dropping its "I have sent this" record so the *next* rebuild would carry
  the blocks again, now also marks the section dirty: the next rebuild was the thing that never
  happened, because Minecraft believes it has meshed the section and has no reason to look at it again.
  That is bounded to 32 sections a tick and only while the arena can still grow - at the device's own
  `max_buffer_size` a rebuild would be refused exactly as the last one was, once a tick, forever.

The diagnostic that would have caught all of this in one line is on the terrain line now: `arena N of
M slot(s) handed out (X%, Y MB), the largest section L slot(s)`. `L` is the measurement the constant is
a guess at, and the pair is what says whether a refusal is a nearly-full arena, a fragmented one, or one
enormous section. The first session against the corrected numbers, same world and 16 chunks:

```
arena 51,753,658 of 65,712,000 slot(s) handed out (79%, 197 MB), the largest section 168,014 slot(s)
```

Zero refusals, and three things worth keeping. The pool is the size the world needs with a fifth of it
spare, which is the first time those two constants have been checked against anything - and they are
only right *together*, as a product: 1 369 columns × 48 000 slots came out at 79% of the pool. The
largest single section is 168 014 slots, 672 KB, **ten times** the per-section constant: a section is
not "about 16 000 slots", it is whatever its geometry is. And the line itself is now a diagnostic
rather than a flood - see below.

### The terrain line was half a megabyte of log every thirty seconds

The line above was written once per frame, and a world frame takes about 6 ms: 805 lines and 567 KB in
the thirty seconds of the run it was measured on, which buried everything else the game said. It is
now written when there is something to *say*:

- **the first frame of a session, and any frame where something changed** - the pass drew nothing, the
  arena started refusing sections, or it grew - writes the whole line: fluids, arena, largest section,
  bob, camera, and the JVM side's own counters;
- **any other frame** writes the arena and the two counters and nothing else, once a second:
  `; 11,661,278 section draw(s), arena 51,753,658 of 65,712,000 slot(s) handed out (79%, 197 MB), the
  largest section 168,014 slot(s), 0 refused`.

The state that gets the full line is the state that has to be reported *now*: a refusal is geometry
that was baked and never reached the screen, and a growth is the answer to it - a second late is a
second of the world not changing. The rest is a heartbeat, and a heartbeat that costs 200 bytes a
second is one that can be left on.

### A block that is baked into nothing, and the line that now says so

Three ways the baker can turn a block into nothing, and until this line existed **none of them said
anything in the game's log**. They are worth separating because they have one symptom between them - a
registered block that is not drawn, that still occludes its neighbours, so the block behind it loses
the face between them and what the player sees is a hole in the world with no visible cause:

| What happened | What it leaves | Where it was reported |
| --- | --- | --- |
| a texture a model names cannot be read | the sprite is never packed, and every face that samples it is dropped | a `log::warn!` on the native side, which does not reach the game's log file |
| a face's sprite is not in the atlas | the face is dropped; the block has a hole - or *is* the hole, if it was the model's only face | nowhere |
| a state's mesh comes out with no faces at all | the block is drawn as nothing, and still culls its neighbours | nowhere |

The middle one is the quietest thing in this renderer: `get_atlas_uv` returning `None` drops a face by
`?`, on purpose - a block with one bad texture reference must not take the block registry down - and
the result is indistinguishable, from the outside, from a model that never existed. A mushroom cap was
that for a whole session of looking at the wrong thing (see the rotation section): the state had a key,
the key had a mesh, the mesh had no faces, and the terrain line was full of numbers about the arena.

So all three are counted where they happen and reported where the log is. The native side keeps the
counts and the first few names (`MISSING_SPRITES`, `UNREADABLE_TEXTURES`, and the states whose mesh
`is_empty`), and `blockBakeDiagnostics` hands them to the JVM, which logs them right after the block
cache:

```
wgpu: the block models did not all bake: 2 texture(s) a model names could not be read (so their faces
are untextured): minecraft:textures/block/brown_mushroom_block.png; 64 block state(s) baked to a mesh
with no faces (drawn as nothing, and still occluding their neighbours): minecraft:brown_mushroom_block x64
```

Nothing is printed when everything baked, which is the normal case - and the point of the line is that
the abnormal one is one grep away from the run that showed it.

### A sprite allocated after the atlas was uploaded is a block that is invisible

The mushroom blocks, and the four days of looking at the wrong thing that they cost. Every number the
renderer had said they were fine: the state was registered, the model resolved, the mesh was built, the
faces were **drawn** - `brown_mushroom_block seen 663 drawn 1,712 culled 2,266`, from the counters the
section above describes - and the blocks were not on screen. The one visible symptom beyond that was
reported as "the block touching the mushroom disappears when you look through it", which is the
mushroom's own occlusion flags doing their job on a block that is not drawn: its neighbour loses the
face between them, exactly as vanilla would have it, and what is missing is the skin.

The baker was innocent, and the reason is a seam between two passes that bake block models:

1. `bake_blocks` bakes every `variants` model, allocates the sprites they name, allocates the fluid
   sprites, and calls `Atlas::upload` - which is what copies the atlas *image* to the texture the pass
   samples.
2. `cacheBlockStates` then asks for a mesh per block **state**, and that is what bakes the `multipart`
   models - one state at a time, lazily, because a multipart's mesh depends on the properties.

`Atlas::allocate` writes the CPU image and the maps. It does not touch the GPU. So a sprite that only a
multipart model names is allocated into the image *after* the upload and copied to the GPU **never**:
its rectangle is in `uv_map` (so the face is baked and the block is not reported as untextured), and the
texels behind that rectangle are wgpu's zero-initialization, `(0, 0, 0, 0)`. The terrain shader alpha-
tests, so every face that samples it is discarded.

`brown_mushroom_block`, `red_mushroom_block`, `mushroom_block_inside` and `mushroom_stem` are exactly
that: two `template_single_face` models whose sprites nothing else in the game names. Every other
multipart block in vanilla - fences, panes, walls, buttons, redstone - names textures that its own
`variants` counterpart already put in the atlas, which is why the bug looked like "the mushrooms are
invisible" rather than "multiparts are invisible". It would have hit vines, sculk veins and glow lichen
the same way, and it hit whoever else had a texture only a multipart names.

The fix is the flag `Atlas::allocate` should always have set: `sprites_since_upload`, and
`upload_if_dirty`, which copies the image when the count is not zero. It is called at the seam
(`cacheBlockStates`, right after the per-state bakes) and once per frame as a net (`tick_scene`), which
logs a warning when it fires - a sprite that arrives outside the seam is a block that was invisible
until that frame, and the warning is what says so rather than another afternoon of watching a block
that is not there.

Two things about the search are worth keeping. The **counter that answered it** was three numbers per
watched block - seen, drawn, culled - and it answered in one run what four rounds of reading the baker
had not: `drawn` alone means the fault is after the bake, and everything before that point can be
dropped from the enquiry. And the **console was the only place the Rust log went**: the line that would
have shown the empty meshes and the unreadable textures existed for the whole of this hunt, in a stream
nobody was reading. That is why the same facts are now on a `WARN` line in the game's own log.

### A face is hidden by the neighbour's state, not by the neighbour's model

The Rust baker leaves a face out of a section's mesh when the block next to it makes it invisible, and
for a long time it asked the wrong thing: the neighbour's *model*, "does it have a full-size quad on
the side facing us". Full cubes pass that test whether or not they occlude anything, and glass, ice,
leaves, stained glass and every plant are full cubes that occlude **nothing** (`noOcclusion()`, or
`noCollision()` for a plant, both of which set `canOcclude = false`). So the stone placed against a
glass block lost the face it shared with it, and the world showed its insides wherever one of them
touched anything - visible through the block itself, which is exactly where a player looks.

The test is now the one vanilla makes, as far as two per-state bytes can carry it
(`Block.shouldRenderFace`):

| Bit | Where it comes from | Read as |
| --- | --- | --- |
| occlusion | `state.getFaceOcclusionShape(dir) == Shapes.block()` | the neighbour covers that whole face |
| self-hide | `state.skipRendering(state, dir)` | the neighbour is the *same state* and hides the face between them |

The masks are read from the *state*, and **when** they are read matters. `getFaceOcclusionShape`
reads a field that `BlockStateBase#initCache` fills in - and the game calls that at the end of
`Blocks`' class initializer, after every block has been registered. Asking for it from the
registration mixin, where the state is first seen, is a null dereference in the middle of bootstrap:

```
java.lang.NullPointerException: Cannot load from object array because "this.occlusionShapesByFace" is null
    at BlockBehaviour$BlockStateBase.getFaceOcclusionShape(BlockBehaviour.java:554)
    at Registry.occludes(Registry.java:582)
    at Blocks.<clinit>(Blocks.java:53)
```

So `RegistryMixin` hands over the state and its name and nothing else, and the masks are read on the
block cache thread - seconds later, as `Wgpu#helperSetBlockStateIndex` sets each state's key, which is
the one moment both the key and the shapes exist. `BlockFaceFlags.describe` sends them under that key;
the Rust side ANDs them per key (two states can wear one model) and `chunk::face_is_hidden` reads them.
The old geometry test stays as an added condition, so a face is only left out where the state *and*
the model agree: vanilla hides a few partial-occluder faces this draws, and every one of them is a
face between two blocks, where nothing can see it.

Two details are worth the words. The two `Direction` enums are in **different orders** - Java's is
`DOWN, UP, NORTH, SOUTH, WEST, EAST`, this crate's is `West, East, Down, Up, North, South` - so a mask
that arrives from Java is read back through `direction::JAVA_ORDINAL`, and a test pins each face to
the ordinal Java gave it. Reading it directly is invisible on a full cube (all six bits agree) and
wrong on exactly the blocks this exists for. And the self-hide mask is a *per-state* answer, so the
Rust side can only consult it when the neighbour is the same state: a pane's two connection states, or
two colours of stained glass, get their face drawn where vanilla might hide it - an invisible face
between two blocks, not a hole.

`BlockModelFace::layer` is filled from the same kind of signal, and it is what decides which pass draws
a face. Two answers go into it, and the game asks both: what the *model* says (`force_translucent` on a
26.1 texture entry - 110 vanilla models, `glass.json` among them - and the older `render_type`), and
what the *sprite* says, which is where ice, leaves and every plant are decided because none of their
models declares anything at all. The sprite answer is the one the game computes from pixels
(`SpriteContents#computeTransparency`): a texel that is half there makes the face translucent, a texel
that is not there makes it cutout, and the whole sprite is read once as it is allocated
(`Atlas::sprite_layer`).

Which of those layers this renderer draws is decided by the pass it stands in for, and that pass is
bigger than its name suggests. 26.1 opens **one** render pass per section-layer *group*, and the
`OPAQUE` group is `[SOLID, CUTOUT]`: `ChunkSectionsToRender#renderGroup` calls `setPipeline` for the
solid layer, draws every section's solid geometry, and then calls `setPipeline` again for the cutout
layer and draws that - all inside the same pass. The takeover fires on the first of those pipelines and
takes the whole pass with it, so the graph draws **both** layers out of the arena; drawing only solid
was a frame with no leaves, plants or grass overlay in it at all, because Minecraft's cutout draws were
further down the pass that no longer existed. The `TRANSLUCENT` layer is a group of its own, in a pass
of its own, into a render target of its own, so it stays Minecraft's: ice, glass and water are drawn
there, against the depth the graph's pass filled.

It is **per face** rather than per model because a model is not one thing: a grass block is an opaque
cube with a cutout overlay on its sides, and a model-wide answer could not put the cube in one layer
and the overlay in the other. The sprite table is per sprite rather than per face's UV rectangle - the
game's own test is `computeTransparency(u0, v0, u1, v1)` over the rectangle a face samples - so a face
that only samples the opaque part of a partly-cutout sprite is put in the cutout layer too, which is
the same pass either way. What it costs is this renderer drawing less of the world into the layer it
owns, which is the trade to make towards a picture that is right.

### The rotation belongs to the variant, and so does the face it culls against

A blockstate variant may name a model *and* turn it (`"x"`, `"y"`, `"uvlock"`), and the baker used to
read the model name and nothing else. Five things came out of that, four of which were in the picture:

- The geometry was turned, but by the wrong composition. The element's own `rotation` was applied
  *between* the variant's `x` and its `y` (`x`, then the element matrix about its origin, then `y`),
  so an element-rotated model under a rotated variant - a plant on its side, a wall-mounted block -
  came out somewhere neither rotation puts it. Minecraft's order is the element's rotation first and
  the variant's on the result, which is what `vertex_transform` now does.
- **`x` was the other quarter turn.** This is the one that cost a picture. `x: 90` is
  `OctahedralGroup.BLOCK_ROT_X_90 = ROT_90_X_NEG` - a *negative* quarter turn about X, which takes a
  model's up direction to **north** - and the baker had it as the positive one, so `x: 90` and `x: 270`
  were swapped. `x: 180` is its own inverse and every `y` turn was already right, which is why it
  survived: only the quarter turns about X were wrong, and that is **112** of the 1170 blockstate files
  (`amethyst_cluster` and every wall cluster, `basalt`/`bone_block`/every log's `axis=x` and `axis=z`,
  levers and buttons on a wall, and both mushroom blocks). `amethyst_cluster` states the convention in
  one line - its model points up and its `facing=north` variant is `{"x": 90}` - and the mushroom cap
  is what the swap did to the world: the `up` piece was baked onto the *bottom* of the cube facing
  down, where the mushroom block below it, being a full block that occludes, then culled it. A huge
  mushroom had no top.
- The `cullface` was not read at all. Culling used the plane a face's geometry sits on and nothing
  else, so a face declaring **nothing** was left out whenever the neighbour happened to be a full
  block, where vanilla draws it. 16 vanilla block models are in that position (39 faces, `*_inventory`
  aside) - `beacon`, all six of its faces, plus `heavy_core`, `bell_floor`, `coral_fan`,
  `brewing_stand`, `lectern` - and `BlockModelFace::cull` now carries the declaration, with a face
  that declares none never culled.

  This half is overdraw rather than picture, and the numbers say why: a face between two blocks is
  hidden from outside either way, and vanilla is tidy about the field - of the 2392 block models in
  the client jar, the 16 faces that name a direction whose boundary they are not exactly on are all
  geometry a hair inside or outside the cube (`cube_all_inner_faces` at 15.998, the hopper's inside
  top at y = 10, the lever's underside at -0.02), each naming the direction it is nearest to. A model
  from a resource pack need not be that tidy, and reading the field is what makes it mean what it says.
- `uvlock` was ignored, so every `"uvlock": true` variant had its texture turn with the model where
  the game leaves it still.
- The normals were the six axis-aligned literals, never turned. They are not decoration: the vertex
  packs one into a three-bit field (`render::pipeline`) and the shader shades with it, and a face
  whose normal still points where the model file wrote it is lit from the wrong side. The turned
  direction is built from the rotation's matrix rather than from its position form, because a normal
  has no position to be folded about - the `1.0 -` in the position form is where the middle of the
  block is - which also keeps it axis-aligned for the packing.

How much of the world that was: of the 1170 blockstate files in the 26.1 client jar, **131** set
`"uvlock": true`, **407** turn a model about Y, **182** about X, and every one of the 182 turns about
Y as well - 182 of them put both in a **single** variant (`acacia_log`'s `axis=x` is
`{"x": 90, "y": 90}`), which is what makes the order of the two turns observable at all. Stairs and
walls are in both lists, and they are among the most common blocks in a world.

`brown_mushroom_block.json` is the case that ties all of it together, and the one this was noticed
on - a multipart whose six cap pieces are the **same** model under six different rotations, all of
them `"uvlock": true`, beside inside faces that are another model with no `uvlock`:

```json
{ "apply": { "model": "minecraft:block/brown_mushroom_block", "uvlock": true, "y": 90 },
  "when": { "east": "true" } },
{ "apply": { "model": "minecraft:block/brown_mushroom_block", "uvlock": true, "x": 270 },
  "when": { "up": "true" } },
```

Six pieces of one model, six rotations, one texture that must not turn - and, next to them, a piece
whose texture must turn. Nothing but a per-`ModelProperties` rotation can express that. It is also
where the `x` sign showed up, and why the report was "the mushroom block is gone" rather than "the
mushroom block is facing the wrong way": the `up` piece was drawn on the bottom of the cube facing
down, the block below a cap is another mushroom block, that block occludes, and culling is what
happens to a face whose neighbour occludes - so nothing was drawn on top of a cap at all.

The direction a `cullface` names has to be turned too, because it names a neighbour of the *block*:
`Direction.rotate(modelState.transformation().getMatrix(), face.cullForDirection())` is what the game
does, and the rotated direction is what the culling test compares against the world as baked. So
`face_data` returns the declaration already turned, and `BlockModelFace::cull` is the neighbour this
face now touches rather than the one the model file was written against.

`uvlock` is the interesting one, and the whole of it is one matrix. Minecraft's
`BlockMath#getFaceTransformation`:

```java
Transformation faceAction = VANILLA_UV_TRANSFORM_LOCAL_TO_GLOBAL.get(originalSide);
faceAction = transformation.compose(faceAction);
Vector3f transformedNormal = faceAction.getMatrix().transformDirection(new Vector3f(0, 0, 1));
Direction newSide = Direction.getApproximateNearest(...);
return VANILLA_UV_TRANSFORM_GLOBAL_TO_LOCAL.get(newSide).compose(faceAction);
```

so the transform is `GLOBAL_TO_LOCAL[newSide] * R * LOCAL_TO_GLOBAL[declared]`, applied to the UV as
the point `(u - 0.5, v - 0.5, 0)` in sprite-normalized coordinates. Two things in there are easy to
get wrong and were:

- `newSide` is the face the **declared** face's normal ends up pointing at, not the south face's.
  `faceAction` is `R ∘ LOCAL_TO_GLOBAL[declared]`, whose local `(0, 0, 1)` is the declared face's own
  normal; taking the south face's instead gives the right answer for a south face and the wrong one
  for the other five. A test hand-derives a north face turned 90° about X - which becomes the up face,
  and whose transform comes out as `rotX(180) ∘ rotY(180)`, the sprite turned half way round.
- `uvlock` is *not* "no transform". A north face turned 90° about Y becomes the east face, and those
  two frames compose with the rotation to the identity - the sprite keeps every corner it had. It is
  still locked: the face it is on turned, and the numbers on it did not. With `uvlock` off the UVs stay
  attached to the corners the rotation moves, which is the texture turning with the block.

There is one more thing about *where* this happens. A `ModelMesh` is cached per variant - `Block`'s
variants each keep an `Arc<ModelMesh>`, and a multipart model keeps one per key - and the rotation is
part of the variant rather than of the model file, so it has to be applied inside `bake`, per
`ModelProperties`, before the faces of several properties are merged into the one mesh. Rotating a
finished mesh could not express a multipart block whose two variants are turned differently, and doing
it per state would rebuild meshes that are shared.

### A face was turned by a quarter turn, and no uniform texture could show it

Two tables decide which way round a face's texture goes, and both live in the game:

- **`FaceInfo`** says which corner of the element's box each of a face's four vertices is;
- **`CuboidFace.UVs#getVertexU`/`#getVertexV`** says which corner of the sprite that vertex samples:

```java
public float getVertexU(int index) { return index != 0 && index != 1 ? this.maxU : this.minU; }
public float getVertexV(int index) { return index != 0 && index != 3 ? this.maxV : this.minV; }
```

This baker had both, hand-written out six times - once per direction, as literal `p101`-style vertices
with literal `uv.1.0`/`uv.0.1` expressions beside them - and **two of the six pairs were wrong**: the UP
face walked its sprite from the wrong corner (a half turn) and the DOWN face from one corner along (a
quarter turn). The four side faces were right, which is what made the report so specific:

> the Rust terrain is nearly identical to the game now; a new one: one or some of a block's faces are
> rotated against the game's - some stones' top faces by 90, 180 or 270 degrees

A rotation of a face is invisible on a texture with no direction to it - which is why "stone" is exactly
the kind of block this is hard to *see* on and easy to *measure*: the pairing is checkable against the
game's own arithmetic without a screen. `FaceBakery#defaultFaceUV` is the same two facts as six lines:

```java
case DOWN  -> new UVs(from.x(), 16.0F - to.z(), to.x(), 16.0F - from.z());
case UP    -> new UVs(from.x(), from.z(), to.x(), to.z());
case NORTH -> new UVs(16.0F - to.x(), 16.0F - to.y(), 16.0F - from.x(), 16.0F - from.y());
case SOUTH -> new UVs(from.x(), 16.0F - to.y(), to.x(), 16.0F - from.y());
case WEST  -> new UVs(from.z(), 16.0F - to.y(), to.z(), 16.0F - from.y());
case EAST  -> new UVs(16.0F - to.z(), 16.0F - to.y(), 16.0F - from.z(), 16.0F - from.y());
```

and each line is two facts: **which world axis the sprite's `u` runs along** (`x` on an UP face, `z` on a
WEST one, the negative of one where the line is written `16 - to`) and **which way `v` runs** - down every
side, along `+z` on an UP face and `-z` on a DOWN one.

The six hand-written blocks are one table now (`face_vertices`, the corner of the box and the corner of
the sprite side by side, because it is the *pairing* that has to be right), the two faces are corrected,
and the test checks the table against those six lines rather than against itself.

**The last line of that section used to say the corner orders stayed this baker's own, and that was the
wrong call** - see "The order was the game's all along" below. Those orders were the third thing wrong in
this table, and they are why this round went as deep as it did.

### The order was the game's all along, and a mirror hides from every test that only asks 'which corner'

The paragraph above left the four corners in an order of this baker's own invention: `FaceInfo`'s four
corners for the face, re-arranged to suit this renderer, with the sprite corners worked out to match. The
reasoning looked sound - vanilla re-sorts its vertices after baking (`recalculateWinding`), so the game's
order "says nothing about" this side's - and it is what the next report was about:

> there is a new problem: one or some of a block's faces are rotated against the game's - for example some
> stones' top faces, by 90, 180 or 270 degrees

**`FaceInfo`'s winding is this renderer's after all, and saying otherwise is a regression that shipped.**
The claim was that `FaceInfo` is written for `calculateFacing`, which only asks which direction a quad is
*about*, so a face comes out of it wound against `Ccw` and vanilla turns it round afterwards. That is
false, and it can be checked against the game's own data without running anything: take the first three
corners of `FaceInfo`'s row for a face, and their cross product points **out** of the block. Checked for
all six:

| face | first three `FaceInfo` corners | cross |
| --- | --- | --- |
| DOWN | `(minX,minY,maxZ) (minX,minY,minZ) (maxX,minY,minZ)` | `(0,-1,0)` |
| UP | `(minX,maxY,minZ) (minX,maxY,maxZ) (maxX,maxY,maxZ)` | `(0,+1,0)` |
| NORTH | `(maxX,maxY,minZ) (maxX,minY,minZ) (minX,minY,minZ)` | `(0,0,-1)` |
| SOUTH | `(minX,maxY,maxZ) (minX,minY,maxZ) (maxX,minY,maxZ)` | `(0,0,+1)` |
| WEST | `(minX,maxY,minZ) (minX,minY,minZ) (minX,minY,maxZ)` | `(-1,0,0)` |
| EAST | `(maxX,maxY,maxZ) (maxX,minY,maxZ) (maxX,minY,minZ)` | `(+1,0,0)` |

So the game's order is the renderer's winding, and reversing it here was wrong. What made the wrong
version hard to see is that **reversing a quad changes nothing any test in this file can observe**: it is
still four corners, still a closed walk, and still a winding - a winding that now faces the other way, but
the sprite corners had been turned to match, so `a_face_pairs_its_corners_the_way_the_games_own_uv_lines_do`
was told what to expect by the same turned `default_face_uv`. Two consistent wrongs.

That is exactly how the *next* report came in:

> 似乎是修uv那一轮让方块侧面的贴图都反了（倒过来了），草也反了，火焰也反了

which is the shape of the failure: `default_face_uv` had been turned as well, on the same "the quad is
reversed" argument, and a face turned twice is a face turned once. Every side face of every model that
writes no `uv` - which is the whole of `defaultFaceUV`: slabs, stairs, panes, plants - came out upside
down. `default_face_uv` is now the game's six lines **verbatim**, with neither axis turned:

```rust
match declared {
    Direction::Down => [fx, 16.0 - tz, tx, 16.0 - fz],
    Direction::Up => [fx, fz, tx, tz],
    Direction::North => [16.0 - tx, 16.0 - ty, 16.0 - fx, 16.0 - fy],
    Direction::South => [fx, 16.0 - ty, tx, 16.0 - fy],
    Direction::West => [fz, 16.0 - ty, tz, 16.0 - fy],
    Direction::East => [16.0 - tz, 16.0 - ty, 16.0 - fz, 16.0 - fy],
}
```

**Two more things were wrong in the arrangement, and one of them is still the reason to be careful.**

**The bit order of a corner is `z`, `y`, `x` - not `x`, `y`, `z`.** The baker writes its eight corners
out as `p000`..`p111` and puts them in one array, so the bit a name carries is `x` in 4, `y` in 2, `z` in
1 - the reverse of the order the axes are usually spoken in. A test that reads the bits as `(x, y, z)`
gets a **mirrored box**, and a mirrored box is a symmetry of all six faces: every fact about a full cube
still holds, every "which corner is which" check still passes, and nothing shows up until a pairing is
asked about an axis by name. `corner_position` is that array written out by name now, shared by the bake
and the tests, so there is one reading of the bits rather than one per caller. This one is real and stands.

**Three: the pairing itself.** `defaultFaceUV` writes each of a face's lines out of two of the box's axes,
and which two - and which way round - is the whole of the pairing. Nothing here is a hand-derivation any
more. The test **derives** it from the two game tables together: for a slab (where all three axes have
different extents, so no two are confusable) it asks `default_face_uv` which axis each coordinate follows
and which way it runs, then asks that corner of the sprite each corner of the box samples should be, then
asks `face_vertices` whether that is what it says. It is the one statement of the pairing that is not the
table.

**How the table was actually found, after two hand-written ones failed.** By printing it. A throwaway test
walked `FaceInfo`'s rows and `default_face_uv`'s rectangle together and printed the pairs they imply, and
the table was written from that output - and the printed rows and the test's independent derivation agreed
on all six faces. That is the method worth keeping: when two hand-derivations have disagreed with the
screen, **derive it from the game's data and print it** rather than reasoning about it a third time.

**The lesson worth keeping** is about which tests can see what. A mirrored box satisfies every "which
corner is this" question; a pairing turned as a whole satisfies every "does the walk go round" question.
So does a *reversed* quad, which is the one that cost the most here: it is invisible to every test in this
file and it silently re-defined what the neighbouring function was expected to say. What sees a mirror is
a question about a **named axis**; what sees a turn is a question about a **specific corner of the
sprite** - and neither of them sees a test whose expectations were written from the same wrong premise.
For that, the expectations have to come from data the code does not own.

### The title screen wrote a warning per frame

`upload_late_sprites` is the net under the two places that bake block models, and it warned every time
it caught something:

```
wgpu-mc: wgpu_mc:atlases/block was uploaded again after the frame: 5 sprite(s) had been added to it
```

Which is the right thing to say and the wrong thing to say *repeatedly*: the title screen allocates
sprites while it settles, a resource reload allocates hundreds, and a line per upload is a console with
nothing else in it - the same argument as the terrain line, so it gets the same answer. At most one line
a second, carrying the total since the last one and how many uploads it took, and the first upload after
each line is reported at once so nothing is swallowed.

The other half of that report was worth acting on: a sprite the atlas already holds was being allocated a
**second rectangle** every time it was pushed again. The map the baker reads kept pointing at whichever
copy was written last, so nothing looked wrong - but each repeat cost atlas space that cannot be given
back, and the 2048x2048 atlas has a fixed amount of it. `allocate_one` now returns early for a path that
is already in the map; a resource reload, which is the one thing that changes the pixels behind a path,
clears the map first and so still allocates it again.

### A face between the lines of the vertex format

A baked vertex holds its position in **sixteenths of a block**: eight bits an axis, plus one bit for
"this coordinate is exactly 256". Everything a model normally asks for is on that grid, and geometry
that is on it is drawn exactly where it was baked. A model is free to ask for something that is not,
and the leaf litter does: `template_leaf_litter_*` is **one quad at 0.25/16 of a block**, with an `up`
face and a `down` face, and the block below it draws its own top face at the boundary.

Truncating to the grid - which is what the encoder used to do - puts that quad at y = 0, which is
*exactly* the plane the ground's top face is on. Two coplanar surfaces are then decided per pixel by
the last bits of two projected z values that are equal only to within rounding, so the leaf texture and
the ground show through each other in patches that move as the camera **turns** and not at all as it
**moves** - moving both surfaces together leaves their difference where it was. That is the flicker,
and it is the same bug for pink petals, flower clusters and anything else that sits lower than a
sixteenth.

The encoder rounds **to the nearest** line now, and a face that is strictly *between* two block
boundaries is never put on one of them, whichever side it came from. On-grid geometry does not move at
all, and the rule is monotone, so nothing crosses anything.

Rounding **up** was the first fix here, and the fire found out why it was not enough. Vanilla's fire side
quads are at `z = 0.01/16` of a block - a hundredth of a texel, and a hundredth of a texel is exactly
what that offset is for - and the variant that burns against the *opposite* wall is the same quad turned
by `y: 180`, so it arrives at `15.99/16`. Rounding up sent the first to 1/16, where it is harmless, and
the second to 16/16, which is the wall's own face: fire that flickers against the wall it is attached to,
with the wall showing through it in stripes that move as the camera does. A plane one step below a block
boundary has to stay below it. `0.01/16` needs about 1/1024 of a block to be representable at all, which
is four more bits an axis than this vertex format has - that is the known gap, and this rule is what
keeps the two cases the models actually use out of it.

### The cutout test stopped testing anything when the atlas got a mip chain

The terrain shader is inherited from the demo, and so was its alpha test:

```wgsl
if (col.a == 0.0f) { discard; }
```

That is a correct cutout test on an atlas with **one mip level**, where a texel is either leaf or hole
and a hole is exactly zero. The mod's atlas is not that atlas: it is built with a mip chain
(`ATLAS_MIP_LEVELS`, full image and three halves) and sampled with `mipmap_filter: Linear`. At level 0 a
hole is still exactly zero and the equality holds - so **near** leaves were cut out correctly, which is
why the bug looked like a rendering *region* rather than a bug. At any level above 0 the hole is no
longer a texel: it is the average of leaves and gaps, a small non-zero alpha, the equality fails for
every texel of the face, and since the terrain pass **replaces** the target rather than blending into it
(`blending: replace`), the whole quad is painted at full strength. Leaves went solid, in exactly the
places where the sprite stops covering enough pixels to stay on level 0.

The derivative that picks the mip level is screen-space, so the boundary between the two is a
screen-space iso-line: it follows **distance, viewing angle and field of view**, and on the flat leaves
of a canopy it is one or a few straight lines. Reported as "high quality inside a range, opaque outside
it, and the range moves with the camera" - which is a description of `log2(texels per pixel)` crossing
1.0, not of a culling frustum.

The test now reads the cutoff from the layer being drawn:

```wgsl
if (col.a < section_pos.alpha_cutout) { discard; }
```

`0.5` for the cutout layer and `0.0` for the solid one, which is what Minecraft's own two terrain
pipelines say - `CUTOUT_TERRAIN` defines `ALPHA_CUTOUT` as `0.5F` and `SOLID_TERRAIN` defines nothing,
and a cutoff of zero is a test nothing fails. The value travels in the immediate the pass already hands
each draw, so it costs no extra binding: `@pc_section_position` is sixteen bytes now - the section
position and a float - and the shader's `SectionPosition` struct is what fixes that size. A wider struct
than the layout declares is a draw reading past its own data, so the size in `RenderGraph::new`'s table
and the struct in `terrain.wgsl` are the two halves of one number and have to be changed together.

### The side of every block was as bright as its top

`DefaultVertexFormat.BLOCK` has no normal in it. The direction of a face reaches the shader in exactly
one place: the vertex **colour**, which the game builds as `CardinalLighting.DEFAULT.byFace(direction)` -
down 0.5, up 1.0, north and south 0.8, west and east 0.6 (`BlockModelLighter#prepareQuadFlat` writes it
as a grey `Color`, and the ambient-occlusion path scales the corner light by it). `terrain.vsh` then
says all of it in one line:

```glsl
vertexColor = Color * sample_lightmap(Sampler2, UV2);
```

The baker's colour was the **tint only** - `get_block_color(pos, tint_index)` where a face had a tint
index, white where it did not - and the shader multiplied that by its own light and ambient-occlusion
approximations. Nothing in the baker knew a direction was worth a factor, so every face was drawn at the
brightness of an upward one: north and south faces 1.25x too bright, east and west 1.67x, the bottoms
twice. On a world of cubes that is not a subtle thing - it reads as the sides being **overexposed**
against vanilla, which is how it was reported, and it is invisible in any single-frame test because
nothing is torn or missing.

The shade is now applied where a face is baked, in the one place both the tint and the direction are
known (`scale_rgb(color, face_shade(dir))`), so the tint stays a tint and the lightmap keeps doing the
rest. Fluids go through the same line and get the same table, which is what `FluidRenderer` does with
`ARGB.scaleRGB(tintColor, up * (north | west))` - for a side, a top and a bottom that is
`CardinalLighting.DEFAULT.byFace` in every case.

Two things are still not the game's:

- **The nether's table.** `CardinalLighting.NETHER` is 0.9 for up *and* down, and this path does not
  know which dimension it is baking for; the overworld's table is used everywhere.
- **`"shade": false`.** A model can turn the directional factor off, and the game then uses `up()` for
  every face. The baker does not read that flag, so an unshaded model gets the table anyway. Faces in
  the `any` bucket are the exception - they arrive with `Direction::Up` and are left alone.

- **Animated textures animate now, for models.** See "The fire burned still" above for how, and the
  known gap beside it for the two things it does not cover: fluids, which are meshed by hand rather
  than from a model, and a resource reload, which moves the game's sprites and is not followed.

### The fire burned still, and the fix is that the game was already animating it

`TextureAtlas#cycleAnimationFrames` renders each due frame of every animated sprite **into the game's
own block atlas** (an `Animate <atlas>` pass per mip level, which `WgpuCommandEncoder` counts). The
terrain samples the **atlas this mod packs for itself** (`Atlas`, `ATLAS_DIMENSIONS` 2048, built at
startup), so every animated sprite in it holds whatever frame it was copied at: fire, lava, water,
campfire, the lot. The passes above are the only thing that makes an animated texture change, and they
change a texture nothing draws from.

Two ways out, and the one taken is the cheap one. *Copying* would mean running the game's animation
passes and then moving the rectangle they wrote - the pass's scissor says which one - out of the game's
atlas and into this one, per mip level, per frame, with the copy falling behind the game's own clock.
The other way is to stop copying and **draw the game's atlas where it already is**:

- a face whose sprite the game animates is baked with **the game's own coordinates**
  (`Atlas::sprite_rects`, which `WgpuNative.registerSprite` fills from the game's stitcher - the same
  thing that was already being sent for the layer table) and marked in the vertex with one of the ten
  bits the vertex format has always reserved for an animated UV index;
- the terrain pipeline binds the game's atlas texture and the sampler the game samples it with, and the
  fragment stage picks between the two textures on that bit;
- nothing is copied, nothing is re-uploaded, and the frames are the ones the game is drawing anyway.

Which sprite is animated is *not* sent across the bridge. It is read from the `.mcmeta` files this side
already downloads while it packs its own atlas (`Atlas::animated_textures`, now keyed by sprite rather
than a list of anonymous animations), so a sprite is animated here exactly when the game animates it.

That read is the part that had never once succeeded, and it is worth writing down because of how quiet
it was. A sprite arrives at `Atlas::allocate` under the name a model calls it by (`minecraft:block/fire_0`)
and its image is fetched as a *file* (`minecraft:textures/block/fire_0.png`), and the metadata was looked
for by appending `.mcmeta` to the **name** - `minecraft:block/fire_0.mcmeta`, a path no pack ships. A
missing `.mcmeta` is the ordinary case (most sprites are not animated), so the lookup failed for every
sprite in the game, silently, for the entire life of the table: `animated_textures` was collected,
stored, cleared on reload - and empty. The conversion is now one named function
(`sprite_metadata_path`), with a test, so the two halves cannot drift apart again.

Three details are what make it fit rather than nearly fit:

- **The two atlases are different sizes, so the UVs are quantized differently.** This side's atlas is
  2048 wide and the shader decodes a coordinate as `value / 2048`; the game's coordinates are `0..1`
  over an atlas of the game's own size, so they are stored as the sixteen bits filled edge to edge and
  decoded as `value / 65535`. The shader picks the scale by the same flag that picked the bake, in the
  vertex stage:
  ```wgsl
  let game_atlas = (v3 >> 16u) & 1u;
  let uv_scale = select(0.00048828125, 1.0 / 65535.0, game_atlas == 1u);
  ```
- **The game's rectangle is all the baker needs.** A face's `uv` is a fraction of the *sprite* -
  sixteen units to the sprite, whatever its size in pixels - so the corners are that fraction of the
  game's rectangle, and the game's atlas never has to be measured here. That arithmetic is shared with
  the game's own `Atlas::sprite_rects` rather than derived from this side's atlas layout.
- **The pass has to have the atlas before a face is baked for it, not merely before it is drawn.**
  That is the ordering, not an optimization: a block *model* - and every face in it - is baked once,
  when the block states are cached (`BlockCache#start`), and drawn from that cache for the rest of the
  session. So the flag the baker reads is set by the **handover** of the atlas, which happens on the
  line before the bake starts, and not by the graph being built again with it. What makes that safe is
  where a graph is replaced: at the end of a frame, in the same step that moves the sections the baker
  finished into the arena - so no frame can draw a face flagged for the game's atlas before the pass
  that samples it exists. A named resource that is missing is a *skipped pipeline*, so the slot is
  filled with a one-texel white texture until the real one arrives; nothing samples it, and losing the
  terrain pass because an animated texture was not ready yet is not a trade worth making.

`wgpu-mc-jni` runs naga over the shipped `terrain.wgsl` in a test, because a WGSL mistake in that file
is not a shader that draws wrong - it is a validation error while the pipeline is built, which runs the
panic hook and ends the process.

### The entity shadows fought the ground because the ground was the only thing drawing itself far away

Reported as stripes under entities that flickered, stayed in the same place in the world, and had nothing
to do with what the entity was standing on. The selection outline was clean, and so was everything else
Minecraft drew.

An entity's shadow in 26.1 is not a decal with its own depth: it is one or more **pieces**, each a quad
lying on the top face of a block (`EntityRenderer#extractShadowPiece` builds them from `belowShape`, the
collision shape of the block under the entity), submitted with the entity's camera-relative position and
drawn afterwards by `RenderPipelines.ENTITY_SHADOW` - whose depth state is `LESS_THAN_OR_EQUAL` with no
write. So the shadow and the terrain face under it are **coplanar**, and vanilla relies on the depth test
seeing the shadow: `<=` passes when the two depths are equal.

Whether they are equal is decided by the last bits of two positions, and this renderer was computing its
half of that comparison in the worst possible way. The vertex shader was handed the section's **absolute**
position and the camera's translation was folded into the view matrix:

```wgsl
var section_origin = vec3<f32>(f32(section_pos.x), f32(section_pos.y), f32(section_pos.z)) * 16.0;
var world_pos = pos + section_origin;                 // e.g. 20013.0
vr.pos = mat4_persp * mat4_view * mat4_model * vec4(world_pos, 1.0);   // view = R * translate(-camera)
```

`f32` steps by `2^-6` - **0.016 blocks** - at 200,000, and by 0.004 at 30,000. That error is a function of
the *world* position and not of the camera, which is why the stripes stayed put while the camera moved, and
it applied to the terrain and to nothing else: every other draw in the frame (entities, the shadow, the
outline, particles) is positioned by Minecraft on the CPU, camera-relative, in doubles.

Vanilla never forms the big number. Its terrain vertex shader says

```glsl
vec3 pos = Position + (ChunkPosition - CameraBlockPos) + CameraOffset;
gl_Position = ProjMat * ModelViewMat * vec4(pos, 1.0);
```

with `ModelViewMat` the camera's rotation *alone*: the difference between two block positions (small
integers), plus the camera's fraction of a block, plus the position inside the section. Everything in that
sum is small, so its last bits are worth something.

The pass now does the same thing from this side of the bridge:

- `@pc_section_position` carries the section **relative to the section the camera is in**, so
  `section_origin` is at most a few thousand blocks and usually a few dozen;
- the view matrix is `viewRotation * translate(-(cameraPos - cameraSectionOrigin))` - the camera's offset
  *inside its own section*, a number below sixteen - instead of `translate(-cameraPos)`;
- the culler builds the same boxes the immediate names, because the frustum comes from that same matrix
  pair: sections are compared in camera-section-relative blocks, which is the space the matrix reads.

The camera's section therefore has to be sent, and with the matrices rather than beside them: the view
matrix no longer knows which section it was built against (`-R^T t` now gives the *offset*, not the
position), so the graph cannot read it back. It is one extra integer per axis on a call that already
existed, from the same camera state, on the same line - a frame's disagreement being sixteen blocks of
terrain in the wrong place.

Two tests carry it. One is the culling test, rewritten for the new space: a section in front of the camera
is inside the frustum when it is placed relative to the camera's section and outside it when placed by its
absolute name - the "ground culled out from under the camera" failure, which is a world with holes in it.
The other is the reason for all of it, as arithmetic: the same world point through both arrangements, in
`f32`, against an `f64` reference. The absolute form is off by hundredths of a block; the relative form by
millionths, and it is asserted to be at least a hundred times closer.

### A resource reload reloads now, and it is five separate things that had to be told

The renderer used to read the resource pack **once**. Whatever pack was stitched into the first reload
was the pack it drew for the rest of the session: F3+T, a pack switched in the options, a pack added -
the game's own atlases, models and textures all reloaded around it and the terrain kept the pictures it
started with. That was written down as a known gap ("a resource reload re-stitches the game's atlas and
this side does not follow it"), and closing it means walking everything that came out of a pack:

| what | where it lives | what a reload does to it |
| --- | --- | --- |
| the sprites this side packs | `Atlas`'s image, allocator and `uv_map` | cleared, then packed again from the new files |
| which sprite is animated | `Atlas::animated_textures`, read from `.mcmeta` | cleared; re-read as each sprite is allocated |
| the game's rectangles and layers | `Atlas::sprite_rects`, `sprite_layers` | cleared, then re-sent from the new stitch |
| the game's atlas texture | the graph's `@texture_mc_block_atlas` | MC builds a *new* `GpuTexture` per stitch; the new one is bound and the graph rebuilt |
| the block models | `BlockManager`, baked from blockstate and model JSON | baked again, against the new atlas |
| the geometry in the arena | every section's vertices, holding baked UVs | every section offered to the baker again |
| the shaders | `wgpu_mc:shaders/*.wgsl` | already reloaded (`ShaderReloadListener`) |

Two of those are the ones that would have made the rest pointless, and both are silent:

**The atlas map was never cleared.** `Atlas::allocate` skips a path the map already has, so an atlas
cleared of everything *except* `uv_map` re-packs nothing: the old pixels stay at the old rectangles and
the result is indistinguishable from a reload that did nothing. `Atlas::clear` cleared the allocator, the
animation table and the two sprite tables - and not the one the baker reads. It does now, and it marks
the texture for an upload so a reload that allocates nothing ends blank rather than stale.

**Nothing made a section stale.** The section feed sends only what *changed* since the last offer
(`RustChunkBake.sent`), and a reload does not change any blocks - the models those blocks bake to changed,
which is not something the comparison can see. So even with every model re-baked and the atlas re-packed,
`allChanged` would have offered every section, every offer would have carried nothing, and the arena
would have gone on drawing launch-day geometry. `RustChunkBake.forgetSent` is the missing half: it drops
this side's record of what Rust has, without touching the world generation stamp, so the re-mesh carries
blocks again.

The registry is the third piece, and it is a JVM-side one. The native side *drops* the block registry at
the end of every bake - the states cross as JNI global references and are released as soon as their keys
are out, which is the right trade when the registry is built once - but baking a `multipart` model needs
the states, and the game registers a block exactly once and never again. `BlockRegistryFeed` records what
`RegistryMixin` offers, in registration order, and replays it. The order matters and is recorded rather
than re-derived: a block's index is in every state's stored key and in every baked vertex, so a replay in
a different order would shuffle all of them.

What the reload costs is what the launch costs - a few seconds on the block cache thread - and it happens
once per reload. The window where it looks wrong is the re-mesh: sections are re-baked one at a time, and
a section still holding last pack's UVs samples the new atlas at the old rectangles until its turn comes.
That is a couple of seconds behind the game's own "Reloading Resource Packs" screen.

### Nothing was ever registered: 26.1's atlas manager has two id spaces

Every row of that table was wired and every one of them was silent, because both calls that hand the game's
own atlas over answered `null` and were wrapped in a `try`/`catch` that logged a `warn`:

```text
wgpu: the block atlas's sprites were not registered: java.lang.IllegalArgumentException: Invalid atlas id: minecraft:textures/atlas/blocks.png
wgpu: the block atlas texture was not bound to the terrain pass: java.lang.IllegalArgumentException: Invalid atlas id: minecraft:textures/atlas/blocks.png
```

`AtlasManager` keeps **two** maps and its `AtlasConfig` carries both ids - a *texture* id
(`TextureAtlas.LOCATION_BLOCKS`, `minecraft:textures/atlas/blocks.png`, what the atlas is registered under
in the `TextureManager`) and a *definition* id (`AtlasIds.BLOCKS`, `minecraft:blocks`, the directory of
sprite sources it is stitched from) - and `getAtlasOrThrow` reads the second:

```java
// AtlasManager
private final Map<Identifier, AtlasEntry> atlasByTexture = new HashMap<>();
private final Map<Identifier, AtlasEntry> atlasById = new HashMap<>();

public TextureAtlas getAtlasOrThrow(Identifier atlasId) {
    AtlasEntry atlasEntry = this.atlasById.get(atlasId);           // the *definition* id
    if (atlasEntry == null) throw new IllegalArgumentException("Invalid atlas id: " + atlasId);
```

So asking for `LOCATION_BLOCKS` throws, and both callers did. What that cost is the whole animated-texture
path and the whole fluid-sprite path: no rectangle table reached the native side, so no face was ever baked
for the game's atlas - the fire did not animate, the lava fall did not animate, and every sprite was served
from this side's frozen copy of it. The `warn` was the only sign, one line each, in a log with hundreds of
binding-plan lines above it.

It is fixed by asking for the definition id (`AtlasIds.BLOCKS`, named once in `BlockCache.BLOCKS_ATLAS` so
the next reader does not have to know which of the two the manager wants), and it is worth saying why it
went unnoticed for a whole round: **a registration that fails leaves a renderer that draws exactly what it
drew before**, which is the correct fallback and also indistinguishable from a feature that is switched
off. The line that says the sprites arrived is `wgpu: registered N sprite(s) of the block atlas`, and it is
the one to look for.

### One texture, one filter: the two atlases have to be sampled the same way

The first session with the registrations working came back with "these are all blurry, there is none of the
game's crisp pixels left" - about the fire, the lava, the water and every other sprite the game animates,
which are exactly the faces that had just started drawing from the game's atlas.

They were the only faces in the frame sampled with a **bilinear** filter. A face the game animates is baked
with the game's coordinates and samples `@sampler_mc_block_atlas`, and every face beside it - the stone
under the lava, the leaves above the fire - samples this side's own copy through the sampler
`TextureManager::new` builds, which is `NEAREST` both ways. The game-atlas sampler had been written as "the
sampler the game itself samples its atlas with" (`LevelRenderer` builds `CLAMP_TO_EDGE, LINEAR, LINEAR` and
the video settings' anisotropy for the chunk layers), which is a defensible thing to write down and the
wrong thing to do here:

- **a block texture is sixteen texels across, and the filter that magnifies it decides whether it has pixels
  at all.** Filling two hundred screen pixels with sixteen texels is either sixteen squares or a smear, and
  a smear is what "blurry, no crisp pixels" is;
- a face from one atlas lands next to a face from the other *in the same quad of the same block* - an
  animated lava surface beside its own still sides - so the two atlases being filtered differently is a
  seam the eye reads as one of them being out of focus;
- and the game's choice is not a fact about the *texture*: it is a fact about the game's pipeline, which
  applies the player's `Texture Filtering` and `Mipmap Levels` settings. This side's terrain does not, and
  matching one of the two samplers to the game while leaving the other alone is the worst of the three
  options.

So the game-atlas sampler is now the same sampler as this side's own, field for field, apart from the
address mode (`ClampToEdge` rather than `Repeat`: a mip level of the game's atlas is written per sprite and
must not wrap). Both are `NEAREST` within a level, a blend between the two levels a face lands between, and
the whole chain.

### The fire animated and nothing else did: 1074 blockstates are baked before the atlas is read

The animated-texture path asks two questions per face - does the game animate this sprite, and where is it
in the game's atlas - and the second one is answered by a table the JVM sends (`registerSprite`), queued
and drained on the native side.

The drain sat in the middle of `cacheBlockStates`, in the per-state loop. `cacheBlockStates` calls
`bake_blocks` *before* that loop, and `bake_blocks` is what bakes every `variants` blockstate - which is
**1074 of the game's 1170 blockstate files**, the campfire and the magma block and the sea lantern among
them. So those faces were baked with the table still empty, had no rectangle to be sent to, and sampled
this side's frozen copy; only the 96 `multipart` blocks animated. Fire is `multipart`, which is exactly
why the fire was the one that worked and nothing else was.

The drain now runs before the first model is baked, with the count in a comment so the ordering is not
re-discovered: a face is baked for an atlas once, and the table has to be in place before the first one.

### The fire animation is a Quality-page switch, and a setting that is baked is not a setting that is read

`Fire animation` - `Fast` / `Fancy`, on the Quality page next to the graphics preset - decides whether
the block textures the game animates move on the terrain this renderer draws. `Fancy` (the default)
draws those faces from the game's own block atlas, which the game animates frame by frame; `Fast` bakes
them against this renderer's own copy of the sprite, which is one frame - the picture the renderer drew
before the animated-texture path existed. It is a fidelity choice rather than a speed one, and the
schema's description says so rather than implying a saving: the game renders those frames either way.

Three things about it are structural rather than cosmetic.

**A setting that is baked is applied by baking again.** The switch is answered once per face while the
face is baked, and the answer is *in the vertex* - the coordinates are in one atlas's space or the
other's, and a flag says which. So there is no draw-path read to flip: moving the switch invalidates
every baked block model, and the only way to apply it is to bake them again. The native side notices the
change where the settings arrive (`sendSettings`), calls back into the JVM through the class loader
bridge, and `BlockCache` picks it up on the next client tick - exactly the path a resource reload takes,
except for one line: a reload forgets the **pack** (the atlas, the game's rectangles, the sprite tables)
and this forgets only the **bake** (the block list, the state list, the diagnostics). Emptying the atlas
here would have thrown away the game's rectangles for its animated sprites, and nothing in that call
would have asked for them again - so `Fast` would have quietly stopped animating anything forever after
one Apply. That is why the native entry point takes a `reload` flag rather than being two functions or
one.

**The settings document has to be sent whole, from one place.** `sendSettings` parses what it is given
as the renderer's entire config, and every field of it has a serde default - so a page that sent only
its own rows would reset every setting on the other pages. That used to be safe by accident: the
renderer's rows were all on one page, and the send lived in `Page.apply`. The Quality page is mostly
Minecraft's own options, so the row made it a *mixed* page, and the send moved up to `OptionPages.apply`,
which is the only place that sees every page and can build one complete document. A page that sends its
own rows is a page that silently rewrites the config.

**`#[serde(default)]` on an enum setting is not the enum's default.** `EnumSetting`'s own default is
`{ selected: 0 }` - the *first* variant, which here is `Fast`, the animation off. So a config file
written before the setting existed, or hand-edited without it, would have turned the animation off for
exactly the player who never asked. The field names its default function instead
(`animated_textures_default`), and a test reads a legacy config and asserts the animation is on.

### The same switch had to reach the first-person fire and the fire on a burning entity

Three places draw fire, and the switch only reached one of them. The terrain is this renderer's, and it
answers the question once per face at bake time - the coordinates are baked in the game's atlas's space or
this side's, with a flag saying which. The other two are **Minecraft's own passes**:
`ScreenEffectRenderer` draws the burning overlay over the first-person view, and `FlameFeatureRenderer`
draws the flames on a burning entity. Both sample `fire_0` and `fire_1` out of the block atlas, and
neither of them asks this mod anything - so with the switch on `Fast` the terrain went still and the player
and the mobs went on crackling.

**What the three share is the atlas, and that is where this is fixed.** `TextureAtlas.tick` (26.1 calls it
`cycleAnimationFrames`) ticks every animated sprite in the atlas, which is what advances their frames; an
animation that does not advance does not move, whichever pass reads it. So the mixin stops the ticks for
the two fire sprites while the switch is off, and all three places stop at once:

```java
@Redirect(method = "cycleAnimationFrames",
          at = @At(value = "INVOKE",
                   target = "Lnet/minecraft/client/renderer/texture/SpriteContents$AnimationState;tick()V"))
private void wgpu_mc$tickUnlessFrozen(SpriteContents.AnimationState state) { … }
```

**Which state is fire is the fiddly part, and the first two ways of answering it both took the game down.**
`AnimationState` does not say which sprite it belongs to.

The obvious route is to follow the objects the tick loop already holds: the state to its `AnimatedTexture`,
that to the outer `SpriteContents`, and that to the sprite's name - compared against the identifiers
`ModelBakery` builds its `FIRE_0`/`FIRE_1` sprite ids from, rather than against the paths spelled out
again. That is what this did first, with two `@Accessor`s, and it cannot be done:

- **A field accessor is matched by name *and type*, not by name.** The type is the accessor method's own
  return type, so an accessor answering `Object` is not a wider version of one answering
  `AnimatedTexture` - it is a member that does not exist. The session died on the title screen with
  `InvalidAccessorException: No candidates were found matching this$0:Ljava/lang/Object;`. This is worth
  stating plainly because the intuition runs the other way: widening a reference to `Object` is free
  inside a method body, and the descriptor is a different thing.
- **And the type cannot be spelled out instead.** `SpriteContents$AnimatedTexture` is package-private, so
  a mod's own package cannot name it in a signature - and a type parameter erases to `Object`, which is
  the same member that does not exist. (A `@Shadow` field with a deliberately wrong name also builds
  fine; nothing about an accessor is checked at build time, which is why the client run below is the real
  test.)

So the association is recorded where the sprite is already a **parameter** instead:
`SpriteContents.createAnimationState` is a public method of a public class, it is handed the
`SpriteContents` it is making the state for, and it returns the state - both halves are in the signature,
and nothing private is named. That is `SpriteContentsMixin` writing and `TextureAtlasMixin` reading, and
the two cannot see each other's fields, so the association lives in a class of its own.

**That class is in the mod's package and not beside the mixins, which was the second crash.** Mixin
refuses to load a plain class out of a package it owns: every class under a configured mixin package is
treated as a mixin, and one that is not fails its *class load* with

```
IllegalClassLoadError: dev.birb.wgpu.mixin.render.AnimationSprites is in a defined mixin
package dev.birb.wgpu.mixin.* owned by wgpu_mc
```

which is not a warning at load time - it landed inside a resource reload, took the whole reload down with
it, and left the title screen **black and unclickable** with no crash report. `AnimationSprites` lives in
`dev.birb.wgpu.render` for that reason, and it is the only place the association could go.

**If the trace ever fails, fire animates and one warning is logged.** It does not fall back to freezing
the whole atlas: an atlas that quietly stops moving would be a stranger bug than the one being fixed.

The frame it stops on is a real one. Both strips are 32 frames - `fire_1` in order, `fire_0` starting at
the back half of its sheet - and holding whatever frame is in the atlas is the same thing the terrain does
with this switch off, which is to hold the one frame it copied.

### The corners were dark in the right places and only a third as deep as the game's

Ambient occlusion is baked, not drawn: in 26.1 the per-corner brightness is computed on the CPU
(`BlockModelLighter#prepareQuadAmbientOcclusion`) and multiplied into the vertex colour, so the shader
never sees a corner value as such. What it computes is the **mean of four samples**:

```java
float lightLevel1 = (shade3 + shade0 + shadeCorner03 + shadeCenter) * 0.25F;
```

two side neighbours, the diagonal block and the block the face looks at, each through
`BlockBehaviour#getShadeBrightness` - which is

```java
return state.isCollisionShapeFullBlock(level, pos) ? 0.2F : 1.0F;
```

So a corner is one of five values: `1.0`, `0.8`, `0.6`, `0.4`, `0.2`. A fully enclosed corner in
vanilla is **five times darker** than an unoccluded one.

This renderer had three things wrong with that, and the one that was reported - "the ambient occlusion
is too weak" - was the smallest of them:

- the **occluder test** was `!state.isAir()`. Everything that is not air darkened the corners it
  touched, so a torch, a plant, a slab, a pane, a fluid and a glass block all cast the same shadow a
  stone block does;
- only **three** samples were taken - the diagonal and the two sides, never the block the face looks at;
- the value was quantized to `3 - occluders` and then put through `0.6 + 0.4 * corner` in the shader.
  The curve started at `0.6`, so the darkest a corner could get was 0.6 where vanilla draws 0.2: the
  shading was in the right *places* on a flat wall and a third as deep, which is a picture that reads as
  "the AO is weak" rather than as anything being wrong.

The vertex now carries the **count** of the four samples that fill their whole block, and the fragment
shader turns it back into the brightness with `1 - 0.2 * count` - the same average, in one line, with
the five steps coming out at `1.0, 0.8, 0.6, 0.4, 0.2`. Carrying the count rather than the brightness
keeps the curve in one place: a wrong constant there is one line, where a wrong constant in the baker is
a re-bake of every model in the game. Interpolating the count and then applying the curve is the same as
the other way round, because the curve is affine - so the blend a pixel gets is the blend vanilla's four
corner colours get. A test in `chunk.rs` asserts the identity over every count there is, because it is
the whole reason the integer in the vertex format is equivalent to the float the game bakes.

The occluder test is the game's own answer now, sent per state from the JVM next to the two face masks
(`registerBlockStateFaceFlags`'s fourth argument, `FaceFlags::shades`). It has to be the game's answer
rather than something derived here, and the pair that proves it is **glass and ice**: both fill their
block, both have an empty occlusion shape - they are the same shape by every test this side can make -
and `TransparentBlock` overrides `getShadeBrightness` to `1.0` while `IceBlock` does not. Glass casts no
corner shadow in vanilla and ice does, and the only thing that tells them apart is the method itself.
That is also why the flag is not the occlusion mask: an "all six faces covered" test, which is what this
first used, gets ice wrong.

Still not the game's, at that point: the **light** curve. Vanilla samples its lightmap *texture*
(`sample_lightmap(Sampler2, UV2)`), while this shader approximated it with `max(sky, block) * 0.7 + 0.3` per
vertex - a different and much larger approximation than the corner value was. The lightmap handover came
later (see "The lighting was a straight line through a curve the game had already built"), and it brought
the last piece of the same corner with it.

### The corner was darker than the game's, and it was the light rather than the occlusion

With the lightmap in and the occlusion curve fixed, the next report was "the shadow in corners is darker than
the game's". The occlusion was not it, and checking that took two reads:

- **the four samples are the same four.** `BlockModelLighter#prepareQuadAmbientOcclusion` takes its samples
  at `basePosition + info.corners[i]`, and `AdjacencyInfo.corners` are `Direction`s - unit steps in the plane
  of the face around `basePosition` - with `shadeCorner` being one of each pair summed (`corners[0] +
  corners[2]`), and `shadeCenter` the cell **in front of the face** for a cubic face
  (`centerPosition.relative(direction)`) or the owning block for a partial one. That is exactly this side's
  `p1` (the diagonal), `p2`/`p3` (its two sides) and `pos + dir`;
- **the average is the same average.** Vanilla's four values are `getShadeBrightness` samples, each `1.0` or
  `0.2`, so their mean is `1 - 0.2 * count` - the number the vertex carries here. The only place vanilla
  differs is a shortcut that *darkens* (it reuses `shade0` for the diagonal when both cells beyond the
  corners are view-blocking, where this side samples the diagonal), so it cannot be why this side was darker.

It was the **light**, one line away, and the game has a rule for it that is easy to read past:

```java
// LightCoordsUtil.smoothBlend(neighbor1, neighbor2, neighbor3, center)
if (sky(center) > 2 || block(center) > 2) {
    if (sky(neighbor1) == 0) neighbor1 |= center & 0xFF0000;   // and the block channel, for all three
    ...
}
return neighbor1 + neighbor2 + neighbor3 + center >> 2 & 16711935;
```

**Every zero among the three neighbours is lifted to the value of the cell in front of the face**, whenever
that cell holds any light at all, and only then averaged. Which matters because of what the four cells around
a corner usually are: the two sides and the diagonal of a corner inside a building are the **insides of solid
blocks**, and the light stored inside a solid block is nothing at all - so a plain average is dragged down by
cells no light reaches and no camera sees, and the corner comes out at a fraction of the game's. This side
averaged the four plainly, per vertex, which is why its corners were dark and its open faces were not.

`smooth_blend` is that function now, per vertex, with the samples it already had - and it is applied to the
**light**, so the occlusion counts stay as they were. The test pins the rule on the case that made it
visible: three dark cells and a lit one give the lit one's value, a cell holding `4` keeps its `4` and the
average of `4, 14, 14, 14` floors to `11`, a cell holding `1` is not worth lifting anything to, and the two
channels are lifted separately (`15` sky lifts the sky of its neighbours and leaves their block light
alone). The client's own `options.txt` has `ao:true`, so the game being compared against takes the same
path - the AO and the smooth light are one method in `BlockModelLighter`, not two settings.

### The fluid faces: a sprite offset is a fraction, a fluid surface is eight ninths, and lava is neither

A fluid is not a block model - no elements to bake, no variant to look up - so the fluid mesher builds
its quads itself, out of the four **corner heights** of the block (`FluidRenderer`'s shape - renamed from
`LiquidBlockRenderer` in 26.1: a
surface that slopes, sides clipped to it, no face between two blocks of the same fluid). What it samples
is `*_still` on the top and the underside and `*_flow` on the sides, which is the game's own choice.

Those sprites are animated, and their faces were the last ones still baked against this side's copy of
them - so a lava fall was a still picture while the fire next to it moved. They go through the same
decision everything else does now (`FluidSprite::flags` writes the vertex flag, `Atlas::game_atlas_rect`
makes the call), and because the game's rectangle is one *frame* of the sprite, that also fixes the
frame: sixteen frames of `lava_flow.png` are sixteen frames, not one face.

Two more things came out of reading the game's `FluidRenderer` next to ours, and the second is the
picture that was reported as "lava looks like a cube".

**A sprite offset is a fraction.** Every coordinate the game's fluid renderer uses is a fraction of the
sprite:

```java
float u0  = sprite.getU(0.0F);                  // the sprite's left edge
float u1  = sprite.getU(0.5F);                  // its middle
float v01 = sprite.getV((1.0F - hh0) * 0.5F);   // the fluid's height, in the top half of the sprite
float v1  = sprite.getV(0.5F);                  // the sprite's middle row
```

This side's form added whole **pixels** to the rectangle's corner
(`[sprite.0.0 + u, sprite.0.1 + v]`), and it produced exactly the game's picture for exactly one pack:
vanilla's, where `water_flow.png` and `lava_flow.png` are 32x32 a frame and the half-sprite offsets *are*
sixteen pixels. A 16-pixel pack got the whole sprite where the game takes half of it; a 64-pixel pack got
the top-left sixteenth. The offsets are fractions now (`FluidSprite::at`, which is `getU`/`getV`), a test
asserts that over every offset the mesher passes the new form lands on the same pixel as the old one *for
a 32-pixel sprite* - so nothing about a vanilla pack moved - and the frame rule
(`FluidSprite::frame`: an animated sprite is packed as a strip of square frames, so one frame is as tall
as the strip is wide) is what keeps a 16x512 strip from putting sixteen frames on one face.

**A fluid surface is `amount / 9`, and lava is not special.** The mesher's own comment said lava was drawn
at the full block "because lava is thick enough to fill the block it is in", and a falling fluid at the
full block "because it is a column rather than a surface", and claimed both came from the game's
`getOwnHeight`. The game says:

```java
// FluidRenderer#getHeight(level, fluidType, pos, state, fluidState)
if (fluidType.isSame(fluidState.getType())) {
    BlockState above = level.getBlockState(pos.above());
    return fluidType.isSame(above.getFluidState().getType()) ? 1.0F : fluidState.getOwnHeight();
}
```

`getOwnHeight` is `amount / 9` for every fluid there is, and a lava **source** is amount 8 - never 9, for
a source or for a falling fluid. So vanilla draws a lava lake with every surface one ninth of a block
below the top, and that ninth is the whole difference between a lava lake that reads as a liquid surface
and one that reads as a floor of lava-coloured cubes: at the full height the sides and the top are one
unbroken shape with no surface anywhere in it. The only thing that lifts a block to the full height is
more of the same fluid *directly above it* - which is the column case, and it is a property of the
neighbours rather than of the fluid. The mesher asks the block above each of the four corner blocks now
(`fluid_height(amount, same_above)`), and the "falling" bit it used to branch on was never even set: the
payload writes `kind | amount << 2` and nothing else.

**A moving surface turns its sprite.** `FluidRenderer#tesselate` draws a *still* sprite on the top face only
when the fluid's own flow is zero. `FlowingFluid#getFlow` is the gradient of the fluid's own height over its
four horizontal neighbours:

```java
// FlowingFluid#getFlow
float distance = fluidState.getOwnHeight() - neighborHeight;   // downhill, or - for a neighbour the fluid
if (distance != 0.0F) { flowX += step.getStepX() * distance; } // can flow *past* - the fluid one block
return new Vec3(flowX, 0.0, flowZ).normalize();                // under it, one block down less 8/9

// FluidRenderer#tesselate, when that is not (0, 0)
float angle = (float)Mth.atan2(flow.z, flow.x) - (float)(Math.PI / 2);
float s = Mth.sin(angle) * 0.25F;
float c = Mth.cos(angle) * 0.25F;
u00 = sprite.getU(0.5F + (-c - s));  v00 = sprite.getV(0.5F + (-c + s));   // and three more
```

The four offsets that builds are the corners of a square **half the sprite across**, turned by the angle,
and they go to the face's corners north-west, south-west, south-east, north-east - which is the order the
four corner heights are already in. `fluid_flow` and `flowing_top_offsets` are the same arithmetic, and a
test walks sixteen angles asserting the quarter's side is 0.5 *in sprite fractions* whatever the angle: the
game's `sin * 0.25` and `cos * 0.25` are each half of the quarter's extent in one axis, and reading them as
the whole extent is the mistake that makes a flowing face a half-sprite square.

The one step of `getFlow` that is not a height is `blocksMotion()`. A neighbour holding none of this fluid
that the fluid can flow *past* - air, a plant, anything without collision - is looked through to the block
below it, whose fluid counts as one block down less the eight ninths a falling fluid is drawn short by, and
that is what points a stream at the edge it is about to pour over. It is the fifth field of the per-state
flags the JVM sends now (`BlockFaceFlags.kt` reads `blocksMotion()` off the state,
`registerBlockStateFaceFlags` carries it), and it is independent of the four that were already there: a slab
stops a fluid and occludes nothing. The falling branch of the game's function is left out - it needs the
fluid's `FALLING` property, which the payload does not carry - and what it would add is a vertical component
the caller never reads: the top face only ever wants the horizontal *direction*.

**A fluid is two objects, and only one of them was ever handed over.** The next report was "in a lava fall
there are gaps between the stepped flowing lava - the flowing state is wrong on the Rust side", and it was:

```java
// Fluid
public boolean isSame(Fluid other) { return other == this; }        // identity, not "the same kind"

// LiquidBlock, the fluid state per level
this.stateCache.add(fluid.getSource(false));                        // level 0: FlowingFluid#getSource
for (int level = 1; level < 8; level++)
    this.stateCache.add(fluid.getFlowing(8 - level, false));        // levels 1-7: FlowingFluid#getFlowing
this.stateCache.add(fluid.getFlowing(8, true));                     // level 8: the *falling* state
```

So a fluid is a source object and a flowing object (`Fluids.LAVA`, `Fluids.FLOWING_LAVA`), a block answers
whichever its level came from, and `FLOWING_LAVA.isSame(LAVA)` is **false**. The JVM's `fluidByte` classified
against the source alone, so every flowing block - every falling block (level 8), every spreading block
(levels 1-7) - came out as kind 3, "a fluid this mesher does not know", and `bake_fluid_faces` skips those:

```kotlin
val kind = when {
    type.`isSame`(Fluids.WATER) || type.`isSame`(Fluids.FLOWING_WATER) -> 1   // was: source only
    type.`isSame`(Fluids.LAVA) || type.`isSame`(Fluids.FLOWING_LAVA) -> 2     // was: source only
    else -> 3                                                                 // ...and 3 is not drawn
}
```

Which means: a lava lake was drawn only where its blocks were *sources*, a lava fall was not drawn at all
(its whole column is level 8), and the hole where the fall should be is what read as "gaps between the
stepped flowing lava". Both halves of each fluid are named now, and a test on the writer's source asserts it
- the mistake is a missing name, and a missing name is what a test can see.

The same identity comparison is in vanilla's own renderer (`FluidRenderer.isNeighborSameFluid`), where it
means a *source* and a *flowing* block are different fluids to the height arithmetic: a source next to a
flowing neighbour averages a `0.0` in. This side merges the two kinds, so its surfaces are *smoother* there -
continuous where vanilla steps down - which is a deliberate difference rather than an oversight.

**A fluid face is lit by the fluid, not by what it faces.** The second half of the same report was "when
there is a block above the lava, the lava goes black - you can just about make out the texture, the
brightness is very low". `FluidRenderer` asks its own `getLightCoords` for every face it writes:

```java
private int getLightCoords(BlockAndTintGetter level, BlockPos pos) {
    return LightCoordsUtil.max(LevelRenderer.getLightCoords(level, pos),
                               LevelRenderer.getLightCoords(level, pos.above()));
}
```

so the fluid's **own cell** is half of the answer everywhere - the top face, every side, and (with the cell
below instead) the underside. This side read a single neighbour: the block above for the surface, the block
the side faces for a side, the block below for an underside. A solid block has no light in it, so lava with a
block over it took the light of the *block* as its surface light and came out black; its sides took the
light of the stone beside them and were black for the same reason. A lava cell holds block light 15 of its
own, so the brightest of the two is the lava's own emission and a covered lava surface is lit by the lava.

Two things about that `max`: it is `LightCoordsUtil.max`, which takes the maximum of the block nibble and the
maximum of the sky nibble **separately** - not the larger of the two packed bytes, where the sky nibble being
the high half makes the darker cell compare as the brighter one - and it is per face, so all four faces of a
fluid block now read `light_here`.

**A whole block of fluid is not averaged at all.** The gaps in the fall were still there after the fluid
byte was fixed, and the reason is the second half of the same `if` in `FluidRenderer#tesselate`:

```java
float heightSelf = this.getHeight(level, type, pos, blockState, fluidState);
if (heightSelf >= 1.0F) {
    heightNorthEast = 1.0F;      // no averaging *at all*: a block that stands a whole block tall
    heightNorthWest = 1.0F;      // - which is every block of a falling column, because the same fluid
    heightSouthEast = 1.0F;      // is directly above it - has all four corners at its own top
    heightSouthWest = 1.0F;
} else {
    ... the four calculateAverageHeight calls ...
}
```

This mesher averaged all four corners for every block. So a full-height block's corner was dragged down by
whatever was *diagonally* beside it - the thin spreading lava at the foot of the fall, the step it poured
over - and its side faces stopped below its own top. The block above it starts at the block boundary, and
the difference between the two is an open slit in the wall: not a full-height hole, a **triangle**, because
one corner of the face still reached the top and the other did not. Repeated at every block of the column,
that is what "in a lava fall the stepped flowing lava has gaps between it" was.

The rule is `own_height >= 1.0` now, which is `fluid_height(amount, the block above holds this fluid)` - the
same `same_above` the surfaces already use. Two things came out of writing it down:

- the geometry is **testable without a GPU**. `bake_fluid_faces` was split into the sprite lookup and
  `bake_fluid_faces_with`, which takes the sprites as an argument, so a test can mesh a world built by hand
  and read the quads back out of the vertex bytes (a byte an axis, in sixteenths, plus the flag that means
  sixteen blocks). The test asserts that **both** top corners of every wall of a full block of fluid are the
  top of that block, and it was checked against the old code: the north face of the middle block of a
  synthetic fall comes out `y 1.0 -> 2.0` on the west corner and `1.0 -> 1.6875` on the east one, which is
  the slit, in numbers;
- and the other half of the rule has a test too - a *thin* layer of fluid still averages its corners, which
  is what makes a lake's surface slope and a stream follow its flow. The two are one branch in the game, so a
  test for one of them is only worth anything next to a test for the other.

The same tool answered the next report in one round. "A purely vertical fall is complete, the cracks only
appear on a stepped one" is a statement about *where* the two surfaces fail to meet, and it says which face
is missing: at the foot of a drop the falling column's last block is a whole block tall (the same fluid is
above it) while the lava it lands in is not, so the landing's surface - averaged, and lower - is below the
column's, and the band between them is the riser of the step. The face on that band was being skipped for
holding the same fluid. The synthetic step in the test showed it as an empty list where the column's side
should have been, and with the rule above the face is there and spans its own block.

**And then a face inside the fluid is not the same as a face on it.** "The internal culling is off too -
inside the lava you can see the texture of flowing lava that is not exposed to air" is about the *other*
half of that face: the game draws the whole block, and everything below the neighbour's surface is geometry
inside the fluid, carrying a flow texture, that nothing can see from outside and a camera *inside* the lava
looks straight at. So the band is what gets drawn now, not the block: its floor is the neighbour's own
surface measured at the shared edge, by the same average, and its top is this block's. Two blocks of one
fluid at one level have no band at all - their corner heights come out of the same four blocks - so a lake
still pays nothing for it.

**The same report had a second half: the fluid object.** "The lava's flowing state still does not match the
game" is the other place `Fluid#isSame` being *identity* shows up, and this mesher had been treating the two
halves as one liquid everywhere:

| the game asks | with |
| --- | --- |
| is the same fluid above this block (which decides its height) | `fluidType.isSame(above.getFluidState().getType())` |
| does this neighbour affect the flow | `neighbourFluid.isEmpty() \|\| neighbourFluid.getType().isSame(this)` |
| what does this corner average | `getHeight` per block, and `-1.0` for one that is solid and is not this fluid |

A source beside a flowing block is **two liquids that do not join** to all three. That is not a detail about
mixing: the flow vector decides which way the rotated quarter of the flowing sprite points, so a neighbour
that should not count turns the pattern, and the height rule decides where the surface is. The byte carries
the bit now (`fluidByte`'s third field, which used to be documented as "falling" and never written by
anyone), the mesher draws both halves with one set of sprites and asks the game's question everywhere else,
and a test on the writer's source asserts that both halves are named:

```kotlin
val (kind, flowing) = when {
    type.`isSame`(Fluids.WATER) -> 1 to false
    type.`isSame`(Fluids.FLOWING_WATER) -> 1 to true
    type.`isSame`(Fluids.LAVA) -> 2 to false
    type.`isSame`(Fluids.FLOWING_LAVA) -> 2 to true
    else -> 3 to false
}
```

**And with the halves distinct, the corner average had to become the game's.** This side averaged the corner
blocks that held the same fluid - a plain mean, and a different *set* of blocks: air contributed nothing at
all, so a surface came out level where the game's tapers at an open edge, and the other half of the fluid
was worth a full block where the game gives it a zero. `sampled_height` and `add_weighted_height` are the
game's `getHeight` and `addWeightedHeight` now, sentinel included: `1.0` for a column, `amount/9` for a
surface, `0.0` for a block that does not hold this fluid and is not solid (air, a plant, the other half),
and `-1.0` for one that *is* solid, which the average drops. A height of `0.8` or more counts **ten times**,
which is what keeps a surface from sagging towards its edges. Two tests pin the arithmetic to one number
each: a lone `8/9` block in air has corners at `8/9 * 10/12`, and the same block against stone at
`8/9 * 10/11` - the stone dropped rather than counted.

Still the game's and not ours, in the fluid path:

- the faces vanilla **culls** and this does not: a fluid's side against a block whose occlusion shape
  covers it (`isFaceOccludedByNeighbor`), which is a face drawn between two opaque neighbours - invisible
  either way, and paid for in the arena - and `isNeighborStateHidingOverlay`, the rule that lets a
  half-transparent neighbour hide the fluid face behind it;
- the last thousandth of a block, and the inside of a fluid. Vanilla lowers a fluid's surface and insets
  every side by `0.001F` so that a fluid face does not fight with the block face it lies against, and
  writes every face that is not a water overlay **twice**, wound both ways (`addBackFace`), so a fluid is
  visible from inside itself. Neither survives this side's vertex format: a position is a sixteenth of a
  block an axis (see "A face between the lines of the vertex format"), where a thousandth rounds away, and
  the terrain pass culls back faces, so a face seen from inside a fluid is not drawn. The surface's own
  height is on that grid too - `8/9` lands on `14/16` - which is a sixteenth of a block below where vanilla
  puts it;
- the sides between two blocks of one fluid, which this side draws **only where they are exposed**. The game
  draws all of them - `isNeighborSameFluid` is on the top face alone in 26.1 - and every one that is inside
  the fluid is invisible from outside; this side leaves those out, which is the same picture for fewer
  vertices, and the one difference is that a camera *inside* a fluid sees the game's interior faces and not
  these;
- `blocks_motion` stands in for `isSolid()` in `sampled_height`, the one place the corner average needs it.
  The two agree for everything a fluid meets - air, stone, and a fluid itself, which is not solid - and
  differ for a cobweb and a bamboo sapling, which this then counts as a `0.0` sample where the game drops
  them;
- water's **colour**, which is one constant here and a function of the biome in the game:
  `BiomeColors.getAverageWaterColor` is the biome's temperature and downfall, so an ocean, a swamp and a
  cold river are three colours in vanilla and one here. It was invisible while water was not drawn at all,
  and it is the next thing to close - see "The water was baked and never drawn" below, and the note on
  `WATER_TINT`;
- the **order** the translucent layer is drawn in. The game sorts translucent geometry back to front,
  because that is what blending needs when two surfaces overlap; this side draws the layer far-to-near by
  section and in bake order within one, which is right for the water surface and can be wrong where two
  translucent blocks overlap inside the same section. It is a picture difference, not a hole.

### Minecraft kept meshing the world for a pass it was no longer drawing

Taking the terrain pass over stopped Minecraft *drawing* its sections, and did nothing about it *building*
them. `RustChunkBakeMixin` injects at the head of `RebuildTask#doTask`, calls the Rust bake, and lets the
task carry on: the game then looked up a model for every block in the section, built the vertices, packed
them into the scratch buffers, ran the sort-key pass and uploaded the result into its own uber buffers -
for a pass that had been taken over. On a populated world that is the whole cost of the terrain pipeline,
paid for geometry nothing reads.

**What made it survivable was stopping it, and what made stopping it delicate is that the rebuild does
three jobs and only one of them is the mesh.** `SectionCompiler#compile` also collects the section's
**renderable block entities** and computes its **visibility set**, and both are read by things that are
not the terrain:

```java
// LevelRenderer
for (SectionRenderDispatcher.RenderSection section : this.visibleSections) {
    List<BlockEntity> renderableBlockEntities = section.getSectionMesh().getRenderableBlockEntities();
```

That list is how a chest is found at all - it is the *only* source of block entities to render - so
throwing the whole compile away is every chest, sign and banner in the world disappearing. And
`SectionOcclusionGraph` walks `facesCanSeeEachother` to decide which sections are even worth visiting, so
an empty mesh turns the culling off for everything behind it. The picture would look almost right.

So the compile still runs and the hook is on the last thing it produces: `new CompiledSectionMesh(pointOfView, results)`, the one call in `doTask` handed both the results and the layers. Emptying
`renderedLayers` there leaves the caller taking its `results.renderedLayers.isEmpty()` path, which is the
path an **all-air section** takes - the one state in this dispatcher that is known to be safe:

```java
if (results.renderedLayers.isEmpty()) {
    SectionMesh oldMesh = RenderSection.this.setSectionMesh(compiledSectionMesh);
    ...
    return SectionTaskResult.SUCCESSFUL;   // what an empty chunk already does every frame
}
```

`MeshData` is `AutoCloseable` and its `close` is what returns the scratch buffers to a **fixed pool**, so
the layers are closed one at a time before the map is emptied - `EnumMap.clear()` does not close what was
in it, and a bare `clear()` there leaks the whole section's staging buffers out of that pool.

**And the gate is not the setting.** `meshesInRust()` asks whether Rust has *this* section, not whether
the path is on, because a payload leaving is not the same as a mesh existing: `bakeSections` returns as
soon as the payload is copied, the bake happens on the Rust side's own thread off a queue, the result
lands in the arena some frames later, and the arena refuses sections it has no room for. Dropping
Minecraft's mesh for a section Rust is not drawing yet is a section **nothing** draws - and for a section
whose blocks are not changing there is no second rebuild to notice. So the answer is a set of the
sections Rust has been handed **at least once**, which the next rebuild of the same section closes by
definition. A section offered for the first time keeps Minecraft's mesh one rebuild longer, and every
rebuild after that drops it.

The two tables are cleared together everywhere, and that is a hole-in-the-world invariant rather than
tidiness: a stale "Rust has it" entry is a section this side believes is drawn while Rust has been told to
forget it. `forgetRefused` is the one that is not bookkeeping - the arena refusing a section is the one
case where Rust was told and is still not drawing - so the refusal path drops the mark and `redirty` asks
the game for the rebuild that will use Minecraft's mesh again. The rule is a test rather than a review,
because there is no log line for getting it wrong: from both sides the section looks handled.

### Turning the switch off has to give the world back

While Minecraft's meshes were being built anyway, turning `rust terrain` off needed nothing: they were the
fallback all along, and the switch only said who drew them. That is no longer true - with the path on, the
sections in view are holding **empty** meshes - so `RustChunkBake.refresh()` now calls `allChanged()` on
**both** transitions rather than only on the way on:

```kotlin
if (enabled == previous && reportedState) {
    return
}
// ... both directions rebuild the world ...
Minecraft.getInstance().execute { Minecraft.getInstance().levelRenderer.allChanged() }
```

A switch-off without that is the graph off and Minecraft with nothing to draw, which is the whole world
gone. `allChanged` is the same call a resource reload makes, and the sections come back over the next
second or two - the window the `on` direction already had. Only a real change reaches it, so a session
that never touches the switch never pays for it.

### The water was drawn before the entities, so a mob in a lake looked dry

The second terrain group's takeover recorded **both** groups in one call:

```java
// the opaque group's pass - and, at the time, the translucent one's too
graph.render_with_mvp(...);   // draws `terrain` and then `translucent_terrain`
```

which reads as an optimization - one call, both groups, in the order `graph.yaml` lists them - and is
the wrong moment in the frame for the second one. Minecraft's own order is
`LevelRenderer#addMainPass`:

```java
684:  chunkSectionsToRender.renderGroup(ChunkSectionLayerGroup.OPAQUE, ...);   // solid + cutout
699:  this.submitEntities(poseStack, levelRenderState, this.submitNodeStorage);
725:  this.featureRenderDispatcher.renderTranslucentFeatures();
742:  chunkSectionsToRender.renderGroup(ChunkSectionLayerGroup.TRANSLUCENT, ...); // water, glass
```

Water is drawn **after** the entities, and it has to be: it is blended into a frame that already has them
in it, so a mob standing in a lake has the water blended over the half of it that is under the surface,
and nothing over the half above. With both groups recorded at the opaque pass, the entities are drawn
*after* the water - so the water is behind them, and the report was

> rust接管地形后，进入水里的生物就像没有入水一样，没有被水遮挡，无论是在水面还是水下

`render_terrain_pass` now takes a flag naming **which group** to record, and each takeover records its
own: the opaque pass at the opaque group's pipeline, the translucent pass at the translucent group's. Each
one opens its own pass over the frame's colour and depth, which is what the game does too.

**The depth clear moved with them, and it had to.** `should_clear_depth` is a local of one `render` call,
so two calls in one frame each start believing they are first - and the second one clearing the frame's
depth would erase the entire opaque world's depth from under the water, after which every face drawn would
be tested against a bare buffer. Which pass claims the clear is now asked of the **layers** rather than of
a name, because that is what actually distinguishes them: the opaque group draw the solid layer, and the
translucent one draws none.

```rust
let opaque_group = Self::terrain_layers(&pipeline_config.geometry)
    .iter()
    .any(|(layer, _)| *layer == RenderLayer::Solid);

let will_clear_depth = should_clear_depth && overridden.is_none() && only.is_none_or(|_| opaque_group);
```

The `graphDrawn` frame flag that used to coordinate the two takeovers is gone with it: it existed to stop
the second visit from recording the graph a second time, and now the second visit is *supposed* to record
its own group. What it also did was refuse the translucent takeover on a frame where the opaque one had
not happened - which is now impossible to get wrong in that direction, because each group's pass is the
one that draws it.

### The surface of a lake was invisible from inside it, because it is drawn twice and this drew it once

A fluid's top face is baked with its normal **up**, and this renderer culls back faces - so a player under
water looking at the surface is looking at the back of a quad and sees nothing at all:

> 在水里向上看看不见水面的纹理

Vanilla draws that surface **twice**, and the flag is right there in `FluidRenderer`:

```java
fluidState.shouldRenderBackwardUpFace(level, pos.above())   // the top quad's `addBackFace`
```

`FluidState#shouldRenderBackwardUpFace` is three-by-three, one block up: if any of those nine cells is
neither the same fluid nor a solid block, the surface gets a second copy of itself wound the other way.
The "solid" test is `BlockState#isSolidRender`, which this side carries as the face flags' occlusion mask
rather than as a flag of its own - a block that occludes all six of its neighbours is exactly one that
renders solid.

**The first version of this fix did nothing at all, and the quad count is what said so.** It wound the
quad and then called `reverse()` on the four corners:

```rust
let mut wound = wind_quad(corners, normal);
if back_face {
    wound.reverse();      // rearranges four vertices, appends none
}
```

which is a *rearrangement* and not a second quad - the surface was still drawn from one side, and the test
that asked for seven quads got six. The back face has to be appended, and its indices have to be written
too: a vertex buffer with eight vertices and six indices draws one quad and leaves the other one in the
arena unread.

```rust
let mut quads = wound.to_vec();
if back_face {
    quads.extend(wound.iter().rev().copied());
}

for quad in 0..quads.len() / 4 {
    let base = first as u32 + (quad as u32) * 4;
    baked_layer.indices.extend(INDICES.iter().flat_map(|index| (index + base).to_ne_bytes()));
}
```

The test asserts **both** halves: an open surface bakes seven quads (one bottom, four sides, a top and its
mirror) and the same fluid under a solid roof bakes six (no mirror, because there is no view from above to
spoil). A back face that is always drawn is two draws and two blends for every lake in the world, and one
that is never drawn is the report above.

### `tex_coords2` and `blend` were hardcoded to zero because nothing was ever going to fill them

The vertex stage wrote `vec2(0.0, 0.0)` and `0.0` into two varyings the fragment stage never mentioned.
Read as source that is a gap - "these are stubbed out, something should be filling them in" - and the
search for what is short, because there is nothing:

**They are leftovers from the GLSL renderer this one replaced**, where `texCoord2` was a second UV set and
`blend` chose between the two. The vertex format this side bakes has no source for either. `Vertex` is
`position`, one `uv` pair, a colour, ten animated-texture bits and a lightmap byte - thirteen bytes, packed
in `render::pipeline` - and the game's own terrain shader is the same shape:

```glsl
in vec3 Position; in vec4 Color; in vec2 UV0; in ivec2 UV2;
```

with a fragment stage that reads `texCoord0` and `vertexColor` and nothing else. So they are removed
rather than filled in: a varying that is always zero reads as "something should be filling this in", and
the next person has to do this same search to find out that nothing should.

**The same search found four more**, which is why it is worth doing as a test rather than by eye:
`world_pos`, `section`, `ao` and `int` were all declared as varyings and never read by the fragment stage
either - `section` was not even *assigned* - and the local `ao` the quad's first vertex carried existed
only to feed the varying that nothing read. All gone. `world_pos` stays as a local, because the vertex
stage uses it for the position and the fog distances.

**And the class is now checked.** `dead_varying_tests` parses the shader, walks the fragment entry point's
argument, and reports every location-bearing struct member the function never touches - a read of `in.ao3`
is an `AccessIndex` off the argument handle, so the set of fields reached that way is the set that mean
something. It is not a blanket "every varying is read": a member may be unread *on purpose*, and the test
accepts that only when the comment directly above the declaration says so, in the words "unused", "never
read", "nothing reads", "not read" or "does not read". Satisfying it that way means somebody wrote down
why, which is the whole point.

Two rounds of getting that check wrong are worth recording, because both are the shape this kind of test
fails in. The first version searched for the member's **name** and looked a fixed distance back - which
finds the name inside the comment explaining some *other* member, and then a window wide enough to cover
anything says "yes" to everything. The second anchored on the declaration correctly but still measured a
fixed window, so it picked up prose from before the comment it wanted. It is line-based now: the anchor is
the `@location(N) name:` line, and the window is the contiguous comment block directly above it. The
detector also has a fixture test - the branching shape is fed to it and must be reported - because a test
that asserts "nothing was found" cannot be told from one whose detector never finds anything.

### The game animates every mip level of its atlas, so there is no moving level to clamp to

The idea was to clamp `lod_max_clamp` on `@sampler_mc_block_atlas` down to the mip level the game is
actually animating - the reasoning being that only level 0 moves and the levels above it are stale
pictures of an old frame. **The reasoning does not hold, and the source says so plainly:**

```java
private void uploadAnimationFrames() {
    if (this.animatedTexturesStates.stream().anyMatch(SpriteContents.AnimationState::needsToDraw)) {
        for (int level = 0; level <= this.maxMipLevel; level++) {
            try (RenderPass renderPass = RenderSystem.getDevice()
                    .createCommandEncoder()
                    .createRenderPass(() -> "Animate " + this.location, this.mipViews[level], OptionalInt.empty())) {
                for (SpriteContents.AnimationState animationState : this.animatedTexturesStates) {
                    if (animationState.needsToDraw()) {
                        animationState.drawToAtlas(renderPass, animationState.getDrawUbo(level));
```

The loop walks **the whole chain**, one render pass per level into that level's own view, and
`getDrawUbo(level)` is `spriteUbosByMip[level]` - one uniform buffer per entry of `byMipLevel`. Every
level gets the current frame in the same call. No level is staler than any other, so a clamp to "the
level that moves" would clamp to all of them, which is no clamp at all.

The chain reaches us intact, which is why the question was worth checking rather than assuming: a sprite
uploads each level through `writeToTexture(..., mip_level, ...)`, our native side passes that
`mip_level` to `Queue::write_texture`, the animate pass targets `mipViews[level]`, and
`createTextureView` resolves `base_mip_level`. Any one of those dropping the level would have made the
premise true and produced exactly this symptom - they do not.

What a clamp to 0.0 *would* do is throw the chain away for this atlas: a distant lump of lava would
resolve one texel of a 16x16 sprite per pixel, which is the aliasing the chain exists to remove. That is
more temporal noise at distance, not less, so the clamp is off by default.

It exists anyway, as **`Atlas sampled from its base mip only`**, because the case for it is a picture
rather than an argument and the two runs are cheap to compare. Applying it rebuilds the graph, since a
sampler is built with the pipelines.

### Every instance flag is a setting now, and the WGPU_* escape hatch works again

`instance_flags` was building the flag set by hand, and three of the four flags it touched were
constants:

| flag | was | is |
| --- | --- | --- |
| `DEBUG` | **always on** | `shader debug info`, on by default |
| `VALIDATION` | always on until it became a switch | `host validation`, off |
| `GPU_BASED_VALIDATION` | behind a switch | `gpu based validation`, off |
| `VALIDATION_INDIRECT_CALL` | **never set** - the code set `DEBUG` alone instead of `InstanceFlags::debugging()` | `validate indirect calls`, on |
| `DISCARD_HAL_LABELS` | never set | `discard backend labels`, off |
| `ALLOW_UNDERLYING_NONCOMPLIANT_ADAPTER` | never set | `allow non-compliant adapter`, off |

The last two are new switches rather than moved constants, and they are the two the renderer had no way
to reach at all.

**And the environment variables work again.** wgpu documents `WGPU_DEBUG`, `WGPU_VALIDATION`,
`WGPU_GPU_BASED_VALIDATION`, `WGPU_VALIDATION_INDIRECT_CALL`, `WGPU_DISCARD_HAL_LABELS` and
`WGPU_ALLOW_UNDERLYING_NONCOMPLIANT_ADAPTER`, and every one of them was silently ignored here: they are
read by `InstanceFlags::with_env()`, which this renderer had never called. It is applied **last**, so the
variables win over the settings - which is the convention they exist for, and a launcher passing
`WGPU_VALIDATION=1` to diagnose a crash means it.

Two flags are deliberately *not* offered, and the reasons are in `instance_flags`:

- **`AUTOMATIC_TIMESTAMP_NORMALIZATION`** exists to save the caller the multiply by the timestamp
  period, and `timing.rs` already does that multiply. Turning it on would add a compute shader to every
  query resolve to save an operation that is not being performed.
- **`STRICT_WEBGPU_COMPLIANCE`** restricts the feature set to the WebGPU specification's, and this
  renderer asks for `IMMEDIATES` and timestamp queries, which are beyond it. The honest version of that
  switch is one that turns off half the renderer, so it is not a switch.

All six are read at launch, so all six need a restart, and the line that reports them is written once a
world is loaded - the same line, and the same reason, as the validation layers: nothing from the adapter
onwards reaches the log, because the instance is built before `setPanicHook` installs `env_logger`. It is
built from the flags the instance **actually** has rather than from the settings, so a `WGPU_*` override
shows up here instead of being invisible:

```text
wgpu-mc: the instance was built with backend validation on, GPU-based validation off, shader debug info
on, indirect-call validation on, backend labels kept, non-compliant adapters off.
```

### The arena drain: one write per run, and a read lock instead of a write lock

Two things on the section-upload path, both found by reading it rather than by a symptom.

**`write_buffer` was called twice per layer per section.** `Queue::write_buffer` is not free and the copy
is not what it costs: wgpu-core allocates a **`StagingBuffer` per call** and frees it after the next
submission. So a frame moving a thousand sections over three layers was making thousands of short-lived
allocations and thousands of small transfers - the exact shape of loading a world or flying forward,
which is when this work is real. A settled world moves nothing and none of it runs.

The ranges are gathered into one scratch buffer and written **one `write_buffer` per run of contiguous
bytes**, with vertices and indices sorted by offset within a layer so a run is always ascending. Whether
ranges are contiguous turned out not to be a guess - consecutive allocations from the free list are
adjacent - so measured on a loading world:

```text
136 baked section(s) moved into the arena (140 in it now), 562 upload range(s) in 1 write(s) - 561 merges
 62 baked section(s) moved into the arena (202 in it now), 244 upload range(s) in 1 write(s) - 243 merges
```

**One write for the whole frame's drain**, against 562 and 244 before. The merge count is on the line
because "coalescing helps" is a claim about those two numbers and nothing else.

The runs are collected first and written afterwards, deliberately: writing inline would mean holding a
borrow of the scratch buffer across the `extend_from_slice` that appends to it.

**And the gather held a `write` lock it never wrote through.** `let sections_source =
scene.section_storage.write()` is only ever iterated - so the render thread took the arena's *exclusive*
lock for the whole of the gather, once a frame, while every bake thread's `allocate` needs that same
lock. The two contend in both directions: a bake holding it makes the frame wait, and the frame holding
it makes every waiting bake wait longer, on the threads trying to keep Minecraft's chunk build moving. It
is a `read()` now, which is also what makes the two access patterns compatible rather than merely brief.

### Three window modes: exclusive, borderless, windowed

The game has two. `Options#fullscreen` is a boolean in `options.txt` whose handler calls
`Window#toggleFullScreen`, and `Window#setMode` turns it into one of two GLFW calls -
`glfwSetWindowMonitor(handle, monitor, ..)` for fullscreen and `glfwSetWindowMonitor(handle, 0L, ..)` for
windowed. There is no third state for a window that covers the monitor **without owning the display mode**.

| | GLFW | display mode | decorations |
| --- | --- | --- | --- |
| Exclusive | `glfwSetWindowMonitor(handle, monitor, ..)` | **changes** | none (the monitor owns it) |
| Borderless | `glfwSetWindowMonitor(handle, 0L, x, y, w, h, -1)` | untouched | **removed by hand** |
| Windowed | the same windowed call, at the saved bounds | untouched | restored |

GLFW has no borderless mode, so borderless is the *windowed* call with the window moved to the monitor's
origin, sized to its **video mode** (not its work area - a borderless window that leaves the taskbar
visible is a maximised window), and `GLFW_DECORATED` cleared. The display mode is never touched, which is
the whole reason to want it.

**The default is windowed**, and it is named explicitly:

```rust
#[serde(default = "no_fullscreen_mode")]
pub fullscreen_mode: EnumSetting,
```

**`#[serde(default)]` alone would have been wrong, and it was measured.** `EnumSetting`'s own `Default` is
`selected: 0` - the *first* variant - so a bare attribute hands every fresh config exclusive fullscreen
however the enum is ordered, and `#[default]` on the variant does nothing. The config was deleted, the
client started, and it took the display over. This is the same trap the `animated_textures` field
documents, hit a second time in the same file.

#### The two dead ends, and what they cost

Both are worth recording because each looked correct and each wasted a round.

**A mixin on `Window#setMode` cannot report its own failure.** That method is called from `Window`'s
constructor, and `Minecraft` wraps the construction in a `try` that catches **exactly one type**:

```java
try {
    windowCandidate = new Window(this, displayData, ..., backend);
    ...
} catch (BackendCreationException var24) { ... }
```

so anything else raised in there escapes, leaves the window null, and the client exits through a path that
**reports nothing** - no crash report, no fatal line, one unrelated `warn` as the only clue, twice. The mode
is therefore applied **after** the window exists, by reflection on the private `setMode` plus the private
`fullscreen` and `handle` fields, from a client tick. A failure there is a logged warning instead of a
silent exit, and nothing runs inside a constructor.

**A row injected into `VideoSettingsScreen` is a row on a screen nothing opens.** `OptionsScreenMixin`
already replaces that whole screen:

```java
if (VIDEO.equals(title)) {
    cir.setReturnValue(Button.builder(VIDEO, btn -> parent.getMinecraft().setScreen(new OptionPageScreen(parent))).build());
}
```

The renderer's own `OptionPageScreen` **is** the video settings page, so the row belongs on the Electrum
page, in the place the game's fullscreen checkbox already occupied - same caption, so a player finds it
where they always did. Two separate mixins (a `@ModifyArg` and then a `@Redirect`, the second with
`require = 1`) were written and both were pointless: they applied cleanly to a screen that is never
constructed.

`options.fullscreen()` itself is left alone. The F11 handler still writes it from `Window#isFullscreen`, the
game still reads it at startup to decide what window to create, and `Window#isFullscreen` is answered from
the mode actually applied - so a player who never opens the page gets exactly the behaviour they had.

#### The mode has to be applied when the settings are read, not on the first frame

The first version applied it from a client tick, `DisplayMode.applyOnFirstFrame`, and that is one frame too
early: **the settings are read later than the first frame**, so `windowMode` answered the static's initial
value - `Exclusive` - and the client took the display over before switching to the mode the config named.
Measured, in one log:

```
[22:15:32] wgpu: the window is in EXCLUSIVE mode
[22:15:43] wgpu-mc: the window mode is now 2; asking the JVM to put the window in it
[22:15:43] wgpu: the window is in OFF mode
```

Eleven seconds of a display mode switch nobody asked for, on the way to the right answer. The apply now
happens in `sendRunDirectory`, right after `Settings::load_or_default`, so the value read back is the one
that was just loaded - and the first-frame call stays as a belt for the mod-constructor path, where the
settings arrive before there is a window. One line, once, in the log now:

```
wgpu: the window is in OFF mode
```

### The fluid flicker: the levels are not blended any more, because two of them are two phases

**Measured, by the player, and it is the kind of reading that settles a question static analysis had been
going round in circles on:** with `atlas_base_mip_only` on - which clamps `lod_max_clamp` to `0.0`, so the
sampler can only return level 0 - **the fluid flicker goes away.** With it off, it comes back.

So the flicker involves the levels above the base one, and there are exactly two things such a level can
contribute. They have to be separated, because only one of them is fixable at the sampler:

- **the level's own content is current.** `TextureAtlas#uploadAnimationFrames` walks every level and draws
  the due frame into each through that level's own view **and its own UBO** -
  `animationState.getDrawUbo(level)` is `spriteUbosByMip[level]`, one per entry of `byMipLevel` - so no
  level is staler than any other. This was checked before anything changed, which is why "the higher
  levels hold no live frame" is not the answer;
- **the blend between two of them is not a blend of two resolutions of one image.** An animated sprite is
  a *scrolling* pattern: `water_flow` and `lava_flow` move their sample point within the frame, so levels
  *n* and *n+1* hold the same frame at the same instant **sampled at two different rates**, and
  `MipmapFilterMode::Linear` mixes two phases of a moving pattern. A static sprite's levels are a
  consistent pyramid and blend cleanly; a moving one's do not.

So the blend is what goes, not the chain - `game_atlas_sampler()` is `block_atlas_sampler()` with
`mipmap_filter: Nearest`, and nothing else differs. The chosen level is still computed from the
screen-space derivative exactly as before, so the chain keeps doing its job at distance; what is gone is
mixing two levels together.

**This is a hypothesis with a measurement behind it rather than a proof.** The measurement says "level 0
is stable, something above it is not", and this removes the one mechanism that *mixes* levels without
discarding them. If the flicker survives it, the remaining reading is that a single coarse level of a
scrolling sprite is itself unstable at this scale, and the answer would be a per-sprite level clamp
instead of a filter.

**What it costs**, stated because it is a real regression for everything else: a static sprite crossing a
level boundary now steps rather than fades. Vanilla asks for `GL_LINEAR_MIPMAP_LINEAR` (`GlSampler` maps
`FilterMode.LINEAR` as the min filter to `9787`, `GL_LINEAR_MIPMAP_LINEAR`), so **this is a deliberate
deviation from the game** and the only one in either sampler.

**The chain is not clamped, and that is not an oversight.** "Sample the level the game is animating" is
the obvious thing to try and the game does not animate one level: `uploadAnimationFrames` renders into
every level in the same call, so a clamp would clamp to *every* level - which is no clamp at all. What it
really does is throw the chain away, and a distant lump of lava would sample one texel of a 16x16 sprite,
which is the aliasing the chain exists to prevent. It remains reachable as `atlas_base_mip_only`, honoured
for both atlases, because the case for it is a picture rather than an argument.

The device diagnostic line that used to claim the sampler's `anisotropy_clamp` was "in effect" now says
what is true: magnification is `Nearest`, wgpu refuses any value above 1 unless all three filters are
linear, so the feature being available changes nothing here.

### Water follows the biome now, and the level-of-detail bias became observable

Two changes that are the same lesson from opposite ends: **a value that cannot be observed cannot be
verified**, and both of these were values that could not be.

**Water was one constant.** The fluid mesher coloured every water face with `WATER_TINT`, a single
`BiomeColors.getAverageWaterColor` value for the default biome, so an ocean, a swamp and a cold river were
one colour where the game draws three. The tint was reachable all along, but not through the path that
already existed: a fluid is coloured by its **fluid** model, not its block model -

```java
// FluidRenderer#tesselate
FluidModel model = this.fluidModels.get(fluidState);
int tintColor = model.fluidTintSource() != null
    ? model.fluidTintSource().colorInWorld(fluidState, blockState, level, pos)
    : -1;
```

- so `helperGetBlockColor` could not answer it even in principle. `helperGetFluidColor` asks the model set
  (`ModelManager#getFluidStateModelSet().get(fluidState).fluidTintSource()`), which for water is NeoForge's
  `FluidTintSources.water()` and answers with the biome.

It is asked **per fluid block**, not per section, because the biome is a per-column property and a section
on a boundary holds two colours - which is exactly what the constant could not do. Lava is not asked at
all: `FluidStateModelSet` builds the lava model with a null tint source, so white is its answer and not a
placeholder. The trait's default is still `WATER_TINT`, so every test and every provider that knows
nothing about biomes draws what the renderer drew before.

**The bias was a `const`, and a constant is not observable.** `ATLAS_LOD_BIAS` was a shader constant for a
round, and with it "the bias works" and "the bias never reached the GPU" are the same picture - because
`textureSampleBias(.., 0.0)` *is* a plain `textureSample`. Worse, moving it needed the shader copied into
the build directory **and** a restart, since nothing watches the shader files (`mark_pipelines_stale` is
called on atlas and lightmap handover only). Two rounds of "I moved it and nothing happened" could not
distinguish those cases, and that is a fault in how it was built rather than in the reading.

It is now a `FloatSetting` (`-4.0 .. 4.0`, step `0.5`, default `0.0`) written into the per-draw immediate
block - `SectionPosition` grew a fifth `f32` member, so `@pc_section_position` is **20 bytes**, and the
shaders read `section_pos.lod_bias` where they read the constant. **Read per draw**, so applying it takes
effect on the next frame: nothing is baked and no pipeline is rebuilt.

Both are counted, because in both cases the failure that matters looks like success:

```
faces baked since the last report: N game-atlas, 0 own-atlas, 0 leaf face(s) forced opaque,
                                  2137411 fluid tint(s) read from the game
```

**Zero fluid tints** would mean the game answered "no tint" for every fluid - a different bug from the one
this closed, and invisible without a count. The reading for the bias is the one the setting exists to
produce: move it and watch the picture.

### The game's leaves switch now reaches the baker

**`Fast`/`Fancy` leaves did nothing here at all: both settings drew identical leaves.** That is the bug,
and the cause is that the two sides decide a face's layer from different things.

The game's answer is one line:

```java
// ModelBlockRenderer
public static boolean forceOpaque(boolean cutoutLeaves, BlockState blockState) {
    return !cutoutLeaves && blockState.getBlock() instanceof LeavesBlock;
}
```

With it true, a leaf face goes to the **solid** layer whatever its sprite says - and the solid layer has
no alpha test, so the transparent gaps in a leaf texture are filled by the texture's own colour and the
block reads as a solid mass. `Options#cutoutLeaves` is the option, `GraphicsPreset` moves it (`Fast`
false, both `Fancy` presets true), and the video settings screen exposes it directly.

This side decides a face's layer from its **sprite** (`Atlas::sprite_layer`, where leaves are cut out
because their texture has holes), and the option was never read at all - so there was nothing that could
have moved a leaf. Two things were needed:

- **`FaceFlags::leaves`**, the `instanceof LeavesBlock` half, read on the JVM where the `BlockState` is -
  the native side holds block *names*, and a name comparison would miss every mod's leaves. It rides in
  the per-state table `BlockFaceFlags.describe` already fills, which is one more argument on a call that
  was already happening once per state.
- **`CUTOUT_LEAVES`**, pushed by `WgpuNative.setCutoutLeaves` before each bake, because the answer is
  written into the geometry and not read as the frame draws.

`force_opaque(cutout_leaves, is_leaves)` is the rule, split out so its truth table can be tested without
moving either global - and the test asserts the thing an `&&` written the wrong way round would break:
**the switch moves leaves and nothing else.** With the operands swapped, `Fast` would force every
non-leaf block opaque and leave the leaves alone, which is a world where the trees are fine and the
glass, plants, ice and water are all wrong - a bug that would be blamed on the block models.

`BlockCache` also watches the option per tick now, because it is a vanilla option and nothing in this
mod's own settings path was ever going to notice it moving; a change takes the same route as the
animated-texture switch (a note for the next tick, then `Bake.Setting`).

**Measured, with `cutoutLeaves:false` in the run's own `options.txt`:**

```
faces baked since the last report: N game-atlas, 0 own-atlas, 191641 leaf face(s) forced opaque
```

The count is the reading that matters: **zero with the option off would mean the switch is still not
reaching the baker**, which is exactly the state it was in before this. The layer report alone could not
show it, because leaves are a small part of a section and the solid layer is already the largest of the
three by a wide margin.

### The translucent layer's quads are sorted, which is the one thing that cannot be deferred

Blending is order-dependent and there is no depth test to hide it: two panes of glass or two surfaces of
water inside one section blend in **the order they are drawn**, and this side drew them in bake order. The
only sorting it had was between sections (`sort_for_drawing`, back to front), which is the game's own
granularity for *its* translucent mesh but says nothing about the quads inside one.

This is `SectionCompiler#compile`'s `mesh.sortQuads(builders.buffer(layer), vertexSorting)`, and the rule
is the game's:

- the point sorted by is the quad's **centroid**, and the game's centroid is the midpoint of its **first
  and third vertices** (`MeshData#unpackQuadCentroids`: `x0` is the first vertex and `x1` the one two
  strides later, then averaged);
- the order is **descending** by squared distance - `VertexSorting#byDistance` sorts with
  `Floats.compare(keys[o2], keys[o1])`, putting the largest first, so the farthest quad is drawn first;
- the coordinates are **section-relative**, because that is the space a section's vertices are baked in.

**Only the translucent layer.** The solid and cutout layers are opaque, so the depth test decides their
order and sorting them would be work with no effect.

**The index buffer is what gets sorted, not the vertices** - and that is the half worth copying. A quad's
four vertices are the same four whichever order they are visited in, and its six indices are already
relative within it (see the `INDICES` constant), so permuting the six-index groups reorders the quads and
nothing else has to move. Our indices are relative where the game's are absolute, so the permutation is
the only part that transfers.

Three things are pinned by tests, because a wrong order here is **a wrong blend and nothing else** - all
the geometry present, the depth test passing, and a player describing "the water looks off":

| test | what it stops |
| --- | --- |
| `the_farthest_quad_is_drawn_first` | a sort that does nothing, or one that is ascending; the keys are read back out of the buffer it produced rather than from the order it was expected to produce |
| `a_quad_keeps_its_own_indices` | a permutation that reorders within a group, which would turn every translucent face inside out and look like a lighting bug |
| `a_layer_that_is_not_whole_quads_is_left_alone` | reordering against a vertex buffer whose quads are not where the sort assumes - which attaches each quad's indices to another quad's vertices |

**Not done: the incremental re-sort.** The game re-sorts a section when the camera crosses a block
boundary if the section's octant changed (`LevelRenderer#scheduleTranslucentSectionResort`, using
`TranslucencyPointOfView` - the camera's section relative to the section's, clamped to `-1..1` - and
spreading the work over frames). This side sorts once, at bake, from the section's origin, and a section
baked with the camera in one octant keeps that order as the camera moves. For a section-sized mesh that is
the same approximation the game itself falls back to for a camera that has not crossed a block, and it is
the state this change leaves; the re-sort is a separate piece of work.

### Every sprite is drawn from the game's atlas now, because the mip chain is the game's

**The mip chain this side built was wrong in a way no filter could fix, and that is what the two-filter
question was really about.**

Vanilla builds a sprite's mip chain **before the atlas is packed**:

```java
// SpriteContents#increaseMipLevel, reached from the stitcher
this.byMipLevel = MipmapGenerator.generateMipLevels(
    this.name, this.byMipLevel, mipLevel, this.mipmapStrategy, this.alphaCutoffBias, this.transparency);
```

with `AUTO -> transparency.hasTransparent() ? CUTOUT : MEAN` (`MipmapGenerator:97-98`), and `CUTOUT`
meaning the alpha **coverage** of each level is rescaled back to the base level's (`scaleAlphaToCoverage`,
`:149-150`). The stitcher then pads each sprite by `1 << mipLevel`
(`Stitcher:33: this.padding = 1 << mipLevel << Mth.clamp(anisotropyBit - 1, 0, 4)`), so **no sprite's
mips can reach its neighbours'**.

This side packed its own atlas first and mipped **the whole packed image** afterwards, which is wrong
twice:

| | vanilla | here |
| --- | --- | --- |
| when mips are built | per sprite, before packing | on the packed sheet, after |
| what a sprite's edge averages with | its own padding | **whatever sprite is next to it in the sheet** |
| alpha above level 2 | coverage held at the base level's | **arithmetic mean - a 16x16 cutout sprite has no opaque texel left** |

The first bleeds a neighbour's colour into a face's edge; the second makes a face half-transparent at
distance. **Both are invisible under `Nearest`, because `Nearest` never interpolates and so never reads
the damaged texels** - which is exactly why `Nearest` was holding the picture together, and why turning
bilinear on shipped a blurred, washed-out world.

So `decide_game_atlas` no longer requires the game to animate the sprite. Every sprite with a rectangle
from `registerSprite` is drawn from the game's atlas, whose chain is built the way above. The
`animated_textures` setting still gates it, and `game_atlas_bound` is still a hard requirement - a
rectangle from one stitch read against another atlas is a face with the wrong texture on it. The
parameter is kept and ignored so the truth table still documents the case it was written for.

**This side's own atlas is kept as a fallback**, not deleted: a sprite the game never told us about - a
resource reload in flight, a sprite registered late - still has to be drawn from somewhere, and the
frozen copy is the failure this renderer has always had. So the revert is one condition.

And the sampler follows the chain rather than compensating for it:

```rust
mag_filter: wgpu::FilterMode::Linear,
min_filter: wgpu::FilterMode::Linear,
mipmap_filter: wgpu::MipmapFilterMode::Linear,
anisotropy_clamp: 16,
```

`LevelRenderer:678-681` builds `CLAMP_TO_EDGE, FilterMode.LINEAR, FilterMode.LINEAR, maxAnisotropy`, so
this is the game's own answer, and it is only *correct* against the game's chain.

**16 is the honest maximum for `anisotropy_clamp`.** `wgpu-hal`'s `MAX_ANISOTROPY` is 16 and wgpu-core
clamps to `[1, 16]` before the backend sees the value, so a larger number would be silently reduced - and
there is no query for the driver's real limit, because `Limits` has no anisotropy field at all. It is
only legal because all three filters are linear: wgpu rejects any `anisotropy_clamp` above 1 unless the
min, mag **and** mipmap filters are linear, which is why `Nearest` had to stay and `anisotropy_clamp`
had to be 1 for as long as it did.

### Reverted: the non-indexed terrain draw path

**This was rolled back, and the note is here because the idea is still right and the trap is worth not
walking into twice.**

The change made the terrain draws non-indexed - one `draw` per layer instead of `draw_indexed` - so that
the six vertices of a quad are generated in the shader from the quad's number rather than fetched from an
index buffer. The reasoning stands: an indirect *indexed* draw gives the shader a `vertex index` and the
hardware an index buffer, and the terrain's vertices are in a storage buffer, so the shader cannot turn
that `vertex index` into an address without reading the index value back out - which WGSL cannot do.

It came out with wrong perspective, water missing in stripes, and what looked like holes. The corruption
was reported, the addressing was re-derived from `allocate` (where `vertex_range` is provably counted in
dwords, because `allocate_range` is handed a byte length divided by four), corrected, and **still wrong**.
So the diagnosis was incomplete, and the change was reverted rather than iterated on blind: the draw path
is back to `draw_indexed` with the index buffer bound, and both shaders are byte-identical to the commit
before it.

Two things learned that are worth keeping, both now recorded in the shader comments they belong to:

- **`@builtin(first_instance)` does not exist in WGSL.** Naga rejects it outright. It does not need to
  exist: an indirect draw's `first instance` is what `@builtin(instance_index)` reports for the first
  instance, so a draw with an `instance_count` of one reads it there.
- **The lightmap UV table is indexed by vertex, not by corner.** The six indices of a quad name vertices
  `0, 3, 1, 1, 3, 2`, so `uv[vi & 3]` is the vertex and is correct for an indexed draw; a non-indexed draw
  has `vi` running over six corners and needs a different table.

What was **not** reverted is the sampler change below, which is a separate defect found on the way.

### The two atlas samplers disagreed, and the switch that compares them only reached one

`graph.rs` built the game's atlas sampler by hand on top of `render::atlas::block_atlas_sampler`, and
applied `atlas_base_mip_only` itself:

```rust
if ATLAS_BASE_MIP_ONLY.load(Ordering::Relaxed) {
    descriptor.lod_min_clamp = 0.0;
    descriptor.lod_max_clamp = 0.0;
}
```

That is the **only** place the switch was read. This renderer's own atlas - `@sampler`, built in
`atlas.rs` - kept the default `lod_max_clamp` of 32, so turning the switch on clamped the game's atlas to
its base level and left this side's at every level. Not the comparison its name describes, and not the
comparison its own comment describes either.

The switch now lives in `render::atlas::block_atlas_sampler`, which both samplers are built from, with
`address_mode` the only difference between them (`Repeat` for this side's atlas, whose coordinates come
from its own packing, and `ClampToEdge` for the game's). **A sampler constructed twice can disagree; one
constructed once cannot** - and this is the second time these two have drifted, the first being the
revision where only the game's sampler was bilinear and the fire and lava were blurry while the blocks
around them were clean.

The comment on why there is no clamp *by default* is still right and was kept: `TextureAtlas#uploadAnimationFrames`
walks the whole mip chain and renders the current frame into every level through that level's own view and
UBO, so no level is staler than any other and clamping to the level that moves clamps to every level.

### The terrain draws are not indexed any more, which is what an indirect draw needs

**This is groundwork, not the indirect buffer.** `Scene::indirect_buffer` is still created and still
unused; what changed is the thing that was blocking it, and the reason is worth writing down before the
next attempt.

An indirect *indexed* draw hands the shader a `vertex index` and the hardware an index buffer. The
terrain's vertices do not live in a vertex buffer - they are a storage buffer the shader addresses
itself - so the shader cannot turn that `vertex index` into an address: it would have to **read the index
value back out of a buffer**, and there is no way to do that in WGSL. `@builtin(vertex_index)` counts
indices, and the index is what it needs.

Unless there are no indices. Every quad the baker emits is the same six values over its own four
vertices, in a winding that is fixed and already documented (`INDICES` in `chunk.rs`), so the six can be
**generated** from the quad's number and the vertex's position within the six:

```wgsl
const QUAD_INDICES = array<u32, 6>(0u, 3u, 1u, 1u, 3u, 2u);

let quad = vi / 6u;
let corner = vi % 6u;
let vertex_dword = base_vertex + 4u * (quad * 4u + QUAD_INDICES[corner]);
```

So the draws are now `draw(0..quads * 6, first_vertex..first_vertex + 1)` - **not indexed**, one instance
each - and the index buffer is gone from the draw path. Nothing was given up for it: the four vertices of
a quad are shared between its two triangles and by nothing else, so the vertex cache was not doing
anything here.

**Two things this turned up.**

`@builtin(first_instance)` **does not exist in WGSL** - Naga rejects the shader, which the shader tests
caught immediately. It does not need to exist: the value of an indirect draw's `first instance` is what
`@builtin(instance_index)` reports for the first instance of that draw, so a draw with an `instance_count`
of one reads the layer's vertex base there. That is the field that makes batching possible at all, because
a push-constant block covers a whole `multi_draw_*` call.

And the lightmap UV table had to stop being indexed by the vertex: the six corners name vertices
`0, 3, 1, 1, 3, 2`, so a table indexed by corner is not a table indexed by vertex - corner 1 is vertex 3.
`uv[vi & 3]` was the vertex when `vertex_index` ran over four vertices; it runs over six corners now.
Getting that wrong is a lightmap that is subtly rotated per corner, which is exactly the kind of thing
that looks like a texture bug.

**What is still missing, precisely.** The indirect buffer needs `section_pos` to stop being a push
constant, because one immediate block covers the whole batch and every section has a different position -
so the section's **absolute** position has to live in the arena, written when it is baked (absolute
because a camera-relative one would change as the camera moves, and that would mean rewriting the arena
every frame). That is a 16-byte header per section: `SectionRanges` gains a range, and the allocator, the
free paths, the refused/trimmed paths and the drain all learn about it.

### Multi-draw indirect prerequisites, and a flag this renderer was clearing by accident

`InstanceFlags::VALIDATION_INDIRECT_CALL` is not optional for an indirect-drawing renderer, and this one
was not setting it. `InstanceFlags::debugging()` is `DEBUG | VALIDATION | VALIDATION_INDIRECT_CALL`, and
the switch that replaced it chose `DEBUG` (plus `VALIDATION` when asked) - so the flag was dropped.

It is not only validation. wgpu's own note: with it off, *"the value of `@builtin(instance_index)` will
not take into account the value of the `first_instance` argument present in the indirect buffer"* - and
`first_instance` is where a batched terrain draw would carry the vertex base the vertex stage addresses
`chunk_data` with. On D3D12 that is a wrong picture rather than an error. It is set unconditionally now:
what it costs is a bounds check on a handful of integers per *indirect call*, not per section, so it is
not what the `host validation` switch trades against, and what it protects is the meaning of the
arguments rather than whether they are legal.

Measured on this machine, through the same reporting path as the validation layers:

```text
wgpu-mc: indirect draws: execution yes, real batched multi-draw yes.
         Without the count feature wgpu emulates multi_draw_* as one draw per entry, which is a loop
         by another name.
```

`MULTI_DRAW_INDIRECT_COUNT` is the feature that matters - **`wgpu` has no `Features::MULTI_DRAW_INDIRECT`
at all**, the non-count calls are gated only on `DownlevelFlags::INDIRECT_EXECUTION`, and without the
count feature they are emulated as one draw per entry. `INDIRECT_FIRST_INSTANCE` is the other
prerequisite, because `first_instance` must be 0 without it and that is the field a shader's vertex base
would ride in.

### The section compile was skipped after the work, not before it

`SectionCompilerMixin` used to redirect `new CompiledSectionMesh(...)` inside `RebuildTask.doTask`, which
is **after** `SectionCompiler.compile` has built every vertex. So the only thing it saved was the upload
and the derived data: the per-block model lookup, the part collection, the lighting, the vertex building,
the packing into the scratch buffers and the sort-key pass were all still paid in full and then thrown
away.

It redirects the two calls that do the work instead, inside `compile`:

| at | what is skipped |
| --- | --- |
| `ModelBlockRenderer.tesselateBlock` (`SectionCompiler.java:107`) | block geometry |
| `FluidRenderer.tesselate` (`:103`) | fluid geometry |

What is left per block is an `isAir` test, an `isSolidRender` test, an `hasBlockEntity` test and two
virtual calls that return immediately. **Measured**: 1,109,503 block geometry calls skipped in one session.

**The two things that must not be skipped are not.** `visGraph.setOpaque` feeds `results.visibilitySet`,
which `SectionOcclusionGraph` walks to decide which sections are worth visiting at all - skipping it would
make every section look transparent and the occlusion graph meaningless. `handleBlockEntity` fills
`results.renderableBlockEntities`, which is how `LevelRenderer.extractVisibleBlockEntities` finds a chest
or a sign. Both sit outside the two redirected calls and both still run.

With no geometry produced, `startedLayers` stays empty, so `renderedLayers` is empty and
`transparencyState` is never set - exactly the state an all-air section compiles to, which is the one state
in this dispatcher known to be safe. Nothing has to be closed, because nothing was built, and that is the
other half of the saving: `MeshData.close` is what returns the scratch buffers to a fixed pool.

NeoForge's `ClientHooks.addAdditionalGeometry` is deliberately left alone. It is a different mechanism - a
mod's renderer is handed the `ModelBlockRenderer` and builds its own geometry - and that geometry is not
something this renderer can build in Rust, so it stays Minecraft's.

**Two things about the injection that cost a round each, recorded because both are traps:**

1. **A full descriptor that is right can still match nothing.** The namespace moves between versions -
   `BlockAndTintGetter` is in `client.renderer.block` now and not `world.level`, `BlockStateModel` is in
   `client.renderer.block.dispatch` - and Mixin reported `No refMap loaded` and `Scanned 0 target(s)` with
   the descriptor taken from the decompiled source. It was verified against the bytecode instead
   (`javap -c`), and then it applied. When an injection fails, read the descriptor out of the class file
   rather than out of the source.
2. **`method = "compile"` is not specific enough here.** `SectionCompiler` has two `compile` overloads -
   a four-argument one that delegates to the five-argument one - and the injection only applied once the
   target named the five-argument descriptor explicitly.

### Host-side validation is a switch now, and it does not do what its name suggests

`InstanceFlags::VALIDATION` was unconditional, so every launch loaded a vendor debug layer. It is the
`host validation` debug setting now, off by default, and `gpu based validation` needs it - resolved as a
pair in `debug::validation_flags` rather than left to the backend, because `wgpu-types` documents the
implication and one place resolving it wrongly is a layer that silently does nothing.

**What the flag actually does is not what this file used to say.** It does not turn wgpu's own checking on
or off - wgpu-core validates every call unconditionally, which is why `wgpu_core::validation` warnings
appear in the log with the setting off. What it asks for is the *backend's* validation:

| | |
| --- | --- |
| D3D12 | `ID3D12Debug::EnableDebugLayer` |
| Vulkan | the Vulkan validation layer |
| GLES | `glEnable(GL_DEBUG_OUTPUT)` |

A layer written by the graphics vendor, reporting through the driver's debug output, catching what wgpu
cannot see - a barrier in the wrong place, a resource used before its GPU work finished. `GPU_BASED_VALIDATION`
is that same layer running its checks on the GPU instead of on the recorded commands, which is why it needs
`VALIDATION` and why it is the slow one.

So turning `host validation` off does not make an invalid call silent. It removes the vendor's second
opinion and its debug output, and it keeps wgpu's error that names the call that caused it.

**The line about it could not be written where the flags are decided.** The instance is built in
`create_renderer`, which runs a phase before `setPanicHook` installs `env_logger` - measured, not assumed:
a `println!` beside `Instance::new` lands in the console about eleven seconds before the first line this
side reaches the log. The pre-existing `renderer created through ..` line sits one statement after the
instance is built and **has never appeared in a log either**, which is what made it obvious rather than
mysterious. `instance_flags` records the answer and the line is written where the arena is sized, which is
a line that does reach the log:

```text
wgpu-mc: the instance was built with the backend validation layer off and GPU-based validation off
         (the `host validation` and `gpu based validation` debug settings). wgpu's own validation is
         not a setting and never runs with those off - it is what names the call an error came from.
```

### Every entity model failed to parse, and the warning was the only sign

```text
wgpu-mc: 416 entity model layer(s) could not be read: minecraft:sheep#wool_undercoat (missing field
`data`), minecraft:hanging_sign/crimson/wall#main (missing field `data`), minecraft:villager_no_hat#main
(missing field `data`), ...
```

`LayerDefinitions.createRoots()` returns `Map<ModelLayerLocation, LayerDefinition>`, and `LayerDefinition`
has two fields: `mesh` and `material`. The Rust side was reading this:

```rust
pub struct Wrapper2 { data: ModelPartData }
pub struct Wrapper1 { data: Wrapper2 }
```

- the shape of a *baked* `ModelPart`, which no `LayerDefinition` has ever serialised as. So **all 416
layers failed** with `missing field 'data'`, and the parse being per layer is the only reason it was a
warning rather than a crash: the failures were skipped, the registry kept whatever it had, and entity
rendering carried on with a stale or empty set.

**What the game actually sends**, and what the code now reads:

```text
LayerDefinition  -> { mesh, material }
MeshDefinition   -> { root }
PartDefinition   -> { cubes, partPose, children }
PartPose         -> a *record*: { x, y, z, xRot, yRot, zRot, xScale, yScale, zScale }
CubeDefinition   -> { origin, dimensions, grow, mirror, texCoord, texScale, visibleFaces }
```

`ModelCuboidData` had `offset`, `textureUV` and `textureScale` - names no version of the game sends - and
now has `origin`, `texCoord` and `texScale`. `material`, `comment`, `texScale` and `visibleFaces` are
deliberately not named: serde ignores what it is not asked for, which is the right property for a shape
this side is a guest in. Every field is `#[serde(default)]` for the same reason - a missing one degrades a
model instead of discarding it.

**Three things fell out of fixing the shape**, each of which had been broken as long as the parse failed:

| | |
| --- | --- |
| `PartPose` scales were read as nothing | `scale_x/y/z` were hardcoded to `1.0`. The game expresses a baby variant as `PartPose.scaled(0.5)` **on the pose and nothing else**, so every baby model would have rendered at adult proportions. |
| the pose was read as the translation | The game's `PartDefinition#bake` translates each cube by its **own** `origin` and rotates the part about the pose's pivot. The pose's `x`/`y`/`z` are almost always zero and the pivot almost never is, so this was backwards in both halves. |
| `grow` was ignored | `CubeDefinition#bake` adds the deformation to each dimension, which is how a hat or an armour layer is "the head plus a quarter". |

The last one is worth its own note, because it is the same trap one level down: `CubeDeformation`'s fields
are `growX`/`growY`/`growZ`, where `origin` is a `Vector3f` whose fields really are `x`/`y`/`z`. A lookup
with the wrong name does not fail - it misses, reads as zero, and every armoured model comes out a little
too small. `a_grown_cube_is_its_dimensions_plus_the_growth` caught exactly that, in this side's own code,
on its first run.

**The guard is a fixture, not a comment.** `model_shape_tests` parses a literal transcription of what Gson
emits - including `material`, `comment`, `texScale` and `visibleFaces`, which are ignored - and pins the
part tree, the scale, the pivot-versus-origin split and the growth. A shape this side does not own has to
be pinned by a sample of it, or the next rename is silent.

### The shape behind all of these: a section stops being drawn and nobody says so

Five routes, one shape, and it took five rounds because each was found on its own:

| route | how a section stops being drawn | what reported it |
| --- | --- | --- |
| an allocation was refused | it keeps the geometry it had, or has none | `refused_pending` -> `forgetRefused` (was capped at 4096, then gated on `canGrow`) |
| the bake queue was full | the task is dropped and the bake never runs | **nothing** - fixed here |
| the payload was rejected | `send` returned having taken nothing | `REJECTED_MASK`, but the mesh decision ignored it |
| the trim dropped it | it is beyond `width + 2` chunks | **nothing** - fixed here |
| a level change | the whole arena is dropped | `forgetAll` clears both sides together, which is why this one was never a hole |

**The invariant is one line:** a section this side stops drawing has to be reported to the JVM, because the
JVM is suppressing Minecraft's mesh for it on the strength of `rustHas` - and `rustHas` only means "this
side was told about it". Every route above is a way for that claim to stop being true, and the answer is
the same each time: hand the position over so `sent` and `rustHas` drop it, and ask the game for a rebuild.

The queue-full route is worth reading in the source, because its comment asserted the opposite:

```rust
// The queue is full, so this bake is dropped on the floor ... this is the bit that makes the JVM
// offer it again.
None => return (rejected | (1 << CENTER)) as jint,
```

The bit makes the JVM forget the section was sent, so the *next* rebuild carries its blocks - and the next
rebuild is the thing that does not happen, because a rebuild happens when the game decides a section is out
of date and this section was just brought up to date. The offer was not "not lost work"; it was lost, and
the mesh for it was already gone. Both drop paths now queue the position for a rebuild, which is the only
mechanism that actively asks the game for one.

### The trim dropped sections without telling anyone, and the buffer list had no memory bound

Two problems with one cause, found by flying forward.

**The trim was silent, which is a third route to the same hole.** `SectionStorage::trim` removed the
sections beyond `width + 2` chunks and gave their ranges back, and said nothing to the JVM. The JVM
suppresses Minecraft's mesh for every section it believes Rust holds - `rustHas` - so a trimmed section was
drawn by neither renderer, and it did not come back either: the JVM counts it as sent, so the next rebuild
carries nothing for it. Exactly the refusal hole and the dropped-bake hole, arriving by the trim.

`trim` now returns the positions it dropped and they go down the same channel a refusal does
(`refused_pending`), because to the other side the two mean the same thing: a section this renderer is not
drawing, which Minecraft's mesh has to come back for. `forget_trimmed` is the name for it on the storage
side.

**And the memory bound was "four buffers", which is not a bound.** `ARENA_BUFFERS` was set to the number a
32-chunk view needs, and each buffer is created at the device's own `max_buffer_size` - so the arena could
reach four times 1.31 GB, and with the arena *appending* rather than refusing there was nothing to stop it.
A player flying forward grew it without limit: a bug about holes became a bug about memory.

`ARENA_MEMORY_BUDGET` is 3.5 GB and it is checked where growth happens, before an arena is added. Past it
the arena stops growing and `at_capacity` does what it does at the device's limit: the sections that do not
fit are left to Minecraft, which is the right answer for a view the arena cannot hold. It is bytes rather
than a buffer count because the buffers are not the same size - the first is whatever the world reported,
the rest are created at the ceiling.

**What this says about the earlier rounds.** The trim being silent is the *same* mistake as the refusal
path, the dropped queue task and the mesh-suppression decision: a section stops being drawn by this side and
nothing tells the side that is suppressing its fallback. Four routes, one shape. The memory bound is the
other half of the same change - making the arena bigger removed the limit that had been standing in for
correctness.

### The arena was sized for the world entry, not for the view

A run at 48 chunks, measured:

```text
the section arena holds 50,460,000 slot(s), 192 MB, for 12 chunk(s) of view     <- at world entry
the section arena was full, so it grew to 2 arena(s) of 536870911 slot(s), 2240 MB total
```

**The arena is sized to whatever render distance the world was entered at** - 12 chunks here, 192 MB -
and the view then becomes whatever the player set. `set_arena_slots` cannot resize an arena that holds
anything (`set_pool` refuses when the storage is non-empty, for the reason it gives: a range allocator
cannot be resized under live allocations), so the only way up is appending arenas - and the growth path
added **one per refusal**.

That made the catch-up a race against the burst of bakes a growing view produces, and it is a race the
refusals win: each one is a section whose bake did not fit. Since the mesh for those sections has already
been dropped by the time the refusal is known, the sections in that window are the holes.

**The growth now meets the target in one go.** `pending_arena_growth` is a marker rather than a size - the
refusal that sets it only knows that something did not fit - so the target is read from what the view
actually asks for, `arena_slots(current width)`, and arenas are appended until the pool reaches it or
`ARENA_BUFFERS` runs out. The measured run went from 192 MB to 2.24 GB in one growth instead of ~15.

`at_capacity` is now set from what happened rather than from whichever branch was taken last: true only
when another arena was refused, cleared otherwise. It is what decides whether the JVM keeps Minecraft's
mesh for a section this side cannot take, so it is cleared as deliberately as it is set.

**A note on the measurement that misled me.** The terrain report's arena figure is
`used_slots() of pool_slots()`, and `used_slots` is `pool - free`, where `free` is summed across every
arena's free list. Read beside `LAYER_DRAWN_TOTAL` it looks like it should reconcile with the drawn
count, and it does not - that counter is cumulative while the drawn/empty pair in the same line is
per-report. Two runs an hour apart read "67% used, 3953 solid drawn" and "61% used, 106 solid drawn",
which cannot both be a description of the same quantity. The utilisation number is sound on its own; it is
the company it keeps on that line that is not.

### `send` could return having taken nothing, and the mesh was dropped anyway

`send` reports the other side's answer in one word, and the decision about Minecraft's mesh was reading
only part of it:

```kotlin
val rejected = answer and REJECTED_MASK     // records refused: nothing was taken
val resync = answer and RESYNC != 0
...
return resync                               // <- and nothing else came back
```

`bakeNow` then decided with `noteTookSection(!firstLook[0] && !atCapacity())` - which asks whether this side
has *told* Rust about the section and whether the arena has room, but never whether Rust **accepted** the
payload. So a rebuild whose payload was refused still dropped Minecraft's mesh, while `rustHas` was
correctly left unset for it. The section was then drawn by neither renderer.

The refusal paths are all real and all reachable while entering a world at a large view distance:

- **the bake queue was full** and `BakeTask::new` dropped the task (`rejected | (1 << CENTER)`);
- **the records were refused** because they were written for a world Rust has forgotten
  (`rejected | RESYNC`);
- **the block registry was empty**, which returns `REJECTED_MASK` for all 27.

Only an *arena* refusal reaches the re-offer drain (`refused_pending`), so a dropped bake was not re-offered
either - it depended entirely on a later rebuild happening, which is the assumption this whole file keeps
running into.

`send` now returns `Answer(resync, centreTaken)`:

```kotlin
val centreTaken = !resync && (rejected and (1 shl CENTER)) == 0
```

- the **centre slot** decides this section, because a neighbour's refusal only costs the next rebuild an
  extra payload - and the decision is now `!firstLook[0] && !atCapacity() && accepted`.

This is the third time the same mistake has been found in this file, at three different depths: `bake`
answering "did it not throw", the mixin overwriting the recorded answer, and now `send` reporting only half
of what it received. Each one dropped a mesh for a section Rust had not taken. The report line carries a
counter per outcome now, so which one is wrong is a number rather than a reading:

```
Rebuilds: S meshed by Rust's answer, R refused by it, C left to Minecraft because the arena is full,
          F left to it because Rust was never told
```

**`R` non-zero means Rust is refusing payloads** - the bake queue or a stale world - and is the number to
look at first on a run that still has holes.

### The arena is several buffers now, because one was half of what a large view needs

The measurements that forced it:

| | |
| --- | --- |
| `arena_cap_slots` (device `max_buffer_size / 4`) | 328,560,000 slots = **1.31 GB in one buffer** |
| a section's measured cost | 264,591,404 slots / 13,744 drawn = **19,251 slots** |
| sections a 32-chunk view is drawn from | 65 x 65 x 8 = about **33,800** |
| what that needs | 33,800 x 19,251 = about **2.6 GB** |

**One buffer stops at half of that**, so at 32 chunks the sections past the ceiling were refused, and a
refused section was one neither renderer drew. No amount of care in the refusal path could change the
ratio - which is why every earlier fix here made the holes fewer and never made them go.

**What changed.** The arena is a list of buffers rather than one, and a range now names the buffer it is an
offset into:

- **`SectionRanges` gained `buffer: u32`**, and it travels with every range through every free path -
  deferred, refused, trimmed, replaced - because a range given back to the wrong pool is a range that pool
  hands out while the buffer holding it still does. `ReleasedRange` and `DrawnLayer` name those pairs
  rather than spelling out the tuples, because three positional fields of which two are `Range<u32>` is
  the shape that gets passed in the wrong order.
- **The draw loop rebinds when a section's buffer changes** - both the bind group the vertex stage reads
  `chunk_data` out of and the index buffer `draw_indexed` reads - because binding one and not the other
  draws a section out of another section's geometry. Sections are gathered in storage order and the
  allocator hands out of one arena until it is full, so this is a handful of rebinds per frame rather than
  one per section. A section's layers are all placed in **one** arena for the same reason.
- **Growth appends a buffer instead of growing one.** The old path allocated a bigger buffer and
  `copy_buffer_to_buffer`'d the old contents into it; appending costs one allocation and no copy, because
  every range already handed out still names the buffer it was handed out in. `ARENA_USAGE` therefore
  loses `COPY_SRC`: nothing copies an arena any more.
- **The ceiling is `ARENA_BUFFERS` (4) buffers**, about 5.2 GB - chosen rather than derived, because the
  number it trades against is video memory the driver has to actually hand over and wgpu reports a
  per-buffer limit and no total. It is the first thing to lower if a driver refuses the allocations. At the
  limit, `at_capacity` is set and the JVM keeps Minecraft's mesh for whatever does not fit.
- **The refusal path asks about refusals now, not about the ceiling.** An arena is a pool of *contiguous*
  ranges, so one with twenty per cent free and no room in the size class being asked for refuses exactly as
  a full one does - a run was measured at 80% full with 33 refusals and the guard dormant, because it was
  keyed on the device limit. `terrainArenaAtCapacity` answers "is this side keeping up" -
  `at_capacity || refusals_waiting() > 0` - which is the question the JVM actually needs.

**Verified so far**: 109 + 58 tests, `clippy -D warnings`, and a run at 32 chunks that drew 9,970 sections
with no validation errors and no panics. **That run's first arena fitted, so the second-buffer path has not
executed yet** - the rebinding and the append are covered by compilation and by the single-arena path
still working, and the multi-arena path needs a world that fills 1.31 GB to exercise. That is the thing to
watch on the next run: the growth line names how many arenas there are.

### The arena cannot hold a large view distance, and that is the whole of it

At 32 chunks the holes are not occasional, they are everywhere - and the reason is a number rather than a
bug. From a run at 32 chunks:

```text
264,591,404 of 328,560,000 slot(s) handed out (80%, 1009 MB), the largest section 122,760 slot(s), 33 refused
```

Two things that says, and they point the same way:

- **The pool is the device's buffer limit**: `arena_cap_slots` is `max_buffer_size / 4`, so 328,560,000
  slots is a hard ceiling of **1.31 GB in one buffer**. We already ask for `adapter.limits()`, so this is
  the device's own number, not a request that could be raised.
- **A section costs what it costs**: 264,591,404 slots across 13,744 drawn sections is **19,251 slots
  each**, near the 20,000 the sizing constant assumes. So the pool holds `328,560,000 / 19,251 = ` **about
  17,000 sections**, while a 32-chunk view is drawn from about `65 x 65 x 8 = ` **33,800**.

**The arena is half the size it needs to be at 32 chunks**, and no amount of care in the refusal path
changes that: sections are refused because there is nowhere to put them, and refused sections are sections
this side does not draw. Every fix in this file so far has been about making a refusal *survivable* - and
they are, which is why the holes got fewer - but the count of refusals is set by that ratio, not by the
refusal handling.

What follows from it, and what is left to do:

- **The guard has to hold for the whole run, not after the first refusal.** It now asks the native side
  whether *any* refusal is outstanding (`refusals_waiting`), rather than whether the pool is at the
  device's limit - the earlier version of that question was answered `false` through all 33 of the
  refusals above, because a pool with 20% free and no room in the size class being asked for refuses
  exactly as a full one does.
- **The fix that removes the holes is to stop having one buffer.** Nothing about the arena requires a
  single allocation: draws would need a bind group per buffer and the pool would need to hand out
  `(buffer, range)` rather than `range`. That is a real change and it is the one that raises the ceiling
  from 1.31 GB to `max_buffer_size x how many buffers the device will give` - which is what the sizing
  constant already assumed it had.
- Until then the honest summary is: **at a view distance the arena can hold, there are no holes; past it,
  the sections that do not fit are drawn by Minecraft**, which is what the guard is for.

### The suppression decision now says which of its three answers fired

A hole that survives a fix leaves the question "which input is still wrong", and the suppression decision
has three of them: the section was already Rust's, the arena is at its buffer limit, or Rust was never told
about the section. Counting them per rebuild answers that without another round of reading the code, and
the line rides on the once-a-second report beside the claim:

```
wgpu: this side claims Rust has 1732 section(s); the arena is drawing 512, with 0 awaiting a rebuild
      and 0 bake(s) queued. Rebuilds: 313 meshed by Rust's answer, 0 left to Minecraft because the
      arena is full, 481 left to it because Rust was never told
```

**A whole run at 16 chunks:**

| outcome | count |
| --- | --- |
| meshed by Rust's answer (so the mesh is dropped) | 2,247 |
| left to Minecraft because the arena is full | **0** |
| left to Minecraft because Rust was never told | 5,575 |
| arena refusals | 1 |
| empty bakes | 3 of 7,629 |

Two things that says. The capacity guard added for the `max_buffer_size` case **never fired in this run**,
because this arena never filled - so it is not what made the user's holes get fewer, and whatever did is
still unaccounted for. And the largest bucket by far is "Rust was never told": a rebuild that runs before
the section has been offered to Rust keeps Minecraft's mesh, and it is the *next* rebuild of that section
that hands it over. A section rebuilt only once therefore keeps Minecraft's mesh for the session - the safe
direction, but it is the shape to watch for the holes that remain.

### `bake` answered "did it not throw", and that answer dropped Minecraft's mesh

The hook that starts a bake records whether Rust took the section, and that record is what
`SectionCompilerMixin` reads to decide whether Minecraft's mesh for the same section is geometry nobody
will read. The hook was:

```java
RustChunkBake.noteTookSection(RustChunkBake.bake(this.region));
```

and `bake` ended in:

```kotlin
return try {
    bakeNow(region)
    true                      // <- "it returned normally"
} catch (throwable: Throwable) {
    false
}
```

**Every refusal inside `bakeNow` is a plain return**, so every one of them answered `true`: the bake
queue was full and the task dropped, the payload rejected, the arena out of room. The section was then
recorded as Rust's, `SectionCompilerMixin` dropped Minecraft's mesh for it, and Rust had not taken it -
**drawn by neither side, which is the 16x16x16 hole.** It is the failure the mixin's own doc comment warns
about, produced by the value it was being handed.

Two things were wrong and both are fixed:

- **`bake` reads back the decision** instead of inventing one: `bakeNow` records it where the tables it
  depends on are, and `bake` returns `tookThisSection.get()`.
- **The mixin no longer writes it a second time.** It was calling `noteTookSection(bake(region))`, which
  *overwrote* the finer answer `bakeNow` had already recorded - so the capacity check below was being
  clobbered by the coarser one on every rebuild.

**And the decision now accounts for an arena that cannot grow**: `noteTookSection(!firstLook[0] &&
!atCapacity())`. At the device's `max_buffer_size` a refusal is permanent, and a section this side will
never be able to draw is one the game has to. `atCapacity` is a new native query, set in
`grow_arena_if_asked` at the only place that knows the request was clamped - and it is a fact the JVM
cannot work out from the outside, because the mesh is dropped on a chunk-build thread *before* the bake is
queued, so the refusal arrives after the geometry is already gone.

This is also the answer to "the arena only holds sections Minecraft re-meshed, so freshly explored terrain
has holes": the arena is fed by the rebuild, the rebuild is what suppresses the mesh, and the two were
joined by an answer that was true for refusals - so any section whose bake was refused was suppressed and
never drawn.

### What the hole is *not*, measured

Three fixes went in for this and all three were wrong, so the negative results are worth more than the
patches were. Each was a plausible mechanism rather than a measurement, and each was reported as the
answer.

**Not the occlusion list.** `LevelRenderer.visibleSections` is a snapshot - it is refilled by
`applyFrustum`, which runs only when the camera has turned by more than two degrees or the occlusion graph
reports a change - and the terrain gather treated it as a whitelist. That is a real defect and it was
changed, but turning occlusion culling off does not fill the holes, which settles it.

**Not the refusal list's cap.** The list of refused positions was capped at 4096 and dropped what it could
not hold, which is a section never re-offered and therefore a permanent hole. Also real, also fixed, and
also not this.

**Not the refusal queue's gate.** A refusal whose section the arena could not grow for was dropped rather
than queued, after `rustHas` had already been cleared - the same hole by another route. Real, fixed, and
not this.

**What the measurement says instead**, from a run with the logging switch on:

```text
wgpu: this side claims Rust has 9619 section(s); the arena is drawing 2593, with 0 awaiting a rebuild
      and 0 bake(s) queued
```

**Zero refusals, zero pending rebuilds, zero empty bakes** out of 2,800 bake lines. The arena is not full,
nothing is waiting, and every bake that ran produced geometry. So the hole is not in the refusal path, not
in the arena's capacity, and not in the mesher - and the three fixes above, while each a real bug, were
answers to questions nobody had asked.

The report line is new, and it exists because nothing compared the two halves of the claim before it:
`rustHas` is this side saying "Rust has this section", and the mixin that drops Minecraft's mesh reads that
same claim. A section that was told and never published is drawn by neither renderer, and the only way to
see it is to put the claim and the fact on one line. `arenaSections` is the native side of it.

**Where the evidence points now, and why it is not yet a fix.** The one reproduction detail that
discriminates: a hole that never fills while the player stands still, and fills the moment anything dirties
it or a neighbour. That is a section which is *in* the arena and *not drawn* - the gather skips a section
whose layers are all empty (`if !any { continue }`), and Rust's own mesh is suppressed for it, so the two
conditions together are a hole with nothing in the log to mark it. What is not known is which of the two
clauses is true for the sections in question: whether the arena holds no layer, or the gather is not
reaching it. Answering that needs the position of a hole, which is what the next diagnostic has to carry.

### The fix for the hole had the hole in it, behind a different gate

The refusal queue was added because a refusal dropped past the per-tick budget was a 16x16x16 hole
nothing would fill. It worked, and the holes stayed - and the reason is a single `&&` in the drain that
feeds it:

```kotlin
if (canGrow && pendingRedirty.size < PENDING_REDIRTY_LIMIT) {
    pendingRedirty.add(key)
```

`canGrow` is `terrainArenaCanGrow`: false once the arena is at the device's own buffer limit. The
argument for it was that a rebuild for a section the arena cannot hold is a spin - the arena doubles on a
refusal and stops, so past that point a rebuild per refusal is work that cannot converge.

**That argument is right about the spin and wrong about the alternative**, and the two lines above the
`if` are what make it wrong:

```kotlin
sent.remove(key)
rustHas.remove(key)
```

The key has already been taken out of `rustHas`, so Minecraft's mesh for that section stops being
suppressed - but nothing asks for it to be *rebuilt* either. A mesh that is neither drawn by Rust nor
rebuilt by the game is the same hole by another route, and it is permanent, because the only thing that
would have re-offered the section was the rebuild that was never requested. It is the exact bug the queue
was written to fix, reintroduced by the gate that was meant to keep the queue cheap.

The rebuild is asked for now, whatever the arena's state - and it **converges with a full arena**, because
of what the rebuild does. It bakes, the bake fails to allocate, the refusal clears `rustHas` again, and the
bake answers *not taken*: `noteTookSection(false)` leaves Minecraft's own mesh in place, so the section
ends up drawn by the game, which is the correct answer for a section this side has no room for. The cost
is one rebuild per refused section, and the rate is the budget's in `redirtyDue`, which is where a rate
belongs.

`canGrow` still gets read, for the log line rather than for a decision:

```
the section arena refused N section(s) (it can/cannot grow); forgetting them, ... queueing M of those
```

**What this cost to find is worth recording.** Two earlier attempts fixed real bugs that were not this
one - a capped refusal list, and a backoff that could livelock - on the strength of a plausible mechanism
rather than a measurement, and each was reported as the answer. The measurement that settled it came from
two questions instead: whether the same holes appear in vanilla (`no - turning the terrain takeover off
fills them in`), and whether the game's own mesh covers them when the takeover is off (`yes`). That pair
says the section is in neither renderer, which points at the publish path and not at culling - and the
occlusion-culling change, which looked like the obvious suspect, was innocent.

### The occlusion list is a snapshot, and the terrain pass treated it as a fact

`LevelRenderer` fills `visibleSections` in exactly one place:

```java
private void applyFrustum(Frustum frustum) {
    this.clearVisibleSections();
    this.sectionOcclusionGraph.addSectionsInFrustum(frustum, this.visibleSections, this.nearbyVisibleSections);
}
```

and `applyFrustum` is called from one place, behind a condition:

```java
if (this.sectionOcclusionGraph.consumeFrustumUpdate() || camRotX != this.prevCamRotX || camRotY != this.prevCamRotY) {
    this.applyFrustum(offsetFrustum(frustum));
}
```

`camRotX` and `camRotY` are `floor(xRot / 2)` - **two degrees**. So the list is refilled when the camera
has turned by more than two degrees, or when the occlusion graph says something changed, and **between
those moments it is a snapshot of whenever the last refill happened**. That is fine for the game, which
uses it to decide what to draw *this* frame from meshes that already exist. It is not fine as a whitelist
for a renderer that owns sections the list has not caught up with.

The terrain pass used it as one. `SectionVisibility::OutOfSight` skipped the section outright, and the
section it skipped is one the game's own mesh has been suppressed for - `rustHas` marks a section as Rust's
the moment the bake is committed, and `SectionCompilerMixin` drops Minecraft's mesh for it. So:

- Rust has baked the section and skipped it, because the stale list did not name it;
- Minecraft is not drawing it, because Rust was told about it;
- and a section neither side draws is a 16x16x16 hole, in the shape of the section and nothing else.

It matches the report exactly, which is why this is the answer rather than another candidate: the holes
appear while **unrendered terrain is streaming in** - new sections are exactly the ones the list has not
caught up with - and "any deliberate rebuild fills them", because a rebuild is the camera turning or the
graph updating, which is the refill.

**The switch is `Terrain occlusion culling`, on by default, under the terrain heading.** It is not a
marker file and not a hard-coded decision, because which answer is right depends on the measurement: with
it on, the frame does less work but every section the list is late for is a hole; with it off, every
section the frustum contains is drawn and there is nothing to attribute. Turning it off and flying the same
route is the experiment - if the holes go with it, this is the mechanism, and the fix is to stop treating a
snapshot as an authority rather than to remove the culling.

With it off, the report's `not named by the game's occlusion graph` count still appears - the gather still
counts what the list left out - so the two runs are comparable rather than the number simply vanishing.

### Two ways a refused section became a 16x16x16 hole, and one of them was the fix for the other

The symptom: selecting a large render distance loads a lot of unrendered terrain, and single sections come
out completely missing - a square hole you can see the ground through, which any deliberate rebuild fills.
That last part is the diagnosis: the section is not lost, it is *not being offered*, and the offer only
happens when something else dirties it.

A refusal is the one thing that leaves a section undrawn, because it is the one thing that happens after
both sides have agreed Rust owns it. `sent` records the section, `rustHas` suppresses Minecraft's own mesh
for it, the bake happens, and then the arena has no room - so nothing draws it. The refusal channel exists
to undo that, and it lost entries in two separate places.

**One: the list of refused positions had a cap and dropped what it could not hold.**

```rust
if self.refused_positions.len() < REFUSED_LIMIT {   // 4096
    self.refused_positions.push(pos);
} else {
    REFUSED_DROPPED.fetch_add(1, Relaxed);          // counted, then forgotten
}
```

Past 4096 refusals between two ticks the position was gone: never handed to the JVM, so the JVM kept its
`sent` record, so nothing offered the section again, and `rustHas` kept Minecraft's mesh suppressed. A
large render distance is exactly the state that produces thousands of refusals at once, which is why this
is the test that found it.

The cap is not raised - it is **gone, because there was nothing to cap**. A refusal leaves its section in
the storage (that is the documented point of `allocate` returning `None`), so the storage is already the
set of sections waiting for room. The positions live in a `HashSet` alongside it now, bounded by the arena
itself, and `insert` clears the mark because that is the call that publishes. The only way a refusal is
still lost is a level change, which clears the arena it described - and that is not a hole, because the
world it belonged to is gone.

**Two: the backoff added to stop a rebuild storm could livelock.**

The refusal drain asks the game to rebuild, and the rebuild becomes an offer, which reserves a bake slot.
So it backs off while the bake queue is deep. The first version of that backoff was:

```kotlin
if (backedUp()) { return 0 }
```

and the queue it protects is fed by **nothing else** - so "the pool is full, therefore ask for nothing" and
"nothing finishes, therefore the pool stays full" is a livelock, and the sections in that queue are holes
for the session. It is the exact failure the queue was added to fix, reintroduced one function below it.

The backoff shrinks the budget instead of stopping the drain:

```kotlin
val budget = if (backedUp()) 1 else REDIRTY_PER_FRAME
```

One a frame is a trickle rather than a stop: sixty asks a second against a pool of a few hundred slots,
slow enough that it cannot be the storm the backoff exists for, and non-zero, so the pool is always being
given work that can finish and make room. The test pins the shape, not just the string - it splits
`redirtyDue` and fails if `backedUp()` is consulted anywhere before the budget is computed, because that
is the early return waiting to happen again.

### A refused section was dropped by the drain that was supposed to rescue it

A refusal is the one thing that leaves a section permanently undrawn, and the path that handles it had a
hole in the middle. When the arena has no room for a baked section, the native side reports the position
(`SectionStorage::refused`, a `mem::take` - **handed over exactly once**), and this side has to do two
things: drop the section's record from `sent`, so the next rebuild of it carries its blocks again, and
then *ask the game for that rebuild*, because a rebuild only carries what changed and the rebuild is the
thing that never comes on its own.

The second half was already there and was throttled three ways - but it marked at most
`REDIRTY_PER_TICK` of a batch dirty and **dropped the rest**. And a dropped one was not merely delayed:
its `sent` record had already been removed and the refusal had already been consumed, so nothing would
ever offer that section again. It stayed a hole for the session, with no log line and nothing to look at.

So the drain remembers instead of discarding. Refused keys go into a `LinkedHashSet` (`RustChunkBake`),
which is the de-duplication as well as the queue - a section refused twice before it was retried is one
rebuild and not two - and `redirtyDue` spends a small budget of them **per frame**:

- **`REDIRTY_PER_FRAME` a frame**, because a burst of rebuilds for sections whose blocks have not changed
  is frame time the player pays for. What is skipped is still in the queue;
- **`setSectionDirty`, never `setSectionDirtyWithNeighbors`.** The refusal was for one section; the
  neighbours variant drags its 26 neighbours into the rebuild queue, which turns one refusal into 27
  rebuilds - and none of those neighbours was refused, so each of their rebuilds offers a payload and
  reserves a bake slot for nothing;
- **and `backedUp()`: no requests while the bake pool is behind.** This is the one that matters when the
  arena is small, and it is the storm the design has to avoid - marking sections dirty makes Minecraft
  offer them, every offer applies a 27-section payload and reserves a bake slot, and a full queue drops
  the offer. Without the backoff that loop runs at whatever rate the frames do: *queue full, mark dirty,
  offer again, still full*. `queuedBakes` and `maxQueuedBakes` come from the native side so the two sides
  agree about what "backed up" means rather than one writing down the other's constant, and the threshold
  is two thirds - below that the pool is working and there is room to feed it.

**Per frame, not per tick**, for the two things that throttle it: the budget it spends and the pool it
backs off from are both drained by frames. A tick is 50 ms, and fifty milliseconds of rebuild requests
arriving in one lump is the spike the per-frame budget exists to avoid. The hook is
`GameRenderer#render` at `HEAD` (`GameRendererMixin`), which is the frame - and at `HEAD` rather than
`TAIL` because `setSectionDirty` starts a task on Minecraft's chunk-build threads, so asking at the top
gives those threads the frame to work in.

The queue is bounded at the native side's own refusal cap, past which it has already forgotten refusals
of its own; and the drain only queues at all while `terrainArenaCanGrow`, because a refusal whose section
cannot have room made for it would be refused again - the arena doubles on a refusal and stops at the
device's buffer limit, and past that a rebuild per refusal is a spin rather than a convergence.

Checked in a run, on the line the drain prints:

```
the section arena refused 2 section(s); forgetting them, so the next rebuild of each carries its
blocks again, and queueing 2 of those rebuild(s) (2 drained so far, 2 waiting)
```

### The terrain pass was allocating a HashMap per section, and drawing water in hash order

Four things were wrong with one loop, and the last is the one that was a picture difference.

**The push constants were a `HashMap<String, (Vec<u8>, ShaderStages)>`, built per section per layer.** A
pass over four hundred sections allocated four hundred `HashMap`s, four hundred `String`s and four
hundred `Vec`s, to write sixteen bytes each. The layout knows what a draw needs - it was built from the
config - so the offsets and sizes are resolved **once, when the pipeline is built**
(`BoundPipeline::immediates`) and a draw hands over a slice parallel to them, built in a fixed-size array
on its own stack (`set_immediates`). It asserts the length against the layout, which caught nothing in
testing and is there because a short value is a shader reading whatever follows it in the buffer.

**The counters were relaxed `fetch_add` per section per layer**, for numbers a log line reads once a
second - and two of them had no reader at all. They are local `u64`s now, added once each at the end of
the pass.

**The frustum test ran once per layer**, on a box that does not depend on the layer. The gather is now one
pass over the sections - occlusion list, frustum, and which layers the arena holds - into a list reused
between passes, and the draw loops walk that list and nothing else. They never touch the arena's
`HashMap` again.

**And the translucent layer was drawn in `HashMap` iteration order.** That is the picture difference, and
it is not a subtle one: water blends and does not write depth, so each face mixes with what is already in
the target and has to be drawn before the face behind it. Hash order is no order at all, and it changed
as the map grew - so two panes of glass, or a lake and the water behind it, blended in whatever sequence
the hasher produced. `sort_for_drawing` puts the list far-to-near when one of the layers being drawn
blends, and **leaves it alone otherwise**: the opaque two write depth and are ordered by the depth test,
so sorting them would be work for nothing.

The sort is keyed on the section, not on the face. Sorting faces is what the game does for *its*
translucent mesh, which is rebuilt per section and re-sorted when the camera moves a block; a section is
the granularity this side has, and it is the granularity the game orders its own translucent sections at.

Two notes on what the diagnostics now mean. `culled` and `out of sight` are decided during the gather -
once per section, before a layer is chosen - so a two-layer pass reports about half what it used to; the
ratio between them, which is what the line is read for, is unchanged. And `drawn` and `empty` are still
per layer, because they are decided where the layer is drawn.

### `textureSample` under an `if` is undefined behaviour, however uniform the condition looks

The terrain fragment stage picked which atlas to read like this:

```wgsl
var texel: vec4<f32>;
if (in.game_atlas == 1u) {
    texel = textureSample(t_game_atlas, t_game_sampler, in.tex_coords);
} else {
    texel = textureSample(t_texture, t_sampler, in.tex_coords);
}
```

`textureSample` takes its level of detail from the derivatives of the coordinates, and WGSL defines those
only in **uniform control flow**. A `textureSample` inside a branch is undefined behaviour - not "slow",
not "discouraged", undefined - whether or not the condition happens to hold for every fragment.

The argument for the branch was real and is worth writing down, because it is the argument that will be
made again: `game_atlas` is `@interpolate(flat)`, so every fragment of one primitive takes the same
branch and the derivative is the one it would have had. That is an argument about the *picture coming out
right on the hardware it was tried on*. It is not an argument about the program being defined, and the
gap between the two is a driver that decides to execute both sides of a uniform branch, or one that
vectorises a quad across a primitive boundary - and this shader runs on whatever driver the player has.

The samples are hoisted and the choice is a `select` between the values:

```wgsl
let texel_from_game = textureSample(t_game_atlas, t_game_sampler, in.tex_coords);
let texel_from_ours = textureSample(t_texture, t_sampler, in.tex_coords);
let texel = select(texel_from_ours, texel_from_game, in.game_atlas == 1u);
```

Both fetch the same coordinates with the same sampler *shape*, and both atlases are the same size, so the
two mip chains are indexed identically and the pair costs what the branch cost whenever both sides were
live. `select` rather than `mix`: this is a choice and not a blend, and a half-way value would be one
atlas bleeding into the other at every sprite edge.

Both `terrain.wgsl` and `terrain_solid.wgsl` had it, and both are fixed - the vertex stage's lightmap
fetch is a `textureSampleLevel` with an explicit level, which is legal anywhere and has to be, because a
vertex stage has no derivatives to take a level from.

**The test is structural, and that is the whole point of it.** The offending code is now quoted in the
comment above the fix, so a test that looked for the string `textureSample` under an `if` would fail on
the explanation of why it must not be there. Instead the shader is parsed with naga and walked: a
`textureSample` whose level is `Auto` and which sits one or more branches deep is a failure. Two
supporting tests keep the detector honest, because "found nothing" is also what a detector that finds
nothing at all reports - the branching shape is fed to it as a fixture and must produce two findings one
branch deep, and the hoisted shape must produce none.

### The block atlases are point-sampled, and the game never point-samples them

**This is a rolled-back change and the note is kept for the evidence, not for the code.** The filters are
back to `Nearest` on both atlases; what follows is why the game disagrees, and what is still open.

`LevelRenderer` builds one sampler for its terrain and uses it for both groups:

```java
this.chunkLayerSampler = RenderSystem.getDevice().createSampler(
    AddressMode.CLAMP_TO_EDGE, AddressMode.CLAMP_TO_EDGE,
    FilterMode.LINEAR, FilterMode.LINEAR, maxAnisotropy, OptionalDouble.empty());
...
chunkSectionsToRender.renderGroup(ChunkSectionLayerGroup.OPAQUE, this.chunkLayerSampler);
chunkSectionsToRender.renderGroup(ChunkSectionLayerGroup.TRANSLUCENT, this.chunkLayerSampler);
```

so the game's block atlas is **bilinear**, in both passes, always.

**The video option that sounds like it turns filtering off does not.** `TextureFilteringMethod` is
`NONE`, `RGSS` or `ANISOTROPIC`; `FilterMode` has no off switch at all - `NEAREST` and `LINEAR` and
nothing else - and two of the three values resolve to `maxAnisotropy = 1`. Everything the option moves
is the anisotropy. So there is no setting under which the game's block textures are point-sampled, and
the default is bilinear.

Both filters matter and they do different jobs: `mag_filter` is what a sixteen-texel texture filling two
hundred pixels looks like - sixteen squares or a smear - and `min_filter` is the same question at
distance, where the atlas is minified and point sampling picks one texel out of the four a pixel covers.
The second is the one that was under suspicion for the distant-lava flicker.

**The change was rolled back rather than kept because it is not a free variable.** The flicker is a
sample-frequency artifact, and the sampling rate is what a filter decides: carrying a filter change while
measuring it moves the thing being measured. So the filters went back to where they were, and the
question of which is right waits until there is a measurement that can tell. The one-line change is
`mag_filter`/`min_filter` in `render::atlas::block_atlas_sampler`.

**What is *not* rolled back, and is the part that was actually a bug.** The two atlases are both in one
frame - a face whose sprite the game animates samples the game's `blocks.png`, and the grass, stone and
leaves beside it sample this side's copy - and there was a revision where only the game's sampler was
bilinear. A frame holding one of each is a frame with both filters in it, and a player handed that path
said *"these are all blurry, there is none of the game's crisp pixels left"* about the fire, the lava and
every other animated sprite, while the blocks around them were clean. The two were made to agree, and
they still do: both build from one function, `render::atlas::block_atlas_sampler`, and the test asserts
the **agreement** rather than the filter, so it would hold for either answer and only fires if the two
drift apart. Rolling back to `Nearest` rolled back both, which is exactly why that is one function.

The one thing that is meant to differ is the address mode: `Repeat` for this side's atlas (its
coordinates come from its own packing) and `ClampToEdge` for the game's. A descriptor needs no device,
which is the only reason the test can exist at all - the two samplers are created deep inside
`RenderGraph::new` and `TextureManager::new`, both of which want a `Gpu`.

**Anisotropy is missing entirely**, in either direction: there is no pipeline field for it the way there
is one for `depth_write`, so a player on `ANISOTROPIC` does not get it. Kept here because it is the one
part of the game's sampler that neither answer has.

### The frustum is not occlusion culling, and the game already had the answer

The terrain pass culled its sections against the camera's frustum. That is what Minecraft does *first*
and what its `SectionOcclusionGraph` then throws most of away: the graph walks outward from the camera
through the sections it can actually see - a section is reached only through a neighbour whose face
toward it is not opaque - so the couple of thousand sections inside the frustum of a normal view become
the few hundred that are not behind a hill. Drawn with the frustum's set, every section of a cave
system, a ravine or a forest floor is submitted, and the vertex stage transforms geometry the depth test
then discards.

`LevelRenderer#visibleSections` is that graph's answer and it is rebuilt every frame in `setupRender`,
which is before the terrain pass runs. It is read rather than recomputed, because recomputing it is what
the game is already doing and the game's version is the one with the graph in it:

- `VisibleSectionsAccessor` reads the list, `RenderSectionNodeAccessor` reads each section's own
  `sectionNode` - a `SectionPos.asLong`, the same packing `RustChunkBake` keys its records by and
  `section_pos` unpacks, so a section cannot be named one way on one side and another way on the other;
- `TerrainPass#sendVisibleSections` hands the keys over once per frame, and **only when they change** -
  the list is the same list most frames, and a native call per frame to pass a thousand identical longs
  is the cost that avoids;
- the pass obeys it **including when it is empty**. `None` is "nothing has been sent", where a frustum
  is the best answer there is, and `Some(empty)` is "the game looked and saw nothing", where drawing the
  arena would be drawing exactly what the game decided not to. Read as "not told", the feature draws the
  whole world for a frame the game deliberately emptied and looks like it does nothing at all - which is
  why `section_visibility` is a function with a test rather than three lines inside the draw loop.

The frustum test stays. It is nearly free, and the two disagree in both directions: the graph's answer
is a frame old and conservative about what a neighbour hides, so a section it left out may be one the
camera has since turned toward.

**Two things about the accessor were wrong first**, and both are the same trap this project has paid for
before - an accessor is matched by name and **erased descriptor**:

- the field is `ObjectArrayList<RenderSection>`, not `List<RenderSection>`, so declaring the interface it
  happens to satisfy is `InvalidAccessorException: No candidates were found matching
  visibleSections:Ljava/util/List;` at load. The *declared* type is what has to be named;
- the same shape of mistake as the `this$0:Ljava/lang/Object` one, which is why the doc comment says so.

The report line grew an `N not named by the game's occlusion graph` count beside the frustum's, because
the two are the interesting pair: out-of-sight much larger than culled is the game culling properly, and
out-of-sight at zero with the frustum culling hard is the graph's list not arriving at all. Its
per-layer figures had to become **frame-wide totals** rather than drained counters: the opaque group is
two pipelines over one geometry, so a report at a pipeline boundary showed whichever of them the graph
reached last - a solid layer reported as `0 drawn` while it was drawing, which is how that was noticed.

### The solid layer was paying for a `discard` it could never reach

The terrain shader ended its fragment stage with an alpha test at a cutoff that travels in the push
constant:

```wgsl
if (col.a < section_pos.alpha_cutout) {
    discard;
}
```

and one pipeline drew **both** opaque layers, with `alpha_cutout` set to `0.5` for the cutout layer and
`0.0` for the solid one. The solid test can never fire. It cost the whole pipeline early-Z anyway.

That is not a driver quirk to work around, it is the shape of the hardware: a fragment shader that can
throw a fragment away cannot have run before the depth test, because whether it throws it away is not
known until it has. So the GPU has to run the fragment stage for every covered pixel and only then ask
about depth - and hierarchical-Z, which rejects whole tiles before the fragment stage, is gone with it.
The solid layer is most of the screen, so this was most of the frame's fill rate spent on fragments that
were about to fail the depth test.

`alpha_cutout` is a `var<immediate>`, which is why nothing could be done about it in one shader: its
value is not known until the draw call, so the branch cannot be folded away at pipeline creation. wgpu
has no pipeline-overridable constant here, so the two layers are two shaders and two pipelines -
`terrain_solid.wgsl` / `terrain_solid` with no test at all, and `terrain.wgsl` / `terrain` with it. That
is the same split Minecraft makes: its `SOLID_TERRAIN` defines no `ALPHA_CUTOUT` and its `CUTOUT_TERRAIN`
declares `0.5`.

Three things had to move with it, and each one is a way to get this wrong that the tests now check:

- **`terrain_layers` is keyed on the pipeline name, not the geometry.** The two pipelines share
  `@geo_terrain`, so the geometry-keyed lookup that was there would have handed *both* of them *both*
  layers - putting the solid layer straight back through the shader with the test in it, with nothing to
  see in the picture to say so;
- **the opaque pass names both pipelines.** `render_with_mvp_only`'s `only` became a slice, because
  Minecraft's opaque group is two layers: naming one of them is a layer that is baked and never drawn,
  which is the state the transparent layer was in for a while;
- **the depth clear follows the solid layer.** It was already asked of the layers rather than of a name,
  so `terrain_solid` claims the frame's depth and `terrain` loads it, whichever the graph reaches first.

Two tests hold it: the layer split is asserted for all three pipelines (including that the geometry is
*not* a key any more), and the shipped shader sources are read to assert that `terrain` and
`terrain_solid` differ by exactly the `discard` - the solid one must not contain the instruction at all.
A third parses and validates the new file with naga, because a shader that fails to build is a pipeline
the graph skips **in silence**, and a skipped `terrain_solid` is the solid layer of the world not drawn.

### A pass's binding stash is shared across its pipelines, and that made the diagnostic lie

`WgpuRenderPass` remembers every name bound into a pass and re-emits them when the game sets a new
pipeline in it - the same name sits in a different slot under a different pipeline, so what was written
for the previous one is meaningless and has to be written again in the new one's slots. That is right. What
was wrong is that it re-emitted **everything**, including names the new pipeline's shader has never heard
of:

```java
clearBindings();
for (Map.Entry<String, Bound> entry : boundBindings.entrySet()) {
    writeBinding(entry.getKey(), entry.getValue());   // every name, into any plan
}
```

`SpriteContents.AnimatedTexture#drawToAtlas` picks one of two pipelines **per sprite**, and the animated
sprites in one atlas use both, so the two alternate once per sprite per mip level:

```java
if (this.animationInfo.interpolateFrames) {
    renderPass.setPipeline(RenderPipelines.ANIMATE_SPRITE_INTERPOLATE);
    renderPass.bindTexture("CurrentSprite", ...);
    renderPass.bindTexture("NextSprite", ...);
} else if (this.isDirty) {
    renderPass.setPipeline(RenderPipelines.ANIMATE_SPRITE_BLIT);
    renderPass.bindTexture("Sprite", ...);
}
```

so each pipeline was handed the *other's* names on every switch, and the log said:

```
animate_sprite_blit           bound CurrentSprite          plan: 0:SpriteAnimationInfo, 1:Sprite, 2:Sprite
animate_sprite_blit           bound NextSprite             plan: ...
animate_sprite_interpolate    bound Sprite                 plan: 0:SpriteAnimationInfo, 1:CurrentSprite, 2:CurrentSprite, 3:NextSprite, 4:NextSprite
```

**A complementary pair of warnings that says nothing about either pipeline** - and this is the diagnostic
that exists for the case where a name *is* requested and the shader spells it differently, which is a
binding silently left empty. Two spurious warnings per animation pipeline is exactly the noise that buries
it.

The fix drops a carried-over name the new plan has not got, and does not report it: nothing is being
requested, the game binds what each pipeline's shader declares after `setPipeline`, and a name that
survives a pipeline change is one the previous pipeline's draw needed. Only the two real binding entry
points report now. Checked in a run: the pair is gone from both pipelines, the remaining unplanned-texture
list is empty, and the atlas still binds `Sprite` to `lava_still` and `lava_flow` at every mip level.

**This did not turn out to be the distant-lava flicker**, which is why the section says so: the lava's
animation has no `interpolate` in its `.mcmeta`, so it takes the `blit` branch and its `Sprite` binding was
never the one being dropped. It is a real bug found on the way to a different one, and it is fixed because
of what it was doing to the diagnostics rather than to the picture.

### A pipeline is skipped in silence when its shader is not found, and the shader is named after the pipeline

The second terrain pass - the one that draws water and glass - was written as a pipeline called
`translucent_terrain`, and it never drew a single section. Not slowly, not wrongly: **zero**. The
diagnostic that settled it was per-layer rather than total, because the total could not tell the two
explanations apart:

```
; solid 826+0, cutout 704+0, transparent 0+0 (drawn+empty)
```

The second number of each pair is the sections whose layer the arena had **nothing** in. Solid and cutout
were drawing hundreds of sections a second; the transparent layer was not drawing anything *and* was not
reporting empty layers either - so the loop was never entered, which means the pass did not exist. A layer
whose geometry is missing from the arena reports `empty` for every section; a layer nothing iterates
reports nothing at all. That difference is the whole reason those six numbers are in the report.

The cause is one line in `graph.rs`:

```rust
let shader_resource = ResourcePath(format!("wgpu_mc:shaders/{}.wgsl", pipeline_name));
```

A pipeline's shader is looked up **by the pipeline's own name**. `terrain` finds `terrain.wgsl`;
`translucent_terrain` went looking for `translucent_terrain.wgsl`, which does not exist - and a pipeline
whose shader cannot be read is skipped on purpose, because the block atlas is registered by a resource
reload and a pipeline built before it has a legitimately missing shader. A panic there would end the JVM.
So the graph logged a line at a level the game's log file may not carry, drew the rest of the frame, and
exited zero.

Two pipelines with the same shader and different pipeline state is the normal case, so a pipeline can now
say which shader it wants:

```yaml
  translucent_terrain:
    geometry: "@geo_terrain_translucent"
    shader: terrain          # the same program; the difference is the pipeline state below it
    blending: alpha_blending
    depth_write: false
```

**And the silence is what the test is for.** A skipped pipeline is indistinguishable from a pipeline that
draws nothing, so `every_pipeline_in_the_shipped_graph_has_its_shader` walks the graph and asserts that
each pipeline's resolved shader is one the mod ships - checked by removing the `shader:` line above and
watching it fail with the pipeline's name in the message, because a test that has never failed is a test
nobody has seen work.

### The water was baked and never drawn, and Minecraft's terrain is two passes and not one

The fluid mesher has handled water since it was written: `fluid_sprites` gives water the translucent
layer and lava the solid one, the corners, the flow, the risers and the underside are the same code, and
the sprites are looked up for both. What kept water off the screen was one line:

```rust
let sprites = match kind {
    // Lava only, for now: water belongs in the translucent layer and no pass of ours draws that
    // layer yet - Minecraft still draws its own water ...
    1 => continue,
    2 => &sprites[1],
```

which was the right call while it was written, because **no pass of ours drew the transparent layer at
all**. The graph's terrain pass walked `[Solid, Cutout]` and stopped: the transparent layer had been
baked into the arena since the arena had a third layer, holding ice, stained glass and every other block
model whose sprite blends, and nothing had ever drawn it. So "take over water like lava" is not a copy of
the lava path - lava is *drawable* because it lands in the solid layer, which the opaque pass already
draws.

**And the layer cannot simply be added to that pass.** Minecraft's own terrain is two *groups*, not one
pass with three pipelines:

```java
public enum ChunkSectionLayerGroup {
    OPAQUE(ChunkSectionLayer.SOLID, ChunkSectionLayer.CUTOUT),
    TRANSLUCENT(ChunkSectionLayer.TRANSLUCENT);
}
```

and the two differ in pipeline *state* as well as in when they run: `TRANSLUCENT_TERRAIN` is built with
`BlendFunction.TRANSLUCENT`, an `ALPHA_CUTOUT` of `0.01` rather than the cutout layer's `0.5`, and - the
half that is easy to miss - it does not write depth, because a translucent face has to be tested against
what is behind it and must not become what the next one is tested against. Drawing water in the opaque
pass would be water with no blending at all, which reads as an ocean you cannot see the bottom of rather
than as an error.

So the graph has a second terrain pipeline, `translucent_terrain`, and `PipelineConfig` grew a
`depth_write` flag for it - the one field of a pipeline that is about what it does to the frame rather
than about what it draws:

```yaml
  translucent_terrain:
    geometry: "@geo_terrain_translucent"
    blending: alpha_blending
    depth_write: false
```

`terrain_layers` in `graph.rs` is what decides which layer each pass draws, and it is keyed on the
geometry name so the yaml is the one place the two are told apart.

**Three things about the takeover are worth keeping.**

*The graph is recorded once and both groups are suppressed.* `render_terrain_pass` runs the **whole**
graph, so one call draws the opaque pass and the translucent one in the order they are listed. Recording
it at both pipelines - which is what "take over the second pass too" sounds like - would draw the water
twice, once blended against the frame and then again on top of itself. So the opaque group is where it is
recorded, and each group's own pipeline is where that group's *meshes* are dropped: the opaque pass when
it opens, and the translucent pass later in the same frame, on the strength of `TerrainPass.graphDrawn`.
The flag is cleared at `presentTexture`, which is the one point in a frame that is known to happen
exactly once and after everything else.

*The translucent target usually does not exist, and that is what makes this takeable.*
`ChunkSectionLayerGroup#outputTarget` falls back to the main target, and the target of its own is only
created when a transparency post chain is loaded - `Minecraft.useShaderTransparency()`, which is the
Fabulous preset or a resource pack that ships a chain, and Fabulous is deliberately not offered here. So
on the presets this build offers, the translucent group is a second pass into the **main** target, which
is exactly the pass this takeover is handed. When a pack does turn it on, `TerrainPass.usesOwnTarget`
refuses the takeover and says so once rather than drawing water into a texture whose depth belongs to
another target.

*Water is not free, and the arena had to be resized for it.* The last measured session ran the pool at
**79%** with only the two opaque layers in it, and water is a whole layer more - so `SLOTS_PER_SECTION`
in `arena_slots` went from 16 000 to 20 000, which is a fifth, which is vanilla's own ratio: its
translucent section buffer is 786 432 bytes against 4 194 304 for each of the two opaque ones. The faces
inside the water are already culled by the fluid mesher (a side towards the same fluid is skipped, and so
is a top with the same fluid above it), so what is added is bounded by the water's *surface* and not by
its volume - but `grow_arena` is the backstop, and a session that refuses sections is the diagnostic to
read.

### The lighting was a straight line through a curve the game had already built

The fragment shader ended its lighting with

```wgsl
var light = max(lc.x, lc.y) * 0.7 + 0.3;
```

where `lc` was the vertex's two light levels over fifteen. That is a line from `0.3` at no light to `1.0`
at full light, and it is the *only* thing the terrain's brightness depended on besides the vertex colour
and the ambient occlusion. What it ignores is everything the game puts into its lighting:

- the **gamma and brightness options** (the brightness slider), which bend that curve per player;
- the **time of day**, where the sky light at night is a dim blue rather than a dim grey;
- **night vision**, the **darkness effect**, and a dimension's own ambient light.

None of those are a curve this side could reproduce anyway, because the game does not use a curve: it
builds a **16x16 lightmap texture** (`Lightmap`, `RGBA8`, rebuilt by `Lightmap#render` whenever any of
those inputs move) and its own terrain shader does nothing with it but fetch it:

```glsl
// terrain.vsh
vertexColor = Color * sample_lightmap(Sampler2, UV2);

// sample_lightmap.glsl
vec4 sample_lightmap(sampler2D lightMap, ivec2 uv) {
    return texture(lightMap, clamp((uv / 256.0) + 0.5 / 16.0, vec2(0.5 / 16.0), vec2(15.5 / 16.0)));
}
```

`UV2` is the light pair as a nibble per light scaled by sixteen, so `uv / 256 + 0.5 / 16` is the centre
of the texel that pair names - one fetch per vertex, and the rasterizer interpolates the resulting colour
across the quad. That is now what this renderer does, in the same two places the game does it: the vertex
stage fetches `t_lightmap` at `nibble / 16 + 1/32`, the colour travels to the fragment stage as a varying,
and the fragment stage multiplies it in with no curve of its own left anywhere.

The texture is handed over the same way the block atlas is (`WmNative.bindGameLightmap` →
`bind_game_lightmap`, bindings 7 and 8), with one difference that matters: the game does not build a new
lightmap when the light changes, it **writes into the one it has**. So the handover is per *texture* - the
JVM asks once a frame and compares view identity, which is one pointer comparison - and what follows the
time of day afterwards is the texture's contents, not a new binding.

The fallback is the interesting part, because this is the one resource whose absence would be *visible*:
a missing atlas is a skipped pipeline and a warning, but a missing lightmap would be a world lit at full
brightness by a white placeholder. So the native side builds itself a 16x16 lightmap holding exactly the
curve above (`fallback_lightmap`, one texel per light pair, `max(block, sky) / 15 * 0.7 + 0.3`), and a run
whose handover failed draws the picture this renderer has always drawn. A test reads that image back and
checks four of its texels against the old formula.

Corrected in passing, because the previous section claimed more than it should have: the **ambient
occlusion** curve is the game's average, but its *blend* is this renderer's. The four corner counts are
blended bilinearly across the quad, where vanilla writes four per-vertex colours and lets the rasterizer
interpolate them across the quad's two triangles - which is why vanilla has a faint diagonal through a
shaded face and this renderer does not.

Still not the game's, in the same shader: **fog**. Minecraft's terrain shaders end with `apply_fog(...)`
over `FogEnvironmentalStart`/`End`, `FogRenderDistanceStart`/`End`, `FogColor` and the fog shape, and this
shader has no such term at all - so distant terrain is unfogged rather than fading into the sky. That,
rather than the light, is the next thing the picture is missing.

### The terrain had no fog at all, which is a hard edge where the world ends

Minecraft's terrain shaders end with a fog term:

```glsl
// terrain.vsh
sphericalVertexDistance = fog_spherical_distance(pos);
cylindricalVertexDistance = fog_cylindrical_distance(pos);

// terrain.fsh
fragColor = apply_fog(color, sphericalVertexDistance, cylindricalVertexDistance,
                      FogEnvironmentalStart, FogEnvironmentalEnd,
                      FogRenderDistanceStart, FogRenderDistanceEnd, FogColor);
```

and this shader had nothing of the kind - so the loaded world ended in a **line**: terrain at the render
distance stayed at full brightness and full colour, and the sky began one pixel later. Of everything the
terrain has been missing, this was the one a player sees from anywhere in the world.

The fog is now the game's, transposed from `minecraft:fog.glsl` into the shader:

```wgsl
fn apply_fog(color, spherical, cylindrical, env_start, env_end, render_start, render_end, fog_color) {
    var fog_value = max(linear_fog_value(spherical, env_start, env_end),
                        linear_fog_value(cylindrical, render_start, render_end));
    return vec4(mix(color.rgb, fog_color.rgb, fog_value * fog_color.a), color.a);
}
```

with the game's own two distances (`fog_spherical_distance` is `length(pos)`, `fog_cylindrical_distance`
is `max(length(pos.xz), abs(pos.y))`) and the game's own numbers, which are the interesting part:

- the four distances and the colour are read from **`CameraRenderState#fogData`**, and that object is not
  a copy of the fog - it *is* the fog. `GameRenderer.renderLevel` writes it into the game's fog buffer
  (`FogRenderer#updateBuffer(cameraState.fogData)`) and hands the slice to the level renderer, which binds
  it for the pass this one stands in for. So the water fog, the lava fog, blindness, the darkness effect,
  the biome's fog and the render-distance fade all arrive already decided, and nothing here has to know
  which of them is in force;
- the fog is measured from a **camera-relative** position, and the position this shader computes is
  relative to the camera's *section* (that is the whole of its precision - see the shadow-stripe section).
  So the camera's offset inside its own section travels with the fog block, and the shader subtracts it
  before measuring. The cylindrical distance is why it has to be done in world axes rather than through
  the view matrix: it is `length(pos.xz)`, a *world* horizontal distance, and rotating it would measure
  something else;
- the alpha of the fog colour scales the blend (`fog_value * fogColor.a`), which is how the game fades fog
  out entirely - and it is also what makes "all zeroes" a safe state to start in: a fog colour of
  `(0, 0, 0, 0)` blends nothing, so a frame drawn before the first upload is the unfogged picture rather
  than a world fading to black.

The block travels in a small uniform buffer of its own (`@fog_environment`, binding 9, 48 bytes: the
colour, the four floats, the camera offset and its padding) because the game rebuilds the fog every frame
and this is written from the same place the terrain matrices are.

Wired along with it: the ABI test now checks **both** directions of the JNI bridge. It already caught a
`WgpuNative` declaration with no `#[jni_fn]` behind it - which is an `UnsatisfiedLinkError` at the call -
and it now also catches the reverse, a `#[jni_fn]` nothing declares, which is the direction that fails
*silently*: it is a feature that looks implemented on the Rust side and does nothing at all in the game.
This is exactly how the fog was wired - the Rust half and the shader went in first, and the Kotlin
declaration was a step the compiler could not ask for.

Not taken over with it: `ChunkVisibility`, the per-section fade the game multiplies in before the fog
(`color = mix(FogColor * vec4(1, 1, 1, color.a), color, ChunkVisibility)`), so a section that has just
been meshed appears rather than fades in. The value lives on the game's own render section and changes
per frame, which is a feed of its own; the picture without it is the one this renderer has always drawn.

### The layout said fragment-only, and the vertex stage fetched the lightmap

Entering a world ended the game. The console had the reason, and it was wgpu refusing the terrain pipeline:

```text
In Device::create_render_pipeline, label = 'terrain'
  Error matching ShaderStages(VERTEX) shader requirements against the pipeline
    Shader global ResourceBinding { group: 0, binding: 7 } is not available in the pipeline layout
      Visibility flags don't include the shader stage
```

Binding 7 is the game's lightmap, and the **vertex** stage samples it - that is the whole of the lightmap
handover, one fetch per vertex with the colour interpolated, `vertexColor = Color * sample_lightmap(Sampler2,
UV2)` - while the bind group layout this graph builds for its own pipelines declared textures and samplers
fragment-only:

```rust
// graph.rs, in `ResourceBacking::get_bind_group_layout_entry`, before
visibility: wgpu::ShaderStages::FRAGMENT,
```

Nothing had ever objected because nothing had ever asked a vertex shader for a texture: the buffers were
allowed in every stage, and the two atlases and their samplers are only sampled in the fragment stage. The
lightmap was the first, and it took the terrain - the whole layer, not one block - with it.

**One answer for every binding.** Which stage samples a binding is a property of the *shader*, and the
shaders here belong to the game and to packs; there is no binding this side can be sure only one stage will
read. `ResourceKind::visibility` is that one answer now - `VERTEX_FRAGMENT`, the same thing the game's own
pipeline builder gives every entry (`blaze.rs`) - and the four arms of the layout `match` read it rather
than each spelling out their own. A test reflects the shipped `terrain.wgsl` with naga, the same front end
and the same analysis wgpu runs before it matches a shader against a layout, and fails if any binding an
entry point uses is hidden from that entry point's stage. It also fails if the terrain vertex stage stops
sampling the lightmap, because at that point the test is about nothing and the layout could be narrowed
again. Run against the old code it fails on binding 7, which is what makes it the guard for this rather
than a description of it.

**And the reason a layout mistake was a crash rather than a black screen.** wgpu reports an uncaptured
validation error by *panicking*, and this renderer runs inside `#[jni_fn]` frames - which cannot unwind, so
the panic does not reach a `catch` anywhere: it ends the process. What the player sees is the reason above
followed by

```text
panicked at library/core/src/panicking.rs:225:5:
panic in a function that cannot unwind
```

and no game. `device_call` now wraps the two calls that build a pipeline (the pipeline layout and the
pipeline itself): it catches the panic, logs the wgpu error as an *error* naming the pipeline, and the
pipeline is skipped - which is what every other failure on that path already does (a missing resource, an
unreadable shader, a device without `immediates`). A graph missing one pipeline draws the rest of the
frame, so the next mistake of this kind costs the terrain and a log line rather than the session. It is a
guard and not a licence: a pipeline that will not build is still a bug, and the message is still an error.

### Every plant stood dead centre, because a field of grass is not a grid

`BlockBehaviour.Properties#offsetType` gives a block a **random offset derived from its own
coordinates**, and the plants are the blocks that ask for one:

```java
case XZ -> (state, pos) -> {
    long seed = Mth.getSeed(pos.getX(), 0, pos.getZ());
    float maxHorizontalOffset = block.getMaxHorizontalOffset();
    double x = Mth.clamp(((float)(seed & 15L) / 15.0F - 0.5) * 0.5, -maxHorizontalOffset, maxHorizontalOffset);
    double z = Mth.clamp(((float)(seed >> 8 & 15L) / 15.0F - 0.5) * 0.5, -maxHorizontalOffset, maxHorizontalOffset);
    return new Vec3(x, 0.0, z);
};
case XYZ -> ... // the same, plus y = ((float)(seed >> 4 & 15L) / 15.0F - 1.0) * getMaxVerticalOffset()
```

`ModelBlockRenderer` reads it once per block and adds it to every vertex of the model:

```java
this.random.setSeed(seed);
model.collectParts(level, pos, blockState, this.random, this.parts);
Vec3 offset = blockState.getOffset(pos);
... tesselateAmbientOcclusion(output, x + (float)offset.x, y + (float)offset.y, z + (float)offset.z, ...)
```

This side placed every block at its own coordinates, so **every plant stood exactly in the middle of
its block** - and a field of grass that is a perfect grid is the one thing a field of grass never
looks like. `short_grass`, `fern`, `bush`, `short_dry_grass` and `tall_dry_grass` are `XYZ`;
`dandelion`, `poppy` and every other flower, plus `tall_seagrass` and `mangrove_propagule`, are `XZ`.

Three details of the formula are each a way to get it nearly right, and all three are in the port:

- **the hash takes `y = 0`**, not the block's own height, so a plant at the top of a hill and one at
  the bottom of the same column get the *same* offset;
- **`y` runs `-maxY .. 0`** - `bits / 15 - 1`, not `bits / 15 - 0.5` - so a plant may sink into the
  ground and may not float above it;
- **the horizontal clamp never has to travel.** Its argument spans `-0.25 .. 0.25` exactly and the
  game's default limit is `0.25`, so the limit only matters for pointed dripstone, which raises it and
  is not a plant. Only `maxY` crosses the bridge.

**`getMaxVerticalOffset` is `protected`**, so it cannot be read - and the JVM side recovers it by
asking the game's own `getOffset` at the sixteen positions whose `x` hash bits take all sixteen values,
keeping the candidate limit that reproduces the vertical offsets it returned. Asking the state instead
of the number is also what makes it immune to an override.

The first design carried only `maxY` and used "is it non-zero" as the flag, which is **wrong for every
flower**: an `XZ` offset's vertical limit is *exactly zero*, the same value a block with no offset
sends, so flowers would have stayed dead centre. The bug was caught by a test asserting a flower is
nudged horizontally, and the fix is the separate `offset_xz` bit - which is why the two travel
together and why the doc comment on `offset_max_y` says "zero either for a block that stands where it
was placed *or* for one whose offset is horizontal only".

Five tests hold it: the hash against Java's own values (including a negative coordinate), the exact
offset of `short_grass` at `(10, 0, 10)` to nine places, the three properties above over a 60×60 span
of coordinates, a block with no offset staying put, and two states of one key keeping the offset only
when they agree on it exactly.

### Every plant in the game was 29% too small, because a turned element has to be stretched back

`minecraft:block/tinted_cross` is the parent of `short_grass`, `fern`, `bush`, `tall_grass_bottom`,
`large_fern_bottom`, `sugar_cane`, `bamboo_sapling` and the rest - every plant the game draws as a cross -
and its two elements both say the same thing:

```json
{ "from": [0.8, 0, 8], "to": [15.2, 16, 8],
  "rotation": { "origin": [8, 8, 8], "axis": "y", "angle": 45, "rescale": true } }
```

The element is 14.4 units of block wide. A 45 degree turn about `y` puts its **width** on the diagonal, so
what the block actually covers is `14.4 * cos(45) = 10.18` - **29% narrower** - and `"rescale": true` is the
model saying "and now stretch it back". This side read the rotation, the axis and the angle, and never read
the flag: `grep rescale` was zero hits in `rust/`, zero in `neoforge/src`, and the block models that ask for
one number **39**.

Minecraft's answer is `CuboidRotation#computeRescale`: for each axis, take the unit vector, turn it with the
same matrix the geometry is turned with, and return the reciprocal of its largest component.

```java
private static float scaleFactorForAxis(Matrix4fc rotation, Direction.Axis axis, Vector3f scratch) {
    Vector3f transformedAxisUnit = rotation.transformDirection(scratch.set(axis.getPositive().getUnitVec3f()));
    return 1.0F / Math.max(Math.max(abs(x), abs(y)), abs(z));
}
```

For the 45 degree turn the turned `x` unit is `(cos45, 0, -sin45)`, whose largest component is `cos45`, so the
factor is `1.4142` on `x` and `z` and `1` on `y` - exactly the `1/cos` that undoes the narrowing. A quarter
turn comes out at one on every axis, because a quarter turn preserves lengths, and so does the identity.

The order is not a detail. `CuboidRotation` builds it as `transform.scale(scale)` on a JOML matrix, and
`scaleGeneric` multiplies `m00..m03` by `sx`, `m10..m13` by `sy`, `m20..m23` by `sz` - the **columns** - so
the composition is `R * S`: rotate first, then stretch along the element's own axes. `S * R` would stretch
along the block's axes and then turn the stretched shape, which puts the element's ends somewhere else
again. The whole transform is `origin + R * S * (v - origin)`, which is what `element_rescale` returns a
factor for and what the bake now applies before the variant's rotation.

Four tests hold it: the factor is `sqrt(2)`/`1`/`sqrt(2)` for the cross's turn, the real `tinted_cross`
element is 14.4/16 of a block wide with the rescale and 10.18/16 without it, the ends lie along the turned
`x` axis (which is what separates `R * S` from `S * R`), and the guard - absent flag, quarter turn, identity -
is one on every axis.

### A face that writes no `uv` was covering the whole sprite, and that is not what a slab's side is

A model face may leave `uv` out, and what the game does then is `FaceBakery#defaultFaceUV` - it derives
the rectangle from the **element's own box**, six lines, one per facing:

```java
case DOWN  -> new UVs(from.x(), 16.0F - to.z(), to.x(), 16.0F - from.z());
case UP    -> new UVs(from.x(), from.z(), to.x(), to.z());
case NORTH -> new UVs(16.0F - to.x(), 16.0F - to.y(), 16.0F - from.x(), 16.0F - from.y());
case SOUTH -> new UVs(from.x(), 16.0F - to.y(), to.x(), 16.0F - from.y());
case WEST  -> new UVs(from.z(), 16.0F - to.y(), to.z(), 16.0F - from.y());
case EAST  -> new UVs(16.0F - to.z(), 16.0F - to.y(), 16.0F - from.z(), 16.0F - from.y());
```

This side was using `[0, 0, 16, 16]` - the whole sprite - for every face without one, which is right for
exactly one shape: a full cube. Everything else that is part of a block and samples a sprite the game cut
for it was drawing the texture at the wrong **scale**. A bottom slab's side is half a block tall and its
sprite is not, so it drew the whole sprite squeezed into half a block; a pane drew a full block of glass
in the middle of an empty one; a plant's cross drew its four quads all sampling everything.

`default_face_uv` is the six lines above, evaluated for the element's `from`/`to`, and a face gets it when
it writes no `uv` of its own (`face_data`, so every face that is looked up goes through it). The four
corners come out 180° round from the game's own version of them, and that is not a bug: this side's
vertices are in the opposite order from the game's, and reversing a quad turns both of its texture axes.
Both versions agree on every full cube - where the mirroring cancels - which is exactly why the difference
survived this long.

Two tests hold it down. `a_face_without_a_uv_covers_the_element_and_not_the_sprite` pins the six answers
for a bottom slab and for a post, where every one of the six is different; and
`a_slab_side_covers_half_the_sprite` follows one all the way to the vertex: a bottom slab's side samples
half the sprite, in **texel coordinates the vertex buffer actually holds**, rather than the whole of it.

`ElementBounds` is what carries the element's `from`/`to` into `face_data` in the sixteen units a model
file writes them in, because that is the space the game's six lines are written in.

### A bare texture name in a face is a variable, and the model schema did not know it

`block/heavy_core` names its texture with **no `#`**:

```json
"textures": { "all": "block/heavy_core", "particle": "block/heavy_core" },
"faces": { "north": { "uv": [0, 8, 8, 16], "texture": "all" }, ... }
```

The game resolves both spellings, and `minecraft_assets::api::resolve::ModelResolver` only answered for a
`#reference` - so the bare `all` was taken for a sprite *called* `all`, `ResourcePath::from("all")` became
`minecraft:all`, the atlas had no such sprite, and all six faces were dropped. `unresolved_texture` could
not catch it either, because `Texture::reference()` is `None` for a name with no `#`.

The symptom was a block that is baked, keyed, culled against its neighbours and drawn - and invisible, with
the only trace a line in the missing-sprite report that reads `6 face(s) were dropped for a sprite the atlas
does not have: minecraft:all`. **It is the worst shape a failure can have here**, which is why the fix is in
the vendored copy rather than worked around: `resolve_element_textures` now looks a bare name up in the
`textures` map, and **leaves it alone when it is not a key** - which is what the game does, since it is then
a sprite path and an absent sprite path is dropped exactly as before. Two tests, one for each half.

That report is now empty. What remains beside it is `3 texture(s) a model names could not be read`, which is
`minecraft:textures/missingno.png` - the game's own deliberate placeholder - and the list of states that bake
to no faces at all, which is `air`, the fluids, the beds and the signs and is not a fault.

### Two crashes, and both were a number that had to agree with the shader

- **`SectionPosition` and its immediate had different sizes, and wgpu aborts rather than drawing.** An
  `our_pixel_size: vec2<f32>` was added to the struct for the game's own sampling functions and the Rust
  side was left writing 48 bytes where the shader declared 52:

  ```text
  wgpu error: Validation Error
  Not all immediate data required by the pipeline has been set via set_immediates
  (missing byte ranges: 48..52)
  thread caused non-unwinding panic. aborting.
  ```

  The field turned out to be redundant - it is `texel_ours`/`texel_game`, which were already there - so it
  was removed and the shader builds `pixel_size` from those. **And the size is now asserted**:
  `the_shaders_section_position_is_the_size_the_immediate_declares` asks `naga` for the struct's own byte
  count and compares it with `immediate_size_of("@pc_section_position")`, for both terrain shaders. That
  invariant had never been checked, and its failure mode is a process that stops on the first frame rather
  than a colour that is wrong - it caught the *second* wrong value within a minute of being written.

- **A fluid face's level-of-detail floor was always zero, and the field that would have carried it did not
  exist.** `FluidSprite` had `atlas`, `game` and `animated`; the block-model path takes `rect`, `animated`
  *and* `level_cap` out of `SpriteInAtlas` in one go. So `sprite_level_floor(None, true)` answered zero, the
  shader's bias stayed at the setting's own value, and a fluid face went back to the sampler's automatic
  level choice - which is what the old `-4` offset had been standing in for.

  The order of events is the part worth keeping: setting `UV_ANIMATED` on fluid faces is what *enabled* the
  shader's floor branch for them, so it also enabled the branch that reads that missing field. Before it the
  branch was dead and the field cost nothing; after it, the field was the whole difference between a floor
  and none. Reported by counters that did not exist either - `FLUID_FACES_ANIMATED` read as healthy for
  every fluid in the world while the floor was zero, so a second pair was added beside it
  (`FLUID_FACES_FLOORED` / `FLUID_FACES_UNFLOORED`) and the reading is now `0` unfloored out of hundreds of
  thousands.

### The game's own terrain sampling, ported

`GameRenderer` sets one flag - `options.textureFiltering == TextureFilteringMethod.RGSS` - and the game's
`terrain.fsh` is `UseRgss == 1 ? sampleRGSS(...) : sampleNearest(...)`. **Neither can be expressed with a
sampler**, so both are ported into `terrain.wgsl` and `terrain_solid.wgsl`, and the three settings are three
algorithms rather than one with a knob:

| setting | the game | this side now |
| --- | --- | --- |
| `NONE` ("Fast") | isotropic, hardware level | the same |
| `RGSS` ("Fancy") | four taps on the game's rotated grid, at a level from **`sqrt(min * max)`** of the two derivative lengths | the same, ported |
| `ANISOTROPIC` ("Fabulous") | `anisotropy_clamp = Options#maxAnisotropyValue`, which is `1 << maxAnisotropyBit` - **4 by default** | the same; it used to be a flat 16 |

The geometric mean is the part that matters and the reason a bias never fixed anything permanently: the
hardware's implicit level comes from the *worst* of the two derivatives, which at a grazing angle is the
compressed one, so a surface seen edge-on is sampled several levels coarser than its footprint needs - and a
moving sprite's coarse levels are a running average of its animation. A player measured four levels' worth
of it, twice, on two builds.

The port also made the uniformity test stronger: the shaders now take every level explicitly
(`textureSampleGrad` and `textureSampleLevel`), so
`the_terrain_shaders_never_sample_a_texture_under_a_branch` asserts **zero** auto-level fetches where it used
to assert exactly six.

### A model face can turn shading off, and the schema had no such field

`BlockModelLighter` is

```java
outputInstance.scaleColor(quad.materialInfo().shade()
    ? cardinalLighting.byFace(direction)
    : cardinalLighting.up());
```

and this side only had the first branch. The flag was added to the vendored `minecraft-assets` (the second
reason that copy exists), carried on `FaceData` and `BlockModelFace`, and consumed by
`face_brightness(dir, shade)`. **51 models in the shipped assets say `"shade": false`** - `cross`, `crop`,
`vine_*`, `template_torch`, `template_fire_*`, `template_lantern`, `coral_fan`, `sea_pickle`,
`redstone_dust_*`, `tripwire_*`, `bamboo_*`, `ladder`, `chain` - and every one of them was being multiplied
by its direction's brightness where the game multiplies by 1.

`CardinalLighting` came with it: it is a **dimension** property and not a biome one
(`ClientLevel#cardinalLighting`), there are exactly two tables, and they differ **only at up and down**
(`0.5/1.0` against `0.9/0.9`). The six numbers travel from the JVM as six floats on every bake, in
`Direction`'s own order rather than the record's - which is not the same order, and the mapping is spelled
out at the call site.

### Known gaps

- **A grass block's side reads brighter than the game's, and it is not the tint, the light, the sprite
  rectangle or the sampling - twelve candidates were each measured and each came back correct.** The
  measurements, so nobody repeats them:

  - **The tint is right, end to end.** The bake was made to report a tinted face's whole colour chain:
    `tint #91BD59`, brightness `0.6`, vertex `(87, 113, 53)` - and `0.6 * (145, 189, 89)` is exactly
    `(87, 113, 53)`, channel for channel. The tint that arrives equals the biome's
    `BiomeColors#getAverageGrassColor` at that position, and equals that biome's base colour (the position
    was a taiga, whose `GrassColorModifier` is `NONE`; the only two that change anything are
    `DARK_FOREST`, which halves, and `SWAMP`, which returns a constant).
  - **The light is right.** The *dirt* of the same side is pixel-identical in both renderers
    (`4a4a4a`), and the non-tint path - atlas contents, sampling, colour pipeline - is what that
    traverses.
  - **The sprite rectangle is right.** `TextureAtlasSprite#u0` is `(x + padding) / atlasWidth`, so the
    rectangle is the content and not the padding, and this side sends those four numbers verbatim.
  - **The sampling is right, or at least is not the difference.** An `atlas_lod_bias` of `-4` and of `0`,
    the per-sprite floor on and off, the game's own RGSS with its geometric-mean level, the anisotropic
    sampler, the half-texel shift, the mip chain, and the colours at two distances all produced the same
    side.
  - **And the overlay quad is not the answer, which is the part that cost the most time.** The side looks
    *unchanged* when that quad is dropped at bake time (16 faces, counted) and unchanged when `TINT` is
    switched off - because `grass_block_side_overlay.png` has content in **one or two rows at the top and
    is fully transparent below**, so its visible contribution is a couple of pixels of fringe. The green on
    a grass side comes from `grass_block_side.png` itself, which has a green edge painted into it.
    Measured directly: the base sprite's `y=1` is `(108, 172, 66)` and `y=4` down is dirt, while the
    overlay's `y=1` is grey `(140, 140, 140)` at `a=255` and **everything from `y=4` is `a=0`**.

  So the difference is in the *texture read* of `grass_block_side` itself, and a player's test against an
  older build shows it is not new: **it has been in this pipeline throughout**. What would settle it is a
  way to read back the texel a fragment actually fetched, which this side has no path for.

- **Fluids animate, stand at the right height, and turn their surface with the flow; it is still not the
  game's surface.** See "The fluid faces" above: a fluid's faces sample the game's atlas like everything
  else, the offsets are the game's own, a source block is `8/9` of a block rather than a full cube, and a
  moving surface is a quarter of the flowing sprite turned by `getFlow`. What is still different is the
  weighted corner average, the hidden faces vanilla culls, and water's **colour** - one constant here
  against a biome function in the game. See "The water was baked and never drawn" for the layer and the
  pass, and the three bullets above it for what is left.

- **A resource reload is followed now, with one caveat.** See "A resource reload reloads now" above: the
  atlas, the models and the arena are all rebuilt against the new pack, and the sections are re-meshed
  over the couple of seconds after the game's own reload screen closes. What is *not* rebuilt is the
  entity path, which is Minecraft's own and reloads itself.

- **A sprite is classified as a whole, not per face.** The layer table `Atlas::sprite_layer` fills in
  is one answer per sprite, while the game asks `SpriteContents#computeTransparency(u0, v0, u1, v1)`
  over the rectangle each face samples. For the blocks this was written for - ice, leaves, glass, a
  plant - the whole sprite is one kind of thing throughout and the two agree; for a sprite that is
  part opaque and part cutout, a face that only samples the opaque part is put in the cutout layer as
  well. That is the safe direction (Minecraft's own pass draws it, correctly), and closing it needs
  the face's UV rectangle handed down to the atlas rather than the sprite's name.

- **The vertex position is on a 1/16-block grid, and a model can ask for less than that.** Eight bits
  an axis plus one flag holds 0..16 blocks in sixteenths, and the leaf litter's 1/64 is not on it -
  see "A face between the lines of the vertex format", where rounding up is what keeps it off the
  ground's plane. What rounding cannot fix is geometry *thinner* than 1/16: a 1/64-thick plate's two
  faces both land on the line above, so it is drawn as nothing rather than as a plate. The honest fix
  is two bytes an axis at 1/256 (six bytes of position instead of three, a `u16` decode in
  `shaders/terrain.wgsl`, and 16 → 20 bytes a vertex, which is a quarter more arena for every section
  in the world), and it is worth doing when a block that is thinner than a sixteenth actually matters.

- **The Fabric module's C header is a snapshot from before the 26.1 work.** `fabric/src/main/wgpu-mc.h`
  is what the Fabric backend's `jextract` bindings are generated from, and it is not
  `rust/wgpu-mc-jni/bindings.h`: it still declares the exports this session deleted (`dummy`,
  `thing`, `unmap_buffer`, `drop_buffer_view`, `extract_directives`) and older signatures for the
  ones that stayed (`create_texture_view` without the mip range, `create_render_pass` without the
  encoder handle). A Fabric build against the current cdylib would resolve handles that are no
  longer there, so migrating that module starts with regenerating its header from `bindings.h`.

- **A texture view can outlive its texture.** A long session ended with `In Texture::create_view —
  Texture with 'FBO 2 / Depth' label is invalid`: Minecraft rebuilt a render target - a resize does
  that - closed the old depth texture, and something still asked for a view of it. The JVM side used
  to hand the freed pointer to `create_texture_view`, and wgpu answers that with a validation error,
  which ends the process. `WgpuDevice#createTextureView` now checks `isClosed` first, logs whose
  texture it was, and hands back a view of a placeholder of the same format *and size*: one frame of
  the wrong content is a far better outcome than losing the session. The Rust side refuses a dropped
  pointer the same way, from the tombstone the drop recorded. The caller that keeps a view past its
  texture is still out there, and the log line names it.
- **The swapchain refuses an image** (`get_current_texture` returning `Validation`) when the window
  has been through something the configuration does not survive - a monitor change, a fullscreen
  toggle, a long idle. That used to be logged and dropped, frame after frame, for minutes on end.
  It now takes the same recovery path as `Outdated` and `Lost`: reconfigure from the stored size and
  present mode - forcibly, since an unchanged configuration is what the recovery is there to
  replace - and try once more, with the log line rate-limited to one every 120 frames.
- **The expansions are exercised by the sky and little else.** `TRIANGLE_FAN` and, for non-indexed
  draws, `QUADS` go through generated index buffers. The sky disc is the fan that matters and it is
  drawn every frame; the stars and the sun and moon are drawn from Minecraft's own quad index
  buffer and never take that path. `POINTS` and `DEBUG_LINE_STRIP` have no wgpu equivalent drawn
  here at all, because nothing in a normal run draws them.
- **Every `CommandEncoder` shares one native encoder.** That is what fixed the leak, and it is a
  deliberate assumption: recording happens on the render thread in call order, and wgpu allows one
  pass at a time, so the sharing is invisible. Two passes open at once - Minecraft holding two
  encoders each with a pass in flight - would be a wgpu validation error, and none has been seen in
  a run; if one ever appears, the layout is a per-encoder encoder with a lifetime this side cannot
  get from Blaze3D.

- **`setViewport` is not forwarded.** 26.1 never calls it - a full run with the diagnostics on logs
  no viewport request at all - so the default viewport is always in use. The default is the right
  one now that views carry their mip range: wgpu sizes the render area from the attachment's base
  mip level, which is what makes the per-level atlas passes render at 1/2, 1/4, 1/8 of the atlas
  rather than all at full size. A pass that does set a viewport would still be ignored, and would
  need the same clamping that the scissor rectangle gets.
- **A region clear keeps the colour and loses the depth.** `clear_color_and_depth_textures_region`
  now writes the clear colour into the rectangle the caller asked for, but clears the *whole* depth
  attachment, because a load op cannot be scissored. That is exact for the one caller there is - the
  GUI item atlas' depth is never sampled - and wrong for a caller that needs the depth around its
  region kept. Doing it properly needs a scissored depth draw, i.e. a pipeline of its own. See "A
  scissored clear is not a load op".
- **`drawMultipleIndexed` replays its draws** instead of batching them, because there is no
  indirect-draw entry point in the ABI. Correct, just less batched.
- **Every draw builds its own bind groups.** This was true, and is no longer: dynamic offsets are on
  by default and the cache answers 99.96% of draws - see "The cache was keyed on the wrong address".
  What is left is the *first* draw of every distinct binding set, and a set that never repeats is
  still a `wgpu::BindGroup` of its own.
- **Timestamp queries and fences** are stubs until Rust exposes them.
- **`clearStencilTexture` is a no-op.** Nothing in 26.1's main paths calls it, and the depth
  attachments the port creates have no stencil to clear.
- **Two pipelines make naga warn about `@invariant`.** `Vertex shader with entry point main outputs
  a @builtin(position) without the @invariant attribute and is used in a pipeline with Equal` - the
  vertex stage is translated from GLSL by naga's frontend, which does not emit the attribute, and
  the pipelines that compare with `EQUAL` are the ones that care. On some drivers that can show as
  z-fighting between two passes that should match exactly; none was visible in the runs so far, and
  adding the attribute would mean post-processing naga's output rather than the GLSL.
- **Some pipelines declare samplers their shaders use, and some do not.** The run logs
  `the shader declares uniform Sampler0_wm_texshim, which the pipeline does not provide; giving it
  binding 5, which the pipeline layout will not have` at debug level - the same class of mismatch as
  `Globals`, in the sampler shim rather than the uniform one. It is not fatal, because naga drops the
  unused globals, but it means the pipeline cannot bind a sampler the shader may want. The two shim
  names are this side's own (`shim_samplers` splits a combined sampler in two), which is why they are
  no longer reported as an error: four alarming lines in every startup log for shaders that render
  fine.
- **A `ByteBuffer` upload in any format other than RGBA is stored as if it were RGBA.** That
  overload is handed a `NativeImage.Format` the native side is never told about; the port logs an
  error when it sees anything but `NativeImage.Format.RGBA` rather than corrupting the texture
  quietly. Only RGBA has ever been observed in a run.
- **Window resizing is exercised, but only on DX12 so far.** Three resizes in a row - including one
  that asked for a window taller than the screen - and a maximise to 2560x1334 with a world loaded
  both kept presenting, `acquired=true` on 5280 consecutive frames. The resize path is also what
  produces every use-after-close this side guards against, so it is the test that matters most; a
  Vulkan run of the same sequence is still owed.
- The remaining access-transformer entries were each re-verified against the 26.1 bytecode; entries
  whose target no longer exists are kept as `# REMOVED:` notes so a future rebase can tell
  "checked, gone" apart from "not checked yet".
