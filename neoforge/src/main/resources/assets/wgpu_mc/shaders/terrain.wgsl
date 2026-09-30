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
    @interpolate(flat) @location(22) lod_floor: u32
};

// What one terrain draw is told about itself: the section it draws, and the alpha cutoff its layer
// asks for. Three integers rather than a vec3i because an immediate has to be a struct for the HLSL
// backend (push-constant ... has non-struct type is what a bare vector gets), and four members are
// sixteen bytes with no padding - which is the size the pass declares for them (see
// `@pc_section_position` in `graph.yaml` and the sizes in `RenderGraph::new`).
//
// The section is **relative to the section the camera is in**, not absolute, and that is the whole of
// this renderer's positional precision: `x + 30000` in `f32` steps by four thousandths of a block, and
// the ground and the shadow lying on it are two surfaces whose depth test is decided by exactly those
// last bits. The view matrix carries the camera's offset inside its own section, so the big number is
// never formed - which is what vanilla's `terrain.vsh` does with
// `Position + (ChunkPosition - CameraBlockPos) + CameraOffset`. See `Scene::camera_section_pos`.
struct SectionPosition {
    x: i32,
    y: i32,
    z: i32,
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

var<immediate> section_pos: SectionPosition;

@vertex
fn vert(
    @builtin(vertex_index) vi: u32,
    @builtin(instance_index) base_vertex: u32
) -> VertexResult {
//    var vert1_i = (vi >> 2) << 4;
//    var vert1_i = (vi << 2) & 0xfffffffc;
//    var vert1_i = ((vi >> 2u) << 2u)+base_vertex;

    var offset = vi & 3;
    var vert1_i = vi & ~3u;

    var id = ((vert1_i + offset) << 2u) + base_vertex;

    var vert1_base = ((vert1_i) << 2u) + base_vertex;

    var vert1_v4 = chunk_data[vert1_base + 3u];
    var vert2_v4 = chunk_data[vert1_base + 7u];
    var vert3_v4 = chunk_data[vert1_base + 11u];
    var vert4_v4 = chunk_data[vert1_base + 15u];

    // The ambient-occlusion count of each corner of this quad, straight out of the vertex: how many of
    // the four blocks around that corner fill their whole block. The curve is applied in the fragment
    // stage, where the four are blended - see there for why.
    var v1_ao = f32((vert1_v4 >> 8u) & 0xff);
    var v2_ao = f32((vert2_v4 >> 8u) & 0xff);
    var v3_ao = f32((vert3_v4 >> 8u) & 0xff);
    var v4_ao = f32((vert4_v4 >> 8u) & 0xff);

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

    var x: f32 = f32(v1 & 0xffu) * 0.0625;
    var y: f32 = f32((v1 >> 8u) & 0xffu) * 0.0625;
    var z: f32 = f32((v1 >> 16u) & 0xffu) * 0.0625;

    var r: u32 = (v1 >> 24u) & 0xff;
    var g: u32 = (v2 & 0xff);
    var b: u32 = (v2 >> 8u) & 0xff;

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

    if(((v3 >> 29u) & 1u) == 1u) {
        x = 16.0;
    }

    if(((v3 >> 30u) & 1u) == 1u) {
        y = 16.0;
    }

    if((v3 >> 31u) == 1u) {
        z = 16.0;
    }
    var pos = vec3<f32>(x, y, z);

    var section_origin = vec3<f32>(f32(section_pos.x), f32(section_pos.y), f32(section_pos.z)) * 16.0;
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

@fragment
fn frag(
    in: VertexResult
) -> @location(0) vec4<f32> {
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

    // And the light is the lightmap's own colour - no curve of this renderer's is applied to it at all.
    var light = in.light_color;

    // Which atlas this face samples, and the whole of what the flag does.
    //
    // A face whose sprite the game animates was baked with the game's coordinates and draws from the
    // game's atlas, which the game is already animating; everything else draws from this side's copy
    // of its sprite.
    //
    // **Both are sampled, and the flag picks between the results.** `textureSample` takes an implicit
    // level of detail from the derivatives of its coordinates, and WGSL only defines those in *uniform*
    // control flow - a `textureSample` in a branch is undefined behaviour, full stop, whether or not the
    // condition happens to be the same for every fragment of a primitive. This was the obvious shape
    // instead:
    //
    //     var texel: vec4<f32>;
    //     if (in.game_atlas == 1u) { texel = textureSample(t_game_atlas, ...); }
    //     else                     { texel = textureSample(t_texture, ...); }
    //
    // and the flag *is* flat, so every fragment of one primitive takes the same branch and the
    // derivative is the one it would have had - on the hardware it was tried on. That is an argument
    // about the picture coming out right, not about the program being defined, and the difference
    // between the two is a driver that decides to execute both sides of a uniform branch, or one that
    // vectorises a quad across a primitive boundary. So the samples are hoisted out of the branch and
    // the choice is a `select` on the values: one instruction more, and no undefined behaviour.
    //
    // Both fetch the same coordinates with the same sampler *shape* - nearest within a level, a blend
    // between levels, the same address modes - so the pair cost the same work the branch did whenever
    // both were live, and the two mip chains are indexed identically because the atlases are the same
    // size. `select` on a `vec4<f32>` rather than `mix`, because this is a choice and not a blend: a
    // half-way value would be one atlas bleeding into the other at every sprite edge.
    // `textureSampleBias` rather than `textureSample`, with the immediate bias - zero unless a
    // diagnostic run moved it, and the two behave identically at zero. See the constant for what moving
    // it is for.
    //
    // **And each of the four fetches picks a sampler rather than being given one.** The pair is
    // magnification and minification, and which one applies is decided per fragment by the test below -
    // see `t_game_sampler_magnify` for why one sampler cannot do both jobs. The magnifying fetches use
    // plain `textureSampleBias` on a sampler whose magnification is `Nearest`; the bias is harmless there
    // because a magnified surface is at level 0 whatever the bias says (the sampler clamps at 0), and it
    // is meaningless to a `Nearest` filter anyway.
    let magnified = is_magnified(
        in.tex_coords,
        select(section_pos.texel_ours, section_pos.texel_game, in.game_atlas == 1u),
    );

    // **The level-of-detail floor, which is what a bias could not express.**
    //
    // A face's coordinates span its sprite's content - sixteen or thirty-two texels for a block - while the
    // mip chain under the atlas is built once for the whole session and runs down to the atlas' own width.
    // So a sprite stops being itself *in the atlas* long before the chain ends, and what is left below that
    // point is not the sprite at a smaller size but a running average of it. For a **scrolling,
    // frame-interpolated** sprite - water and lava are both - that average *moves* as the frames advance,
    // so the same pixel changes over time with nothing moving. That is the fluid shimmer, and it is why the
    // fluids shimmer and a static sprite never does: a still sprite's coarse levels are a consistent pyramid.
    //
    // **This began as a bias and the bias could not work.** `-4` was measured against the water: the water
    // shimmer went and the lava's stayed. Both flow sprites are 32x32 texels - measured, not assumed, after
    // an assumption that they differed was wrong - so one offset lands them at the same level, and what
    // differs is what is *in* those levels: `lava_flow` animates at `frametime 3` against a much slower
    // water and its frames differ far more. `-5` left the lava shimmering too.
    //
    // The reason is that a bias is a count of *levels*, and the level at which a sprite stops being itself
    // is a property of the *sprite* - so one number cannot serve two sprites of different sizes, and no
    // number at all can express "do not go past here". So the floor is computed where the sprite's size is
    // known, at bake time, and carried in the vertex: see `UV_LOD_FLOOR_SHIFT` and `sprite_level_floor`.
    //
    // **Magnification keeps its own zero, and the near field is why.** A magnified surface is at level 0
    // whatever any of this says, so a floor is meaningless there and a bias was actively harmful - the
    // revision that applied one to every surface sharpened the distance into a moiré.
    //
    // Four bits, so the field is what the bake wrote and nothing else has to agree with it.
    let floor = select(
        0.0,
        f32(in.lod_floor),
        in.animated == 1u && !magnified,
    );

    let bias = section_pos.lod_bias - floor;

    let texel_from_game = select(
        textureSampleBias(t_game_atlas, t_game_sampler, in.tex_coords, bias),
        textureSampleBias(t_game_atlas, t_game_sampler_magnify, in.tex_coords, bias),
        magnified,
    );
    let texel_from_ours = select(
        textureSampleBias(t_texture, t_sampler, in.tex_coords, bias),
        textureSampleBias(t_texture, t_sampler_magnify, in.tex_coords, bias),
        magnified,
    );
    let texel = select(texel_from_ours, texel_from_game, in.game_atlas == 1u);

    // The light is a colour now, not a number: the game's lightmap has a colour in it (the sky light
    // goes blue at night, the darkness effect tints it), and the game's own shader multiplies it in as
    // it stands. Everything else is this renderer's: the vertex colour carries the tint and the face
    // shading, and the corner value is the ambient occlusion above.
    let col = in.color * vec4(light, 1.0) * vec4(ao, ao, ao, 1.0) * texel;

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
    if(col.a < section_pos.alpha_cutout){
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
