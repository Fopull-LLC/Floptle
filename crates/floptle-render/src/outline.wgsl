// Selection-outline composite: edge-detect a single-channel silhouette mask and
// draw the outline color over the frame. A pixel is an outline pixel when, among
// itself and its 4 neighbors (offset by `width` texels), there is BOTH a masked
// (>0.5) and an un-masked (<0.5) sample — i.e. it straddles the silhouette boundary.
// Because it works on the rendered mask, it hugs ANY shape (meshes and SDF alike).

@group(0) @binding(0) var mask_tex: texture_2d<f32>;
@group(0) @binding(1) var mask_samp: sampler;

struct U {
    color: vec4<f32>,
    texel: vec2<f32>, // 1/width, 1/height
    width: f32,
    _pad: f32,
};
@group(0) @binding(2) var<uniform> u: U;

struct VOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32) -> VOut {
    var p = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    let xy = p[vi];
    var o: VOut;
    o.clip = vec4<f32>(xy, 0.0, 1.0);
    o.uv = vec2<f32>(xy.x * 0.5 + 0.5, 1.0 - (xy.y * 0.5 + 0.5));
    return o;
}

@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    let w = u.texel * u.width;
    let c = textureSample(mask_tex, mask_samp, in.uv).r;
    let l = textureSample(mask_tex, mask_samp, in.uv + vec2<f32>(-w.x, 0.0)).r;
    let r = textureSample(mask_tex, mask_samp, in.uv + vec2<f32>(w.x, 0.0)).r;
    let t = textureSample(mask_tex, mask_samp, in.uv + vec2<f32>(0.0, -w.y)).r;
    let b = textureSample(mask_tex, mask_samp, in.uv + vec2<f32>(0.0, w.y)).r;
    let mx = max(c, max(max(l, r), max(t, b)));
    let mn = min(c, min(min(l, r), min(t, b)));
    if (mx < 0.5 || mn > 0.5) {
        discard; // all on one side of the boundary → not an edge
    }
    return u.color;
}
