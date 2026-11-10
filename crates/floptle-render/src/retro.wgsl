// Upscale blit: draws a single fullscreen triangle and samples the low-res scene
// texture with nearest-neighbor, so the chunky pixels are preserved (and edges
// become chunky too) — the core of the retro / PS1 look.

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
// x = smooth (a render scale) rather than hard pixels (1); y = sharpness
// 0..1, zw = one source texel in uv. FSR 1 is its own two entry points,
// `fs_easu` and `fs_rcas`.
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

// FSR 1 (AMD FidelityFX Super Resolution 1.0, MIT licence): its EASU
// upscale, edge-adaptive over the twelve source texels round a pixel, the
// lobes stretched along the local edge so an edge stays an edge and not a
// bilinear smear, clamped to the four nearest texels so it never rings.
fn easu_luma(c: vec3<f32>) -> f32 {
    return c.g + 0.5 * (c.r + c.b);
}

fn easu_load(p: vec2<i32>) -> vec3<f32> {
    let d = vec2<i32>(textureDimensions(tex));
    return textureLoad(tex, clamp(p, vec2<i32>(0), d - vec2<i32>(1)), 0).rgb;
}

// One of the four 2×2 neighbourhoods' say on the edge's direction and how
// strong it is, weighed by how near the pixel it is.
fn easu_set(dir: ptr<function, vec2<f32>>, len: ptr<function, f32>, w: f32, la: f32, lb: f32, lc: f32, ld: f32, le: f32) {
    var lx = max(abs(ld - lc), abs(lc - lb));
    let dx = ld - lb;
    (*dir).x += dx * w;
    lx = clamp(abs(dx) / max(lx, 1e-5), 0.0, 1.0);
    *len += lx * lx * w;
    var ly = max(abs(le - lc), abs(lc - la));
    let dy = le - la;
    (*dir).y += dy * w;
    ly = clamp(abs(dy) / max(ly, 1e-5), 0.0, 1.0);
    *len += ly * ly * w;
}

fn easu_tap(acc: ptr<function, vec4<f32>>, off: vec2<f32>, dir: vec2<f32>, len: vec2<f32>, lob: f32, clp: f32, c: vec3<f32>) {
    var v = vec2<f32>(dot(off, dir), dot(off, vec2<f32>(-dir.y, dir.x))) * len;
    let d2 = min(dot(v, v), clp);
    var wb = 0.4 * d2 - 1.0;
    var wa = lob * d2 - 1.0;
    wb = 1.5625 * wb * wb - 0.5625;
    let w = wb * wa * wa;
    *acc += vec4<f32>(c * w, w);
}

fn easu(uv: vec2<f32>) -> vec3<f32> {
    let isz = vec2<f32>(textureDimensions(tex));
    var pp = uv * isz - 0.5;
    let fp = floor(pp);
    pp -= fp;
    let o = vec2<i32>(fp);
    let b = easu_load(o + vec2<i32>(0, -1));
    let c = easu_load(o + vec2<i32>(1, -1));
    let e = easu_load(o + vec2<i32>(-1, 0));
    let f = easu_load(o);
    let g = easu_load(o + vec2<i32>(1, 0));
    let h = easu_load(o + vec2<i32>(2, 0));
    let i = easu_load(o + vec2<i32>(-1, 1));
    let j = easu_load(o + vec2<i32>(0, 1));
    let k = easu_load(o + vec2<i32>(1, 1));
    let l = easu_load(o + vec2<i32>(2, 1));
    let n = easu_load(o + vec2<i32>(0, 2));
    let q = easu_load(o + vec2<i32>(1, 2));
    var dir = vec2<f32>(0.0);
    var len = 0.0;
    easu_set(&dir, &len, (1.0 - pp.x) * (1.0 - pp.y), easu_luma(b), easu_luma(e), easu_luma(f), easu_luma(g), easu_luma(j));
    easu_set(&dir, &len, pp.x * (1.0 - pp.y), easu_luma(c), easu_luma(f), easu_luma(g), easu_luma(h), easu_luma(k));
    easu_set(&dir, &len, (1.0 - pp.x) * pp.y, easu_luma(f), easu_luma(i), easu_luma(j), easu_luma(k), easu_luma(n));
    easu_set(&dir, &len, pp.x * pp.y, easu_luma(g), easu_luma(j), easu_luma(k), easu_luma(l), easu_luma(q));
    let r2 = dot(dir, dir);
    let zero = r2 < 1.0 / 32768.0;
    if (zero) {
        dir = vec2<f32>(1.0, 0.0);
    } else {
        dir = dir * inverseSqrt(r2);
    }
    len = len * 0.5;
    len = len * len;
    let stretch = dot(dir, dir) / max(abs(dir.x), abs(dir.y));
    let len2 = vec2<f32>(1.0 + (stretch - 1.0) * len, 1.0 - 0.5 * len);
    let lob = 0.5 - 0.29 * len;
    let clp = 1.0 / lob;
    var acc = vec4<f32>(0.0);
    easu_tap(&acc, vec2<f32>(0.0, -1.0) - pp, dir, len2, lob, clp, b);
    easu_tap(&acc, vec2<f32>(1.0, -1.0) - pp, dir, len2, lob, clp, c);
    easu_tap(&acc, vec2<f32>(-1.0, 1.0) - pp, dir, len2, lob, clp, i);
    easu_tap(&acc, vec2<f32>(0.0, 1.0) - pp, dir, len2, lob, clp, j);
    easu_tap(&acc, vec2<f32>(0.0, 0.0) - pp, dir, len2, lob, clp, f);
    easu_tap(&acc, vec2<f32>(-1.0, 0.0) - pp, dir, len2, lob, clp, e);
    easu_tap(&acc, vec2<f32>(1.0, 1.0) - pp, dir, len2, lob, clp, k);
    easu_tap(&acc, vec2<f32>(2.0, 1.0) - pp, dir, len2, lob, clp, l);
    easu_tap(&acc, vec2<f32>(2.0, 0.0) - pp, dir, len2, lob, clp, h);
    easu_tap(&acc, vec2<f32>(1.0, 0.0) - pp, dir, len2, lob, clp, g);
    easu_tap(&acc, vec2<f32>(1.0, 2.0) - pp, dir, len2, lob, clp, q);
    easu_tap(&acc, vec2<f32>(0.0, 2.0) - pp, dir, len2, lob, clp, n);
    let mn = min(min(f, g), min(j, k));
    let mx = max(max(f, g), max(j, k));
    return clamp(acc.rgb / max(acc.w, 1e-5), mn, mx);
}

// FSR 1's RCAS: sharpen by the four neighbours an output pixel away, as hard
// as the local contrast leaves room for, never past what clips.
fn rcas(e: vec3<f32>, b: vec3<f32>, d: vec3<f32>, f: vec3<f32>, h: vec3<f32>, sharp: f32) -> vec3<f32> {
    let mn = min(min(b, d), min(f, h));
    let mx = max(max(b, d), max(f, h));
    let hit_min = mn / max(4.0 * mx, vec3<f32>(1e-5));
    let hit_max = (vec3<f32>(1.0) - mx) / min(4.0 * mn - vec3<f32>(4.0), vec3<f32>(-1e-5));
    let l = max(-hit_min, hit_max);
    let lobe = max(-(0.25 - 1.0 / 16.0), min(max(l.r, max(l.g, l.b)), 0.0)) * sharp;
    return (lobe * (b + d + f + h) + e) / (4.0 * lobe + 1.0);
}

// FSR 1's first pass: EASU, once for each output pixel, into a picture the
// output's size (or straight to the output when nothing sharpens).
@fragment
fn fs_easu(in: VsOut) -> @location(0) vec4<f32> {
    return vec4<f32>(easu(in.uv), textureSample(tex, samp, in.uv).a);
}

// FSR 1's second pass: RCAS over the upscaled picture (`tex` here), each
// pixel against its four neighbours. Sharpness 1 is RCAS at full strength.
@fragment
fn fs_rcas(in: VsOut) -> @location(0) vec4<f32> {
    let p = vec2<i32>(in.pos.xy);
    let hi = vec2<i32>(textureDimensions(tex)) - vec2<i32>(1);
    let e = textureLoad(tex, clamp(p, vec2<i32>(0), hi), 0);
    let b = textureLoad(tex, clamp(p - vec2<i32>(0, 1), vec2<i32>(0), hi), 0).rgb;
    let d = textureLoad(tex, clamp(p - vec2<i32>(1, 0), vec2<i32>(0), hi), 0).rgb;
    let f = textureLoad(tex, clamp(p + vec2<i32>(1, 0), vec2<i32>(0), hi), 0).rgb;
    let h = textureLoad(tex, clamp(p + vec2<i32>(0, 1), vec2<i32>(0), hi), 0).rgb;
    let sharp = exp2(-(1.0 - clamp(params.y, 0.0, 1.0)) * 2.0);
    return vec4<f32>(clamp(rcas(e.rgb, b, d, f, h, sharp), vec3<f32>(0.0), vec3<f32>(1.0)), e.a);
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
