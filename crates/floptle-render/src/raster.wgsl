// Forward raster: instanced, depth-tested meshes with directional diffuse light
// and a per-material base-color texture.
//
// Group 0 (shared, set once per frame): the camera/light globals + the sampler.
// Group 1 (per mesh): the base-color texture. Per-vertex stream (buffer 0):
// pos/normal/uv. Per-instance stream (buffer 1): camera-relative model matrix
// (locations 3..6), inverse-transpose normal matrix columns (7..9), tint (10).

struct Globals {
    view_proj: mat4x4<f32>,
    light_dir: vec4<f32>,    // xyz = normalized world-space direction TO the light
    light_color: vec4<f32>,
    ambient: vec4<f32>,
};

@group(0) @binding(0) var<uniform> g: Globals;
@group(0) @binding(1) var samp: sampler;
@group(1) @binding(0) var tex: texture_2d<f32>;

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
    let albedo = textureSample(tex, samp, in.uv).rgb * in.color.rgb;
    let lit = g.ambient.rgb + g.light_color.rgb * ndl;
    return vec4<f32>(albedo * lit, 1.0);
}

// Silhouette mask: solid 1.0 wherever the mesh covers a pixel. Rendered into a
// single-channel target; a post-pass edge-detects this into a selection outline
// that hugs the true silhouette (works for any shape).
@fragment
fn fs_mask(in: VsOut) -> @location(0) vec4<f32> {
    return vec4<f32>(1.0, 0.0, 0.0, 1.0);
}
