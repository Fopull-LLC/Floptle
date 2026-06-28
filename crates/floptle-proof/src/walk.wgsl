// Beat 2 — "Stand in the Dream": render shader for the walkable fractal planetoid.
//
// The EYE sees a solid planetoid (smooth core + blended hills "macro" field,
// in lock-step with the CPU collision field in walk.rs) wearing a bounded
// fractal CRUST, plus the player capsule and a contact-glow ring. The FEET
// (walk.rs) collide only against the smooth macro field — render-detailed /
// collide-smooth, the fix the design panel converged on. Reuses Beat 1's
// post.wgsl + present.wgsl verbatim.

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
    capsule_pos: vec4<f32>, // xyz center, w = radius
    capsule_up: vec4<f32>,  // xyz up, w = half-height
    contact: vec4<f32>,     // xyz contact point, w = ring strength
    capsule_fwd: vec4<f32>, // xyz facing direction
};
@group(0) @binding(0) var<uniform> G: Globals;

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
    o.uv = xy * 0.5 + vec2<f32>(0.5, 0.5);
    return o;
}

// ---- macro field constants (LOCK-STEP with walk.rs) ----
const R0: f32 = 30.0;
const KB: f32 = 4.0;
const WARP_A: f32 = 1.0;
const WARP_W: f32 = 0.8;
const WARP_K: f32 = 0.10;
const CRUST_AMP: f32 = 0.3;
// swirling-branch "arms" that spiral up off the planet into the sky
const ARM_STEPS: i32 = 10;
const SWIRLS: f32 = 1.25;
const LAT0: f32 = -0.25;
const LATTOP: f32 = 1.15;
const ARM_RISE: f32 = 22.0;
const ARM_R: f32 = 5.0;
const ARM_KB: f32 = 3.0;

fn smin(a: f32, b: f32, k: f32) -> f32 {
    let h = clamp(0.5 + 0.5 * (b - a) / k, 0.0, 1.0);
    return mix(b, a, h) - k * h * (1.0 - h);
}

fn warpv(p: vec3<f32>) -> vec3<f32> {
    let t = G.time;
    return WARP_A * vec3<f32>(
        sin(WARP_W * t + WARP_K * p.y),
        sin(WARP_W * t + WARP_K * p.z),
        sin(WARP_W * t + WARP_K * p.x),
    );
}

fn bumpd(q: vec3<f32>, dir: vec3<f32>, r: f32) -> f32 {
    let c = normalize(dir) * (R0 * 0.86);
    return length(q - c) - r;
}

// A "branch": a tapering chain of blended spheres following a helix that winds
// around the planet AND lifts off its surface into the sky.
fn arm(q: vec3<f32>, phase: f32) -> f32 {
    var d = 1e9;
    for (var k = 0; k < ARM_STEPS; k = k + 1) {
        let t = f32(k) / f32(ARM_STEPS - 1);
        let lon = phase + 6.2831853 * SWIRLS * t;
        let lat = LAT0 + (LATTOP - LAT0) * t;
        let dir = vec3<f32>(cos(lat) * cos(lon), sin(lat), cos(lat) * sin(lon));
        let center = dir * (R0 + ARM_RISE * t);
        d = smin(d, length(q - center) - ARM_R * (1.0 - 0.5 * t), ARM_KB);
    }
    return d;
}

// Smooth, genuinely-solid "planetoid": a core sphere with blended hills + the
// two swirling branches.
fn f_macro(p: vec3<f32>) -> f32 {
    let q = p + warpv(p);
    var d = length(q) - R0;
    d = smin(d, bumpd(q, vec3<f32>(1.0, 0.0, 0.2), 7.0), KB);
    d = smin(d, bumpd(q, vec3<f32>(-0.8, 0.3, 0.6), 6.0), KB);
    d = smin(d, bumpd(q, vec3<f32>(0.2, 1.0, 0.0), 7.0), KB);
    d = smin(d, bumpd(q, vec3<f32>(0.0, -1.0, 0.3), 5.5), KB);
    d = smin(d, bumpd(q, vec3<f32>(0.5, 0.2, -1.0), 8.0), KB);
    d = smin(d, bumpd(q, vec3<f32>(-0.4, -0.5, -0.8), 6.0), KB);
    d = smin(d, arm(q, 0.0), ARM_KB);
    d = smin(d, arm(q, 3.3), ARM_KB);
    return d;
}

// Bounded fractal crust — visual only, adds OUTWARD so the feet never float.
fn crust(p: vec3<f32>) -> f32 {
    let q = p + warpv(p);
    var c = 0.0;
    var a = 0.5;
    var f = 2.4;
    for (var i = 0; i < 3; i = i + 1) {
        c = c + a * (0.5 + 0.5 * sin(f * q.x) * sin(f * q.y + G.time * 0.2) * sin(f * q.z));
        a = a * 0.5;
        f = f * 2.13;
    }
    return c; // ~0..1
}

fn f_terrain(p: vec3<f32>) -> f32 {
    return f_macro(p) - CRUST_AMP * crust(p);
}

fn capsule_de(p: vec3<f32>) -> f32 {
    if (G.capsule_pos.w <= 0.0) { return 1e9; } // hidden (first person)
    let a = G.capsule_pos.xyz - G.capsule_up.xyz * G.capsule_up.w;
    let b = G.capsule_pos.xyz + G.capsule_up.xyz * G.capsule_up.w;
    let pa = p - a;
    let ba = b - a;
    let h = clamp(dot(pa, ba) / dot(ba, ba), 0.0, 1.0);
    return length(pa - ba * h) - G.capsule_pos.w;
}

// A "clown nose": a small sphere on the upper-front of the capsule, so you can
// read both the facing direction and which way is up at a glance.
fn nose_de(p: vec3<f32>) -> f32 {
    if (G.capsule_pos.w <= 0.0) { return 1e9; } // hidden (first person)
    let c = G.capsule_pos.xyz
        + G.capsule_up.xyz * (G.capsule_up.w * 0.55)
        + G.capsule_fwd.xyz * (G.capsule_pos.w * 1.05);
    return length(p - c) - G.capsule_pos.w * 0.6;
}

fn map(p: vec3<f32>) -> f32 {
    return min(min(f_terrain(p), capsule_de(p)), nose_de(p));
}

fn normal(p: vec3<f32>) -> vec3<f32> {
    let e = vec2<f32>(0.0025, 0.0);
    return normalize(vec3<f32>(
        map(p + e.xyy) - map(p - e.xyy),
        map(p + e.yxy) - map(p - e.yxy),
        map(p + e.yyx) - map(p - e.yyx),
    ));
}

fn pal(t: f32) -> vec3<f32> {
    return vec3<f32>(0.5) + vec3<f32>(0.5) * cos(6.28318 * (vec3<f32>(1.0) * t + vec3<f32>(0.0, 0.33, 0.67)));
}

@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    // NOTE: unlike Beat 1's raymarch.wgsl, NO ndc.y negation here — with the
    // render-to-texture pass chain that flip renders the world upside-down,
    // which is invisible on a symmetric fractal but obvious on a planet.
    var ndc = in.uv * 2.0 - 1.0;
    let aspect = G.resolution.x / max(G.resolution.y, 1.0);
    ndc.x = ndc.x * aspect;
    let tanf = tan(G.fov * 0.5);
    let rd = normalize(G.cam_fwd.xyz + ndc.x * tanf * G.cam_right.xyz + ndc.y * tanf * G.cam_up.xyz);
    let ro = G.cam_pos.xyz;

    var t = 0.0;
    var hit = false;
    var steps = 0;
    var glow = 0.0;
    let MAXS = 100;
    for (var i = 0; i < MAXS; i = i + 1) {
        steps = i;
        let p = ro + rd * t;
        let d = map(p);
        glow = glow + exp(-d * 10.0) * 0.0035;
        if (d < 0.0008 * t + 0.0004) {
            hit = true;
            break;
        }
        t = t + d * 0.9;
        if (t > 130.0) { break; }
    }

    var col = vec3<f32>(0.0);
    if (hit) {
        let p = ro + rd * t;
        let n = normal(p);
        let dterr = f_terrain(p);
        let dcap = capsule_de(p);
        let dnose = nose_de(p);
        let key = normalize(vec3<f32>(0.5, 0.8, 0.35));
        let diff = clamp(dot(n, key), 0.0, 1.0);
        let ao = 1.0 - f32(steps) / f32(MAXS);
        let rim = pow(1.0 - clamp(dot(n, -rd), 0.0, 1.0), 3.0);
        if (dnose < dcap && dnose < dterr) {
            // red clown nose
            col = vec3<f32>(1.0, 0.12, 0.12) * (0.4 + 0.8 * diff) + rim * vec3<f32>(0.5, 0.1, 0.1);
        } else if (dcap < dterr) {
            // body: bright "up" end, darker "down" end so up reads at a glance
            let h = clamp(dot(p - G.capsule_pos.xyz, G.capsule_up.xyz) / G.capsule_up.w * 0.5 + 0.5, 0.0, 1.0);
            let body = mix(vec3<f32>(0.35, 0.4, 0.6), vec3<f32>(1.0, 0.97, 0.92), h);
            col = body * (0.25 + 0.9 * diff) + rim * vec3<f32>(0.3, 0.5, 1.0) * 0.5;
        } else {
            let cv = crust(p);
            let base = pal(0.45 + 0.05 * length(p) + 0.5 * cv + G.time * 0.02);
            col = base * (0.12 + 0.85 * diff) * (0.25 + 0.75 * ao);
            col = col + base * rim * 0.3;
        }
        col = mix(col, vec3<f32>(0.0), clamp(t / 90.0, 0.0, 1.0) * 0.5);
        // contact-glow ring on the surface where the feet touch
        let cd = abs(length(p - G.contact.xyz) - 0.35);
        col = col + G.contact.w * (1.0 - smoothstep(0.0, 0.12, cd)) * vec3<f32>(0.4, 0.9, 1.0) * 0.8;
    } else {
        let bg = pal(0.6 + 0.2 * sin(G.time * 0.1) + 0.3 * rd.y);
        col = bg * 0.04;
    }
    glow = min(glow, 1.2);
    col = col + pal(0.3 + G.time * 0.03) * glow * 0.5;
    return vec4<f32>(col, 1.0);
}
