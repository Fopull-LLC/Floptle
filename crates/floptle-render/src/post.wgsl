// Post-processing passes (full-screen triangle), shared by the PostStack chain:
// bright-pass → separable Gaussian blur → additive composite (bloom), then a radial
// vignette. Sampling an sRGB texture decodes to linear and writing an sRGB target
// re-encodes, so the math here is in linear light (correct for thresholding/blur).

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
struct P {
    a: vec4<f32>, // xy = texel (1/size of src), z = bloom_threshold, w = bloom_intensity
    b: vec4<f32>, // x = vignette_strength, y = vignette_radius, zw = blur_dir (texels)
};
@group(0) @binding(2) var<uniform> p: P;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32) -> VsOut {
    var corners = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    let c = corners[vi];
    var out: VsOut;
    out.pos = vec4<f32>(c, 0.0, 1.0);
    out.uv = vec2<f32>((c.x + 1.0) * 0.5, (1.0 - c.y) * 0.5);
    return out;
}

// Straight passthrough copy.
@fragment
fn fs_copy(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, in.uv);
}

// Bright-pass: keep only the energy above the threshold (soft knee), so only bright
// pixels bloom.
@fragment
fn fs_bright(in: VsOut) -> @location(0) vec4<f32> {
    let c = textureSample(tex, samp, in.uv).rgb;
    let l = dot(c, vec3<f32>(0.2126, 0.7152, 0.0722));
    let k = max(l - p.a.z, 0.0) / max(l, 1e-4);
    return vec4<f32>(c * k, 1.0);
}

// Separable 9-tap Gaussian (run once per axis via blur_dir).
@fragment
fn fs_blur(in: VsOut) -> @location(0) vec4<f32> {
    let w = array<f32, 5>(0.227027, 0.1945946, 0.1216216, 0.054054, 0.016216);
    var sum = textureSample(tex, samp, in.uv).rgb * w[0];
    for (var i = 1; i < 5; i = i + 1) {
        let o = p.a.xy * p.b.zw * f32(i);
        sum = sum + textureSample(tex, samp, in.uv + o).rgb * w[i];
        sum = sum + textureSample(tex, samp, in.uv - o).rgb * w[i];
    }
    return vec4<f32>(sum, 1.0);
}

// Composite: the blurred bloom scaled by intensity (drawn with additive blend over
// a passthrough of the scene).
@fragment
fn fs_composite(in: VsOut) -> @location(0) vec4<f32> {
    return vec4<f32>(textureSample(tex, samp, in.uv).rgb * p.a.w, 1.0);
}

// Vignette: radial darkening toward the corners (last pass).
@fragment
fn fs_vignette(in: VsOut) -> @location(0) vec4<f32> {
    let c = textureSample(tex, samp, in.uv).rgb;
    let d = distance(in.uv, vec2<f32>(0.5)) * 1.41421356; // 0 center .. ~1 corner
    let v = smoothstep(1.0, p.b.y, d);                    // 1 inside radius → 0 at corners
    let f = mix(1.0 - p.b.x, 1.0, v);
    return vec4<f32>(c * f, 1.0);
}
