// Beat 3 — render shader: an actual morphing MANDELBULB fractal you walk on,
// with a moon, the player capsule + clown nose + grapple rope + reticle. Orbit-
// trap palette coloring (the Beat-1 look) over clean lighting. LOCK-STEP field
// with descent.rs. Reuses post.wgsl + present_plain.wgsl.

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

// morphing Mandelbulb DE (signed: <0 inside the solid bulb). `iters` grows with
// the dive to unfold finer detail.
fn bulb_de(p0: vec3<f32>, t: f32, iters: i32) -> f32 {
    let power = 8.0 + 1.5 * sin(t * WMORPH);
    var z = p0;
    var dr = 1.0;
    var r = 0.0;
    for (var i = 0; i < iters; i = i + 1) {
        r = length(z);
        if (r > 2.0) { break; }
        let theta = acos(clamp(z.z / r, -1.0, 1.0));
        let phi = atan2(z.y, z.x);
        dr = pow(r, power - 1.0) * power * dr + 1.0;
        let zr = pow(r, power);
        let th = theta * power;
        let ph = phi * power;
        z = vec3<f32>(sin(th) * cos(ph), sin(th) * sin(ph), cos(th)) * zr + p0;
    }
    return 0.5 * log(max(r, 1e-6)) * r / dr;
}
// orbit trap (min radius during iteration) -> a 0..1-ish value for coloring
fn bulb_trap(p0: vec3<f32>, t: f32, iters: i32) -> f32 {
    let power = 8.0 + 1.5 * sin(t * WMORPH);
    var z = p0;
    var trap = 1e9;
    for (var i = 0; i < iters; i = i + 1) {
        let r = length(z);
        if (r > 2.0) { break; }
        trap = min(trap, r);
        let theta = acos(clamp(z.z / r, -1.0, 1.0));
        let phi = atan2(z.y, z.x);
        let zr = pow(r, power);
        z = vec3<f32>(sin(theta * power) * cos(phi * power), sin(theta * power) * sin(phi * power), cos(theta * power)) * zr + p0;
    }
    return trap;
}
fn moon_de(p: vec3<f32>) -> f32 {
    return length(p - vec3<f32>(0.0, MOON_DIST, 0.0)) - R_MOON;
}
// dive-driven planet scale (W shrinks within a level => continuous zoom) + iters
fn dive_w() -> f32 {
    let dv = G.dive.x;
    return MBS * exp2(-(dv - floor(dv)));
}
fn dive_iters() -> i32 {
    return min(8 + i32(floor(G.dive.x)), 12);
}
fn f_world(p: vec3<f32>) -> f32 {
    let w = dive_w();
    return min(w * bulb_de(p / w, G.time, dive_iters()), moon_de(p));
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
fn map(p: vec3<f32>) -> f32 {
    return min(min(f_world(p), capsule_de(p)), min(nose_de(p), rope_de(p)));
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
            col = vec3<f32>(0.72, 0.74, 0.82) * (0.3 + 0.7 * diff) * (0.4 + 0.6 * ao)
                + rim * vec3<f32>(0.3, 0.4, 0.6) * 0.3;
        } else {
            // the fractal: orbit-trap palette (the Beat-1 look) + clean lighting,
            // tinted by descent depth so deeper octaves read as a chromatic journey
            let w = dive_w();
            let trap = bulb_trap(p / w, G.time, dive_iters());
            let base = pal(0.55 + 0.6 * trap + 0.05 * length(p / w) + 0.04 * G.time + 0.08 * G.dive.x);
            col = base * (0.25 + 0.85 * diff) * (0.35 + 0.65 * ao);
            col = col + base * rim * 0.4;
        }
        col = mix(col, vec3<f32>(0.0), clamp(t / 100.0, 0.0, 1.0) * 0.45);
        let cd = abs(length(p - G.contact.xyz) - 0.30);
        col = col + G.contact.w * (1.0 - smoothstep(0.0, 0.1, cd)) * vec3<f32>(0.4, 0.9, 1.0) * 0.8;
    } else {
        col = pal(0.6 + 0.15 * rd.y + 0.05 * sin(G.time * 0.1)) * 0.05;
    }

    // grapple reticle (screen-center dot)
    let cc = (in.uv - vec2<f32>(0.5)) * vec2<f32>(aspect, 1.0);
    col = mix(col, vec3<f32>(1.0, 0.9, 0.6), (1.0 - smoothstep(0.004, 0.008, length(cc))) * 0.6);
    return vec4<f32>(col, 1.0);
}
