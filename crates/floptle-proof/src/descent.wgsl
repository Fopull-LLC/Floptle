// Beat 3 — render shader: a morphing, POROUS rounded MENGER SPONGE you delve
// *inside* of, with a moon, the player capsule + clown nose + grapple rope +
// reticle. The descent shrinks the player (not the field): walls/corners are
// colored by cell coordinate + face + depth. LOCK-STEP field with descent.rs.
// Reuses post.wgsl + present_plain.wgsl.

struct Globals {
    cam_pos: vec4<f32>,
    cam_right: vec4<f32>,
    cam_up: vec4<f32>,
    cam_fwd: vec4<f32>,
    resolution: vec2<f32>,
    time: f32,
    dt: f32,
    frame: f32,
    feedback: f32,
    warp: f32,
    fov: f32,
    capsule_pos: vec4<f32>,
    capsule_up: vec4<f32>,
    contact: vec4<f32>,
    capsule_fwd: vec4<f32>,
    dive: vec4<f32>,
    grapple: vec4<f32>,
};
@group(0) @binding(0) var<uniform> G: Globals;

struct VOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32) -> VOut {
    var p = array<vec2<f32>, 3>(vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
    let xy = p[vi];
    var o: VOut;
    o.clip = vec4<f32>(xy, 0.0, 1.0);
    o.uv = xy * 0.5 + vec2<f32>(0.5, 0.5);
    return o;
}

// ---- field constants (LOCK-STEP with descent.rs) ----
const MBS: f32 = 45.0;
const WMORPH: f32 = 0.05;
const MOON_DIST: f32 = 150.0;
const R_MOON: f32 = 12.0;
const TAU: f32 = 6.2831853;

fn smin(a: f32, b: f32, k: f32) -> f32 {
    let h = clamp(0.5 + 0.5 * (b - a) / k, 0.0, 1.0);
    return mix(b, a, h) - k * h * (1.0 - h);
}
fn smax(a: f32, b: f32, k: f32) -> f32 { return -smin(-a, -b, k); }
fn box_de(p: vec3<f32>, b: f32) -> f32 {
    let q = abs(p) - vec3<f32>(b);
    return length(max(q, vec3<f32>(0.0))) + min(max(q.x, max(q.y, q.z)), 0.0);
}
fn roty(p: vec3<f32>, a: f32) -> vec3<f32> {
    let s = sin(a);
    let c = cos(a);
    return vec3<f32>(c * p.x - s * p.z, p.y, s * p.x + c * p.z);
}
// ROUNDED Menger sponge (porous: f<0 inside walls, f>0 in tunnels). LOCK-STEP w/ Rust.
fn menger(p0: vec3<f32>, iters: i32) -> f32 {
    let kr = 0.13;
    var d = box_de(p0, 1.0);
    var s = 1.0;
    for (var i = 0; i < iters; i = i + 1) {
        let v = p0 * s;
        let a = vec3<f32>(
            v.x - 2.0 * floor(v.x * 0.5) - 1.0,
            v.y - 2.0 * floor(v.y * 0.5) - 1.0,
            v.z - 2.0 * floor(v.z * 0.5) - 1.0,
        );
        s = s * 3.0;
        let r = abs(vec3<f32>(1.0) - 3.0 * abs(a));
        let da = smax(r.x, r.y, kr);
        let db = smax(r.y, r.z, kr);
        let dc = smax(r.z, r.x, kr);
        let c = (smin(da, smin(db, dc, kr), kr) - 1.0) / s;
        d = smax(d, c, kr / s);
    }
    return d;
}
fn moon_de(p: vec3<f32>) -> f32 {
    return length(p - vec3<f32>(0.0, MOON_DIST, 0.0)) - R_MOON;
}
fn dive_iters() -> i32 {
    return min(4 + i32(floor(G.dive.x)), 11);
}
fn f_world(p: vec3<f32>) -> f32 {
    let world = MBS * menger(roty(p / MBS, G.time * WMORPH), dive_iters());
    return min(world, moon_de(p));
}

fn capsule_de(p: vec3<f32>) -> f32 {
    if (G.capsule_pos.w <= 0.0) { return 1e9; }
    let sq = max(G.dive.z, 0.3);
    let hh = G.capsule_up.w * sq;
    let rr = G.capsule_pos.w / sqrt(sq);
    let a = G.capsule_pos.xyz - G.capsule_up.xyz * hh;
    let b = G.capsule_pos.xyz + G.capsule_up.xyz * hh;
    let pa = p - a;
    let ba = b - a;
    let h = clamp(dot(pa, ba) / dot(ba, ba), 0.0, 1.0);
    return length(pa - ba * h) - rr;
}
fn nose_de(p: vec3<f32>) -> f32 {
    if (G.capsule_pos.w <= 0.0) { return 1e9; }
    let c = G.capsule_pos.xyz
        + G.capsule_up.xyz * (G.capsule_up.w * 0.55)
        + G.capsule_fwd.xyz * (G.capsule_pos.w * 1.05);
    return length(p - c) - G.capsule_pos.w * 0.6;
}
fn rope_de(p: vec3<f32>) -> f32 {
    if (G.grapple.w < 0.5) { return 1e9; }
    let a = G.capsule_pos.xyz;
    let b = G.grapple.xyz;
    let pa = p - a;
    let ba = b - a;
    let h = clamp(dot(pa, ba) / dot(ba, ba), 0.0, 1.0);
    return length(pa - ba * h) - 0.03;
}
// jetpack exhaust: a tapering plume below the feet, length + girth scale with the
// thrust intensity G.dive.w (0..1). Emissive in shading => an obvious flame.
fn jet_de(p: vec3<f32>) -> f32 {
    if (G.capsule_pos.w <= 0.0 || G.dive.w < 0.03) { return 1e9; }
    let rr = G.capsule_pos.w;
    let feet = G.capsule_pos.xyz - G.capsule_up.xyz * G.capsule_up.w;
    let len = rr * (2.5 + 7.0 * G.dive.w);
    let a = feet;
    let b = feet - G.capsule_up.xyz * len;
    let pa = p - a;
    let ba = b - a;
    let h = clamp(dot(pa, ba) / dot(ba, ba), 0.0, 1.0);
    let rad = rr * (1.05 - 0.9 * h); // wide at the feet, tapering to a point
    return length(pa - ba * h) - max(rad, 0.0001);
}
fn map(p: vec3<f32>) -> f32 {
    return min(min(min(f_world(p), capsule_de(p)), min(nose_de(p), rope_de(p))), jet_de(p));
}
fn normal(p: vec3<f32>) -> vec3<f32> {
    let e = vec2<f32>(0.004, 0.0);
    return normalize(vec3<f32>(
        map(p + e.xyy) - map(p - e.xyy),
        map(p + e.yxy) - map(p - e.yxy),
        map(p + e.yyx) - map(p - e.yyx),
    ));
}
fn pal(t: f32) -> vec3<f32> {
    return vec3<f32>(0.5) + vec3<f32>(0.5) * cos(TAU * (vec3<f32>(1.0) * t + vec3<f32>(0.0, 0.33, 0.67)));
}

@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    var ndc = in.uv * 2.0 - 1.0;
    let aspect = G.resolution.x / max(G.resolution.y, 1.0);
    ndc.x = ndc.x * aspect;
    let tanf = tan(G.fov * 0.5);
    let rd = normalize(G.cam_fwd.xyz + ndc.x * tanf * G.cam_right.xyz + ndc.y * tanf * G.cam_up.xyz);
    let ro = G.cam_pos.xyz;

    var t = 0.02;
    var hit = false;
    var steps = 0;
    let MAXS = 140;
    for (var i = 0; i < MAXS; i = i + 1) {
        steps = i;
        let p = ro + rd * t;
        let d = map(p);
        if (d < 0.0006 * t + 0.0004) {
            hit = true;
            break;
        }
        t = t + d * 0.85;
        if (t > 120.0) { break; }
    }

    var col = vec3<f32>(0.0);
    if (hit) {
        let p = ro + rd * t;
        let n = normal(p);
        let dterr = f_world(p);
        let dcap = capsule_de(p);
        let dnose = nose_de(p);
        let drope = rope_de(p);
        let djet = jet_de(p);
        let key = normalize(vec3<f32>(0.4, 0.8, 0.45));
        let diff = clamp(dot(n, key), 0.0, 1.0);
        let ao = 1.0 - f32(steps) / f32(MAXS);
        let rim = pow(1.0 - clamp(dot(n, -rd), 0.0, 1.0), 3.0);
        if (djet < dterr && djet < dcap && djet < dnose && djet < drope) {
            // jetpack flame — EMISSIVE (white-hot core -> orange -> deep red tip),
            // flickering, so it's unmistakable that the jetpack is firing
            let feet = G.capsule_pos.xyz - G.capsule_up.xyz * G.capsule_up.w;
            let along = clamp(dot(feet - p, G.capsule_up.xyz) / (G.capsule_pos.w * 9.0), 0.0, 1.0);
            let flick = 0.78 + 0.22 * sin(G.time * 55.0 + p.x * 31.0 + p.y * 27.0);
            let hot = mix(vec3<f32>(1.8, 1.6, 1.1), vec3<f32>(1.5, 0.45, 0.08), smoothstep(0.0, 0.5, along));
            let flame = mix(hot, vec3<f32>(0.9, 0.12, 0.04), smoothstep(0.5, 1.0, along));
            col = flame * flick * (0.85 + 0.6 * G.dive.w);
        } else if (dnose < dcap && dnose < dterr && dnose < drope) {
            col = vec3<f32>(1.0, 0.12, 0.12) * (0.4 + 0.8 * diff) + rim * vec3<f32>(0.5, 0.1, 0.1);
        } else if (drope < dcap && drope < dterr && drope < dnose) {
            col = vec3<f32>(0.9, 1.0, 0.8) * (0.4 + 0.6 * diff);
        } else if (dcap < dterr) {
            let h = clamp(dot(p - G.capsule_pos.xyz, G.capsule_up.xyz) / G.capsule_up.w * 0.5 + 0.5, 0.0, 1.0);
            col = mix(vec3<f32>(0.3, 0.35, 0.6), vec3<f32>(1.0, 0.97, 0.92), h) * (0.25 + 0.9 * diff)
                + rim * vec3<f32>(0.3, 0.5, 1.0) * 0.5;
        } else if (moon_de(p) < 0.4) {
            col = vec3<f32>(0.72, 0.74, 0.82) * (0.3 + 0.7 * diff) * (0.4 + 0.6 * ao)
                + rim * vec3<f32>(0.3, 0.4, 0.6) * 0.3;
        } else {
            // the fractal walls: color by local cell coordinate + face orientation +
            // descent depth, so each octave reads as its own chromatic stratum and the
            // tunnel walls/corners pick up distinct hues as you delve
            let cell = roty(p / MBS, G.time * WMORPH);
            let face = 0.15 * (n.x - n.z) + 0.1 * n.y;
            let strat = fract(length(cell) * 1.7 + 0.13 * G.dive.x);
            let base = pal(0.5 + face + 0.18 * strat + 0.09 * G.dive.x + 0.03 * G.time);
            col = base * (0.25 + 0.85 * diff) * (0.35 + 0.65 * ao);
            col = col + base * rim * 0.4;
        }
        col = mix(col, vec3<f32>(0.0), clamp(t / 100.0, 0.0, 1.0) * 0.45);
        // contact ring — only when grounded (contact.w>0.5), and with a floored
        // smoothstep edge so a deep dive (csc -> 0) can't divide-by-zero -> NaN.
        if (G.contact.w > 0.5) {
            let csc = exp2(-G.dive.x);
            let cd = abs(length(p - G.contact.xyz) - 0.30 * csc);
            let edge = max(0.1 * csc, 1.0e-4);
            col = col + (1.0 - smoothstep(0.0, edge, cd)) * vec3<f32>(0.4, 0.9, 1.0) * 0.8;
        }
    } else {
        col = pal(0.6 + 0.15 * rd.y + 0.05 * sin(G.time * 0.1)) * 0.05;
    }

    // grapple reticle (screen-center dot)
    let cc = (in.uv - vec2<f32>(0.5)) * vec2<f32>(aspect, 1.0);
    col = mix(col, vec3<f32>(1.0, 0.9, 0.6), (1.0 - smoothstep(0.004, 0.008, length(cc))) * 0.6);
    // final guard: never emit NaN/Inf into the feedback history (blob-proofing)
    col = select(col, vec3<f32>(0.0), col != col);
    col = clamp(col, vec3<f32>(0.0), vec3<f32>(1.0e4));
    return vec4<f32>(col, 1.0);
}
