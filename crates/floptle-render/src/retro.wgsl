// Upscale blit: draws a single fullscreen triangle and samples the low-res scene
// texture with nearest-neighbor, so the chunky pixels are preserved (and edges
// become chunky too) — the core of the retro / PS1 look.

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
// x = smooth (a render scale) rather than hard pixels, y = sharpness 0..1,
// zw = one source texel in uv.
@group(0) @binding(2) var<uniform> params: vec4<f32>;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32) -> VsOut {
    // Oversized triangle covering the screen: clip (-1,-1), (3,-1), (-1,3).
    var corners = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    let c = corners[vi];
    var out: VsOut;
    out.pos = vec4<f32>(c, 0.0, 1.0);
    // map clip → uv, flipping Y (texture origin is top-left)
    out.uv = vec2<f32>((c.x + 1.0) * 0.5, (1.0 - c.y) * 0.5);
    return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    let e = textureSample(tex, samp, in.uv);
    if (params.x < 0.5 || params.y <= 0.0) {
        return e;
    }
    // A smooth upscale, sharpened by local contrast (after AMD's CAS): the
    // four neighbours a source texel away bound the centre, and the less room
    // that leaves before clipping, the less it is sharpened — so an edge gets
    // crisper while flat sky and gradients do not turn to grain.
    let t = params.zw;
    let b = textureSample(tex, samp, in.uv - vec2<f32>(0.0, t.y)).rgb;
    let d = textureSample(tex, samp, in.uv - vec2<f32>(t.x, 0.0)).rgb;
    let f = textureSample(tex, samp, in.uv + vec2<f32>(t.x, 0.0)).rgb;
    let h = textureSample(tex, samp, in.uv + vec2<f32>(0.0, t.y)).rgb;
    let mn = min(min(min(b, d), min(f, h)), e.rgb);
    let mx = max(max(max(b, d), max(f, h)), e.rgb);
    let amp = sqrt(clamp(min(mn, vec3<f32>(2.0) - mx) / max(mx, vec3<f32>(1e-4)), vec3<f32>(0.0), vec3<f32>(1.0)));
    let w = amp * (-1.0 / mix(8.0, 5.0, clamp(params.y, 0.0, 1.0)));
    let rgb = (b * w + d * w + f * w + h * w + e.rgb) / (vec3<f32>(1.0) + 4.0 * w);
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), e.a);
}
