//! The **Neolectrum** checkout, for the tests here that check one side of the bridge against the
//! other side's source.
//!
//! Neolectrum - the NeoForge mod that loads this engine - is a repository of its own
//! (`NTS-Official/Neolectrum`), and a number of this workspace's tests read files out of it: the
//! Kotlin that declares the JNI functions and binds the C ABI, the options screen and language
//! files that name every setting, `updates.json`, `neoforge.mods.toml`, and the shaders this crate
//! compiles at run time. Those tests used to `include_str!` across the two halves of a single
//! checkout (`../../../neoforge/...`). A path like that cannot survive the split - and an
//! `include_str!` that cannot find its file fails the *build*, so a clone of this repository on its
//! own could not even compile its tests, which is the opposite of what two repositories are for.
//!
//! So the files are read at run time, out of a checkout found like this:
//!
//!   * `WGPU_MC_MOD_DIR`, when it is set: a path, absolute or relative to this repository's root.
//!     Naming a directory that is not there is an **error**, because setting the variable is the
//!     only way to say "the checkout is *here*" and a wrong answer to that has to be loud.
//!   * otherwise the sibling checkout `../neolectrum` - where it is for anyone who clones both
//!     repositories side by side, which is how the two are developed.
//!
//! With neither, the checks that need those files **skip rather than fail**: this crate belongs to
//! the engine, and a clone of the engine that has never seen the mod should still run `cargo test`.
//! A skip says why on stderr - `cargo test -- --nocapture` shows it, and so does the output of any
//! test that fails for a reason of its own. Once the checkout *is* there, a file that has moved is
//! an error rather than a skip, because a rename in the mod tree is drift, not absence.
//!
//! The accessors below are the whole of the coupling: every file one of these tests reads from the
//! other repository is named **once**, here, and a check that needs one starts like this:
//!
//! ```ignore
//! let Some(source) = modtree::option_pages_kt() else {
//!     modtree::skip("the options-page check");
//!     return;
//! };
//! ```

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The variable that names the checkout, for a machine that keeps it somewhere else.
pub const DIR_VARIABLE: &str = "WGPU_MC_MOD_DIR";

/// Where the checkout is when nothing says otherwise: beside this repository.
pub const DEFAULT_SIBLING: &str = "../neolectrum";

/// This repository's root: this crate sits directly under it.
fn workspace_root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();

    ROOT.get_or_init(|| {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("`modtree` is a workspace member, so it lives under the repository root")
            .to_path_buf()
    })
}

/// The checkout's root, or `None` when there is no checkout to read from.
pub fn dir() -> Option<&'static Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();

    DIR.get_or_init(
        || match std::env::var_os(DIR_VARIABLE).filter(|value| !value.is_empty()) {
            Some(value) => {
                let named = PathBuf::from(value.as_os_str());
                let named = if named.is_absolute() {
                    named
                } else {
                    workspace_root().join(named)
                };

                assert!(
                    named.is_dir(),
                    "{DIR_VARIABLE} names {}, and that is not a directory",
                    named.display()
                );

                Some(named)
            }
            None => {
                let sibling = workspace_root().join(DEFAULT_SIBLING);

                sibling.is_dir().then_some(sibling)
            }
        },
    )
    .as_deref()
}

/// A path inside the checkout, by the same rules as [`read`] - for the checks that open a file, or
/// walk a directory, themselves.
///
/// A missing checkout is `None`; a checkout that does not have the file is a panic, because that is
/// a rename this check has not been told about.
pub fn path(relative: &str) -> Option<PathBuf> {
    let root = dir()?;
    let path = root.join(relative);

    assert!(
        path.exists(),
        "the Neolectrum checkout at {} has no `{relative}`: the file moved or was renamed, and the \
         check that reads it needs the path it moved to",
        root.display()
    );

    Some(path)
}

/// The text of a file in the checkout, by the same rules as [`path`].
pub fn read(relative: &str) -> Option<String> {
    let path = path(relative)?;

    Some(
        std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("{} is unreadable: {err}", path.display())),
    )
}

/// Says, on stderr, that a check had nothing to read.
pub fn skip(check: &str) {
    eprintln!(
        "skipping {check}: no Neolectrum checkout - set {DIR_VARIABLE} to one, or clone \
         https://github.com/NTS-Official/Neolectrum beside this repository"
    );
}

// ---------------------------------------------------------------------------------------------
// The mod's shaders. Three checks in `wgpu-mc` and one in `wgpu-mc-jni` compile them with naga, so
// a WGSL mistake is caught here rather than by the pipeline creation that panics on it in game.
// ---------------------------------------------------------------------------------------------

/// The directory the mod's `.wgsl` files live in, for the checks that walk it.
pub fn shader_dir() -> Option<PathBuf> {
    path("src/main/resources/assets/wgpu_mc/shaders")
}

/// One of the mod's shaders by file stem: `shader("terrain")` reads `terrain.wgsl`.
pub fn shader(name: &str) -> Option<String> {
    read(&format!(
        "src/main/resources/assets/wgpu_mc/shaders/{name}.wgsl"
    ))
}

// ---------------------------------------------------------------------------------------------
// The mod's metadata: the files that carry the version numbers, and the two files NeoForge itself
// reads (`neoforge.mods.toml` and the `updates.json` it points its update checker at).
// ---------------------------------------------------------------------------------------------

/// `gradle.properties`, where the mod's versions are written down.
pub fn gradle_properties() -> Option<String> {
    read("gradle.properties")
}

/// `updates.json`: what NeoForge's update checker fetches.
pub fn updates_json() -> Option<String> {
    read("updates.json")
}

/// `src/main/resources/META-INF/neoforge.mods.toml`: the mod file itself.
pub fn mods_toml() -> Option<String> {
    read("src/main/resources/META-INF/neoforge.mods.toml")
}

// ---------------------------------------------------------------------------------------------
// The mod's own source: the two halves of the bridge, and the options screen and language files
// that have to agree with the settings schema this crate compiles in.
// ---------------------------------------------------------------------------------------------

/// `WmNative.kt`: the `java.lang.foreign` half, bound by name and by byte offset.
pub fn wm_native_kt() -> Option<String> {
    read("src/main/kotlin/dev/birb/wgpu/rust/WmNative.kt")
}

/// `WgpuNative.kt`: the JNI half, whose `external fun`s are resolved against this crate.
pub fn wgpu_native_kt() -> Option<String> {
    read("src/main/kotlin/dev/birb/wgpu/rust/WgpuNative.kt")
}

/// `OptionPages.kt`: the options screen, which owns the page that carries the renderer's settings.
pub fn option_pages_kt() -> Option<String> {
    read("src/main/kotlin/dev/birb/wgpu/gui/OptionPages.kt")
}

/// `RustChunkBake.kt`: this side of the section feed, and what it records about it.
pub fn rust_chunk_bake_kt() -> Option<String> {
    read("src/main/kotlin/dev/birb/wgpu/chunk/RustChunkBake.kt")
}

/// `RustChunkBakeMixin.java`: the hook that starts a bake, checked for not writing the answer twice.
pub fn rust_chunk_bake_mixin_java() -> Option<String> {
    read("src/main/java/dev/birb/wgpu/mixin/chunk/RustChunkBakeMixin.java")
}

/// One of the shipped language files, by locale: `language("en_us")`.
pub fn language(locale: &str) -> Option<String> {
    read(&format!(
        "src/main/resources/assets/wgpu_mc/lang/{locale}.json"
    ))
}
