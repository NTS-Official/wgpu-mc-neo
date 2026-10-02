struct VO {
    @builtin(position) pos: vec4<f32>,
    @location(0) tex_coords: vec2<f32>,
    @location(1) og_pos: vec3<f32>,
}

const PI = 3.14159265;

@group(0) @binding(0)
var<uniform> projection: mat4x4<f32>;

@group(0) @binding(1)
var<uniform> view: mat4x4<f32>;

@group(0) @binding(2)
var<uniform> model: mat4x4<f32>;

@group(0) @binding(3) var sun_texture: texture_2d<f32>;
@group(0) @binding(4) var moon_texture: texture_2d<f32>;

@group(0) @binding(5) var sample: sampler;

struct PushConstants {
    angle: f32,
    brightness: f32,
    star_shimmer: f32,
    fog_start: f32,
    fog_end: f32,
    fog_shape: f32,
    fog_color_r: f32,
    fog_color_g: f32,
    fog_color_b: f32,
    fog_color_a: f32,
    color_modulator_r: f32,
    color_modulator_g: f32,
    color_modulator_b: f32,
    dimension_fog_color_r: f32,
    dimension_fog_color_g: f32,
    dimension_fog_color_b: f32,
    dimension_fog_color_a: f32,
}

var<immediate> data: PushConstants;

fn rotateX(degrees: f32) -> mat4x4<f32> {
    var theta = radians(degrees);
    var c = cos(theta);
    var s = sin(theta);
    return mat4x4<f32>(
        vec4(1.0, 0.0, 0.0, 0.0),
        vec4(0.0, c, s, 0.0),
        vec4(0.0, -s, c, 0.0),
        vec4(0.0, 0.0, 0.0, 1.0),  
    );
}

fn rotateY(degrees: f32) -> mat4x4<f32> {
    var theta = radians(degrees);
    var c = cos(theta);
    var s = sin(theta);
    return mat4x4<f32>(
        vec4(c, 0.0, -s, 0.0),
        vec4(0.0, 1.0, 0.0, 0.0),
        vec4(s, 0.0, c, 0.0),
        vec4(0.0, 0.0, 0.0, 1.0),  
    );
}

fn radians(degrees: f32) -> f32 {
    return degrees * (PI/180.0);
}

fn identity() -> mat4x4<f32> {
    return mat4x4<f32>(
        vec4(1.0, 0.0, 0.0, 0.0),
        vec4(0.0, 1.0, 0.0, 0.0),
        vec4(0.0, 0.0, 1.0, 0.0),
        vec4(0.0, 0.0, 0.0, 1.0), 
    );
}

@vertex
fn vert(
    @location(0) pos: vec3<f32>,
    @location(1) tex_coords: vec2<f32>
) -> VO {
    var vo: VO;
    vo.og_pos = pos;

    var transformation_matrix = rotateY(-90.0) * rotateX(data.angle * 360.0);
    var dir = transformation_matrix * vec4<f32>(pos, 1.0);

    vo.pos = projection * view * vec4<f32>(dir.xyz, 1.0);
    vo.tex_coords = tex_coords;
    return vo;
}

@fragment
fn frag(in: VO) -> @location(0) vec4<f32> {
    // **Both are sampled, and the sign of `og_pos.y` picks between the results.**
    //
    // What was here instead read well and was undefined behaviour:
    //
    //     if (in.og_pos.y > 0.0) { return textureSample(sun_texture, sample, in.tex_coords); }
    //     else                   { return textureSample(moon_texture, sample, in.tex_coords); }
    //
    // `textureSample` takes its level of detail from the derivatives of its coordinates, and WGSL
    // defines those only in *uniform* control flow - so a `textureSample` under an `if` is undefined,
    // whatever the condition looks like. It is not uniform here either: `og_pos` is an interpolated
    // varying, so a quad straddling the horizon takes both sides of that branch.
    //
    // The cost of the fix is one extra fetch over a patch of sky a few dozen pixels across; the two
    // textures are the sun's and the moon's. `select(moon, sun, ..)` rather than `mix`, because this is a
    // choice and not a blend: a half-way value would be the sun bleeding through the moon at every edge.
    let sun = textureSample(sun_texture, sample, in.tex_coords);
    let moon = textureSample(moon_texture, sample, in.tex_coords);

    return select(moon, sun, in.og_pos.y > 0.0);
}