// Forward raster: instanced, depth-tested meshes with directional diffuse light
// and a per-material base-color texture.
//
// Group 0 (shared, set once per frame): the camera/light globals.
// Group 1 (per mesh/material): the base-color texture + its sampler (so each texture
// chooses its own filtering / wrap mode). Per-vertex stream (buffer 0):
// pos/normal/uv. Per-instance stream (buffer 1): camera-relative model matrix
// (locations 3..6), inverse-transpose normal matrix columns (7..9), tint (10).

struct Globals {
    view_proj: mat4x4<f32>,
    light_dir: vec4<f32>,    // xyz = normalized world-space direction TO the light
    light_color: vec4<f32>,
    ambient: vec4<f32>,
};

@group(0) @binding(0) var<uniform> g: Globals;
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;

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
    @location(11) emissive: vec4<f32>,  // rgb, a = strength
    @location(12) specular: vec4<f32>,  // rgb, a = strength
    @location(13) params: vec4<f32>,    // shininess, rim_strength, unlit, ambient_mul
    @location(14) rim: vec4<f32>,       // rgb
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) color: vec4<f32>,
    // The fragment's position relative to the camera (the model matrix is already
    // camera-relative, ADR-0015), so the camera sits at the origin — view dir is
    // just -normalize(view_pos). Used for specular + rim.
    @location(3) view_pos: vec3<f32>,
    @location(4) emissive: vec4<f32>,
    @location(5) specular: vec4<f32>,
    @location(6) params: vec4<f32>,
    @location(7) rim: vec4<f32>,
};

@vertex
fn vs(in: VsIn) -> VsOut {
    let model = mat4x4<f32>(in.m0, in.m1, in.m2, in.m3);
    let nmat = mat3x3<f32>(in.n0.xyz, in.n1.xyz, in.n2.xyz);
    var out: VsOut;
    let view_pos = model * vec4<f32>(in.pos, 1.0);
    out.clip = g.view_proj * view_pos;
    out.uv = in.uv;
    out.normal = normalize(nmat * in.normal);
    out.color = in.color;
    out.view_pos = view_pos.xyz;
    out.emissive = in.emissive;
    out.specular = in.specular;
    out.params = in.params;
    out.rim = in.rim;
    return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    let n = normalize(in.normal);
    let l = normalize(g.light_dir.xyz);
    let v = normalize(-in.view_pos);
    let ndl = max(dot(n, l), 0.0);
    let texel = textureSample(tex, samp, in.uv);
    let albedo = texel.rgb * in.color.rgb;
    let emissive = in.emissive.rgb * in.emissive.a;
    // Opacity: the material's alpha (in.color.a) times the texture's own alpha.
    let alpha = in.color.a * texel.a;

    // Unlit (fullbright/flat) — pure albedo + emissive, the classic retro look.
    if (in.params.z > 0.5) {
        return vec4<f32>(albedo + emissive, alpha);
    }

    let ambient = g.ambient.rgb * in.params.w;
    var lit = albedo * (ambient + g.light_color.rgb * ndl);

    // Blinn-Phong specular, gated to the lit hemisphere.
    let h = normalize(l + v);
    let shininess = max(in.params.x, 1.0);
    let spec = pow(max(dot(n, h), 0.0), shininess) * in.specular.a * select(0.0, 1.0, ndl > 0.0);
    lit += in.specular.rgb * spec * g.light_color.rgb;

    // Rim / fresnel — a cheap stylized edge glow.
    let rim_f = pow(1.0 - max(dot(n, v), 0.0), 2.0) * in.params.y;
    lit += in.rim.rgb * rim_f;

    return vec4<f32>(lit + emissive, alpha);
}

// Silhouette mask: solid 1.0 wherever the mesh covers a pixel. Rendered into a
// single-channel target; a post-pass edge-detects this into a selection outline
// that hugs the true silhouette (works for any shape).
@fragment
fn fs_mask(in: VsOut) -> @location(0) vec4<f32> {
    return vec4<f32>(1.0, 0.0, 0.0, 1.0);
}
