//! PIX captures, through the libraries PIX ships.
//!
//! PIX is Microsoft's D3D12 debugger. A *timing capture* records what the title did - GPU work,
//! PIX GPU events, API markers, and optionally CPU samples and callstacks - and PIX reads it back
//! on the machine that took it. It is normally started from the PIX UI, either by launching the
//! game through PIX or by attaching to a running one; a title can also ask for one itself:
//! `PIXBeginCapture(PIX_CAPTURE_TIMING, params)` starts it and `PIXEndCapture` finishes it.
//!
//! Both ways need PIX's own libraries *inside this process*, and they are not the same library:
//!
//!  * `WinPixGpuCapturer.dll` hooks D3D12 so PIX can attach for a GPU capture. It has to be loaded
//!    **before the first D3D12 call** - before the device exists - or PIX refuses to attach with
//!    "this process has not loaded WinPixGpuCapturer.dll", which is a thing that actually happened.
//!    [`load_capturers`] is called from the device creation path for exactly that reason.
//!  * `WinPixTimingCapturer.dll` is what a programmatic timing capture runs through.
//!
//! Neither is part of Windows, and neither is in the WinPixEventRuntime package: they come with
//! PIX, in `C:\Program Files\Microsoft PIX\<version>`, which is where [`pix_dll`] looks for the
//! newest of them - the same search `PIXLoadLatestWinPixGpuCapturerLibrary()` does in `pix3.h`, a
//! header function this is not C++ enough to call. `WinPixEventRuntime.dll` *is* the package's, and
//! is looked for beside the game first; PIX's own `WinPixEventRuntime_OneCore.dll` stands in for it,
//! since its `PIXBeginCapture2` is documented as equivalent.
//!
//! Everything is loaded on demand and every failure says what was missing. Linking against any of it
//! would make the mod fail to load on every machine without PIX installed.
//!
//! Everything here is Windows-only. The capture is a no-op elsewhere.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

/// The capture type flags of `pix3.h`. They are flags, not an enumeration: a `DWORD` with one bit
/// set, and `PIX_CAPTURE_TIMING` is the system timing capture this asks for.
const PIX_CAPTURE_TIMING: u32 = 1 << 0;

/// `PIXCaptureStorage::Memory`: fill the tooling memory, then stop. The other mode is a ring buffer
/// for captures that run for a long time, which a switch-driven capture does not need.
const PIX_CAPTURE_STORAGE_MEMORY: u32 = 0;

/// What `PIXBeginCapture` and `PIXEndCapture` answer on success.
///
/// `pix3.h` documents `S_FALSE` for `PIXBeginCapture`, and that is what this side saw for as long as
/// it called PIX's runtime through `PIXBeginCapture2`. The newer PIX on this machine (2603.25)
/// answers plain `S_OK` instead - which the code below read as a refusal, logged as
/// `HRESULT 0x00000000`, and then left the capture it had just started running with nothing counting
/// its frames. The question both calls are really asking is `SUCCEEDED(hr)`, and that is a sign test:
/// every failure these two can return (`E_ACCESSDENIED`, `E_PENDING`, `E_INVALIDARG`, `E_FAIL`) has
/// the high bit set.
fn succeeded(result: i32) -> bool {
    result >= 0
}

/// How many frames a capture records before it stops itself.
///
/// A programmatic capture fills the tooling memory it is given and then discards what comes after,
/// so leaving one running for a session records the wrong thing and holds gigabytes while it does
/// it. Ten seconds of a world is what a timing capture is for; the switch can be turned on again
/// for another one, which writes the next file.
const CAPTURE_FRAMES: u32 = 600;

/// How often the CPU is sampled, in samples per second, while a capture runs.
///
/// The rate Microsoft's own programmatic-timing-capture example passes. It is a whole-process
/// sample, so the number is a compromise: 4 kHz is fine enough to attribute a frame's CPU time to a
/// function and coarse enough that 600 frames stay a few hundred megabytes.
const CPU_SAMPLES_PER_SECOND: u32 = 4000;

/// What the `pix capture` switch asks for, and what is actually happening.
///
/// Two states rather than one because the switch is applied during mod construction - before the
/// logger exists and before there is a frame to capture - and because a capture ends by itself: the
/// request is recorded there and acted on at a frame boundary, which is also where the answer can
/// be logged.
static WANTED: AtomicBool = AtomicBool::new(false);

/// `Idle`, `Running` or `Finished`: see [`tick`].
static STATE: AtomicU8 = AtomicU8::new(IDLE);

/// The frames the running capture has seen.
static FRAMES: AtomicU32 = AtomicU32::new(0);

const IDLE: u8 = 0;
const RUNNING: u8 = 1;
/// A capture ran to its frame limit. It stays here until the switch is turned off, so that a
/// finished capture does not immediately start another one.
const FINISHED: u8 = 2;

/// The `PIXCaptureParameters` union from `pix3.h`, in its timing-capture shape.
///
/// Written out by hand because the header is not part of any SDK this crate builds against. It was
/// checked against the real one afterwards - `Include/WinPixEventRuntime/pix3.h` from the
/// WinPixEventRuntime package on nuget.org, version 1.0.240308001, which is the newest Microsoft has
/// published - and every field below is in that header's order and of its type. Microsoft's own
/// reference for the same shape is the GDK page for `PIXCaptureParameters`.
///
/// The tail is the part the header does not describe. PIX's runtime and capturer are versioned
/// separately from this crate - the one on this machine is years newer than the header - and a
/// capture API that takes a pointer to a struct has no way to be told how long it is: a newer PIX
/// reading a field this crate has never heard of would read whatever happens to be on the stack.
/// [`PixCaptureParameters::reserved`] is there so that it reads zeroes instead, which is what "off"
/// means for every `BOOL` in this struct.
#[repr(C)]
struct PixCaptureParameters {
    file_name: *const u16,
    maximum_tooling_memory_size_mb: u32,
    capture_storage: u32,
    capture_gpu_timing: i32,
    capture_callstacks: i32,
    capture_cpu_samples: i32,
    cpu_samples_per_second: u32,
    capture_file_io: i32,
    capture_virtual_alloc_events: i32,
    capture_heap_alloc_events: i32,
    capture_xmem_events: i32,
    capture_pix_mem_events: i32,
    capture_page_fault_events: i32,
    capture_video_frames: i32,
    reserved: [u64; 16],
}

type BeginCapture = unsafe extern "system" fn(u32, *const PixCaptureParameters) -> i32;
type EndCapture = unsafe extern "system" fn(i32) -> i32;

struct PixRuntime {
    begin: BeginCapture,
    end: EndCapture,
}

/// Loads PIX's capturers into this process, before anything has created a D3D12 device.
///
/// Called from `try_create_renderer`, ahead of the wgpu instance: `WinPixGpuCapturer.dll` hooks
/// D3D12 as it is loaded, and a process that loads it after the device exists is a process PIX
/// refuses to attach to - "the process has not loaded WinPixGpuCapturer.dll" is the message, and it
/// is what the switch exists to prevent.
///
/// The libraries are kept loaded for the life of the process: unloading a capturer that has already
/// hooked D3D12 would leave the hooks pointing at nothing.
pub fn load_capturers(enabled: bool) {
    if !enabled {
        return;
    }

    // The one case where loading PIX's capturer is worse than not loading it. RTSS hooks D3D12 too,
    // and the two layers together have crashed this game twice in the same place - an access
    // violation inside D3D12Core reached through RTSS's own present hook, on the render thread in
    // `blitAndPresent`. Twice with the same fault address, and once of those runs had *no capture
    // running at all* (it was refused for want of elevation), which is why this is decided before
    // anything is loaded rather than warned about afterwards: it is the capturer being in the
    // process, not a capture, that the game does not survive.
    //
    // Refusing costs the feature; loading costs the process. The way back is to take the game out of
    // RTSS's hands, and it is per application: an RTSS profile for the `java.exe` the game runs as,
    // with Application detection level `None`, or RTSS/Afterburner closed.
    if let Some(module) = rtss_module()
        && !marker_exists(RTSS_OVERRIDE_MARKER)
    {
        report_capturer(
            Report::Error,
            format!(
                "wgpu-mc: PIX: {module} is loaded into this process, so RivaTuner Statistics Server \
                 - MSI Afterburner's on-screen display - is already hooking D3D12. PIX hooks the same \
                 runtime, and the two hook chains crash this game inside RTSS's present hook (an \
                 access violation in D3D12Core.dll reached through RTSSHooks64.dll, on the render \
                 thread in `blitAndPresent`, at the same address every time) whether or not a capture \
                 is running. PIX's libraries are therefore NOT loaded this launch and the `pix \
                 capture` switch does nothing. Add the `java.exe` this game runs as to RTSS's profile \
                 list and set its Application detection level to None, or quit RTSS/Afterburner, then \
                 start the game again. To load PIX anyway - once RTSS and PIX are known to coexist - \
                 create a file named `{RTSS_OVERRIDE_MARKER}` next to the game."
            ),
        );
        return;
    }

    load_capturer("WinPixGpuCapturer.dll", "PIX can attach for a GPU capture");
    load_capturer(
        "WinPixTimingCapturer.dll",
        "programmatic timing captures can start",
    );

    CAPTURERS_LOADED.store(true, Ordering::Relaxed);

    // Said here rather than only when a capture is refused, because the two halves of this switch
    // have different requirements: attaching for a GPU capture works in any process, and a timing
    // capture - which is what the switch takes when it is flipped - records through ETW providers
    // and needs an elevated one. An unelevated launch is the common case *even when the terminal was
    // started as administrator*: Gradle reuses a daemon started without elevation, and the game is
    // forked by that daemon, so it inherits the daemon's token. See [`is_elevated`].
    if !is_elevated() {
        report_capturer(
            Report::Warning,
            "wgpu-mc: PIX: this process is not running elevated, so the timing capture the \
             `pix capture` switch takes will be refused (it records through ETW providers, and \
             creating those sessions needs administrator). PIX can still attach to this process for \
             a GPU capture. Starting `gradlew` from an administrator terminal is not enough on its \
             own - Gradle reuses a daemon started without elevation, and the game is forked by that \
             daemon, so it inherits that token - so run `gradlew --stop` first, or pass \
             `--no-daemon`."
                .to_string(),
        );
    }
}

/// Whether PIX's capturers are in this process, which is what a capture needs and what the refusal
/// above leaves false.
static CAPTURERS_LOADED: AtomicBool = AtomicBool::new(false);

/// The marker that loads the capturers even with RTSS in the process.
///
/// A judgement call this side makes about somebody else's bug needs a way to be overruled without a
/// rebuild, and every other switch in this renderer already has one.
const RTSS_OVERRIDE_MARKER: &str = "wgpu-pix-with-rtss";

/// Whether a marker file is present, in the working directory or next to the game directory.
fn marker_exists(name: &str) -> bool {
    std::path::Path::new(name).exists()
        || crate::RUN_DIRECTORY
            .get()
            .map(|directory| directory.join(name).exists())
            .unwrap_or(false)
}

/// The module RivaTuner Statistics Server injects into a game it overlays.
///
/// RTSS ships a 32-bit and a 64-bit hook; an x64 game gets the second one. Looking for the module is
/// the only honest way to ask whether the overlay is in *this* process: RTSS decides per application
/// whether to inject, and its own settings are not readable from here.
#[cfg(windows)]
fn rtss_module() -> Option<&'static str> {
    use winapi::um::libloaderapi::GetModuleHandleW;

    ["RTSSHooks64.dll", "RTSSHooks.dll"]
        .into_iter()
        .find(|name| {
            let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();

            // Safety: the name is NUL-terminated and lives for the call.
            !unsafe { GetModuleHandleW(wide.as_ptr()) }.is_null()
        })
}

#[cfg(not(windows))]
fn rtss_module() -> Option<&'static str> {
    None
}

/// Whether this process runs with an elevated token, which is what a PIX timing capture needs.
///
/// Asked once and remembered: the token does not change while the game runs, and the answer is part
/// of every message about a capture, because "not elevated" and "elevated and still refused" are
/// different problems with different answers.
#[cfg(windows)]
fn is_elevated() -> bool {
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::processthreadsapi::{GetCurrentProcess, OpenProcessToken};
    use winapi::um::securitybaseapi::GetTokenInformation;
    use winapi::um::winnt::{TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};

    static ELEVATED: once_cell::sync::OnceCell<bool> = once_cell::sync::OnceCell::new();

    *ELEVATED.get_or_init(|| {
        let mut token = std::ptr::null_mut();
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut returned = 0u32;

        // Safety: the token belongs to this process, it is closed again, and the buffer passed in is
        // the one the length describes.
        unsafe {
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return false;
            }

            let answered = GetTokenInformation(
                token,
                TokenElevation,
                &mut elevation as *mut TOKEN_ELEVATION as *mut _,
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut returned,
            );

            CloseHandle(token);

            answered != 0 && elevation.TokenIsElevated != 0
        }
    })
}

#[cfg(not(windows))]
fn is_elevated() -> bool {
    false
}

/// What [`load_capturers`] found, for the first [`tick`] to print.
///
/// The loading happens while the device is being created, and this crate's logger is installed by
/// the JVM *after* that - a line logged there is never seen. So the line is kept here and printed
/// from the first frame instead, which is also the first place a log line can be read.
static CAPTURER_REPORT: parking_lot::Mutex<Vec<(Report, String)>> =
    parking_lot::Mutex::new(Vec::new());

/// How loud a line from the capturer loading is, decided while there is no logger to ask.
///
/// A missing library is a failure; an unelevated process is a warning, because the switch also buys
/// something that works without it - PIX can attach for a GPU capture either way.
#[derive(Clone, Copy)]
enum Report {
    /// Constructed by the Windows half of `load_capturer` only, so a build for anything else has the
    /// variant and nothing that names it - which is what `dead_code` is looking at.
    #[cfg_attr(not(windows), allow(dead_code))]
    Info,
    Warning,
    Error,
}

fn report_capturer(level: Report, line: String) {
    CAPTURER_REPORT.lock().push((level, line));
}

/// Prints what the capturer loading found, once there is somewhere to print it.
fn report_capturers_once() {
    for (level, line) in CAPTURER_REPORT.lock().drain(..) {
        match level {
            Report::Info => log::info!("{line}"),
            Report::Warning => log::warn!("{line}"),
            Report::Error => log::error!("{line}"),
        }
    }
}

/// Loads one of PIX's capturers, and says what it is for.
fn load_capturer(file_name: &str, purpose: &str) {
    #[cfg(windows)]
    {
        let path = pix_dll(file_name);

        let Some(path) = path else {
            report_capturer(
                Report::Error,
                format!(
                    "wgpu-mc: {file_name} was not found in {PIX_INSTALL_ROOT}\\<version>, so \
                     {purpose} will not work; install PIX on this machine, or turn the `pix capture` \
                     switch off"
                ),
            );
            return;
        };

        // Safety: loading a DLL and never unloading it. The path comes from the PIX installation.
        let module = unsafe { load_library(&path.to_string_lossy(), true) };

        if module.is_null() {
            report_capturer(
                Report::Error,
                format!(
                    "wgpu-mc: {file_name} could not be loaded from {}, so {purpose} will not work",
                    path.display()
                ),
            );
            return;
        }

        report_capturer(
            Report::Info,
            format!("wgpu-mc: PIX: {} loaded, so {purpose}", path.display()),
        );
    }

    #[cfg(not(windows))]
    {
        let _ = (file_name, purpose);
    }
}

/// The loaded runtime, or `None` if this machine has no usable PIX.
///
/// Loaded once: the DLLs either exist or they do not, and a capture that failed for a missing DLL
/// will fail again.
fn runtime() -> Option<&'static PixRuntime> {
    static RUNTIME: once_cell::sync::OnceCell<Option<PixRuntime>> =
        once_cell::sync::OnceCell::new();

    RUNTIME
        .get_or_init(|| {
            #[cfg(windows)]
            {
                // Safety: loading DLLs and taking function pointers out of them. Every pointer is
                // resolved by name and checked for null before it is called, and no library is ever
                // unloaded - the runtime has to stay mapped for the life of the process, since
                // `PIXEndCapture` can be called long after the load.
                unsafe { load_windows_runtime() }
            }

            #[cfg(not(windows))]
            {
                None
            }
        })
        .as_ref()
}

#[cfg(windows)]
unsafe fn load_windows_runtime() -> Option<PixRuntime> {
    use winapi::um::libloaderapi::GetProcAddress;

    // A programmatic *timing* capture needs both halves, and PIX ships only one of them:
    //
    //  * `WinPixTimingCapturer.dll` comes with PIX and has to be loaded into the process, which is
    //    what `PIXLoadLatestWinPixTimingCapturerLibrary()` in `pix3.h` does - that helper is a
    //    header function, so the search is written out in `pix_dll` here;
    //  * `WinPixEventRuntime.dll` is where `PIXBeginCapture` itself lives, and it comes from the
    //    WinPixEventRuntime package rather than from PIX, so it has to be beside the game (or on
    //    PATH). PIX's own install ships the OneCore build of the same runtime instead, which
    //    exports `PIXBeginCapture2` - the documented equivalent of `PIXBeginCapture`.
    // Safety: loading PIX's libraries into this process and keeping them; see `load_capturers`.
    let capturer = unsafe { load_timing_capturer() };

    // Safety: as above.
    let Some((module, name)) = (unsafe { load_event_runtime() }) else {
        log::error!(
            "wgpu-mc: PIX capture was asked for, but no WinPixEventRuntime.dll could be loaded. It \
             is not part of the PIX installer: put the one from the WinPixEventRuntime package next \
             to the game (or on PATH), and copy PIX's WinPixTimingCapturer.dll beside it"
        );
        return None;
    };

    if capturer.is_none() {
        log::error!(
            "wgpu-mc: PIX capture was asked for, and {name} is loaded, but no \
             WinPixTimingCapturer.dll was found next to the game or in {}\\<version> - a timing \
             capture needs PIX's capturer loaded into the process",
            PIX_INSTALL_ROOT
        );
    }

    // Safety: the module was just loaded, and both names are the ones `pix3.h` exports. The `2`
    // form is what PIX's own runtime build carries; the documentation calls the two equivalent.
    let begin = unsafe { GetProcAddress(module, c"PIXBeginCapture".as_ptr()) };
    let begin = if begin.is_null() {
        unsafe { GetProcAddress(module, c"PIXBeginCapture2".as_ptr()) }
    } else {
        begin
    };
    let end = unsafe { GetProcAddress(module, c"PIXEndCapture".as_ptr()) };

    if begin.is_null() || end.is_null() {
        log::error!("wgpu-mc: {name} has no PIXBeginCapture/PIXBeginCapture2 and PIXEndCapture");
        return None;
    }

    Some(PixRuntime {
        // Safety: the two symbols are exported with these signatures by every WinPixEventRuntime.
        // The casts name them because `GetProcAddress` can only hand back an `FARPROC` - one
        // `extern "system" fn()` - and the two entry points are not that.
        begin: unsafe {
            std::mem::transmute::<*mut winapi::shared::minwindef::__some_function, BeginCapture>(
                begin,
            )
        },
        end: unsafe {
            std::mem::transmute::<*mut winapi::shared::minwindef::__some_function, EndCapture>(end)
        },
    })
}

/// Where PIX installs itself, and the layout of that directory: one subdirectory per version.
#[cfg(windows)]
const PIX_INSTALL_ROOT: &str = r"C:\Program Files\Microsoft PIX";

/// Loads the event runtime, preferring the one beside the game over the one PIX ships.
#[cfg(windows)]
unsafe fn load_event_runtime() -> Option<(winapi::shared::minwindef::HMODULE, &'static str)> {
    for name in ["WinPixEventRuntime.dll", "WinPixEventRuntime_OneCore.dll"] {
        // Safety: loading a library by name and keeping it for the life of the process.
        let module = unsafe { load_library(name, false) };
        if !module.is_null() {
            log::info!("wgpu-mc: PIX: using {name}");
            return Some((module, name));
        }
    }

    // The OneCore build is in the PIX installation rather than anywhere the loader looks by
    // default, so it is tried by path.
    let path = pix_dll("WinPixEventRuntime_OneCore.dll")?;
    // Safety: as above, with a path that came from the PIX installation.
    let module = unsafe { load_library(&path.to_string_lossy(), true) };
    if module.is_null() {
        return None;
    }

    log::info!("wgpu-mc: PIX: using {}", path.display());
    Some((module, "WinPixEventRuntime_OneCore.dll"))
}

/// Loads PIX's timing capturer, which the runtime needs in-process to take a timing capture.
///
/// Returned as a raw module handle that is deliberately never freed: the capturer has to stay
/// loaded for as long as a capture might start.
#[cfg(windows)]
unsafe fn load_timing_capturer() -> Option<winapi::shared::minwindef::HMODULE> {
    if let Some(path) = pix_dll("WinPixTimingCapturer.dll") {
        // Safety: loading a library from the PIX installation and keeping it.
        let module = unsafe { load_library(&path.to_string_lossy(), true) };

        if !module.is_null() {
            log::info!("wgpu-mc: PIX: timing capturer {}", path.display());
            return Some(module);
        }
    }

    // Safety: as above, by name this time - beside the game or on `PATH`.
    let module = unsafe { load_library("WinPixTimingCapturer.dll", false) };
    if module.is_null() {
        return None;
    }

    log::info!("wgpu-mc: PIX: timing capturer beside the game");
    Some(module)
}

/// The newest PIX installation's copy of a DLL, if PIX is installed.
///
/// PIX lays itself out as `<root>\<version>\`, so the versions are compared as numbers rather than
/// as strings - `2603.25` is newer than `2506.3`, and only the second component is ever small.
#[cfg(windows)]
fn pix_dll(file_name: &str) -> Option<std::path::PathBuf> {
    let mut versions: Vec<(u32, u32, std::path::PathBuf)> = std::fs::read_dir(PIX_INSTALL_ROOT)
        .ok()?
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            if !path.is_dir() {
                return None;
            }

            let mut parts = path.file_name()?.to_str()?.split('.');
            let major = parts.next()?.parse().ok()?;
            let minor = parts.next().unwrap_or("0").parse().unwrap_or(0);
            let dll = path.join(file_name);

            dll.is_file().then_some((major, minor, dll))
        })
        .collect();

    versions.sort();
    versions.pop().map(|(_, _, dll)| dll)
}

/// `LoadLibraryW`, with the search order a side-by-side DLL needs.
#[cfg(windows)]
unsafe fn load_library(name: &str, by_path: bool) -> winapi::shared::minwindef::HMODULE {
    use winapi::um::libloaderapi::{LOAD_WITH_ALTERED_SEARCH_PATH, LoadLibraryExW};

    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();

    // `LOAD_WITH_ALTERED_SEARCH_PATH` when the path is known makes the DLL's own directory the
    // first place its dependencies are looked for, which is what PIX's capturer needs to find the
    // rest of the installation.
    //
    // Safety: `wide` is NUL-terminated and lives for the call.
    unsafe {
        LoadLibraryExW(
            wide.as_ptr(),
            std::ptr::null_mut(),
            if by_path {
                LOAD_WITH_ALTERED_SEARCH_PATH
            } else {
                0
            },
        )
    }
}

/// Starts or stops a PIX timing capture, following the `pix capture` debug switch.
///
/// Called when the settings are applied, which is on the render thread - the thread whose work the
/// capture is about.
pub fn set_capturing(enabled: bool) {
    WANTED.store(enabled, Ordering::Relaxed);
}

/// Follows the switch: starts the capture it asks for, stops the one it no longer wants, and stops
/// a capture that has recorded its [`CAPTURE_FRAMES`].
///
/// Called once per presented frame, so a capture starts and ends at a frame boundary - and so that
/// what PIX says about it is logged when there is a log to write to.
pub fn tick() {
    report_capturers_once();

    let wanted = WANTED.load(Ordering::Relaxed);
    let state = STATE.load(Ordering::Relaxed);

    match (wanted, state) {
        // The switch is on and nothing is running: take the capture it asks for.
        (true, IDLE) => {
            FRAMES.store(1, Ordering::Relaxed);
            STATE.store(if begin() { RUNNING } else { FINISHED }, Ordering::Relaxed);
        }
        // Still running: count frames, and stop at the limit rather than filling tooling memory.
        (true, RUNNING) => {
            if FRAMES.fetch_add(1, Ordering::Relaxed) + 1 >= CAPTURE_FRAMES {
                end();
                STATE.store(FINISHED, Ordering::Relaxed);
                log::info!(
                    "wgpu-mc: PIX timing capture reached its {CAPTURE_FRAMES} frames and stopped"
                );
            }
        }
        // The switch went off while a capture was running: stop it.
        (false, RUNNING) => {
            end();
            STATE.store(IDLE, Ordering::Relaxed);
        }
        // A finished capture is re-armed once the switch goes off, so turning it back on starts the
        // next one.
        (false, FINISHED) => STATE.store(IDLE, Ordering::Relaxed),
        _ => {}
    }
}

/// Starts a capture, and answers whether one is running.
fn begin() -> bool {
    if !CAPTURERS_LOADED.load(Ordering::Relaxed) {
        // The common way to arrive here is the switch being turned on in a running game: the
        // capturers are loaded while the device is created, which is what the setting's restart
        // notice is about. The PIX lines above only exist when the switch was already on at startup.
        log::error!(
            "wgpu-mc: PIX's capturers are not in this process, so there is nothing to capture with. \
             They are loaded while the D3D12 device is being created, which is why `pix capture` is \
             marked as needing a restart: leave the switch on and start the game again. If it was on \
             at startup, the PIX lines above say why they could not be loaded - no PIX on this \
             machine, or RivaTuner Statistics Server in this process"
        );
        return false;
    }

    let Some(file) = capture_file() else {
        log::error!("wgpu-mc: no run directory to write a PIX capture to");
        return false;
    };

    let result = call_on_its_own_thread({
        let file = file.clone();

        move || {
            let runtime = runtime()?;

            // The parameters take a wide string, so the UTF-16 buffer has to outlive the call.
            let mut name: Vec<u16> = file.to_string_lossy().encode_utf16().collect();
            name.push(0);

            let parameters = PixCaptureParameters {
                file_name: name.as_ptr(),
                // Both of these are ignored by PIX on Windows, and are here because the struct is
                // the same one the console API takes.
                maximum_tooling_memory_size_mb: 4096,
                capture_storage: PIX_CAPTURE_STORAGE_MEMORY,
                capture_gpu_timing: 1,
                // CPU sampling with callstacks, which is the other half of what a timing capture is
                // for: GPU timing alone says a frame took 5 ms, and these say which code was on the
                // CPU for it and which calls ended up waiting. 4 kHz is the rate Microsoft's own
                // example uses.
                capture_callstacks: 1,
                capture_cpu_samples: 1,
                cpu_samples_per_second: CPU_SAMPLES_PER_SECOND,
                // The memory half. Every one of these feeds a table in the capture that is empty
                // without it - `MemoryUsageSamples` needs the allocator events and the page faults,
                // `FileIORange` the file accesses - and they are all off unless asked for, which is
                // how a capture came back with no memory data at all.
                capture_file_io: 1,
                capture_virtual_alloc_events: 1,
                capture_heap_alloc_events: 1,
                // Xbox only, and reads as a switch the console header names but the PC one keeps in
                // the same place: left off.
                capture_xmem_events: 0,
                // Allocations made through PIX's own allocator API. This renderer does not use it,
                // so nothing is recorded for it either way; on is what a capture with "custom
                // allocator events" ticked asks for, and it costs nothing when nothing calls it.
                capture_pix_mem_events: 1,
                capture_page_fault_events: 1,
                // Xbox only, same as XMem.
                capture_video_frames: 0,
                // Never read by the header this crate knows; see the struct's own comment.
                reserved: [0; 16],
            };

            // Safety: the parameters are the documented layout for a timing capture, and the file
            // name pointer is valid for the duration of the call. What the call answers is a
            // `SUCCEEDED` question rather than a specific code: `pix3.h` documents `S_FALSE`, and
            // the PIX on this machine answers `S_OK` - see [`succeeded`].
            Some(unsafe { (runtime.begin)(PIX_CAPTURE_TIMING, &parameters) })
        }
    });

    match result {
        // No runtime at all; `runtime()` has already said why.
        None => false,
        Some(result) if succeeded(result) => {
            log::info!(
                "wgpu-mc: PIX timing capture started (HRESULT 0x{result:08x}), written to {} - GPU \
                 timing, CPU samples at {} Hz with callstacks, and the memory events (file IO, \
                 VirtualAlloc, HeapAlloc, custom allocator, page faults) that fill the capture's \
                 memory tables",
                file.display(),
                CPU_SAMPLES_PER_SECOND
            );
            true
        }
        Some(result) => {
            log::error!(
                "wgpu-mc: PIX refused to start a timing capture (HRESULT 0x{:08x}{}). {} It also \
                 needs PIX's WinPixTimingCapturer.dll loaded in the process - the log says which \
                 copy was loaded - and no capture already running.",
                result as u32,
                hresult_name(result),
                elevation_note()
            );
            false
        }
    }
}

/// What an HRESULT from PIX means, for the ones a timing capture actually runs into.
fn hresult_name(result: i32) -> &'static str {
    match result as u32 {
        0x80070005 => ", E_ACCESSDENIED",
        0x8000000A | 0x8007000A => ", E_PENDING",
        0x80070057 => ", E_INVALIDARG",
        0x80004005 => ", E_FAIL",
        0x8000FFFF => ", E_UNEXPECTED",
        0x800401F0 => ", CO_E_NOTINITIALIZED",
        0x80010106 => ", RPC_E_CHANGED_MODE",
        _ => "",
    }
}

/// Whether the reason a capture was asked for and not given is the elevation of this process.
///
/// A capture is refused for two reasons in practice, and they are told apart by this one bit: an
/// unelevated process, which cannot create the ETW sessions a timing capture records through, and
/// everything else. Saying which one it was is the difference between a fix and a guess.
fn elevation_note() -> &'static str {
    if is_elevated() {
        "This process is elevated, so elevation is not the reason."
    } else {
        "This process is NOT elevated, and a timing capture needs administrator: it records through \
         ETW providers, and creating those sessions is refused without it. A `gradlew runClient` \
         started from an administrator terminal is not enough on its own - Gradle reuses a daemon \
         started without elevation, and the game is forked by that daemon, so it inherits that \
         token. Run `gradlew --stop` first, or pass `--no-daemon`."
    }
}

/// Stops the capture, without holding the frame up while PIX writes it out.
///
/// `PIXEndCapture` is not a flag flip: it finishes the capture and writes what was recorded, and a
/// capture with the memory events enabled is gigabytes - measured at 2.6 GB for the 600 frames this
/// side asks for, against 265 MB before those events were on. Ten seconds was not enough for it, and
/// the timeout left the capture running with nothing left to stop it: the frame counter that stops
/// it had already handed over. So the call is made on a thread of its own and *not* waited for - the
/// render thread carries on and the answer is logged when the thread has it.
fn end() {
    let spawned = std::thread::Builder::new()
        .name("wgpu-mc PIX stop".to_string())
        .spawn(|| {
            let Some(runtime) = runtime() else {
                return;
            };

            // Safety: `PIXEndCapture` takes whether to discard what was recorded instead of writing
            // it, and is only reached for a capture this side believes it started.
            let result = unsafe { (runtime.end)(0) };

            if succeeded(result) {
                log::info!("wgpu-mc: PIX timing capture stopped (HRESULT 0x{result:08x})");
            } else {
                log::error!(
                    "wgpu-mc: PIX could not stop the capture (HRESULT 0x{:08x}{})",
                    result as u32,
                    hresult_name(result)
                );
            }
        });

    if spawned.is_err() {
        log::error!("wgpu-mc: could not start the thread that stops the PIX capture");
    }
}

/// Runs a PIX call on a thread of its own, and answers what it returned.
///
/// Not on the render thread, and that is not a preference: PIX's runtime initialises COM as it is
/// loaded, and the render thread's COM apartment is already set by the time a frame is being drawn -
/// the call answered `RPC_E_CHANGED_MODE` (0x80010106) every time from there. A fresh thread has no
/// apartment yet, so the runtime can set up whichever one it needs. The call is still made while the
/// render thread waits, so a capture starts and stops at a frame boundary either way.
fn call_on_its_own_thread(body: impl FnOnce() -> Option<i32> + Send + 'static) -> Option<i32> {
    let (sender, receiver) = std::sync::mpsc::channel();

    std::thread::Builder::new()
        .name("wgpu-mc PIX".to_string())
        .spawn(move || {
            let _ = sender.send(body());
        })
        .ok()?;

    match receiver.recv_timeout(std::time::Duration::from_secs(10)) {
        Ok(result) => result,
        Err(error) => {
            log::error!("wgpu-mc: PIX did not answer within ten seconds: {error}");
            None
        }
    }
}

/// Where the capture is written: beside the game's config, numbered so a session's captures do not
/// overwrite each other.
fn capture_file() -> Option<std::path::PathBuf> {
    let directory = crate::RUN_DIRECTORY.get()?;

    let mut index = 1;
    loop {
        let file = directory.join(format!("wgpu-mc-capture-{index}.wpix"));
        if !file.exists() {
            return Some(file);
        }
        index += 1;
    }
}
