// Beat 1 — present pass: upscale the half-res composite to the swapchain,
// tonemap, chromatic aberration, vignette, dither, faint retro scanline.

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
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;

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

fn aces(x: vec3<f32>) -> vec3<f32> {
    let a = 2.51;
    let b = 0.03;
    let c = 2.43;
    let d = 0.59;
    let e = 0.14;
    return clamp((x * (a * x + b)) / (x * (c * x + d) + e), vec3<f32>(0.0), vec3<f32>(1.0));
}

fn hash(p: vec2<f32>) -> f32 {
    return fract(sin(dot(p, vec2<f32>(12.9898, 78.233))) * 43758.5453);
}

@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    let ca = (in.uv - vec2<f32>(0.5)) * 0.0024; // chromatic aberration
    var col = vec3<f32>(0.0);
    col.r = textureSample(tex, samp, in.uv + ca).r;
    col.g = textureSample(tex, samp, in.uv).g;
    col.b = textureSample(tex, samp, in.uv - ca).b;

    col = aces(col * 0.95);

    let vig = smoothstep(1.15, 0.35, length(in.uv - vec2<f32>(0.5)));
    col = col * vig;

    col = col + (hash(in.uv * G.resolution + vec2<f32>(G.frame)) - 0.5) * (1.0 / 255.0);
    col = col * (0.94 + 0.06 * sin(in.uv.y * G.resolution.y * 1.5708 + G.time));

    return vec4<f32>(col, 1.0);
}
