// Beat 1 — feedback / "melting trails" post pass.
// Reads the fresh scene + the previous frame's composite (history), warps the
// history with a swirl + slight zoom, and adds it back for infinite dream-trails.

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
@group(1) @binding(0) var sceneTex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;
@group(1) @binding(2) var histTex: texture_2d<f32>;

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

@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    let scene = textureSample(sceneTex, samp, in.uv).rgb;

    // Swirl + zoom the history sample → trails spiral inward and melt.
    let c = in.uv - vec2<f32>(0.5);
    let ang = G.warp * 0.05 * length(c);
    let s = sin(ang);
    let co = cos(ang);
    var huv = vec2<f32>(c.x * co - c.y * s, c.x * s + c.y * co) * 0.997 + vec2<f32>(0.5);
    huv = huv + 0.0016 * vec2<f32>(sin(in.uv.y * 11.0 + G.time), cos(in.uv.x * 11.0 + G.time));

    let hist = textureSample(histTex, samp, huv).rgb;
    let col = scene + hist * G.feedback;
    return vec4<f32>(col, 1.0);
}
