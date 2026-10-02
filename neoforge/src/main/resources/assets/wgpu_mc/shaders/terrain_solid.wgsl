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

// The magnification pair: `Nearest` magnification with no anisotropy, for surfaces being stretched, where
// `t_sampler` and `t_game_sampler` above are the anisotropic pair for surfaces being squeezed. wgpu will
// not put both behaviours in one sampler; see `atlas_magnify_sampler` in `atlas.rs` and the fuller note in
// `terrain.wgsl`, which this file is the solid-layer half of. The two must pick the same way or the solid
// layer and the cutout layer would filter differently across one seam.
@group(0) @binding(10) var t_game_sampler_magnify: sampler;
@group(0) @binding(11) var t_sampler_magnify: sampler;

// **A minified animated face gets its own sampler, because neither pair above is right for it.** The
// anisotropic one cannot be used beside a `Nearest` mip filter (a validation error, and the mip filter is
// a player's switch), and `Nearest` magnification of a coarse level is what turns a moving sprite into one
// flat patch of its own running average - the "bright spots magnified into one big bright tile" a player
// described. Same filters as `t_sampler` and `t_sampler_magnify` except for the anisotropy.
@group(0) @binding(12) var t_game_sampler_animated: sampler;
@group(0) @binding(13) var t_sampler_animated: sampler;

/// Whether a fragment's coordinates are being **stretched** rather than squeezed. The two derivatives are
/// compared separately rather than through `fwidth`, which is their sum and would move the switch to half a
/// texel; see `terrain.wgsl` for the whole of why that matters.
fn is_magnified(coords: vec2<f32>, texel: f32) -> bool {
    let d = vec2<f32>(dpdx(coords).x, dpdy(coords).y);
    return abs(d.x) < texel && abs(d.y) < texel;
}

// The game's own two sampling functions, ported, **as one level-taking fetch and one geometry**. The
// full note is in `terrain.wgsl` and this is a copy - the two shaders draw one world, and a level or a
// sampler that differed between the layers would be a seam down the middle of it.
//
// What they do, in one line each: `sample_geometry` takes every derivative the two methods need and
// computes the levels from them, and `sample_at_level` fetches at those levels with nothing but
// `textureSampleLevel` - which is what lets the fragment stage branch on which atlas and which sampler
// a face wants, since an explicit level carries no uniformity requirement.
struct SampleGeometry {
    nearest_uv: vec2<f32>,
    nearest_level: f32,
    taps: array<vec2<f32>, 4>,
    low_level: f32,
    high_level: f32,
    level_blend: f32,
    rgss_blend: f32,
};

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

    let uv_texel = uv / pixel_size;
    let texel_center = round(uv_texel) - 0.5;
    var texel_offset = uv_texel - texel_center;

    texel_offset = (texel_offset - 0.5) * pixel_size / texel_screen_size + 0.5;
    texel_offset = clamp(texel_offset, vec2<f32>(0.0), vec2<f32>(1.0));

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

fn sample_at_level(
    source: texture_2d<f32>,
    samp: sampler,
    geometry: SampleGeometry,
    use_rgss: u32,
) -> vec4<f32> {
    let plain = textureSampleLevel(source, samp, geometry.nearest_uv, geometry.nearest_level);

    // **And the four-tap grid is skipped when it cannot change the answer.** `rgss_blend` is
    // `smoothstep(min_pixel_size, min_pixel_size * 2.0, max_texel_size)`, so it is exactly zero whenever a
    // texel covers at least a pixel - and `mix(plain, rgss, 0.0)` is `plain`. Those eight fetches were
    // being spent to compute a value that was then multiplied by zero. See `terrain.wgsl` for the whole of
    // it, including why the test is not `magnified`.
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

// **What one draw of this pass is, indexed by `instance_index`.** The same four members and the same
// two hundred bytes of reasoning as `terrain.wgsl`, which is where the note is: `instance_index` is the
// only channel a `multi_draw_indexed_indirect` has for anything per draw, and the two paths - batched
// and one call per section - read this same record, so which one drew the frame is not visible in it.
struct SectionDraw {
    x: i32,
    y: i32,
    z: i32,
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
    // Whether the game animates this face's sprite: bit 1 of the same ten, `UV_ANIMATED`. Flat for the same
    // reason, and it is what scopes the level-of-detail offset in the fragment stage - a coarse mip of a
    // *moving* sprite is a moving average and a still one's is a fixed pyramid. The full note is in
    // `terrain.wgsl`, which this file is the solid-layer half of and whose values have to match.
    //
    // 21 and not 20: 20 is `fog_distances` above.
    @interpolate(flat) @location(21) animated: u32,
    // How many levels this face may be pushed down the chain, decoded in the vertex stage because the
    // bits live in `v3` and `v3` is a vertex-local. Flat for the same reason the two above are.
    @interpolate(flat) @location(22) lod_floor: u32
};

// What one terrain draw is told about itself **beyond its own `SectionDraw`**: the alpha cutoff its
// layer asks for, and the five per-frame numbers about the two atlases. Seven `u32`s, twenty-eight
// bytes, and no padding - which is the size the pass declares for them (see `@pc_section_position` in
// `graph.yaml` and the sizes in `RenderGraph::new`).
//
// **The section's x, y and z are not here any more.** They are per draw, and the batched path is one
// call for thousands of draws - so they are in `section_draws`, which the vertex stage indexes by
// `instance_index`. The `SectionDraw` struct above `chunk_data` carries the whole of it, and
// `terrain.wgsl` documents it.
struct SectionPosition {
    // **Declared and never read.** Minecraft's own `SOLID_TERRAIN` defines no `ALPHA_CUTOUT` at all,
    // and the point of this file is that neither does it - there is no test at the fragment stage, so
    // a driver keeps early-Z. The field is here only because this push constant block is one layout
    // shared with `terrain.wgsl`, and a struct that left it out would be a different size.
    alpha_cutout: f32,
    // The level-of-detail bias, read by the two atlas fetches below. See the same member in
    // `terrain.wgsl`, which documents why it is an immediate rather than a constant.
    //
    // **Read here even though the alpha cutoff above is not**, and the difference is worth noting: the
    // cutoff exists only for the fragment stage's test, which this shader has none of, while the bias
    // changes what the *texture* fetch returns, so both shaders' fragment stages read it.
    lod_bias: f32,
    // Half a texel for this side's own atlas and for the game's, both sent from the renderer because a
    // shift is half a texel only once the atlas size is known - and the game's atlas is not a fixed size.
    // Zero disables it. The full note is in `terrain.wgsl`, which is the other half of this pair: the two
    // have to carry the same values or one frame holds two sample grids.
    half_texel_ours: f32,
    half_texel_game: f32,
    // One texel of each atlas, as a fraction of it, for the magnification test above. Sent because the
    // game's atlas is whatever its stitcher packed and this side's is `ATLAS_DIMENSIONS`; see `terrain.wgsl`.
    texel_ours: f32,
    texel_game: f32,
    // The game's TextureSize, per atlas, and its UseRgss flag. The two sampling functions these feed are in
    // `terrain.wgsl`, which carries the full note; this file has the same struct because the two shaders
    // draw one world, and an immediate of a different size would be a validation error rather than a bug in
    // either of them.
    use_rgss: u32,
};

var<immediate> section_pos: SectionPosition;

@vertex
fn vert(
    @builtin(vertex_index) vi: u32,
    // The index of this draw's record. See `SectionDraw` above for why it is not the arena slot any
    // more, and `terrain.wgsl` for the whole of it.
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
    let uv_scale = select(0.00048828125, 1.0 / 65535.0, game_atlas == 1u);

    // The half-texel shift, which is under test and is explained in full in `terrain.wgsl`. Selected with
    // the same flag that selects the atlas, because the coordinates are normalised by now and the atlas is
    // the only thing that says how big a texel is.
    let half_texel = select(
        section_pos.half_texel_ours,
        section_pos.half_texel_game,
        game_atlas == 1u,
    );

    var u: f32 = f32((v2 >> 16u) & 0xffffu) * uv_scale + half_texel;
    var v: f32 = f32(v3 & 0xffffu) * uv_scale + half_texel;

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

/// Everything the fragment stage needs from the derivatives, for the atlas it is going to sample.
///
/// The same shape as `terrain.wgsl`'s - one of each of these two functions per shader, because the two
/// draw one world and the arithmetic has to match. See that file for the whole note.
struct FragmentGeometry {
    geometry: SampleGeometry,
    magnified: bool,
};

fn fragment_geometry(in: VertexResult, atlas_texel: f32) -> FragmentGeometry {
    let magnified = is_magnified(in.tex_coords, atlas_texel);

    // The level-of-detail floor. The full note is in `terrain.wgsl`, and `LOD_FLOOR_IS_LEVEL_ZERO` has
    // to agree with it: these two shaders draw one world.
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

/// Everything the fragment stage does **after** it has a texel: the ambient occlusion, the light and the
/// fog.
///
/// **There is no cutout test in here, and that is the whole reason this file exists.** The solid layer is
/// most of the screen, and a `discard` anywhere in a fragment shader is what makes a driver give up on
/// early-Z and hierarchical-Z for the *whole pipeline* - the hardware cannot know whether a fragment will
/// be thrown away until the shader has run, so it has to run it, and one test that can never fire
/// therefore costs exactly what one that always does. That is what drawing the solid layer through a
/// shader built for the cutout layer cost, and it is why the cutoff is not merely `0.0` here: the branch
/// is gone rather than dead.
///
/// Minecraft's own `SOLID_TERRAIN` defines no `ALPHA_CUTOUT`, so it has no test either. Its
/// `CUTOUT_TERRAIN` declares `0.5`, and that one lives in `terrain.wgsl` - the two pipelines this side
/// draws the opaque group with, and the same split the game makes. See `terrain_layers` in `graph.rs`,
/// which is where a layer is paired with the pipeline that draws it.
fn shade(in: VertexResult, texel: vec4<f32>) -> vec4<f32> {
    var occluders = mix(mix(in.ao3, in.ao4, in.light_uv.x), mix(in.ao2, in.ao1, in.light_uv.x), in.light_uv.y);
    var ao = 1.0 - 0.2 * occluders;

    // The light is a colour now, not a number: the game's lightmap has a colour in it, and the game's
    // own shader multiplies it in as it stands. Everything else is this renderer's: the vertex colour
    // carries the tint and the face shading, and the corner value is the ambient occlusion above.
    let col = in.color * vec4(in.light_color, 1.0) * vec4(ao, ao, ao, 1.0) * texel;

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
    // Which atlas this face samples, and the whole of what the flag does. The full note is in
    // `terrain.wgsl`, and this is a copy of its shape: one `select` for the *number* the level is
    // computed from, then real branches over fetches at explicit levels.
    let pixel_size = select(
        vec2<f32>(section_pos.texel_ours, section_pos.texel_ours),
        vec2<f32>(section_pos.texel_game, section_pos.texel_game),
        in.game_atlas == 1u,
    );

    let fg = fragment_geometry(in, min(pixel_size.x, pixel_size.y));

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

/// The same fragment stage with the atlas decided when the pipeline is built. See `terrain.wgsl` for the
/// whole of why this is an entry point rather than a pipeline constant, and `graph.rs` for the test that
/// reads both shaders and asserts that this one reaches exactly one atlas.
@fragment
fn frag_game_atlas(
    in: VertexResult
) -> @location(0) vec4<f32> {
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
