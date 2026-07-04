// Post-processing passes (full-screen triangle), shared by the PostStack chain:
// SSAO multiply (the AO factor itself is computed in ssao.wgsl) → bright-pass →
// separable Gaussian blur → additive composite (bloom), then a radial vignette.
// Sampling an sRGB texture decodes to linear and writing an sRGB target
// re-encodes, so the math here is in linear light (correct for thresholding/blur).

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
// Second texture slot for fs_ssao_apply: the blurred half-res AO factor.
@group(1) @binding(0) var ao_tex: texture_2d<f32>;
@group(1) @binding(1) var ao_samp: sampler;
struct P {
    a: vec4<f32>, // xy = texel (1/size of src), z = bloom_threshold, w = bloom_intensity
    b: vec4<f32>, // x = vignette_strength, y = vignette_radius, zw = blur_dir (texels)
                  //   OR, in the terminal fs_finish pass: z = posterize bands, w = dither
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

// SSAO apply: multiply the scene by the blurred AO factor (linear light — the
// upsample from half-res is smoothed by the linear sampler).
@fragment
fn fs_ssao_apply(in: VsOut) -> @location(0) vec4<f32> {
    let c = textureSample(tex, samp, in.uv).rgb;
    let ao = textureSample(ao_tex, ao_samp, in.uv).r;
    return vec4<f32>(c * ao, 1.0);
}

// Bayer 4×4 ordered-dither threshold in (0,1) (standalone copy — see field.wgsl).
fn bayer4(pix: vec2<u32>) -> f32 {
    var m = array<u32, 16>(0u, 8u, 2u, 10u, 12u, 4u, 14u, 6u, 3u, 11u, 1u, 9u, 15u, 7u, 13u, 5u);
    return (f32(m[(pix.y % 4u) * 4u + (pix.x % 4u)]) + 0.5) / 16.0;
}

// Terminal color pass: vignette (radial darken) then optional posterize (quantize
// each channel to a limited palette / band count). Both are no-ops at their
// identity params (strength 0 / bands < 2), so this one pass serves vignette-only,
// posterize-only, or both. Runs last, at the scene's composited (retro) resolution
// and BEFORE the upscale, so the banding lands on the same chunky pixel grid.
@fragment
fn fs_finish(in: VsOut) -> @location(0) vec4<f32> {
    var c = textureSample(tex, samp, in.uv).rgb;
    // Vignette (skipped when strength p.b.x == 0; radius p.b.y = 1 is the identity).
    let d = distance(in.uv, vec2<f32>(0.5)) * 1.41421356; // 0 center .. ~1 corner
    let vg = smoothstep(1.0, p.b.y, d);                   // 1 inside radius → 0 at corners
    c = c * mix(1.0 - p.b.x, 1.0, vg);
    // Posterize: quantize to `bands` levels per channel in ~gamma space (perceptually
    // even steps), with optional ordered dither so smooth ramps don't hard-step.
    let bands = p.b.z;
    if (bands >= 2.0) {
        let scale = bands - 1.0;
        let g = pow(max(c, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.2)); // linear → ~gamma
        var q = g * scale;
        if (p.b.w > 0.5) {
            let t = bayer4(vec2<u32>(u32(in.pos.x), u32(in.pos.y)));
            q = floor(q + vec3<f32>(t));
        } else {
            q = floor(q + vec3<f32>(0.5)); // nearest level (= round)
        }
        let gq = clamp(q / scale, vec3<f32>(0.0), vec3<f32>(1.0));
        c = pow(gq, vec3<f32>(2.2)); // ~gamma → linear
    }
    return vec4<f32>(c, 1.0);
}
