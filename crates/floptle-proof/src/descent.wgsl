// Beat 3 — Descent into the Fractal Core. Render shader for a LOG-PERIODIC
// nested fractal shell-world: you are INSIDE it, descending forever through
// self-similar octaves, with spiral bridges, density glow, the player capsule +
// clown nose, and the grapple rope. Reuses post.wgsl + present.wgsl verbatim.
// The field is LOCK-STEP with descent.rs (measure_descent gated it green).

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
    capsule_pos: vec4<f32>, // xyz center, w = radius (<=0 hides => first person)
    capsule_up: vec4<f32>,  // xyz up, w = half-height
    contact: vec4<f32>,     // xyz contact point, w = ring strength
    capsule_fwd: vec4<f32>, // xyz facing
    dive: vec4<f32>,        // [dive_level, world_phase, squash, rho_player]
    grapple: vec4<f32>,     // [point.xyz, state] (0 idle, 1 firing, 2 attached)
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
const S: f32 = 2.0;
const RREF: f32 = 16.0;
const SHELL_TH: f32 = 2.3;
const KSH: f32 = 1.6;
const NARMS: i32 = 3;
const STEPS: i32 = 14;
const SWIRLS: f32 = 2.0;
const LATF: f32 = 1.0;
const LATAMP: f32 = 0.55;
const STRUT_R: f32 = 1.4;
const KARM: f32 = 1.8;
const WMORPH: f32 = 0.18;
const SIGMA: f32 = 2.1;
const R_OUT: f32 = 22.6274;
const TAU: f32 = 6.2831853;
const K_MAX: f32 = 1.0;       // bound the planet outward (infinite inward)
const MOON_DIST: f32 = 80.0;
const R_MOON: f32 = 6.0;

fn smin(a: f32, b: f32, k: f32) -> f32 {
    let h = clamp(0.5 + 0.5 * (b - a) / k, 0.0, 1.0);
    return mix(b, a, h) - k * h * (1.0 - h);
}
fn roty(p: vec3<f32>, a: f32) -> vec3<f32> {
    let s = sin(a);
    let c = cos(a);
    return vec3<f32>(c * p.x - s * p.z, p.y, s * p.x + c * p.z);
}
fn strut_center(a: i32, u: f32) -> vec3<f32> {
    let radius = R_OUT * pow(S, -u);
    let ph = f32(a) * 2.094;
    let lon = ph + TAU * SWIRLS * u;
    let lat = LATAMP * sin(TAU * LATF * u + ph);
    return vec3<f32>(cos(lat) * cos(lon), sin(lat), cos(lat) * sin(lon)) * radius;
}
// reference-octave geometry: hollow shell (you're inside) + spiral bridges
fn f_ref(pn: vec3<f32>) -> f32 {
    let q = roty(pn, WMORPH * G.time);
    var d = abs(length(q) - RREF) - SHELL_TH;
    for (var a = 0; a < NARMS; a = a + 1) {
        var da = 1e9;
        for (var i = 0; i < STEPS; i = i + 1) {
            let u = f32(i) / f32(STEPS - 1);
            da = smin(da, length(q - strut_center(a, u)) - STRUT_R, KARM);
        }
        d = smin(d, da, KSH);
    }
    return d;
}
fn oscale(p: vec3<f32>) -> f32 {
    let k = log(max(length(p), 1e-6) / RREF) / log(S);
    return pow(S, min(round(k), K_MAX));
}
fn moon_de(p: vec3<f32>) -> f32 {
    return length(p - vec3<f32>(0.0, MOON_DIST, 0.0)) - R_MOON;
}
// log-periodic planet (bounded outward) unioned with the moon
fn f_world(p: vec3<f32>) -> f32 {
    let os = oscale(p);
    return min(os * f_ref(p / os), moon_de(p));
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
// GEOMETRY-ONLY per-step DE (no noise/density in the march loop — perf rule)
fn map(p: vec3<f32>) -> f32 {
    return min(min(f_world(p), capsule_de(p)), min(nose_de(p), rope_de(p)));
}
fn normal(p: vec3<f32>) -> vec3<f32> {
    let e = vec2<f32>(0.003, 0.0);
    return normalize(vec3<f32>(
        map(p + e.xyy) - map(p - e.xyy),
        map(p + e.yxy) - map(p - e.yxy),
        map(p + e.yyx) - map(p - e.yyx),
    ));
}
fn pal(t: f32) -> vec3<f32> {
    return vec3<f32>(0.5) + vec3<f32>(0.5) * cos(TAU * (vec3<f32>(1.0) * t + vec3<f32>(0.0, 0.33, 0.67)));
}
fn hash3(p: vec3<f32>) -> f32 {
    return fract(sin(dot(p, vec3<f32>(12.9898, 78.233, 37.719))) * 43758.5453);
}
fn vnoise(p: vec3<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * (3.0 - 2.0 * f);
    let x00 = mix(hash3(i + vec3<f32>(0.0, 0.0, 0.0)), hash3(i + vec3<f32>(1.0, 0.0, 0.0)), u.x);
    let x10 = mix(hash3(i + vec3<f32>(0.0, 1.0, 0.0)), hash3(i + vec3<f32>(1.0, 1.0, 0.0)), u.x);
    let x01 = mix(hash3(i + vec3<f32>(0.0, 0.0, 1.0)), hash3(i + vec3<f32>(1.0, 0.0, 1.0)), u.x);
    let x11 = mix(hash3(i + vec3<f32>(0.0, 1.0, 1.0)), hash3(i + vec3<f32>(1.0, 1.0, 1.0)), u.x);
    return mix(mix(x00, x10, u.y), mix(x01, x11, u.y), u.z);
}
// density at the HIT POINT only (gaussians over bridge axes; matches descent.rs rho)
fn rho_at(p: vec3<f32>) -> f32 {
    let os = oscale(p);
    let q = roty(p / os, WMORPH * G.time);
    var s = 0.0;
    for (var a = 0; a < NARMS; a = a + 1) {
        for (var i = 0; i < STEPS; i = i + 1) {
            let u = f32(i) / f32(STEPS - 1);
            let dv = q - strut_center(a, u);
            s = s + exp(-dot(dv, dv) / (2.0 * SIGMA * SIGMA));
        }
    }
    return min(s, 1.0);
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
    var glow = 0.0;
    let MAXS = 120;
    for (var i = 0; i < MAXS; i = i + 1) {
        steps = i;
        let p = ro + rd * t;
        let d = map(p);
        glow = glow + exp(-d * 9.0) * 0.0012;
        if (d < 0.0007 * t + 0.0004) {
            hit = true;
            break;
        }
        t = t + d * 0.85;
        if (t > 140.0) { break; }
    }

    let lv = G.dive.x;
    var col = vec3<f32>(0.0);
    if (hit) {
        let p = ro + rd * t;
        let n = normal(p);
        let dterr = f_world(p);
        let dcap = capsule_de(p);
        let dnose = nose_de(p);
        let drope = rope_de(p);
        let key = normalize(vec3<f32>(0.4, 0.8, 0.45));
        let diff = clamp(dot(n, key), 0.0, 1.0);
        let ao = 1.0 - f32(steps) / f32(MAXS);
        let rim = pow(1.0 - clamp(dot(n, -rd), 0.0, 1.0), 3.0);
        if (dnose < dcap && dnose < dterr && dnose < drope) {
            col = vec3<f32>(1.0, 0.12, 0.12) * (0.4 + 0.8 * diff) + rim * vec3<f32>(0.5, 0.1, 0.1);
        } else if (drope < dcap && drope < dterr && drope < dnose) {
            col = vec3<f32>(0.9, 1.0, 0.8) * (0.4 + 0.6 * diff);
        } else if (dcap < dterr) {
            let h = clamp(dot(p - G.capsule_pos.xyz, G.capsule_up.xyz) / G.capsule_up.w * 0.5 + 0.5, 0.0, 1.0);
            col = mix(vec3<f32>(0.3, 0.35, 0.6), vec3<f32>(1.0, 0.97, 0.92), h) * (0.25 + 0.9 * diff)
                + rim * vec3<f32>(0.3, 0.5, 1.0) * 0.5;
        } else if (moon_de(p) < 0.4) {
            // the moon: clean pale body so it reads as a separate world
            col = vec3<f32>(0.72, 0.74, 0.82) * (0.3 + 0.7 * diff) * (0.4 + 0.6 * ao)
                + rim * vec3<f32>(0.3, 0.4, 0.6) * 0.3;
        } else {
            // CLEAN fractal geometry: palette coloring + diffuse + AO + a gentle
            // radial contour pattern. No noise/glow so the raw geometry is legible.
            let os = oscale(p);
            let pn = p / os;
            let r = rho_at(p);
            let band = 0.5 + 0.5 * sin(length(pn) * 2.5);
            let base = pal(0.5 + 0.07 * length(pn) + 0.3 * r + 0.08 * lv + G.dive.y * 0.1);
            col = base * (0.22 + 0.78 * diff) * (0.4 + 0.6 * ao);
            col = mix(col, base * 1.25, band * 0.18);
            col = col + base * rim * 0.25;
        }
        col = mix(col, vec3<f32>(0.0), clamp(t / 110.0, 0.0, 1.0) * 0.45);
        let cd = abs(length(p - G.contact.xyz) - 0.30);
        col = col + G.contact.w * (1.0 - smoothstep(0.0, 0.1, cd)) * vec3<f32>(0.4, 0.9, 1.0) * 0.8;
    } else {
        col = pal(0.55 + 0.1 * sin(G.time * 0.1) + 0.2 * rd.y) * 0.05;
    }
    glow = min(glow, 1.4);
    col = col + pal(0.3 + G.dive.y * 0.1) * glow * 0.15;

    // grapple reticle (screen-center crosshair dot)
    let cc = (in.uv - vec2<f32>(0.5)) * vec2<f32>(aspect, 1.0);
    col = mix(col, vec3<f32>(1.0, 0.9, 0.6), (1.0 - smoothstep(0.004, 0.008, length(cc))) * 0.6);
    return vec4<f32>(col, 1.0);
}
