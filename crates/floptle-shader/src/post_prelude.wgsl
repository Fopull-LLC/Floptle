// The seam contract for `stage post` shaders — the module a generated post
// chunk is concatenated onto.
//
// Unlike TEST_PRELUDE (a hand-written MIRROR of raster.wgsl, kept in step by
// hand), this file IS the real thing: floptle-render's PostStack builds every
// custom post pipeline from `POST_PRELUDE + POST_FIELD_SHIM + SUPPORT + chunk`,
// and the editor validates against the same text. There is nothing to drift.
//
// The bind groups it declares must match the layouts PostStack builds:
//   group(0) — the frame so far + the chain's own params (the SAME layout every
//              built-in pass uses, so a custom pass can ping-pong between the
//              chain's scratch targets with no bind group of its own).
//   group(1) — the depth buffer the scene was rendered with, and the inverse
//              projection that turns it back into a position.
//   group(2) — the shader's own uniforms (declared by the chunk).

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
struct PostChainParams {
    a: vec4<f32>, // xy = texel (1/chain size)
    b: vec4<f32>,
    c: vec4<f32>,
    d: vec4<f32>,
    e: vec4<f32>, // w = time (seconds)
    f: vec4<f32>,
    g: vec4<f32>, // y = aspect (w/h)
};
@group(0) @binding(2) var<uniform> p: PostChainParams;

@group(1) @binding(0) var post_depth_tex: texture_depth_2d;
struct PostCam {
    inv_proj: mat4x4<f32>, // clip → view
};
@group(1) @binding(1) var<uniform> pc: PostCam;

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

// What `sceneDepth` returns where there is no geometry. A large finite number
// rather than infinity, on purpose: it keeps every comparison and subtraction an
// author writes finite, and it makes a silhouette against the sky the largest
// depth step in the frame — which is exactly what an outline wants to find.
const FLSL_SKY_DEPTH: f32 = 1.0e6;

// One pixel of the chain, in uv. Follows the RETRO resolution when one is set,
// so an effect written in texels stays one pixel wide instead of getting
// magnified by the upscale.
fn flsl_post_texel() -> vec2<f32> {
    return p.a.xy;
}

// The frame so far, in real light — this is upstream of the tonemap, so a bright
// light really does read brighter than 1.0.
fn flsl_post_color(uv: vec2<f32>) -> vec4<f32> {
    return textureSample(tex, samp, clamp(uv, vec2<f32>(0.0), vec2<f32>(1.0)));
}

// Raw depth at a uv. textureLoad because Depth32Float is not filterable, which
// also makes every read here resolution-agnostic: in retro mode this is the
// low-res retro depth and the effect goes chunky with the same pixels.
fn flsl_post_raw_depth(uv: vec2<f32>) -> f32 {
    let dims = vec2<i32>(textureDimensions(post_depth_tex));
    let pix = clamp(vec2<i32>(clamp(uv, vec2<f32>(0.0), vec2<f32>(1.0)) * vec2<f32>(dims)),
                    vec2<i32>(0), dims - vec2<i32>(1));
    return textureLoad(post_depth_tex, pix, 0);
}

// View-space position of whatever is seen at `uv`.
fn flsl_post_view(uv: vec2<f32>) -> vec3<f32> {
    let d = flsl_post_raw_depth(uv);
    let ndc = vec3<f32>(uv.x * 2.0 - 1.0, (1.0 - uv.y) * 2.0 - 1.0, d);
    let v = pc.inv_proj * vec4<f32>(ndc, 1.0);
    return v.xyz / v.w;
}

// Distance from the camera, in world units. The camera looks down -z in view
// space, so the distance is -z.
fn flsl_post_depth(uv: vec2<f32>) -> f32 {
    if (flsl_post_raw_depth(uv) >= 1.0) {
        return FLSL_SKY_DEPTH;
    }
    return max(-flsl_post_view(uv).z, 0.0);
}

// The surface normal, in view space (+z toward the camera), worked out from the
// depth buffer — so it exists for meshes, terrain, raymarched SDF matter and
// anything else that writes depth, with nothing to author or import.
//
// Per axis it differences against whichever neighbour is CLOSER in depth: the
// naive one-sided derivative smears a normal across a silhouette, which is
// precisely where an edge detect is looking.
fn flsl_post_normal(uv: vec2<f32>) -> vec3<f32> {
    if (flsl_post_raw_depth(uv) >= 1.0) {
        return vec3<f32>(0.0, 0.0, 1.0);
    }
    let t = p.a.xy;
    let c = flsl_post_view(uv);
    let px = flsl_post_view(uv + vec2<f32>(t.x, 0.0));
    let mx = flsl_post_view(uv - vec2<f32>(t.x, 0.0));
    let py = flsl_post_view(uv + vec2<f32>(0.0, t.y));
    let my = flsl_post_view(uv - vec2<f32>(0.0, t.y));
    var ddx = px - c;
    if (abs(mx.z - c.z) < abs(px.z - c.z)) { ddx = c - mx; }
    var ddy = py - c;
    if (abs(my.z - c.z) < abs(py.z - c.z)) { ddy = c - my; }
    // Screen +y is down, so ddy × ddx faces the camera (+z in view space).
    let n = cross(ddy, ddx);
    if (dot(n, n) < 1e-20) {
        return vec3<f32>(0.0, 0.0, 1.0);
    }
    var un = normalize(n);
    if (dot(un, -c) < 0.0) { un = -un; }
    return un;
}
