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
    @location(1) tex_coords2: vec2<f32>,
    @location(2) blend: f32,
    @location(3) normal: vec3<f32>,
    @location(4) world_pos: vec3<f32>,
    @location(6) section: u32,
    @location(7) ao: f32,
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
    @interpolate(flat) @location(17) int: u32,
    @location(18) color: vec4<f32>,
    // Which atlas `tex_coords` is in: bit 0 of the ten the vertex format reserves for an animated
    // texture. Flat, because it is a property of the face and not of the corner: every vertex of a
    // quad carries the same answer, and the fragment stage selects between the two atlases with it.
    // It is **not** branched on - see the sampling note in the fragment stage, where a `textureSample`
    // under an `if` would be undefined behaviour whatever this varying says.
    @interpolate(flat) @location(19) game_atlas: u32
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
    // **Declared and never read.** Minecraft's own `SOLID_TERRAIN` defines no `ALPHA_CUTOUT` at all,
    // and the point of this file is that neither does it - there is no test at the fragment stage, so
    // a driver keeps early-Z. The field is here only because this push constant block is one layout
    // shared with `terrain.wgsl`, and a struct that left it out would be a different size.
    alpha_cutout: f32,
};

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
    vr.int = vi & 3;
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

    // This vertex's own occlusion count, unblended. The fragment stage uses the four corner counts
    // instead (`ao1..ao4`, blended by where in the quad the pixel is) - this one is the single value
    // the quad's first vertex carries, kept because the varying exists, not because anything reads it.
    var ao: f32 = f32((v4 >> 8u) & 0xff);

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
    let uv_scale = select(0.00048828125, 1.0 / 65535.0, game_atlas == 1u);

    var u: f32 = f32((v2 >> 16u) & 0xffffu) * uv_scale;
    var v: f32 = f32(v3 & 0xffffu) * uv_scale;

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
    vr.tex_coords2 = vec2(0.0, 0.0);
    vr.game_atlas = game_atlas;
    vr.world_pos = world_pos;
    vr.ao = ao;

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

    vr.blend = 0.0;

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
    let texel_from_game = textureSample(t_game_atlas, t_game_sampler, in.tex_coords);
    let texel_from_ours = textureSample(t_texture, t_sampler, in.tex_coords);
    let texel = select(texel_from_ours, texel_from_game, in.game_atlas == 1u);

    // The light is a colour now, not a number: the game's lightmap has a colour in it (the sky light
    // goes blue at night, the darkness effect tints it), and the game's own shader multiplies it in as
    // it stands. Everything else is this renderer's: the vertex colour carries the tint and the face
    // shading, and the corner value is the ambient occlusion above.
    let col = in.color * vec4(light, 1.0) * vec4(ao, ao, ao, 1.0) * texel;

    // **There is no cutout test here, and that is the whole reason this file exists.**
    //
    // The solid layer is most of the screen, and a `discard` anywhere in a fragment shader is what
    // makes a driver give up on early-Z and hierarchical-Z for the *whole pipeline* - the hardware
    // cannot know whether a fragment will be thrown away until the shader has run, so it has to run
    // it, and one test that can never fire therefore costs exactly what one that always does. That is
    // what drawing the solid layer through a shader built for the cutout layer cost, and it is why
    // the cutoff is not merely `0.0` here: the branch is gone rather than dead.
    //
    // Minecraft's own `SOLID_TERRAIN` defines no `ALPHA_CUTOUT`, so it has no test either. Its
    // `CUTOUT_TERRAIN` declares `0.5`, and that one lives in `terrain.wgsl` - the two pipelines this
    // side draws the opaque group with, and the same split the game makes. See `terrain_layers` in
    // `graph.rs`, which is where a layer is paired with the pipeline that draws it.

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
