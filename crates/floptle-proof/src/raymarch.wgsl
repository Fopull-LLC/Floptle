// Beat 1 — morphing Mandelbox raymarch (stylized, NOT physical lighting).
// Renders one fullscreen pass into a half-res HDR target.

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
};
@group(0) @binding(0) var<uniform> G: Globals;

struct VOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32) -> VOut {
    // One big fullscreen triangle.
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

struct Hit { d: f32, trap: vec3<f32> };

// Mandelbox distance estimator with time-morphing parameters.
fn map(p: vec3<f32>) -> Hit {
    let tt = G.time;
    let scale = mix(-1.7, -2.6, 0.5 + 0.5 * sin(tt * 0.06));
    let minr2 = 0.20 + 0.12 * (0.5 + 0.5 * sin(tt * 0.045));
    let fixr2 = 1.0;
    var z = p;
    var dz = 1.0;
    var trap = vec3<f32>(1e9, 1e9, 1e9);
    for (var i = 0; i < 14; i = i + 1) {
        z = clamp(z, vec3<f32>(-1.0), vec3<f32>(1.0)) * 2.0 - z; // box fold
        let r2 = dot(z, z);
        if (r2 < minr2) {
            let f = fixr2 / minr2;
            z = z * f; dz = dz * f;
        } else if (r2 < fixr2) {
            let f = fixr2 / r2;
            z = z * f; dz = dz * f;
        }
        z = scale * z + p + vec3<f32>(0.12, -0.07, 0.05); // subtle asymmetry (not the mirror fix)
        dz = dz * abs(scale) + 1.0;
        trap = min(trap, abs(z));
    }
    var h: Hit;
    h.d = length(z) / abs(dz);
    h.trap = trap;
    return h;
}

fn normal(p: vec3<f32>) -> vec3<f32> {
    let e = vec2<f32>(0.0006, 0.0);
    return normalize(vec3<f32>(
        map(p + e.xyy).d - map(p - e.xyy).d,
        map(p + e.yxy).d - map(p - e.yxy).d,
        map(p + e.yyx).d - map(p - e.yyx).d,
    ));
}

// IQ cosine palette.
fn pal(t: f32) -> vec3<f32> {
    let a = vec3<f32>(0.5, 0.5, 0.5);
    let b = vec3<f32>(0.5, 0.5, 0.5);
    let c = vec3<f32>(1.0, 1.0, 1.0);
    let d = vec3<f32>(0.0, 0.33, 0.67);
    return a + b * cos(6.28318 * (c * t + d));
}

@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    var ndc = in.uv * 2.0 - 1.0;
    ndc.y = -ndc.y;
    let aspect = G.resolution.x / max(G.resolution.y, 1.0);
    ndc.x = ndc.x * aspect;
    let tanf = tan(G.fov * 0.5);
    let rd = normalize(G.cam_fwd.xyz + ndc.x * tanf * G.cam_right.xyz + ndc.y * tanf * G.cam_up.xyz);
    let ro = G.cam_pos.xyz;

    var t = 0.0;
    var hit = false;
    var trap = vec3<f32>(1.0);
    var steps = 0;
    var glow = 0.0;
    let MAXS = 128;
    for (var i = 0; i < MAXS; i = i + 1) {
        steps = i;
        let p = ro + rd * t;
        let h = map(p);
        glow = glow + exp(-h.d * 9.0) * 0.0045; // volumetric shimmer in the void
        if (h.d < 0.0009 * t + 0.00035) {
            hit = true;
            trap = h.trap;
            break;
        }
        t = t + h.d * 0.85;
        if (t > 60.0) { break; }
    }

    var col = vec3<f32>(0.0);
    if (hit) {
        let p = ro + rd * t;
        let n = normal(p);
        let key = normalize(vec3<f32>(0.5, 0.7, 0.4));
        let diff = clamp(dot(n, key), 0.0, 1.0);
        let rim = pow(1.0 - clamp(dot(n, -rd), 0.0, 1.0), 3.0);
        let ao = 1.0 - f32(steps) / f32(MAXS);
        let base = pal(0.55 + 0.45 * sin(3.0 * (trap.x + trap.y + trap.z) + G.time * 0.2));
        col = base * (0.11 + 0.85 * diff) * (0.22 + 0.78 * ao);
        col = col + base * rim * 0.3;
        col = col + pow(clamp(dot(reflect(rd, n), key), 0.0, 1.0), 32.0) * vec3<f32>(0.22);
        col = mix(col, vec3<f32>(0.0), clamp(t / 50.0, 0.0, 1.0) * 0.6);
    } else {
        let bg = pal(0.6 + 0.2 * sin(G.time * 0.1) + 0.3 * rd.y);
        col = bg * 0.045;
    }
    glow = min(glow, 1.2);
    col = col + pal(0.3 + G.time * 0.03) * glow * 0.5;
    return vec4<f32>(col, 1.0);
}
