// Forward raster: instanced, depth-tested meshes with directional diffuse light.
//
// Per-vertex stream (buffer 0): pos/normal/uv. Per-instance stream (buffer 1): the
// camera-relative model matrix (locations 3..6), its inverse-transpose normal
// matrix as three columns (7..9), and a tint color (10). `g.view_proj` and the
// directional light come from the frame-global uniform. Lighting is in render
// space, which shares world orientation (only translation is stripped), so a
// constant world-space light direction is correct and large-world-safe (ADR-0015).

struct Globals {
    view_proj: mat4x4<f32>,
    light_dir: vec4<f32>,    // xyz = normalized world-space direction TO the light
    light_color: vec4<f32>,  // rgb
    ambient: vec4<f32>,      // rgb
};

@group(0) @binding(0) var<uniform> g: Globals;
@group(0) @binding(1) var tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

struct VsIn {
    @location(0) pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) m0: vec4<f32>,
    @location(4) m1: vec4<f32>,
    @location(5) m2: vec4<f32>,
    @location(6) m3: vec4<f32>,
    @location(7) n0: vec4<f32>,
    @location(8) n1: vec4<f32>,
    @location(9) n2: vec4<f32>,
    @location(10) color: vec4<f32>,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) color: vec4<f32>,
};

@vertex
fn vs(in: VsIn) -> VsOut {
    let model = mat4x4<f32>(in.m0, in.m1, in.m2, in.m3);
    let nmat = mat3x3<f32>(in.n0.xyz, in.n1.xyz, in.n2.xyz);
    var out: VsOut;
    out.clip = g.view_proj * model * vec4<f32>(in.pos, 1.0);
    out.uv = in.uv;
    out.normal = normalize(nmat * in.normal);
    out.color = in.color;
    return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    let n = normalize(in.normal);
    let ndl = max(dot(n, normalize(g.light_dir.xyz)), 0.0);
    let detail = textureSample(tex, samp, in.uv).rgb;
    let lit = g.ambient.rgb + g.light_color.rgb * ndl;
    return vec4<f32>(detail * in.color.rgb * lit, 1.0);
}
