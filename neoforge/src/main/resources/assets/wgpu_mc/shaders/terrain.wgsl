struct UV {
    uv1: vec2<f32>,
    uv2: vec2<f32>,
    blend: f32,
    padding: f32
};

struct UVs {
    uvs: array<UV>
};

struct ChunkOffset {
    x: i32,
    z: i32
}


@group(0) @binding(0) var<uniform> mat4_model: mat4x4<f32>;
@group(0) @binding(1) var<uniform> mat4_view: mat4x4<f32>;
@group(0) @binding(2) var<uniform> mat4_persp: mat4x4<f32>;

@group(0) @binding(3) var t_texture: texture_2d<f32>;
@group(0) @binding(4) var t_sampler: sampler;

// The game's own block atlas, and the sampler the game samples it with. It is bound for the faces
// whose sprite the game animates - see `UV_GAME_ATLAS` in `pipeline.rs`: those are baked with the
// game's coordinates and marked in the vertex, and this is the texture they are meant for. The game
// animates it by rendering each due frame into it, so those faces move for free.
@group(0) @binding(5) var t_game_atlas: texture_2d<f32>;
@group(0) @binding(6) var t_game_sampler: sampler;

// **The magnification pair, which is how the game's own crispness comes back without giving up
// anisotropic filtering.**
//
// The game magnifies with `GL_NEAREST` and minifies with `GL_LINEAR_MIPMAP_LINEAR`, and GL lets it choose
// those independently. wgpu does not: any `anisotropy_clamp` above 1 requires the min, mag *and* mipmap
// filters to be linear, so one sampler cannot both magnify the way the game does and filter
// anisotropically. `t_sampler` and `t_game_sampler` are the anisotropic ones, for surfaces being
// *minified*; these two are `Nearest` magnification with no anisotropy, for surfaces being *magnified*.
//
// The split is not a compromise, because the two ranges do not overlap: anisotropic filtering is a
// minification technique, and a magnified surface is at level 0 by definition and has nothing to
// compress. See the fragment stage for the test that picks between them, and `atlas_magnify_sampler` in
// `atlas.rs` for the whole of why there are two of each.
@group(0) @binding(10) var t_game_sampler_magnify: sampler;

// The same-side pair: `t_sampler` is anisotropic, this one magnifies with `Nearest`. Nothing samples this
// side's atlas in a normal session - every sprite is on the game's - so it is here for the fallback path
// and for the two atlases to stay symmetric.
@group(0) @binding(11) var t_sampler_magnify: sampler;

// **A minified animated face gets its own sampler, because neither pair above is right for it.** The
// anisotropic one cannot be used beside a `Nearest` mip filter (a validation error, and the mip filter is
// a player's switch), and `Nearest` magnification of a coarse level is what turns a moving sprite into one
// flat patch of its own running average - the "bright spots magnified into one big bright tile" a player
// described. Same filters as `t_sampler` and `t_sampler_magnify` except for the anisotropy.
@group(0) @binding(12) var t_game_sampler_animated: sampler;
@group(0) @binding(13) var t_sampler_animated: sampler;

// The game's **lightmap**, and the sampler the game samples it with (`Sampler2` for the terrain is
// `getClampToEdge(LINEAR)`). This is the whole of the lighting: the game builds this 16x16 texture from
// the light levels of the world, the gamma and brightness options, the time of day, night vision and
// the darkness effect, and its own terrain shader does nothing more than fetch it per vertex -
// `vertexColor = Color * sample_lightmap(Sampler2, UV2)`. See `GAME_LIGHTMAP`.
@group(0) @binding(7) var t_lightmap: texture_2d<f32>;
@group(0) @binding(8) var t_lightmap_sampler: sampler;

// The game's **fog**: the colour and the four distances out of the frame's `FogData`, which is the same
// block the game's own terrain shader reads (`layout(std140) uniform Fog`). The last three floats are not
// part of the game's block at all - they are the camera's offset inside its own section, which is what
// turns the section-relative position this shader computes into the camera-relative one a fog distance is
// measured from. See `crate::renderer::FOG`.
struct FogEnvironment {
    color: vec4<f32>,
    environmental_start: f32,
    environmental_end: f32,
    render_distance_start: f32,
    render_distance_end: f32,
    camera_offset: vec3<f32>,
    padding: f32,
};

@group(0) @binding(9) var<uniform> fog: FogEnvironment;

@group(1) @binding(0) var<storage> chunk_data: array<u32>;

// **What one draw of this pass is, indexed by `instance_index`**, which is the only channel a
// `multi_draw_indexed_indirect` has for anything per draw: an immediate is set once for the whole call
// and one call may cover thousands of sections.
//
// Both paths read it - the batched one and the one that issues a `draw_indexed` per section - so the
// switch between them is a switch in the number of calls and not in the picture. The pass writes it
// once per pass, before its first draw, and each dispatch entry's `first_instance` is the index of its
// own record.
//
// See `SectionDraw` in `mc/mod.rs`, which is the same four members written by the other side.
struct SectionDraw {
    // The section, **relative to the section the camera is in** - the same relative position the
    // immediate used to carry for one draw at a time, and for the same reason: `x + 30000` in `f32`
    // steps by four thousandths of a block, and the depth test between the ground and a shadow lying on
    // it is decided by exactly those bits. See `SectionPosition`.
    x: i32,
    y: i32,
    z: i32,
    // The u32 slot in the arena this draw's vertices start at. This was `@builtin(instance_index)`,
    // which is the record's own index now.
    word_base: u32,
};

@group(2) @binding(0) var<storage> section_draws: array<SectionDraw>;

struct VertexResult {
    @builtin(position) pos: vec4<f32>,
    @location(0) tex_coords: vec2<f32>,
    // **There is no `location(1)` or `location(2)` here, and that is the answer to "fill in
    // `tex_coords2` and `blend`" rather than a gap.** This struct used to declare both and the vertex
    // stage used to write `vec2(0.0, 0.0)` and `0.0` into them, which is a varying that is written and
    // never read - so the compiler removes it and the value never means anything. What they came from is
    // the reason they could not be filled in: they are leftovers from the GLSL renderer this one
    // replaced, where `texCoord2` was a second UV set and `blend` chose between the two, and they have
    // no source in the vertex format this side bakes.
    //
    // `Vertex` is `position`, one `uv` pair, a colour, ten animated-texture bits and a lightmap byte -
    // see `render::pipeline`, which is where the thirteen bytes are packed. There is no
    // second UV pair for `tex_coords2` to carry, and no blend weight for `blend` to be. The game's own
    // terrain shader is the same shape and has neither:
    //
    //     in vec3 Position; in vec4 Color; in vec2 UV0; in ivec2 UV2;
    //
    // - and its fragment stage reads `texCoord0` and `vertexColor` and nothing else. The lightmap that
    // `blend` would have selected against is fetched in the vertex stage here, into `light_color` below.
    //
    // So the two are gone rather than kept at zero: a varying that is always zero reads as "something
    // should be filling this in", and the next person to look at it has to do this same search to find
    // out that nothing should.
    @interpolate(flat) @location(12) ao1: f32,
    @interpolate(flat) @location(13) ao2: f32,
    @interpolate(flat) @location(14) ao3: f32,
    @interpolate(flat) @location(15) ao4: f32,
    @location(16) light_uv: vec2<f32>,
    // The light at this vertex, fetched from the game's lightmap by the vertex stage - which is where
    // the game's own terrain shader fetches it, so the rasterizer interpolates the *colour*, exactly as
    // it does for the game's terrain.
    @location(5) light_color: vec3<f32>,
    // How far this vertex is from the camera, the two ways the game measures it for fog. Interpolated,
    // as the game's own varyings are: the fog is applied per pixel from a per-vertex distance.
    @location(20) fog_distances: vec2<f32>,
    @location(18) color: vec4<f32>,
    // Which atlas `tex_coords` is in: bit 0 of the ten the vertex format reserves for an animated
    // texture. Flat, because it is a property of the face and not of the corner: every vertex of a
    // quad carries the same answer, and the fragment stage selects between the two atlases with it.
    // It is **not** branched on - see the sampling note in the fragment stage, where a `textureSample`
    // under an `if` would be undefined behaviour whatever this varying says.
    @interpolate(flat) @location(19) game_atlas: u32,
    // Whether the game animates this face's sprite: bit 1 of the same ten, `UV_ANIMATED`. Flat for the
    // same reason - it is the face's property and every corner carries it - and it is what scopes the
    // level-of-detail offset in the fragment stage, because a coarse mip of a *moving* sprite is a moving
    // average and a still one's is a fixed pyramid. See the level-of-detail floor in the fragment stage.
    //
    // **21 and not 20**: 20 is `fog_distances` above. This list is not in order, and a location named
    // twice is a shader that fails to compile rather than one that silently picks a winner.
    @interpolate(flat) @location(21) animated: u32,
    // How many levels this face may be pushed down the chain, decoded in the vertex stage because the
    // bits live in `v3` and `v3` is a vertex-local. Flat for the same reason the two above are.
    @interpolate(flat) @location(22) lod_floor: u32,
    // Whether this face belongs to Minecraft's **`cutout_mipped`** pipeline rather than its `cutout`
    // one: bit 6 of `v3`'s flag half, `UV_CUTOUT_MIPPED`. Flat for the same reason as the three above,
    // and it is what picks between the game's two cutouts in the fragment stage - 0.1 against 0.5.
    @interpolate(flat) @location(23) mipped: u32
};

// What one terrain draw is told about itself **beyond its own `SectionDraw`**: the alpha cutoff its
// layer asks for, and the five per-frame numbers about the two atlases. Seven `u32`s, twenty-eight
// bytes, and no padding - which is the size the pass declares for them (see `@pc_section_position` in
// `graph.yaml` and the sizes in `RenderGraph::new`).
//
// **The section's x, y and z are not here any more, and that is what the batched path cost.** They were
// the first three members, written per draw, and `multi_draw_indexed_indirect` is one call: everything
// an immediate holds is held for every draw in it, so the position had to move to something the vertex
// stage can index per draw - `section_draws` above, by `instance_index`.
//
// The rest stays, because it is not per draw: the bias and the two texel sizes are frame constants, and
// the cutoff is a property of the layer, which is one immediate write per layer.
struct SectionPosition {
    // `0.5` for the cutout layer: Minecraft's own `CUTOUT_TERRAIN`, which declares `ALPHA_CUTOUT`.
    // The solid layer is **not** drawn with this shader - it has its own, with no test at all and no
    // `discard` in it, because a `discard` anywhere costs the whole pipeline its early-Z. See
    // `terrain_solid.wgsl`.
    alpha_cutout: f32,
    // **The level-of-detail bias the two block-atlas fetches are given, per draw.**
    //
    // It was a `const` for a round, and that was a mistake worth recording: **a constant is not
    // observable.** With `const ATLAS_LOD_BIAS: f32 = 0.0` the fetch is spelled `textureSampleBias` but
    // behaves exactly as a plain `textureSample`, so "the bias does nothing" and "the bias is not
    // reaching the GPU" produce the same picture - and changing the number needs the shader copied into
    // the build directory *and* the client restarted, because **nothing watches the shader files**
    // (`mark_pipelines_stale` is called on atlas and lightmap handover only). Two rounds of "I moved it
    // and nothing happened" could not distinguish those cases.
    //
    // As an immediate it is a value the draw hands over, so a setting can move it and the next frame
    // uses it - which is the whole difference between this and the constant.
    //
    // Zero is the honest default and the shader's normal state: it makes the fetch below identical to a
    // plain `textureSample`. A positive value samples a *coarser* level, a negative one a finer level -
    // which is how the two readings are told apart: blur that clears with a negative bias means the
    // chosen level was too coarse, and blur unchanged by any bias means the level was never the problem.
    lod_bias: f32,
    // **Half a texel of this side's own atlas, and half a texel of the game's, under test.**
    //
    // Two fields and not one, because the two atlases are not the same size and the shift has to be
    // measured against the one the sample is going to come out of: the game's stitcher packed
    // `blocks.png` at 2048x2048 on some of the launches this was written against and 1024x1024 on others,
    // while `ATLAS_DIMENSIONS` is fixed. A single number would be half a texel on one of them and a
    // quarter on the other.
    //
    // Zero disables it. See the note at the decode for what it is for and what it cannot do from here.
    half_texel_ours: f32,
    half_texel_game: f32,
    // **One texel of each atlas, as a fraction of it**, which is what the magnification test compares the
    // coordinates' screen-space derivative against.
    //
    // Sent rather than written here for the reason everything else about the atlases is: this side's is
    // `ATLAS_DIMENSIONS` and never moves, while the game's is whatever its stitcher packed - 2048x2048 on
    // some of the launches this was written against and 1024x1024 on others. `1 / 2048` written into a
    // shader would call a surface magnified on one launch and minified on the next.
    texel_ours: f32,
    texel_game: f32,
    // The game's TextureSize, per atlas: one texel as a fraction of it, which is the `pixel_size` the two
    // sampling functions below are given.
    //
    // It is `texel_ours` and `texel_game` again, and it is sent a second time rather than read from those two
    // because the pair has two different jobs - `is_magnified` compares a derivative against one texel, and a
    // sampling function divides by it - and a field that meant two things would be the one place they could
    // drift apart. Two `f32` written consecutively read as one `vec2`.
    // The game's UseRgss: 1 when its textureFiltering option is RGSS, 0 otherwise. See `sample_rgss`.
    use_rgss: u32,
};

/// Whether a fragment's coordinates are being **stretched** rather than squeezed - which is exactly
/// "magnification", and is the test that picks between the two samplers.
///
/// The two derivatives are compared **separately**, and that is not a detail: `fwidth` is their *sum*, so
/// testing `fwidth < texel` asks for each of them to be under half a texel, not under one - which moves the
/// switch to where an isotropically-magnified surface is already half minified and puts the seam between
/// the two samplers where it is most visible. This test asserts what magnification actually is: neither
/// direction covers a whole texel in one pixel.
///
/// The result is flat across a primitive - a derivative is, because it is computed per 2x2 quad - so this is
/// uniform control flow by construction rather than by luck, and both fetches may be evaluated and selected
/// between.
fn is_magnified(coords: vec2<f32>, texel: f32) -> bool {
    let d = vec2<f32>(dpdx(coords).x, dpdy(coords).y);
    return abs(d.x) < texel && abs(d.y) < texel;
}

// **The game's own two sampling functions, ported.** See `assets/minecraft/shaders/core/terrain.fsh`,
// whose `main` is `UseRgss == 1 ? sampleRGSS(...) : sampleNearest(...)`.
//
// They are here rather than expressed with a sampler because **neither can be**: a sampler has one
// `anisotropy_clamp` and no way to say "four taps on a rotated grid at this exact level", and neither
// exposes the level the hardware would otherwise pick.
//
// `pixel_size` is one texel of the atlas being sampled, as a fraction of it - `1.0 / TextureSize` in the
// game, which is why the two numbers travel in the immediate instead of being written here.

/// **Everything the two sampling methods need that comes from a derivative**, taken once per fragment
/// and - the whole reason this is a struct - *before any branch*.
///
/// `dpdx` and `dpdy` are defined only in uniform control flow, and the fragment stage has two choices
/// ahead of it that are both per-vertex flags: which atlas a face samples, and which of the three
/// samplers its level wants. So the derivatives, the levels and the adjusted coordinates are all taken
/// at the top level of `frag`, and what is left inside the branches is `textureSampleLevel` and
/// arithmetic, neither of which carries a uniformity requirement. See [`sample_at_level`], and
/// `the_terrain_shaders_never_sample_a_texture_under_a_branch` in `graph.rs`, which is the test that
/// holds the shape.
struct SampleGeometry {
    /// The coordinates `sampleNearest` fetches from, with the texel-centre correction applied.
    nearest_uv: vec2<f32>,
    /// The level `sampleNearest` fetches at.
    nearest_level: f32,
    /// The four coordinates `sampleRGSS` taps, on the game's own rotated grid.
    taps: array<vec2<f32>, 4>,
    /// The two levels `sampleRGSS` blends between, and the fraction between them.
    low_level: f32,
    high_level: f32,
    level_blend: f32,
    /// How much of the supersampled fetch survives against the plain one. Below about one texel per
    /// pixel the surface wants the plain fetch's sharpening, which RGSS would only blur.
    rgss_blend: f32,
};

/// The geometry for one fragment: the coordinates it samples, one texel of the atlas it samples, and
/// the level-of-detail shift the draw handed over.
///
/// **Called once, from the top level of `frag`, and nowhere else.** The derivatives under it are what
/// every level here is computed from, and a derivative taken under a branch is undefined behaviour
/// rather than a slightly wrong number.
///
/// The two levels it produces are the game's own two answers to "which level":
///
///  * `nearest_level` is the hardware's answer for these derivatives - what the game's `textureGrad`
///    would have used - **plus the bias**. The game's fetch has no bias, and the two things this side
///    adds to it are both shifts: the `atlas_lod_bias` setting and the per-sprite floor. At a bias of
///    zero and a floor of none the two agree, which is the state the comparison runs in.
///  * the RGSS pair comes from the **geometric mean** of the two derivative lengths rather than the
///    worse of them, which is the whole of what the game's `sampleRGSS` does differently:
///
///    ```glsl
///    float effectiveDerivative = sqrt(minDerivative * maxDerivative);
///    ```
///
///    The hardware's implicit level is derived from the worst of the two, which at a grazing angle is
///    the compressed one - so a surface seen edge-on is sampled from several levels coarser than its
///    footprint needs, and a *moving* sprite's coarse levels are a running average of its animation
///    rather than a smaller copy of it. That average is the fluid shimmer, and four levels is the order
///    a player measured.
fn sample_geometry(uv: vec2<f32>, pixel_size: vec2<f32>, bias: f32) -> SampleGeometry {
    let du = dpdx(uv);
    let dv = dpdy(uv);

    let derivative_x = length(du);
    let derivative_y = length(dv);
    let min_derivative = min(derivative_x, derivative_y);
    let max_derivative = max(derivative_x, derivative_y);

    let texel_screen_size = sqrt(du * du + dv * dv);
    let max_texel_size = max(texel_screen_size.x, texel_screen_size.y);
    let min_pixel_size = min(pixel_size.x, pixel_size.y);

    let nearest_level = max(0.0, log2(max_derivative / min_pixel_size) + bias);

    let mip_exact = max(0.0, log2(sqrt(min_derivative * max_derivative) / min_pixel_size) + bias);
    let low_level = floor(mip_exact);
    let high_level = low_level + 1.0;
    let level_blend = fract(mip_exact);

    // **The texel-centre correction, and it is not a detail.** `uv / pixel_size` is texel coordinates,
    // `round(..) - 0.5` the centre of the nearest texel, and the difference between the two scaled down
    // by how large a texel is on screen - so a magnified surface lands exactly on centres, where the
    // answer is exact, and the correction fades out as the surface minifies, where the level is doing
    // the filtering instead.
    let uv_texel = uv / pixel_size;
    let texel_center = round(uv_texel) - 0.5;
    var texel_offset = uv_texel - texel_center;

    texel_offset = (texel_offset - 0.5) * pixel_size / texel_screen_size + 0.5;
    texel_offset = clamp(texel_offset, vec2<f32>(0.0), vec2<f32>(1.0));

    // The game's own rotated grid - `vec2(0.125, 0.375)` and its three rotations - scaled by this
    // atlas' texel here, once, rather than per tap and per branch.
    let taps = array<vec2<f32>, 4>(
        uv + vec2<f32>(0.125, 0.375) * pixel_size,
        uv + vec2<f32>(-0.125, -0.375) * pixel_size,
        uv + vec2<f32>(0.375, -0.125) * pixel_size,
        uv + vec2<f32>(-0.375, 0.125) * pixel_size,
    );

    return SampleGeometry(
        (texel_center + texel_offset) * pixel_size,
        nearest_level,
        taps,
        low_level,
        high_level,
        level_blend,
        smoothstep(min_pixel_size, min_pixel_size * 2.0, max_texel_size),
    );
}

/// The game's two sampling functions, chosen by its `UseRgss` flag, **fetching at levels the caller
/// already has**.
///
/// **Every fetch in here is a `textureSampleLevel`, and that is what makes it legal to call from inside
/// a branch.** An explicit level takes no derivative, so WGSL's uniform-control-flow rule - the one that
/// makes a `textureSample` under an `if` undefined behaviour however uniform the condition looks - does
/// not apply to it. Everything that *does* need a derivative is in [`SampleGeometry`], taken before any
/// branch is entered, and this function never calls `dpdx` or `dpdy`.
///
/// The two are ports of the game's own `sampleNearest` and `sampleRGSS`: the plain fetch at the
/// hardware's level with the texel centres corrected, and the four taps on the rotated grid blended
/// between the two levels the geometric mean straddles. Neither can be expressed with a sampler - a
/// sampler has one `anisotropy_clamp` and no way to say "four taps on a rotated grid at this exact
/// level", and nothing exposes the level at all.
fn sample_at_level(
    source: texture_2d<f32>,
    samp: sampler,
    geometry: SampleGeometry,
    use_rgss: u32,
) -> vec4<f32> {
    // The plain fetch is taken either way, because the game's `sampleRGSS` ends by blending its result
    // against this one rather than choosing between the two.
    let plain = textureSampleLevel(source, samp, geometry.nearest_uv, geometry.nearest_level);

    // **And the four-tap grid is skipped when it cannot change the answer**, which is the magnified case -
    // the surfaces the player is looking at most closely.
    //
    // `rgss_blend` is `smoothstep(min_pixel_size, min_pixel_size * 2.0, max_texel_size)`, so it is
    // *exactly* zero whenever a texel covers at least a pixel - and `mix(plain, rgss, 0.0)` is `plain`,
    // bit for bit. Those eight fetches were being spent to compute a value that was then multiplied by
    // zero: at the game's `Fancy` setting a magnified fragment cost **nine** fetches to return the first
    // one, and a block face sixteen texels across stops being magnified only once it is a few blocks
    // away, so this is most of the near and middle field rather than a corner case.
    //
    // The test is `rgss_blend` and not `magnified` on purpose. `magnified` is `|dpdx.x| < texel &&
    // |dpdy.y| < texel`, which is a different question - it can be true while `max_texel_size`, the
    // length of the whole derivative vector, is past `min_pixel_size`, and there the blend is not zero and
    // the taps do matter. Skipping on `magnified` would be a picture change; skipping on the blend being
    // zero is not.
    if use_rgss == 0u || geometry.rgss_blend == 0.0 {
        return plain;
    }

    var low = vec4<f32>(0.0);
    var high = vec4<f32>(0.0);

    for (var i = 0u; i < 4u; i = i + 1u) {
        low = low + textureSampleLevel(source, samp, geometry.taps[i], geometry.low_level);
        high = high + textureSampleLevel(source, samp, geometry.taps[i], geometry.high_level);
    }

    let rgss = mix(low * 0.25, high * 0.25, geometry.level_blend);

    return mix(plain, rgss, geometry.rgss_blend);
}

var<immediate> section_pos: SectionPosition;

/// Everything the fragment stage needs from the derivatives, for the atlas it is going to sample.
///
/// `atlas_texel` is one texel of that atlas as a fraction of it - `section_pos.texel_ours` or
/// `section_pos.texel_game` - and it is the only thing about the two entry points that differs before the
/// sampling itself: the level-of-detail floor, the bias and the geometry are the same arithmetic for both.
///
/// **The `select` over the two texel sizes is not in here.** It is a `select` on a varying, and the whole
/// point of the second entry point is that it does not exist there.
struct FragmentGeometry {
    geometry: SampleGeometry,
    /// Whether this fragment is magnified, which picks the sampler.
    magnified: bool,
};

fn fragment_geometry(in: VertexResult, atlas_texel: f32) -> FragmentGeometry {
    let magnified = is_magnified(in.tex_coords, atlas_texel);

    // The level-of-detail floor, which is what a bias could not express. See the note in `frag` for the
    // whole of it: four bits the bake wrote, and `LOD_FLOOR_IS_LEVEL_ZERO` picking which floor.
    const LOD_FLOOR_IS_LEVEL_ZERO: bool = false;

    let floor = select(
        0.0,
        select(64.0, f32(in.lod_floor), !LOD_FLOOR_IS_LEVEL_ZERO),
        in.animated == 1u && !magnified,
    );

    let bias = section_pos.lod_bias - floor;

    return FragmentGeometry(
        sample_geometry(in.tex_coords, vec2<f32>(atlas_texel), bias),
        magnified,
    );
}

/// Everything the fragment stage does **after** it has a texel: the ambient occlusion, the light, the
/// colour, the cutout test and the fog.
///
/// Shared by both entry points, which is what keeps the second one from being a second copy of the shader:
/// the only thing that differs between them is which texture the texel came out of.
fn shade(in: VertexResult, texel: vec4<f32>) -> vec4<f32> {
    // The ambient-occlusion corner, blended across the quad's four corners - and then the curve.
    //
    // This read `0.6 + 0.4 * corner` for as long as the vertices carried a *brightness* step, and that
    // is where the corners went wrong: Minecraft's own corner value is the average of four
    // `getShadeBrightness` samples, each `0.2` for a block that fills its whole block and `1.0` for
    // everything else - so its range is `0.2 .. 1.0` in five steps, and a shadowed corner in vanilla is
    // five times darker than the brightest one. Against a curve that starts at `0.6`, the darkest a
    // corner could get was 0.6: the shading was there, in the right places, and only about a third as
    // deep as the game's - which reads as "the ambient occlusion is too weak" rather than as anything
    // missing.
    //
    // The vertex carries the *count* - 0 to 4 of the four blocks that darken that corner - and the curve
    // is `1 - 0.2 * count`, which is exactly that average. The four corners are blended here, where
    // Minecraft's are what the rasterizer makes of four per-vertex colours: the curve is the game's, the
    // blend is this renderer's (bilinear over the corners rather than linear across two triangles, which
    // is why a face here has no diagonal seam through its shading).
    var occluders = mix(mix(in.ao3, in.ao4, in.light_uv.x), mix(in.ao2, in.ao1, in.light_uv.x), in.light_uv.y);
    var ao = 1.0 - 0.2 * occluders;

    // The light is a colour now, not a number: the game's lightmap has a colour in it (the sky light
    // goes blue at night, the darkness effect tints it), and the game's own shader multiplies it in as
    // it stands. Everything else is this renderer's: the vertex colour carries the tint and the face
    // shading, and the corner value is the ambient occlusion above.
    let col = in.color * vec4(in.light_color, 1.0) * vec4(ao, ao, ao, 1.0) * texel;

    // The cutout test, at the cutoff the layer being drawn declares.
    //
    // This read `if (col.a == 0.0f)` for as long as the atlas had one mip level, where "transparent"
    // and "exactly zero alpha" are the same texel. The atlas has a mip chain now (`ATLAS_MIP_LEVELS`,
    // sampled with `mipmap_filter: Linear`), and a hole in a leaf texture is only zero at level 0: at
    // any level above it, the hole is an *average* of leaves and gaps, a small non-zero alpha - so
    // nothing was discarded and the face was painted whole, at full strength, because this pass
    // replaces the target rather than blending into it. Leaves therefore came out solid beyond the
    // distance at which a sprite stops covering enough pixels to stay on level 0, and the boundary
    // between the two moved with the camera's distance, angle and field of view.
    //
    // **This shader is only bound for layers that are cut out** - the cutout layer here, and the
    // translucent one through the pipeline that names it. The solid layer draws with
    // `terrain_solid.wgsl`, which has no test, because the mere presence of the `discard` below is
    // what makes a driver drop early-Z for the entire pipeline. See `terrain_layers` in `graph.rs`.
    // **Minecraft's cutoff, which is two numbers and not one.** `RenderPipelines` declares
    // `pipeline/cutout_terrain` at `ALPHA_CUTOUT 0.5` and the mipped cutout family at `0.1`, and this
    // renderer has one cutout layer for both - so the number comes from the face's own pipeline through
    // the varying rather than from the layer alone. A mip of a leaf or a grass tuft is mostly the
    // low-alpha fringe the game keeps and 0.5 discards, which is why the two renderers' frames differed
    // most on exactly those sprites. See `UV_CUTOUT_MIPPED`.
    let cutoff = select(section_pos.alpha_cutout, 0.1, in.mipped != 0u);

    if (col.a < cutoff) {
        discard;
    }

    // And the fog, in the same place the game's own fragment shader applies it: after the alpha test, so
    // what is tested is the texture's own alpha rather than a fogged one. It is applied to the whole
    // terrain pass, which is why the far edge of the loaded world fades into the sky instead of ending.
    return apply_fog(
        col,
        in.fog_distances.x,
        in.fog_distances.y,
        fog.environmental_start,
        fog.environmental_end,
        fog.render_distance_start,
        fog.render_distance_end,
        fog.color,
    );
}

@fragment
fn frag(
    in: VertexResult
) -> @location(0) vec4<f32> {
    // Which atlas this face samples, and the whole of what the flag does.
    //
    // A face whose sprite the game animates was baked with the game's coordinates and draws from the
    // game's atlas, which the game is already animating; everything else draws from this side's copy
    // of its sprite.
    //
    // **One texel of it, and the one `select` that stays.** This picks a *number* the level is computed
    // from rather than a fetch, and it has to be decided before the derivatives are taken - so it cannot
    // be one of the branches below, and there is nothing in it to branch over. Built from the fields
    // that were already here rather than sent a second time: a duplicate of the same number is a second
    // thing that can disagree, and it cost four bytes of padding in an immediate whose size has to match
    // this struct exactly. **wgpu aborts the process when it does not** (`Not all immediate data
    // required by the pipeline has been set ... missing byte ranges: 48..52`), which is how the
    // duplicate was found.
    let pixel_size = select(
        vec2<f32>(section_pos.texel_ours, section_pos.texel_ours),
        vec2<f32>(section_pos.texel_game, section_pos.texel_game),
        in.game_atlas == 1u,
    );

    // **Every derivative, every level and every adjusted coordinate, taken once - here, and outside
    // every branch.** `dpdx` and `dpdy` are defined only in uniform control flow, and the choice below is
    // a per-vertex flag. See [`SampleGeometry`] for what this returns and for the two levels the game's
    // own functions are built on.
    let fg = fragment_geometry(in, min(pixel_size.x, pixel_size.y));

    // **Which of the game's three sampling methods is in force**, as real branches.
    //
    // Those are twenty-four combinations - two atlases, three samplers, RGSS or not - that this used to
    // reach by *evaluating all of them* and `select`ing between the results, on the argument that a
    // `textureSample` in a branch is undefined behaviour. **The argument does not apply to a fetch at an
    // explicit level**: `textureSampleLevel` takes no derivative, so it carries no uniformity
    // requirement, and the things that do - `dpdx` and `dpdy` - are all in `sample_geometry` above. So
    // the branches fetch only what they need, and nothing is computed per branch.
    //
    // `the_terrain_shaders_never_sample_a_texture_under_a_branch` in `graph.rs` holds the shape: it
    // counts the *auto-level* fetches - `Auto` and `Bias`, the two that ask the hardware for a level -
    // and requires none of them under a branch. A `textureSampleLevel` under one is not a finding; it is
    // what this is made of.
    var texel: vec4<f32>;

    if in.game_atlas == 1u {
        if fg.magnified {
            texel = sample_at_level(t_game_atlas, t_game_sampler_magnify, fg.geometry, section_pos.use_rgss);
        } else if in.animated == 1u {
            texel = sample_at_level(t_game_atlas, t_game_sampler_animated, fg.geometry, section_pos.use_rgss);
        } else {
            texel = sample_at_level(t_game_atlas, t_game_sampler, fg.geometry, section_pos.use_rgss);
        }
    } else {
        if fg.magnified {
            texel = sample_at_level(t_texture, t_sampler_magnify, fg.geometry, section_pos.use_rgss);
        } else if in.animated == 1u {
            texel = sample_at_level(t_texture, t_sampler_animated, fg.geometry, section_pos.use_rgss);
        } else {
            texel = sample_at_level(t_texture, t_sampler, fg.geometry, section_pos.use_rgss);
        }
    }

    return shade(in, texel);
}

/// **The same fragment stage with the atlas decided when the pipeline is built**, which is the one thing
/// that can remove the branch rather than execute it.
///
/// It samples the game's block atlas and nothing else, so `t_texture` and the three samplers that go with
/// it are not referenced here at all - and this is what the graph builds the terrain pipelines from
/// whenever [`crate::mc::block::faces_are_all_the_games`] says the arena holds no face that fell back.
/// The numbers, from a normal session: **204,878 faces baked against the game's atlas and 0 against this
/// side's**, so the two-atlas shader is the exceptional one and this is the common one.
///
/// **Why an entry point and not a pipeline constant.** With `override atlas: u32` the source would still
/// contain both atlases and both sampler sets; naga substitutes the override and the *backend compiler*
/// would then fold the branch away - reliably, but somewhere this repository cannot check. A second entry
/// point is smaller in the source itself, which is checkable: a test reads both shaders with naga and
/// asserts that this entry point reaches exactly one texture and its three samplers, and that `frag`
/// reaches both. See `the_single_atlas_entry_point_samples_one_atlas` in `graph.rs`.
///
/// The atlas is not the *sampler*, so the three-way choice between magnify, animated and plain is still
/// here - those are properties of the fragment, not of the build.
@fragment
fn frag_game_atlas(
    in: VertexResult
) -> @location(0) vec4<f32> {
    // `texel_game` is one texel of the game's atlas, which is the `pixel_size` this variant would have
    // selected anyway - so this is the same number `frag` computes, without the `select`.
    let fg = fragment_geometry(in, section_pos.texel_game);

    var texel: vec4<f32>;

    if fg.magnified {
        texel = sample_at_level(t_game_atlas, t_game_sampler_magnify, fg.geometry, section_pos.use_rgss);
    } else if in.animated == 1u {
        texel = sample_at_level(t_game_atlas, t_game_sampler_animated, fg.geometry, section_pos.use_rgss);
    } else {
        texel = sample_at_level(t_game_atlas, t_game_sampler, fg.geometry, section_pos.use_rgss);
    }

    return shade(in, texel);
}

@vertex
fn vert(
    @builtin(vertex_index) vi: u32,
    // **The index of this draw's record**, which is what the pass writes into `first_instance` of an
    // indirect dispatch entry and into the instance range of a plain `draw_indexed`. It used to *be*
    // the section's arena slot; the slot is in the record now, because a `multi_draw` cannot be told
    // anything per draw except through a buffer.
    @builtin(instance_index) instance: u32
) -> VertexResult {
//    var vert1_i = (vi >> 2) << 4;
//    var vert1_i = (vi << 2) & 0xfffffffc;
//    var vert1_i = ((vi >> 2u) << 2u)+base_vertex;

    let draw = section_draws[instance];
    let base_vertex = draw.word_base;
    var offset = vi & 3;
    var vert1_i = vi & ~3u;

    var id = ((vert1_i + offset) << 2u) + base_vertex;

    var vert1_base = ((vert1_i) << 2u) + base_vertex;

    // **The ambient-occlusion count of each corner of this quad, straight out of the vertex**: how many
    // of the four blocks around that corner fill their whole block. The curve is applied in the fragment
    // stage, where the four are blended - see there for why.
    //
    // Three bits, because four is as high as the count goes. Those bits used to be the position's "this
    // coordinate is exactly 16" flags, and the position used to hold its own count here; the two swapped
    // when the position needed eight more bits an axis than a byte could hold.
    var vert1_v3 = chunk_data[vert1_base + 2u];
    var vert2_v3 = chunk_data[vert1_base + 6u];
    var vert3_v3 = chunk_data[vert1_base + 10u];
    var vert4_v3 = chunk_data[vert1_base + 14u];

    var v1_ao = f32((vert1_v3 >> 29u) & 0x7u);
    var v2_ao = f32((vert2_v3 >> 29u) & 0x7u);
    var v3_ao = f32((vert3_v3 >> 29u) & 0x7u);
    var v4_ao = f32((vert4_v3 >> 29u) & 0x7u);

    var uv = array<vec2<f32>,4>(
            vec2(1.0,1.0),
            vec2(0.0,1.0),
            vec2(0.0,0.0),
            vec2(1.0,0.0));

    var light_uv = uv[vi & 3];

    var vr: VertexResult;
    vr.ao1 = v1_ao;
    vr.ao2 = v2_ao;
    vr.ao3 = v3_ao;
    vr.ao4 = v4_ao;

    vr.light_uv = light_uv;

    var v1 = chunk_data[id];
    var v2 = chunk_data[id + 1u];
    var v3 = chunk_data[id + 2u];
    var v4 = chunk_data[id + 3u];

    // **Sixteen bits an axis, at 1/2048 of a block.** That is the scale VulkanMod's compressed terrain
    // vertex uses for the same three numbers (`POSITION_INV = 1.0 / 2048.0`), and it is eight bits an axis
    // finer than what this format held before. The low half of the first word is x and its high half is y;
    // z is the low half of the second, with u above it.
    //
    // There is no "this coordinate is exactly 16" flag to test any more. Eight bits could not name 16, so
    // each axis carried one; sixteen bits reach 32 blocks, and 16.0 is 32768 like any other coordinate.
    var x: f32 = f32(v1 & 0xffffu) * 0.00048828125;
    var y: f32 = f32(v1 >> 16u) * 0.00048828125;
    var z: f32 = f32(v2 & 0xffffu) * 0.00048828125;

    // The colour, in the fourth word's low three bytes - and the light in its top one. Both moved when the
    // position took the space they were in.
    var r: u32 = v4 & 0xffu;
    var g: u32 = (v4 >> 8u) & 0xffu;
    var b: u32 = (v4 >> 16u) & 0xffu;

    vr.color = vec4(f32(r) * 0.003921568627451, f32(g) * 0.003921568627451, f32(b) * 0.003921568627451, 1.0);

    // The quad's first vertex carries a single occlusion count in the same byte the four corners come
    // from, and **it is not read**: the fragment stage blends the four corner counts instead
    // (`ao1..ao4`, by where in the quad the pixel is), which is the whole of the ambient occlusion.
    // There was a varying for it, and the varying was written and never read, so both are gone - see
    // the note on `VertexResult` for why an unused varying is worth removing rather than zeroing.

    // Which atlas these coordinates are in, and therefore what one step of the sixteen bits the
    // vertex holds them in is worth.
    //
    // There are two, and they are not the same size. This side's own atlas is 2048 wide
    // (`ATLAS_DIMENSIONS`), so a step is a texel of it and the decode is a division by 2048. The
    // game's coordinates are the game's own, `0..1` over an atlas of whatever size the game stitched,
    // and are stored as the sixteen bits filled edge to edge (`UV_GAME_SCALE`) - which is also what
    // `Atlas::sprite_rects` measured them in. A face is baked for one of the two and says which, so
    // the scale is picked here by the same flag rather than being one number for both.
    let game_atlas = (v3 >> 16u) & 1u;
    // Bit 1 of the same ten: whether the game animates this face's sprite. See `UV_ANIMATED`.
    let animated = (v3 >> 17u) & 1u;
    // The level-of-detail floor: four bits at bit 18, written by the bake. See `UV_LOD_FLOOR_SHIFT`.
    let lod_floor = (v3 >> 18u) & 0xfu;
    // And the mipped-cutout bit, two bits above the floor's four: bit 6 of the flag half, which is bit
    // 22 of `v3`. See `UV_CUTOUT_MIPPED`.
    let mipped = (v3 >> 22u) & 1u;
    let uv_scale = select(0.00048828125, 1.0 / 65535.0, game_atlas == 1u);

    // **A half-texel shift, under test.** Both are sent from the renderer rather than written here,
    // because a shift is half a texel only once the atlas size is known and the game's atlas is not a
    // fixed size: the stitcher packed `blocks.png` at 2048x2048 on some launches of the runs this was
    // written against and 1024x1024 on others, while this side's own atlas is always 2048.
    //
    // What it is for: a face's coordinates run from one edge of its sprite to the other, so a sample lands
    // *between* texels rather than on one, and at magnification that is two texels blended at every point.
    // Half a texel would put the samples on texel centres instead.
    //
    // **The shift the two atlases need is selected with the same flag that selects the atlas**, and it has
    // to be: the coordinates are normalised by then, so the only thing that says how big a texel is is the
    // atlas the sample is going to come out of.
    let half_texel = select(
        section_pos.half_texel_ours,
        section_pos.half_texel_game,
        game_atlas == 1u,
    );

    var u: f32 = f32((v2 >> 16u) & 0xffffu) * uv_scale + half_texel;
    var v: f32 = f32(v3 & 0xffffu) * uv_scale + half_texel;

    // **`v` is written through with no `1.0 - v`, and that is not an omission.** The flip everybody
    // remembers - `texCoord0.y = 1.0 - texCoord0.y;` - was the OpenGL sampler's convention: GL's texture
    // origin is the *bottom* of the picture, while a Minecraft UV is measured **downward from the top row**.
    // Three things say so, and all three are checkable in this tree:
    //
    //  - the model's own tables put the sprite's `minV` on the *top* corners of a face: the game's
    //    `CuboidFace.UVs.getVertexV` gives vertices 0 and 3 `minV`, and `defaultFaceUV(SOUTH)` is
    //    `(from.x, 16 - to.y, to.x, 16 - from.y)` - `16 - to.y` is the top of the element. `sprite_vertices`
    //    and `default_face_uv` in `mc/block.rs` are those two tables transcribed, and the README records
    //    what turning them produced: every side face of every `defaultFaceUV` model upside down;
    //  - `TextureAtlasSprite#v0 = (y + padding) / atlasHeight`, measured from the *top* of the atlas, so
    //    `minV` is the sprite's first row in the image - and the game's own atlas is built by *rendering*
    //    sprites into it under `ortho2D(0, w, 0, h)` with that same top-down `y`, which lands the sprite's
    //    image row on the texel row `v0` names, the same orientation an upload of the PNG would give;
    //  - this backend's sampler is wgpu's, and its texture origin is the first row of what was uploaded -
    //    which is the PNG's first row. That is the convention above, so sampling it directly is what is
    //    upright; 26.1's own `terrain.vsh` writes `texCoord0 = UV0;` and its `terrain.fsh` samples it with
    //    no flip either, and `GlCommandEncoder#writeToTexture` uploads rows in order - the flip is gone from
    //    vanilla too, because 26.1 samples through the same top-left convention this side does.
    //
    // Adding one here would therefore *introduce* the mirror: the green of a grass block's side at the
    // bottom of the block, on every face of every model.

    // No flag test and no `16.0` here any more: see the decode above for where those three bits went.
    var pos = vec3<f32>(x, y, z);

    var section_origin = vec3<f32>(f32(draw.x), f32(draw.y), f32(draw.z)) * 16.0;
    var world_pos = pos + section_origin;

    vr.pos = mat4_persp * mat4_view * mat4_model * vec4(world_pos, 1.0);

    // The two patches every Minecraft GLSL shader gets appended to its `main` by this backend's
    // preprocessing (see `preprocessing.rs`), written out here because this shader is not one of them:
    // `OPENGL_TO_WGPU_MATRIX_AST` and `EMULATE_GL_CLIP_SPACE_AST`.
    //
    // Minecraft's projections are OpenGL's, so they expect a clip space whose `y = +1` is the top of
    // the render target and whose depth runs -1..1, and this backend gives every *its* shaders the
    // same two fixes rather than converting the matrices. Without them the terrain lands mirrored
    // about the horizontal plane - the ground in the sky - which is a picture that says what happened
    // only if you know the patch exists.
    vr.pos.z = 0.5 * vr.pos.z + 0.5 * vr.pos.w;
    vr.pos.y = -vr.pos.y;

    vr.tex_coords = vec2<f32>(u, v);
    vr.game_atlas = game_atlas;
    vr.animated = animated;
    vr.lod_floor = lod_floor;
    vr.mipped = mipped;

    // The lighting, fetched the way the game's own terrain shader fetches it: one lightmap texel per
    // vertex, and the rasterizer interpolates the *colour*, which is what `vertexColor = Color *
    // sample_lightmap(Sampler2, UV2)` comes to once the quad is rasterized.
    //
    // The texel is the light pair itself. `UV2` in the game's vertex format is a nibble per light
    // scaled by sixteen - so 0..255 - and `sample_lightmap` divides by 256 and adds half a texel:
    //
    //     vec4 sample_lightmap(sampler2D lightMap, ivec2 uv) {
    //         return texture(lightMap, clamp((uv / 256.0) + 0.5 / 16.0, vec2(0.5 / 16.0), vec2(15.5 / 16.0)));
    //     }
    //
    // which for a nibble is `nibble / 16 + 1/32` - the centre of texel `nibble`. The clamp in the
    // game's line is a formality for a nibble, so there is none here. A vertex shader has no
    // derivatives, so the level is explicit; the lightmap has one level to fetch from.
    var light_pair = vec2<f32>(f32(v4 & 15u), f32((v4 >> 4u) & 15u));
    var light_texel = light_pair * 0.0625 + 0.03125;
    vr.light_color = textureSampleLevel(t_lightmap, t_lightmap_sampler, light_texel, 0.0).rgb;

    // How far this vertex is from the camera, which is what the fog is a function of.
    //
    // `world_pos` is relative to the camera's *section*, so the camera's own offset inside that section
    // has to come off it to get the camera-relative position the game measures from - the same sum the
    // game's `terrain.vsh` writes out in full, `Position + (ChunkPosition - CameraBlockPos) +
    // CameraOffset`. The two distances are the game's own (`fog.glsl`):
    //
    //     float fog_spherical_distance(vec3 pos) { return length(pos); }
    //     float fog_cylindrical_distance(vec3 pos) {
    //         float distXZ = length(pos.xz);
    //         float distY = abs(pos.y);
    //         return max(distXZ, distY);
    //     }
    //
    // and they are why the offset is in the shader's world axes rather than folded into the view matrix:
    // the horizontal distance is a *world* one, so rotating it would measure the wrong thing.
    var camera_relative = world_pos - fog.camera_offset;
    var spherical = length(camera_relative);
    var cylindrical = max(length(camera_relative.xz), abs(camera_relative.y));
    vr.fog_distances = vec2<f32>(spherical, cylindrical);

    return vr;
}

/// The game's fog curve, transliterated from `minecraft:fog.glsl`.
///
/// A line from 0 at `start` to 1 at `end`, and the fog is the *larger* of the two - one for the fog of
/// the environment (water, lava, blindness, darkness, the biome's own) and one for the render distance,
/// which is what keeps the far edge of the loaded world from being a hard line.
fn linear_fog_value(vertex_distance: f32, fog_start: f32, fog_end: f32) -> f32 {
    if (vertex_distance <= fog_start) {
        return 0.0;
    }

    if (vertex_distance >= fog_end) {
        return 1.0;
    }

    return (vertex_distance - fog_start) / (fog_end - fog_start);
}

/// The game's `apply_fog`: the strength is the larger of the two curves, and the colour's own alpha
/// scales how much of it is mixed in - which is how the game fades fog out entirely.
fn apply_fog(
    color: vec4<f32>,
    spherical: f32,
    cylindrical: f32,
    environmental_start: f32,
    environmental_end: f32,
    render_distance_start: f32,
    render_distance_end: f32,
    fog_color: vec4<f32>,
) -> vec4<f32> {
    var fog_value = max(
        linear_fog_value(spherical, environmental_start, environmental_end),
        linear_fog_value(cylindrical, render_distance_start, render_distance_end),
    );

    return vec4<f32>(mix(color.rgb, fog_color.rgb, fog_value * fog_color.a), color.a);
}
