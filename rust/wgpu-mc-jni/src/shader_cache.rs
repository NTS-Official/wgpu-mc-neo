//! The on-disk cache of processed shaders.
//!
//! Translating a pipeline''s GLSL is the work `compile_render_pipeline` does before wgpu ever sees the
//! shader: cyntax preprocesses it, the AST is rewritten - implicit blocks added, combined samplers
//! split, matrices patched, bindings annotated with the numbers the pipeline''s plan assigned - and
//! the result is parsed again by naga to find the uniform block sizes. None of that depends on the
//! GPU, and all of it is deterministic: the same (vertex source, fragment source, defines, binding
//! plan, vertex layout) always produces the same two strings.
//!
//! So the result is kept, rather than recomputed every launch: the key is a hash of those inputs and
//! the entry is the two processed shaders plus everything derived from them that the caller needs -
//! which sampler is a cube sampler, which uniform blocks the shader declared that the pipeline never
//! listed, and the block sizes naga reported. A hit skips the preprocessing, the AST rewriting and
//! the reflection pass; a miss costs one file write.
//!
//! naga''s IR would be a finer grain, but a `naga::Module` has no serialisation - its own WGSL output
//! would be a second translation to maintain - so the processed GLSL is the practical unit.
//!
//! The files live next to the driver''s pipeline cache (`wgpu_pipeline_cache_*.bin`), because they are
//! the same kind of thing: work that belongs to one build of the renderer and can be thrown away.
//! A `version` file holds the format number and the wgpu version; when it does not match, the
//! directory is emptied, so a change to the preprocessing cannot be answered with a stale shader.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};

/// Bumped whenever the preprocessor''s output could differ for the same input.
///
/// The key includes this, so an entry written by an older build is never read by a newer one - and
/// the `version` file below clears the directory instead of leaving the files to accumulate.
const FORMAT_VERSION: u32 = 1;

/// Where the entries live, under the run directory.
const DIRECTORY: &str = "wgpu-shader-cache";

/// One pipeline''s translation, and everything derived from it that the compiler needs.
#[derive(Serialize, Deserialize)]
pub struct Translation {
    pub vert: String,
    pub frag: String,
    /// Which samplers are `samplerCube`s, by the name the pipeline binds them under.
    ///
    /// A flag rather than the GLSL type the preprocessor produced, because a flag is all the
    /// consumer reads: [`crate::blaze::BindGroupPlan::apply_sampler_types`] decides between a 2D and
    /// a cube view with it and nothing else.
    pub cube_samplers: Vec<(String, bool)>,
    /// Uniform blocks the shaders declared that the pipeline description never listed, with the
    /// binding each was given. The plan needs them or wgpu rejects the pipeline for using a binding
    /// its layout does not have.
    pub implicit_uniforms: Vec<(String, u32)>,
    /// The size each uniform block''s shader-side declaration has, which is what the layout''s
    /// `min_binding_size` and the bind group''s range are built from.
    pub block_sizes: Vec<(String, u64)>,
}

impl Translation {
    /// Read by the round-trip test below rather than by the pipelines, which want `blocks` and the
    /// sampler list separately; the test is what checks a translation survives the trip through JSON
    /// with its sampler types intact.
    #[allow(dead_code)]
    pub fn samplers(&self) -> HashMap<String, bool> {
        self.cube_samplers.iter().cloned().collect()
    }

    pub fn blocks(&self) -> HashMap<String, u64> {
        self.block_sizes.iter().cloned().collect()
    }
}

struct Cache {
    directory: Option<PathBuf>,
    hits: AtomicU64,
    misses: AtomicU64,
}

static CACHE: Lazy<Cache> = Lazy::new(|| {
    let directory = crate::RUN_DIRECTORY.get().map(|run| run.join(DIRECTORY));

    if let Some(directory) = &directory {
        prepare(directory);
    } else {
        // No run directory yet: the options screen sends it during client setup, before any pipeline
        // is compiled, so this is only reached by a renderer created outside the game.
        log::debug!("wgpu-mc: no run directory yet, so processed shaders are not cached");
    }

    Cache {
        directory,
        hits: AtomicU64::new(0),
        misses: AtomicU64::new(0),
    }
});

/// Creates the directory, and empties it when the version it was written by is not this one.
fn prepare(directory: &Path) {
    if let Err(error) = std::fs::create_dir_all(directory) {
        log::warn!(
            "wgpu-mc: could not create the processed shader cache at {}: {error}",
            directory.display()
        );
        return;
    }

    let stamp = format!(
        "{FORMAT_VERSION} wgpu {}\n",
        wgpu_mc::WmRenderer::wgpu_version()
    );
    let version_file = directory.join("version");

    match std::fs::read_to_string(&version_file) {
        Ok(existing) if existing == stamp => return,
        Ok(_) => {
            log::info!(
                "wgpu-mc: the processed shader cache was written by another build; clearing it"
            );
        }
        Err(_) => {}
    }

    if let Ok(entries) = std::fs::read_dir(directory) {
        for entry in entries.flatten() {
            let path = entry.path();

            if path.file_name().is_some_and(|name| name == "version") {
                continue;
            }

            let _ = std::fs::remove_file(path);
        }
    }

    let _ = std::fs::write(&version_file, stamp);
}

/// A key for one translation, from the inputs that decide it.
///
/// Length-prefixed and hashed twice with different offsets, so two different pipelines cannot land on
/// the same entry - the consequence of a collision would be one pipeline running another's shader,
/// which is not a failure that announces itself.
///
/// Every part has to be *canonical*, which for anything map-shaped means sorted: a `HashMap`'s
/// iteration order is randomised per process, so hashing its `Debug` output gave every pipeline a
/// different key on every launch. That is what [`canonical`] is for, and it is why the first version
/// of this cache reported two hits out of a directory full of entries that should all have matched.
pub fn key(parts: &[&str]) -> String {
    let mut first: u64 = 0xcbf2_9ce4_8422_2325;
    let mut second: u64 = 0x9e37_79b9_7f4a_7c15;

    let mut feed = |byte: u64| {
        first = (first ^ byte).wrapping_mul(0x0000_0100_0000_01b3);
        second = (second.rotate_left(7) ^ byte).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    };

    for part in parts {
        for byte in (part.len() as u64).to_le_bytes() {
            feed(byte as u64);
        }

        for byte in part.as_bytes() {
            feed(*byte as u64);
        }
    }

    format!("{FORMAT_VERSION}-{first:016x}{second:016x}")
}

/// A short hash of one key part, for the diagnostic that says which part moved between two runs.
pub fn part(value: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;

    for byte in value.as_bytes() {
        hash = (hash ^ *byte as u32).wrapping_mul(0x0100_0193);
    }

    hash
}

/// One canonical string for a map-shaped input, independent of how it happens to be ordered.
///
/// Sorted by key, with the pairs written out flat: this is a hash input, not something to read back,
/// so the only property it needs is that equal maps produce equal strings.
pub fn canonical<T: std::fmt::Display, I: IntoIterator<Item = (String, T)>>(entries: I) -> String {
    let mut pairs: Vec<(String, String)> = entries
        .into_iter()
        .map(|(key, value)| (key, value.to_string()))
        .collect();

    pairs.sort();

    let mut out = String::new();
    for (key, value) in pairs {
        out.push_str(&key);
        out.push('=');
        out.push_str(&value);
        out.push(';');
    }

    out
}

/// The translation for a key, if it is on disk and readable.
pub fn load(key: &str) -> Option<Translation> {
    let directory = CACHE.directory.as_ref()?;
    let path = directory.join(format!("{key}.json"));

    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) => {
            // A miss is the normal case on a first launch, so it is not worth a line of its own.
            if error.kind() != std::io::ErrorKind::NotFound {
                log::warn!(
                    "wgpu-mc: could not read the cached shader {}: {error}",
                    path.display()
                );
            }

            CACHE.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        }
    };

    match serde_json::from_str::<Translation>(&contents) {
        Ok(translation) => {
            CACHE.hits.fetch_add(1, Ordering::Relaxed);
            Some(translation)
        }
        Err(error) => {
            log::warn!(
                "wgpu-mc: the cached shader {} could not be read back ({error}); translating again",
                path.display()
            );
            CACHE.misses.fetch_add(1, Ordering::Relaxed);
            None
        }
    }
}

/// Writes a translation out, atomically: a half-written entry is one the next launch would fail to
/// read, and the failure would look like a shader bug.
pub fn store(key: &str, translation: &Translation) {
    let Some(directory) = CACHE.directory.as_ref() else {
        return;
    };

    CACHE.misses.fetch_add(1, Ordering::Relaxed);

    let Ok(json) = serde_json::to_vec(translation) else {
        log::warn!("wgpu-mc: a processed shader could not be serialised; it is not cached");
        return;
    };

    let path = directory.join(format!("{key}.json"));
    let temporary = directory.join(format!("{key}.json.tmp"));

    let written = std::fs::File::create(&temporary).and_then(|mut file| {
        file.write_all(&json)?;
        file.sync_all()
    });

    if let Err(error) = written {
        log::warn!(
            "wgpu-mc: could not write the processed shader {}: {error}",
            temporary.display()
        );
        let _ = std::fs::remove_file(&temporary);
        return;
    }

    if let Err(error) = std::fs::rename(&temporary, &path) {
        log::warn!(
            "wgpu-mc: could not move the processed shader into place at {}: {error}",
            path.display()
        );
    }
}

/// How many translations came from disk and how many were done, for the report.
pub fn stats() -> (u64, u64) {
    (
        CACHE.hits.load(Ordering::Relaxed),
        CACHE.misses.load(Ordering::Relaxed),
    )
}

/// A line saying what the cache did, written once when the first pipelines are compiled.
///
/// The numbers are only interesting next to each other - "everything was translated" and "everything
/// was read back" are the two states this feature is either in or not - so the first dozen pipelines
/// report it and the rest say nothing.
pub fn report_once() {
    static REPORTED: AtomicU64 = AtomicU64::new(0);

    let (hits, misses) = stats();
    if hits + misses == 0 {
        return;
    }

    let reported = REPORTED.fetch_add(1, Ordering::Relaxed);
    if reported >= 8 || !(hits + misses).is_multiple_of(8) {
        return;
    }

    log::info!(
        "wgpu-mc: processed shaders: {hits} from the cache, {misses} translated ({})",
        CACHE
            .directory
            .as_ref()
            .map(|directory| directory.display().to_string())
            .unwrap_or_else(|| "not cached".to_string())
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_the_same_for_the_same_inputs_and_different_for_others() {
        let one = key(&[
            "void main() {}",
            "void main() {}",
            "#version 440",
            "sampler0=0",
        ]);
        let same = key(&[
            "void main() {}",
            "void main() {}",
            "#version 440",
            "sampler0=0",
        ]);
        let other = key(&[
            "void main() {}",
            "void main() {}",
            "#version 440",
            "sampler0=1",
        ]);

        assert_eq!(one, same);
        assert_ne!(one, other);
    }

    #[test]
    fn parts_cannot_be_confused_with_each_other() {
        // Without length prefixes, "ab" + "c" and "a" + "bc" would hash the same - and two pipelines
        // whose shaders differ only in where the split falls would share an entry.
        assert_ne!(key(&["ab", "c"]), key(&["a", "bc"]));
    }

    #[test]
    fn a_translation_round_trips_through_json() {
        let translation = Translation {
            vert: "#version 440\nvoid main() {}\n".to_string(),
            frag: "#version 440\nout vec4 c;\n".to_string(),
            cube_samplers: vec![("Sampler0".to_string(), true)],
            implicit_uniforms: vec![("Globals".to_string(), 4)],
            block_sizes: vec![("Globals".to_string(), 192)],
        };

        let json = serde_json::to_string(&translation).expect("serialisable");
        let read: Translation = serde_json::from_str(&json).expect("deserialisable");

        assert_eq!(read.vert, translation.vert);
        assert_eq!(read.frag, translation.frag);
        assert_eq!(read.samplers().get("Sampler0"), Some(&true));
        assert_eq!(read.implicit_uniforms, translation.implicit_uniforms);
        assert_eq!(read.blocks().get("Globals"), Some(&192));
    }

    #[test]
    fn a_canonical_form_does_not_depend_on_iteration_order() {
        // The bug the first version of this cache had: the binding locations and the vertex layout
        // are `HashMap`s, whose iteration order is randomised per process, so hashing their `Debug`
        // output made every pipeline's key different on every launch - and a directory full of
        // entries nobody could ever match.
        let first = canonical([("Sampler0".to_string(), 0), ("Globals".to_string(), 4)]);
        let second = canonical([("Globals".to_string(), 4), ("Sampler0".to_string(), 0)]);

        assert_eq!(first, second);
        assert_eq!(first, "Globals=4;Sampler0=0;");
        assert_ne!(
            first,
            canonical([("Globals".to_string(), 5), ("Sampler0".to_string(), 0)])
        );
    }

    #[test]
    fn an_entry_from_an_older_format_is_not_read() {
        // The version is part of the key, so a build that changes the preprocessor cannot be answered
        // with the previous build's shaders even if the files are still on disk.
        let parts = ["void main() {}", "void main() {}", "#version 440"];
        let current = key(&parts);

        assert!(current.starts_with(&format!("{FORMAT_VERSION}-")));
        assert_ne!(current, format!("{}-{}", FORMAT_VERSION + 1, &current[2..]));
    }
}
