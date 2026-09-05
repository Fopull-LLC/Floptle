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
    c: vec4<f32>, // grade: x exposure (stops), y contrast, z saturation, w temperature
    d: vec4<f32>, // grade: x tint, y lift, z gamma, w gain
    e: vec4<f32>, // lens: x aberration, y distortion, z grain amount, w time (seconds)
    f: vec4<f32>, // x sharpen, y denoise, z dof focus (view depth), w dof range
    g: vec4<f32>, // x dof max blur (texels), y aspect (w/h), z grain size,
                  //   w = TONEMAP mode (0 clip, 1 Reinhard, 2 ACES, 3 AgX)
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

// ---------------------------------------------------------------------------
// The look chain: grade → lens → sharpen → denoise, plus grain in fs_finish.
//
// Each one is its own full-screen pass and each is skipped entirely when its
// settings are the identity — `PostSettings` decides that in Rust, once, so a
// scene that uses none of this pays for none of it. They are all here rather
// than folded into `fs_finish` because ORDER IS THE FEATURE: a grade applied
// after chromatic aberration is grading the fringe, and grain sharpened is
// crawling static rather than film.
// ---------------------------------------------------------------------------

// Colour grade: exposure, white balance, contrast, saturation, lift/gamma/gain.
//
// Linear light throughout (the target is sRGB, so the write re-encodes), which
// is what makes exposure a stop rather than a brightness slider and keeps a
// contrast push from tinting the midtones.
//
// c.x exposure (stops)  c.y contrast  c.z saturation  c.w temperature
// d.x tint              d.y lift      d.z gamma       d.w gain
@fragment
fn fs_grade(in: VsOut) -> @location(0) vec4<f32> {
    var c = textureSample(tex, samp, in.uv).rgb;

    // Exposure in STOPS, so +1 is twice the light — the unit a photographer and
    // a renderer already share, and the only one where the number keeps meaning
    // the same thing when the scene's brightness changes.
    c = c * exp2(p.c.x);

    // White balance. Temperature warms (+) or cools (−) along blue↔amber; tint
    // runs the perpendicular green↔magenta axis, which is the one that fixes a
    // scene that has gone subtly sickly and that a single "temperature" slider
    // can never reach. Both are gentle channel gains, normalised so the green
    // channel — where most of the luminance is — is left alone.
    let temp = p.c.w;
    let tint = p.d.x;
    let wb = vec3<f32>(1.0 + temp * 0.5 - tint * 0.1, 1.0 + tint * 0.3, 1.0 - temp * 0.5 - tint * 0.1);
    c = c * max(wb, vec3<f32>(0.0));

    // Contrast about 18% grey — the scene-referred mid point. Pivoting on 0.5
    // instead (the obvious choice) darkens every realistic image as you add
    // contrast, because linear 0.5 is far brighter than a mid grey.
    let mid = 0.18;
    c = max((c - mid) * p.c.y + mid, vec3<f32>(0.0));

    // Saturation against Rec.709 luma, so desaturating leaves brightness where
    // it was. Values above 1 are allowed and clamped at the end.
    let luma = dot(c, vec3<f32>(0.2126, 0.7152, 0.0722));
    c = max(mix(vec3<f32>(luma), c, p.c.z), vec3<f32>(0.0));

    // Lift / gamma / gain — the three-way that every grading suite exposes,
    // because they touch shadows, midtones and highlights nearly independently.
    // lift raises the floor, gain scales the ceiling, gamma bends between them.
    c = c * p.d.w + vec3<f32>(p.d.y);
    c = pow(max(c, vec3<f32>(0.0)), vec3<f32>(1.0 / max(p.d.z, 1e-3)));

    return vec4<f32>(c, 1.0);
}

// Lens: barrel/pincushion distortion and chromatic aberration, in one pass
// because they are the same lens.
//
// Doing them together is not just a saving: aberration is a property of how the
// lens bends each wavelength, so it has to be measured in the SAME distorted
// space, or the fringe slides off the edge it belongs to.
//
// e.x = chromatic aberration, e.y = distortion (+barrel / −pincushion)
@fragment
fn fs_lens(in: VsOut) -> @location(0) vec4<f32> {
    let ca = p.e.x;
    let k = p.e.y;
    // Centred, aspect-corrected so a circle is a circle: without this the
    // distortion is an ellipse and the fringe is thicker left-right than
    // top-bottom on every non-square window.
    let aspect = max(p.g.y, 1e-3);
    var d = (in.uv - vec2<f32>(0.5)) * vec2<f32>(aspect, 1.0);
    let r2 = dot(d, d);
    // Standard radial polynomial. Normalised by the corner radius so `k` means
    // the same amount of bend at any aspect.
    d = d * (1.0 + k * r2);
    let base = d / vec2<f32>(aspect, 1.0) + vec2<f32>(0.5);

    // Aberration: each channel samples at a slightly different radius. Red long,
    // blue short — the direction real glass disperses.
    let off = d * ca * 0.02;
    let ruv = (d + off) / vec2<f32>(aspect, 1.0) + vec2<f32>(0.5);
    let buv = (d - off) / vec2<f32>(aspect, 1.0) + vec2<f32>(0.5);

    // Off the edge reads BLACK rather than clamping. A clamped sample smears the
    // border pixel outward into a streak that looks like a rendering fault; a
    // barrel-distorted frame genuinely has no picture in its corners, and saying
    // so is honest and is what a lens does.
    let inb = step(0.0, base.x) * step(base.x, 1.0) * step(0.0, base.y) * step(base.y, 1.0);
    let c = vec3<f32>(
        textureSample(tex, samp, clamp(ruv, vec2<f32>(0.0), vec2<f32>(1.0))).r,
        textureSample(tex, samp, clamp(base, vec2<f32>(0.0), vec2<f32>(1.0))).g,
        textureSample(tex, samp, clamp(buv, vec2<f32>(0.0), vec2<f32>(1.0))).b,
    );
    return vec4<f32>(c * inb, 1.0);
}

// Unsharp mask. f.x = amount.
//
// A cross rather than a full 3×3 box: the diagonals contribute a quarter of the
// weight and cost half the taps, and at retro resolutions the diagonal term is
// what turns sharpening into ringing.
@fragment
fn fs_sharpen(in: VsOut) -> @location(0) vec4<f32> {
    let t = p.a.xy;
    let c = textureSample(tex, samp, in.uv).rgb;
    let n = textureSample(tex, samp, in.uv + vec2<f32>(0.0, -t.y)).rgb
          + textureSample(tex, samp, in.uv + vec2<f32>(0.0, t.y)).rgb
          + textureSample(tex, samp, in.uv + vec2<f32>(-t.x, 0.0)).rgb
          + textureSample(tex, samp, in.uv + vec2<f32>(t.x, 0.0)).rgb;
    let blur = (c + n) * 0.2;
    // Clamped to the local neighbourhood's range, which is what stops an
    // unsharp mask from haloing: the result may not exceed what was already
    // there, so an edge gets crisper and does not grow a bright rim.
    let hi = max(max(c, n * 0.25), vec3<f32>(0.0));
    return vec4<f32>(clamp(c + (c - blur) * p.f.x, vec3<f32>(0.0), max(hi * 1.25, c)), 1.0);
}

// Bilateral denoise. f.y = amount (0..1).
//
// A plain blur would remove the noise and the picture together. Bilateral
// weights each tap by how far its COLOUR is from the centre as well as how far
// its position is, so it averages within a flat region and refuses to average
// across an edge — which is exactly the distinction between grain and detail.
//
// 3×3 at a single radius: enough for the dither and sampling noise the engine's
// own passes produce, and cheap enough to leave on.
@fragment
fn fs_denoise(in: VsOut) -> @location(0) vec4<f32> {
    let t = p.a.xy;
    let c0 = textureSample(tex, samp, in.uv).rgb;
    // Tighter range as the amount goes DOWN, so a small amount is a gentle
    // clean-up rather than a small amount of mush.
    let sigma = mix(0.02, 0.25, clamp(p.f.y, 0.0, 1.0));
    var sum = c0;
    var wsum = 1.0;
    for (var y = -1; y <= 1; y = y + 1) {
        for (var x = -1; x <= 1; x = x + 1) {
            if (x == 0 && y == 0) { continue; }
            let o = vec2<f32>(f32(x) * t.x, f32(y) * t.y);
            let c = textureSample(tex, samp, in.uv + o).rgb;
            let dist = length(c - c0);
            // Spatial weight: the diagonals are further away, so they count less.
            let sw = select(0.7071, 1.0, x == 0 || y == 0);
            let w = sw * exp(-(dist * dist) / (2.0 * sigma * sigma));
            sum = sum + c * w;
            wsum = wsum + w;
        }
    }
    return vec4<f32>(mix(c0, sum / wsum, clamp(p.f.y, 0.0, 1.0)), 1.0);
}

// Depth of field.
//
// A CoC-weighted gather, one pass: for every pixel, work out how far out of
// focus it is (its circle of confusion) and average a disk of that radius.
//
// One pass rather than the usual near/far split because the split exists to
// stop a sharp foreground bleeding onto a blurred background, and the guard
// below buys most of that for a fraction of the cost: a tap only contributes if
// its OWN CoC is at least as large as the distance it is being pulled from. A
// sharp pixel therefore cannot smear into its neighbours, only be smeared over.
//
// The kernel is a 16-point spiral, not a box: a box blur turns a bright
// highlight into a square, which is the one artefact everybody recognises as
// wrong. A spiral gives a round bokeh for free.
//
// f.z = focus distance (view depth), f.w = range, g.x = max blur (texels),
// a.xy = texel.
@group(1) @binding(0) var dof_depth: texture_depth_2d;
// One camera block for every pass that needs the frame's own geometry: depth of
// field reads `inv_proj`, motion blur reads the other three. One struct and one
// buffer rather than two, because they share a bind-group layout and a second
// one that drifted by a single field would be a validation error at draw time
// in whichever project happened to use that pass.
struct DofCam {
    inv_proj: mat4x4<f32>,
    // Clip → camera-relative world, this frame.
    inv_view_proj: mat4x4<f32>,
    // Camera-relative world (this frame's origin) → the PREVIOUS frame's clip.
    prev_view_proj: mat4x4<f32>,
    // x = shutter, y = taps, z = max streak (px), w unused.
    motion: vec4<f32>,
};
@group(1) @binding(1) var<uniform> dof_cam: DofCam;

// View-space depth from the depth buffer. Reversed-Z aware via the inverse
// projection, so this reads whatever the camera actually rendered — including
// an orthographic one, where the naive `near*far/…` form is simply wrong.
fn dof_view_depth(uv: vec2<f32>) -> f32 {
    let dims = vec2<i32>(textureDimensions(dof_depth));
    let px = clamp(vec2<i32>(uv * vec2<f32>(dims)), vec2<i32>(0), dims - vec2<i32>(1));
    let d = textureLoad(dof_depth, px, 0);
    let clip = vec4<f32>(uv.x * 2.0 - 1.0, (1.0 - uv.y) * 2.0 - 1.0, d, 1.0);
    let view = dof_cam.inv_proj * clip;
    return -view.z / max(view.w, 1e-6);
}

// Signed circle of confusion in texels: 0 inside the focus band, growing to the
// max blur as the pixel leaves it. Signed so a tap can compare "am I nearer
// than the pixel I am bleeding onto".
// SIGNED distance out of the focus band, -1..1: negative in front of focus,
// positive behind it, 0 anywhere sharp.
//
// The two sides get their own range because they are not the same thing. A lens
// goes soft almost immediately on the near side and holds shape much further on
// the far side, and more to the point they are the two numbers people reach for:
// a portrait wants the foreground gone and the background readable. The old
// single `range` is still expressible — the near side defaults to half of it,
// which is exactly what this used to hardcode.
fn dof_signed(uv: vec2<f32>) -> f32 {
    let z = dof_view_depth(uv);
    let d = z - p.f.z;
    let near_r = max(p.f.w, 1e-3);
    let far_r = max(p.b.x, 1e-3);
    return clamp(select(d / near_r, d / far_r, d > 0.0), -1.0, 1.0);
}

// Circle of confusion in texels.
fn dof_coc(uv: vec2<f32>) -> f32 {
    return abs(dof_signed(uv)) * p.g.x;
}

// How far out the iris reaches at this angle, as a fraction of the full radius.
// 1 everywhere for a round aperture; the outline of a regular polygon for a
// bladed one, which is where hexagonal bokeh comes from — a real iris is a ring
// of straight blades, not a circle.
fn dof_blade(ang: f32) -> f32 {
    let n = p.b.y;
    if (n < 2.5) {
        return 1.0;
    }
    let seg = 6.2831853 / n;
    let half = seg * 0.5;
    let a = (ang + p.b.z) % seg;
    return cos(half) / max(cos(a - half), 1e-3);
}

// How much a tap's own brightness counts for. At boost 0 every tap weighs the
// same and a bright point averages away into grey. Above 0, light past white
// carries more — which is what turns a specular glint into a visible disc of
// bokeh instead of a slightly pale smear.
//
// The threshold is 1.0 because the frame reaching this pass is scene-referred:
// "brighter than white" is a real, meaningful thing to test for here, and was
// not before the chain went floating-point.
fn dof_weight(c: vec3<f32>) -> f32 {
    let peak = max(c.r, max(c.g, c.b));
    return 1.0 + p.b.w * max(peak - 1.0, 0.0);
}

@fragment
fn fs_dof(in: VsOut) -> @location(0) vec4<f32> {
    let signed = dof_signed(in.uv);
    let coc = abs(signed) * p.g.x;
    let c0 = textureSample(tex, samp, in.uv).rgb;

    // The tuning view: cool where the near side is going soft, warm where the
    // far side is, the picture itself where it is sharp. Which half of the band
    // a pixel is on is the thing you cannot read off a blurred picture, and it
    // is the thing you need in order to place the focus.
    if (p.c.y > 0.5) {
        let t = abs(signed);
        let tint = select(vec3<f32>(1.0, 0.45, 0.25), vec3<f32>(0.25, 0.6, 1.0), signed < 0.0);
        let luma = dot(c0, vec3<f32>(0.2126, 0.7152, 0.0722));
        return vec4<f32>(mix(c0, tint * (0.25 + luma), t * 0.85), 1.0);
    }

    // A pixel that is in focus is left EXACTLY alone — not blurred by a
    // zero-radius kernel, which would still cost every tap and still soften it
    // by a fraction of a texel through the sampler.
    if (coc < 0.75) {
        return vec4<f32>(c0, 1.0);
    }
    // A golden-angle spiral, so the taps are evenly spread with no preferred
    // axis; `dof_blade` then squeezes it to the iris's shape.
    let taps = max(p.c.x, 4.0);
    let w0 = dof_weight(c0);
    var sum = c0 * w0;
    var wsum = w0;
    for (var i = 0.0; i < taps; i = i + 1.0) {
        let fi = i + 0.5;
        let ang = fi * 2.39996323;                 // golden angle in radians
        let rad = sqrt(fi / taps) * coc * dof_blade(ang); // sqrt = uniform area density
        let o = vec2<f32>(cos(ang), sin(ang)) * rad * p.a.xy;
        let uv = clamp(in.uv + o, vec2<f32>(0.0), vec2<f32>(1.0));
        // The guard: a tap only contributes if it is itself blurred by at least
        // as much as the distance it is reaching. Without it, a sharp character
        // standing in front of a defocused wall grows a halo of wall.
        let tap_coc = dof_coc(uv);
        // `textureSampleLevel`, not `textureSample`: this loop runs after a
        // per-pixel early return (the in-focus pixel above), which is
        // non-uniform control flow, and a browser's WGSL compiler refuses an
        // implicit-derivative sample there. The frame has one mip level, so
        // level 0 is the same texel it always was.
        let s = textureSampleLevel(tex, samp, uv, 0.0).rgb;
        let w = step(rad - 0.5, tap_coc) * dof_weight(s);
        sum = sum + s * w;
        wsum = wsum + w;
    }
    return vec4<f32>(sum / wsum, 1.0);
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
// ---- Tonemap: the ONE place the scene meets the display ---------------------
//
// Everything upstream is scene-referred — linear light at whatever intensity it
// actually has, in a floating-point target, unbounded above 1. A display is
// bounded at 1. Something has to decide how to get from one to the other, and
// doing nothing is itself a decision: the hardware clamps each channel on its
// own, so a colour whose red saturates first slides toward yellow and then
// white. That is why blown highlights in an untonemapped renderer go strange
// colours rather than simply going bright.
//
// Four answers, because this is a look and not a correctness setting:
//
//   0 CLIP     — do nothing. What the engine did before there was a choice, and
//                still the right answer for flat 2D and pixel art, where every
//                colour was authored inside 0..1 and any curve over it is a
//                filter nobody asked for.
//   1 REINHARD — c / (1 + c). Never clips, never has a shoulder worth the name:
//                everything bright washes toward grey. Cheap, predictable,
//                and the honest choice when you want the range compressed and
//                nothing else editorialised.
//   2 ACES     — the filmic curve everyone knows: crushed toe, long shoulder,
//                highlights that roll off warm. Contrasty and confident, and it
//                does have opinions — strong colours drift on the way up.
//   3 AGX      — desaturates as it approaches white, the way film and a camera
//                sensor do, so a saturated light gets BRIGHTER instead of
//                turning into a flat block of its own hue. Gentler and more
//                neutral than ACES; the better default for a scene lit by
//                coloured lights.
fn tonemap(c: vec3<f32>, mode: f32) -> vec3<f32> {
    let m = i32(mode + 0.5);
    if (m == 1) {
        return c / (1.0 + c);
    }
    if (m == 2) {
        // Narkowicz's fit of the ACES filmic curve — the standard cheap version.
        let a = 2.51;
        let b = 0.03;
        let d = 2.43;
        let e = 0.59;
        let f = 0.14;
        return clamp((c * (a * c + b)) / (c * (d * c + e) + f), vec3<f32>(0.0), vec3<f32>(1.0));
    }
    if (m == 3) {
        // AgX, in the form that matters here: let a channel that has run past
        // the display SPILL into the other two, then roll off.
        //
        // The mix is toward `vec3(peak)` — a NEUTRAL at the brightest channel's
        // own level — and not toward the colour's luminance. Toward luminance
        // would pull the bright channel DOWN, which is a desaturation that also
        // darkens; toward the peak it pulls the dark channels UP, so more light
        // reads as brighter AND whiter. That is what film does, and it is what
        // stops a very bright blue light from sitting at a hard blue ceiling
        // where four times the light looks exactly like one times it.
        //
        // Below white nothing spills, so ordinary colour is untouched — and on a
        // NEUTRAL colour the mix is the identity, so this reduces exactly to
        // Reinhard on greys. Both of those are the point, not a shortcut.
        let peak = max(max(c.r, c.g), c.b);
        let spill = clamp(1.0 - 1.0 / max(peak, 1.0), 0.0, 1.0);
        let w = mix(c, vec3<f32>(peak), spill * 0.9);
        return w / (1.0 + w);
    }
    return c;
}

@fragment
fn fs_finish(in: VsOut) -> @location(0) vec4<f32> {
    var c = textureSample(tex, samp, in.uv).rgb;
    // Tonemap FIRST of this pass, so everything below is working on
    // display-referred colour. A vignette multiplies (it is a lens shading a
    // frame, not a light being removed from the scene) and grain lives in the
    // recording — both belong after the picture has been mapped, not before it.
    c = tonemap(max(c, vec3<f32>(0.0)), p.g.w);
    // Colour-vision filter first: it corrects the picture the game made, so it
    // belongs before the looks the scene applies on top (`floptle/0079`).
    // a.x = simulate, a.z = filter mode, a.w = strength — lanes the bloom pass
    // uses and this one does not.
    c = color_vision(c, p.a.z, p.a.x, p.a.w);
    // Vignette (skipped when strength p.b.x == 0; radius p.b.y = 1 is the identity).
    let d = distance(in.uv, vec2<f32>(0.5)) * 1.41421356; // 0 center .. ~1 corner
    let vg = smoothstep(1.0, p.b.y, d);                   // 1 inside radius → 0 at corners
    c = c * mix(1.0 - p.b.x, 1.0, vg);
    // Film grain, last, because grain is the thing the picture is recorded ON —
    // anything downstream of it (a sharpen, a blur) turns it into crawling
    // static instead. e.z = amount, e.w = time, g.z = grain size in pixels.
    if (p.e.z > 0.0) {
        // Hash the CELL, not the pixel, so `size` is a real control: at size 2
        // the grain is 2×2 clumps, which is what film looks like at a distance
        // and what a retro target needs (a per-pixel hash under an integer
        // upscale is invisible, then suddenly a flat shimmer).
        let cell = floor(in.uv / max(p.a.xy, vec2<f32>(1e-6)) / max(p.g.z, 1.0));
        let n = fract(sin(dot(cell, vec2<f32>(12.9898, 78.233)) + p.e.w * 37.0) * 43758.5453);
        // MULTIPLICATIVE, and scaled by how bright the pixel already is: real
        // film grain lives in the emulsion's response, so it is strongest in the
        // midtones and all but absent in the blacks. Added grain instead lifts
        // every shadow into grey mud, which is the tell of a cheap filter.
        let l = clamp(dot(c, vec3<f32>(0.2126, 0.7152, 0.0722)), 0.0, 1.0);
        let weight = 4.0 * l * (1.0 - l);          // peaks at mid grey, 0 at both ends
        c = c * (1.0 + (n - 0.5) * 2.0 * p.e.z * weight);
    }
    return vec4<f32>(max(c, vec3<f32>(0.0)), 1.0);
}


// ---- motion blur ------------------------------------------------------------
//
// The streak is reconstructed rather than rendered: take the pixel's depth, put
// it back in the world, and ask where that same point WAS in the previous
// frame's picture. The difference is how far it travelled across the screen, and
// smearing the frame along it is the blur.
//
// What that buys, and what it costs. It gets every kind of camera motion right —
// a pan, a whip, a dolly, a roll — because the camera is the thing both matrices
// describe, and camera motion is most of what a player ever sees blurred. Two
// things it does not do, both of them structural rather than bugs:
//
//  1. Object motion. A car crossing a locked-off shot is a point standing still
//     in the world as far as this is concerned, so it stays sharp. That half
//     needs a velocity buffer — a second render target written by every draw
//     path in the engine — and it is not here.
//  2. Reach outside a moving surface. This is a GATHER: each pixel collects
//     along ITS OWN velocity, so a fast-moving surface softens within its own
//     footprint rather than throwing light onto what is behind it. At a big
//     depth step — a near railing against a far valley — the railing smears and
//     the valley does not receive the smear. Fixing that means dilating velocity
//     across tiles first, which is another two passes.
//
// See docs/subsystems/post-processing.md.
fn motion_velocity(uv: vec2<f32>) -> vec2<f32> {
    let dims = vec2<i32>(textureDimensions(dof_depth));
    let px = clamp(vec2<i32>(uv * vec2<f32>(dims)), vec2<i32>(0), dims - vec2<i32>(1));
    let d = textureLoad(dof_depth, px, 0);
    // The far plane is the sky. It has no position to reproject, and treating it
    // as a point at infinity is what makes a pan smear the sky the RIGHT amount:
    // the reprojection below already handles it, because a direction transformed
    // by a rotation is still the same direction.
    let clip = vec4<f32>(uv.x * 2.0 - 1.0, (1.0 - uv.y) * 2.0 - 1.0, d, 1.0);
    let world = dof_cam.inv_view_proj * clip;
    if (abs(world.w) < 1e-9) {
        return vec2<f32>(0.0);
    }
    let p_rel = world.xyz / world.w;
    let prev = dof_cam.prev_view_proj * vec4<f32>(p_rel, 1.0);
    if (prev.w <= 1e-6) {
        return vec2<f32>(0.0); // behind the old camera: it had no picture of this
    }
    let pn = prev.xy / prev.w;
    let prev_uv = vec2<f32>(pn.x * 0.5 + 0.5, 1.0 - (pn.y * 0.5 + 0.5));
    return uv - prev_uv;
}

@fragment
fn fs_motion(in: VsOut) -> @location(0) vec4<f32> {
    let c0 = textureSample(tex, samp, in.uv).rgb;
    let texel = p.a.xy;
    var v = motion_velocity(in.uv) * dof_cam.motion.x;
    // Clamp in PIXELS, not in uv: the same uv length is twice the streak on a
    // 2160-tall frame as on a 1080-tall one, and a ceiling that changed with the
    // window would be a ceiling nobody could tune.
    let px = v / max(texel, vec2<f32>(1e-9));
    let len = length(px);
    let cap = max(dof_cam.motion.z, 0.0);
    if (len < 0.5 || cap <= 0.0) {
        return vec4<f32>(c0, 1.0);   // a still pixel is left exactly alone
    }
    if (len > cap) {
        v = v * (cap / len);
    }
    let taps = clamp(dof_cam.motion.y, 4.0, 32.0);
    var sum = c0;
    var wsum = 1.0;
    // Symmetric about the pixel: a one-sided smear drags every edge in the
    // direction of travel and reads as the picture sliding, not as exposure.
    for (var i = 1.0; i <= taps; i = i + 1.0) {
        let t = (i / taps) * 0.5;
        let a = clamp(in.uv + v * t, vec2<f32>(0.0), vec2<f32>(1.0));
        let b = clamp(in.uv - v * t, vec2<f32>(0.0), vec2<f32>(1.0));
        // Explicit level 0 for the same reason as the depth-of-field loop: the
        // still-pixel return above makes this non-uniform control flow.
        sum = sum + textureSampleLevel(tex, samp, a, 0.0).rgb + textureSampleLevel(tex, samp, b, 0.0).rgb;
        wsum = wsum + 2.0;
    }
    return vec4<f32>(sum / wsum, 1.0);
}
