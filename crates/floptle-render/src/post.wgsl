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
                  // a in fs_finish: x = simulate deficiency, z = colour filter mode,
                  //   w = filter strength (floptle/0079)
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

// Colour-vision filter (`floptle/0079`). `mode`: 1 = protanopia, 2 = deuteranopia,
// 3 = tritanopia; 0 returns the colour untouched. `simulate` shows the deficiency
// (for the developer) instead of correcting for it (for the player).
//
// The pipeline is the standard one: linear RGB → LMS cone response, collapse the
// missing cone's axis (Viénot/Brettel/Mollon), LMS → RGB. Correcting then takes
// the error the deficiency loses and pushes it into channels the viewer CAN
// still separate — so two colours that were the same to them stop being the same,
// which is the whole point. Simulating just returns the collapsed colour.
//
// Done in ~gamma space, like the posterize below: the matrices are derived for
// display-encoded values, and running them on linear light shifts hues visibly.
fn color_vision(c_lin: vec3<f32>, mode: f32, simulate: f32, strength: f32) -> vec3<f32> {
    if (mode < 0.5 || strength <= 0.0) {
        return c_lin;
    }
    let c = pow(max(c_lin, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.2));
    // RGB → LMS.
    let l = 17.8824 * c.r + 43.5161 * c.g + 4.11935 * c.b;
    let m = 3.45565 * c.r + 27.1554 * c.g + 3.86714 * c.b;
    let s = 0.0299566 * c.r + 0.184309 * c.g + 1.46709 * c.b;
    var l2 = l;
    var m2 = m;
    var s2 = s;
    if (mode < 1.5) {
        l2 = 2.02344 * m - 2.52581 * s;          // protanopia: no L cone
    } else if (mode < 2.5) {
        m2 = 0.494207 * l + 1.24827 * s;         // deuteranopia: no M cone
    } else {
        s2 = -0.395913 * l + 0.801109 * m;       // tritanopia: no S cone
    }
    // LMS → RGB.
    let sim = vec3<f32>(
        0.0809444479 * l2 - 0.130504409 * m2 + 0.116721066 * s2,
        -0.0102485335 * l2 + 0.0540193266 * m2 - 0.113614708 * s2,
        -0.000365296938 * l2 - 0.00412161469 * m2 + 0.693511405 * s2,
    );
    var outc = sim;
    if (simulate < 0.5) {
        // Daltonize: redistribute what the deficient axis dropped.
        let err = c - sim;
        let shift = vec3<f32>(
            0.0,
            0.7 * err.r + 1.0 * err.g,
            0.7 * err.r + 1.0 * err.b,
        );
        outc = c + shift;
    }
    // `strength` blends against the original, so a partial correction is a real
    // setting rather than an on/off switch.
    let mixed = clamp(mix(c, outc, strength), vec3<f32>(0.0), vec3<f32>(1.0));
    return pow(mixed, vec3<f32>(2.2));
}

// Terminal color pass: the colour-vision filter, then the vignette (radial
// darken). Both are no-ops at their identity params (mode 0 / strength 0), so
// the one pass serves either or both. Runs last, at the scene's composited
// (retro) resolution and BEFORE the upscale.
//
// **Posterize is not here.** It used to be, and that was the bug (`floptle/0127`):
// quantizing the finished frame quantizes the light along with the palette, and a
// light is a multiplier on the palette rather than a value in it. It now runs as
// its own pass over the art, before the 2D light composite — see `palette.wgsl`.
// The vignette is downstream of that quantize for the same reason, and it is the
// corroboration that the rule is the right one: a vignette is a smooth radial
// darkening, and it was banding for exactly the reason a light was.
@fragment
fn fs_finish(in: VsOut) -> @location(0) vec4<f32> {
    var c = textureSample(tex, samp, in.uv).rgb;
    // Colour-vision filter first: it corrects the picture the game made, so it
    // belongs before the looks the scene applies on top (`floptle/0079`).
    // a.x = simulate, a.z = filter mode, a.w = strength — lanes the bloom pass
    // uses and this one does not.
    c = color_vision(c, p.a.z, p.a.x, p.a.w);
    // Vignette (skipped when strength p.b.x == 0; radius p.b.y = 1 is the identity).
    let d = distance(in.uv, vec2<f32>(0.5)) * 1.41421356; // 0 center .. ~1 corner
    let vg = smoothstep(1.0, p.b.y, d);                   // 1 inside radius → 0 at corners
    c = c * mix(1.0 - p.b.x, 1.0, vg);
    return vec4<f32>(c, 1.0);
}
