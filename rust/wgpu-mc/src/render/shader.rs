use std::borrow::Cow;

use crate::mc::resource::{ResourcePath, ResourceProvider};
use crate::wgpu::{ShaderModule, ShaderModuleDescriptor};

#[cfg(target_arch = "wasm32")]
pub trait WmShader {
    fn get_frag(&self) -> (&ShaderModule, &str);

    fn get_vert(&self) -> (&ShaderModule, &str);
}

#[cfg(not(target_arch = "wasm32"))]
pub trait WmShader: Send + Sync {
    fn get_frag(&self) -> (&ShaderModule, &str);

    fn get_vert(&self) -> (&ShaderModule, &str);
}

#[derive(Debug)]
pub struct WgslShader {
    pub module: ShaderModule,
    pub frag_entry: String,
    pub vert_entry: String,
}

impl WgslShader {
    /// Reads a WGSL shader and builds a module from it, **with naga's runtime checks off**.
    ///
    /// # Why the checks are off
    ///
    /// `create_shader_module` asks naga to make the shader safe against itself: every indexing of a
    /// runtime-sized array gets an `arrayLength` load and a `min` in front of it, so that an
    /// out-of-range index reads the last element (or zero) instead of whatever is past the binding.
    /// **For this renderer's terrain shaders that is nine clamped loads per vertex** - eight into the
    /// section arena and one into `section_draws` - and the arena is bound as one buffer with
    /// `min_binding_size: None`, so the length the clamp compares against is the *whole* arena. The
    /// check can therefore only ever fire on an index that is wrong by more than the entire arena,
    /// which is not a mistake the indices can make: they are `word_base` (the slot the Rust baker
    /// allocated) plus offsets into the section it baked there, and the instance number the pass wrote
    /// into `first_instance`, which is bounded by `SECTION_DRAW_CAPACITY`.
    ///
    /// So the promise this makes is a real one and it is about *this* repository's shaders: **every
    /// storage buffer here is addressed by a value the baker or the pass computed to be in range, and
    /// every loop has a trip count the shader can see** (the four taps of RGSS, the four corners of a
    /// quad). What is given up is the backstop: an index that is wrong by a whole arena's worth reads
    /// past the binding and gets whatever the driver's own robustness returns - zeros, on Vulkan, which
    /// is geometry at the origin rather than a crash. A loop that never ends has nothing to break it.
    ///
    /// `ShaderRuntimeChecks` has more in it than bounds checks - loop bounding, signed integer division
    /// and overflow, the ray-query and mesh-shader checks - and they are all turned off together
    /// because `unchecked()` is the one constructor for "none of them". The last two are inert here:
    /// no shader in this crate has a ray query or a mesh shader.
    pub fn init(
        resource: &ResourcePath,
        rp: &dyn ResourceProvider,
        device: &wgpu::Device,
        frag_entry: String,
        vert_entry: String,
    ) -> Option<Self> {
        let shader_src = rp.get_bytes(resource)?;

        let shader_src = std::str::from_utf8(&shader_src).ok()?;

        // SAFETY: the two promises above. The buffers these shaders index are `chunk_data` (the arena)
        // and `section_draws`, both written by this side; the values indexed with are the section's
        // slot in the arena and the record number the pass chose, and no shader here loops without a
        // bound it can see. Note what is *not* claimed: nothing verifies this at run time any more, so
        // a change that makes an index unbounded is a change that has to be made carefully - see the
        // tests in `graph.rs` that pin the sizes and the shapes these indices depend on.
        let module = unsafe {
            device.create_shader_module_trusted(
                ShaderModuleDescriptor {
                    // Named, which was `None` before: with the bounds checks gone, the label is what
                    // makes a driver-side complaint about one of these shaders name the file it came
                    // from rather than a handle.
                    label: Some(&resource.0),
                    source: wgpu::ShaderSource::Wgsl(Cow::from(shader_src)),
                },
                wgpu::ShaderRuntimeChecks::unchecked(),
            )
        };

        Some(Self {
            module,
            frag_entry,
            vert_entry,
        })
    }
}

impl WmShader for WgslShader {
    fn get_frag(&self) -> (&ShaderModule, &str) {
        (&self.module, &self.frag_entry)
    }

    fn get_vert(&self) -> (&ShaderModule, &str) {
        (&self.module, &self.vert_entry)
    }
}

#[derive(Debug)]
pub struct GlslShader {
    pub frag: ShaderModule,
    pub vert: ShaderModule,
}

impl GlslShader {
    pub fn init(
        frag: &ResourcePath,
        vert: &ResourcePath,
        rp: &dyn ResourceProvider,
        device: &wgpu::Device,
    ) -> Self {
        let frag_src = rp.get_bytes(frag).unwrap();
        let vert_src = rp.get_bytes(vert).unwrap();

        let frag_src = std::str::from_utf8(&frag_src).unwrap();
        let vert_src = std::str::from_utf8(&vert_src).unwrap();

        let frag_module = device.create_shader_module(ShaderModuleDescriptor {
            label: None,
            source: wgpu::ShaderSource::Glsl {
                shader: Cow::from(frag_src),
                stage: wgpu::naga::ShaderStage::Fragment,
                defines: Default::default(),
            },
        });

        let vert_module = device.create_shader_module(ShaderModuleDescriptor {
            label: None,
            source: wgpu::ShaderSource::Glsl {
                shader: Cow::from(vert_src),
                stage: wgpu::naga::ShaderStage::Vertex,
                defines: Default::default(),
            },
        });

        Self {
            frag: frag_module,
            vert: vert_module,
        }
    }
}

impl WmShader for GlslShader {
    fn get_frag(&self) -> (&ShaderModule, &str) {
        (&self.frag, "main")
    }

    fn get_vert(&self) -> (&ShaderModule, &str) {
        (&self.vert, "main")
    }
}

#[cfg(test)]
mod unguarded_index_tests {
    use crate::wgpu::naga;

    /// The two terrain shaders, which is where the indexing this module promises about lives.
    const TERRAIN: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../neoforge/src/main/resources/assets/wgpu_mc/shaders/terrain.wgsl"
    ));
    const TERRAIN_SOLID: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../neoforge/src/main/resources/assets/wgpu_mc/shaders/terrain_solid.wgsl"
    ));

    /// How many expressions index a **storage buffer** in `source`, by the global they start from.
    ///
    /// Follows a chain, because `chunk_data[i]` and `chunk_data[i][j]` are the same promise: the base is
    /// an ordinary expression and only the bottom of the chain names the buffer. Component accesses
    /// (`AccessIndex`, `chunk_data[i].x`) are not counted - they are a fixed offset into a value that is
    /// already in a register, not a second read of the buffer.
    fn storage_reads(source: &str) -> Vec<(String, String)> {
        let module = naga::front::wgsl::parse_str(source).expect("the terrain shader parses");

        let mut found = Vec::new();

        // **Entry points as well as plain functions.** naga keeps them apart, and the vertex stage -
        // which is where every one of these reads is - is an entry point: walking only
        // `module.functions` counts zero, which is how the first version of this test passed a shader it
        // had not looked at.
        let entry_points = module
            .entry_points
            .iter()
            .map(|entry| (entry.name.clone(), &entry.function));
        let functions = module.functions.iter().map(|(handle, function)| {
            (
                function
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("<function {}>", handle.index())),
                function,
            )
        });

        for (name, function) in entry_points.chain(functions) {
            for (_, expression) in function.expressions.iter() {
                let naga::Expression::Access { base, .. } = expression else {
                    continue;
                };

                // The bottom of the chain: an `Access` over an `Access` is `a[i][j]`, and the buffer is
                // whichever global the innermost one names.
                let mut base = *base;
                let global = loop {
                    match function.expressions.try_get(base) {
                        Ok(naga::Expression::GlobalVariable(global)) => break Some(*global),
                        Ok(naga::Expression::Access { base: inner, .. }) => base = *inner,
                        _ => break None,
                    }
                };

                let Some(global) = global else {
                    continue;
                };

                let variable = &module.global_variables[global];

                if matches!(variable.space, naga::AddressSpace::Storage { .. }) {
                    found.push((name.clone(), variable.name.clone().unwrap_or_default()));
                }
            }
        }

        found
    }

    /// **Every storage read in every shader this crate builds, because every one of them is unguarded.**
    ///
    /// `WgslShader::init` builds its modules with `ShaderRuntimeChecks::unchecked()`, so naga adds no
    /// clamp in front of any of these - that is the point of it, and on the terrain's vertex stage it is
    /// nine clamped `arrayLength` loads per vertex saved. What it turns into is a list somebody has to
    /// keep: an index expression that is not bounded by construction is a read past the binding, and
    /// this table is where that is true.
    ///
    /// Into the arena, eight of them per terrain shader: the four corners of the quad this vertex
    /// belongs to, read twice - once for the occlusion count packed into each corner's fourth word, and
    /// once for the position, the light and the colour. The ninth is `section_draws[instance]`, which
    /// carries the arena slot and the section's position. Every one is `word_base + k` for `k` in
    /// `0..16`, or the draw's own record number; see the shader and `SectionDraw` for why those are in
    /// range.
    ///
    /// Asserted rather than described because **the numbers are load-bearing now**: with the checks on, a
    /// read that went out of range was clamped and the picture was merely wrong. This is what says the
    /// counts out loud, so a change that adds one has to come here and say why it is safe.
    ///
    /// A file that is not in this table must have none, which is the half that catches a *new* shader.
    const UNGUARDED_STORAGE_READS: &[(&str, usize)] =
        &[("entity", 1), ("terrain", 9), ("terrain_solid", 9)];

    #[test]
    fn the_shaders_this_crate_builds_index_a_storage_buffer_only_where_it_is_listed() {
        // The same directory `graph.yaml`'s pipelines name their shaders in; two levels up from
        // `rust/wgpu-mc` is the repository root.
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../neoforge/src/main/resources/assets/wgpu_mc/shaders");

        let mut seen = Vec::new();

        for entry in std::fs::read_dir(&directory)
            .unwrap_or_else(|err| panic!("{} is unreadable: {err}", directory.display()))
            .map(|entry| entry.expect("a readable directory entry"))
        {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("wgsl") {
                continue;
            }

            let name = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .expect("a UTF-8 file name")
                .to_string();

            let source = std::fs::read_to_string(&path)
                .unwrap_or_else(|err| panic!("{} is unreadable: {err}", path.display()));

            let reads = storage_reads(&source);
            let expected = UNGUARDED_STORAGE_READS
                .iter()
                .find(|(listed, _)| *listed == name)
                .map_or(0, |(_, count)| *count);

            assert_eq!(
                reads.len(),
                expected,
                "{name}.wgsl has {} storage-buffer index expression(s) and this table says {expected}: \
                 {reads:?}. Every one is unguarded - `ShaderRuntimeChecks::unchecked` in this file - so \
                 a new one is a new promise that it is in range, and that promise belongs in the table",
                reads.len()
            );

            seen.push(name);
        }

        // A renamed or moved shader would otherwise take its row of the table with it, and the count
        // above would then be read against nothing.
        for (listed, _) in UNGUARDED_STORAGE_READS {
            assert!(
                seen.contains(&listed.to_string()),
                "{listed}.wgsl is listed as indexing a storage buffer and no such shader is in {}: the \
                 files are {seen:?}",
                directory.display()
            );
        }

        // And the directory was really read: a walk that found nothing would satisfy every assertion
        // above.
        assert!(
            seen.len() > 8,
            "only {} shader(s) were found in {}: {seen:?}",
            seen.len(),
            directory.display()
        );
    }

    /// The terrain vertex stage specifically, which is where the per-vertex cost was measured.
    ///
    /// The table above holds the counts; this holds the *shape* of the one that was counted, because the
    /// number alone does not say which stage pays for it - and it is the vertex stage, once per vertex,
    /// that made nine worth removing.
    #[test]
    fn the_terrain_vertex_stage_is_where_the_nine_reads_are() {
        for (name, source) in [("terrain", TERRAIN), ("terrain_solid", TERRAIN_SOLID)] {
            let reads = storage_reads(source);

            assert!(
                reads.iter().all(|(function, _)| function == "vert"),
                "{name}.wgsl indexes a storage buffer outside `vert`: {reads:?}. The vertex stage is \
                 where this was measured and where the promise was made"
            );
        }
    }
}
