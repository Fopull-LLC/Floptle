// Screen-space ambient occlusion (the PostStack's SSAO pass): estimates, per
// pixel, how much nearby geometry hoods the surface — from the depth buffer alone,
// so it covers raster meshes and raymarched SDF matter alike (both write real
// depth). View-space position is reconstructed from depth via the inverse
// projection; the normal from depth differences (picking the nearer neighbor per
// axis so silhouettes don't smear); occlusion from a golden-angle spiral of
// hemisphere samples, each reprojected and depth-compared with a range check so
// far-behind geometry never darkens a foreground surface.
//
// Output is a single-channel AO factor (1 = open, 0 = fully occluded) into a
// half-res R8 target; the PostStack blurs it and multiplies it over the scene.
// Depth is read with textureLoad (Depth32Float is non-filterable), which also
// makes the pass resolution-agnostic: in retro mode it samples the low-res retro
// depth and the AO simply goes chunky with the pixels.

@group(0) @binding(0) var depth_tex: texture_depth_2d;
struct SsaoParams {
    proj: mat4x4<f32>,     // camera projection (view → clip)
    inv_proj: mat4x4<f32>, // clip → view
    // x = radius (world units), y = strength (0..1), z = depth bias, w unused.
    params: vec4<f32>,
};
@group(0) @binding(1) var<uniform> sp: SsaoParams;

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

fn load_depth(pix: vec2<i32>) -> f32 {
    let dims = vec2<i32>(textureDimensions(depth_tex));
    let p = clamp(pix, vec2<i32>(0), dims - 1);
    return textureLoad(depth_tex, p, 0);
}

// View-space position of the surface seen at `uv` with depth `d`.
fn view_pos(uv: vec2<f32>, d: f32) -> vec3<f32> {
    let ndc = vec3<f32>(uv.x * 2.0 - 1.0, (1.0 - uv.y) * 2.0 - 1.0, d);
    let v = sp.inv_proj * vec4<f32>(ndc, 1.0);
    return v.xyz / v.w;
}

fn pos_at(pix: vec2<i32>, dims: vec2<f32>) -> vec3<f32> {
    let uv = (vec2<f32>(pix) + 0.5) / dims;
    return view_pos(uv, load_depth(pix));
}

@fragment
fn fs_ssao(in: VsOut) -> @location(0) vec4<f32> {
    let dims_i = vec2<i32>(textureDimensions(depth_tex));
    let dims = vec2<f32>(dims_i);
    let pix = clamp(vec2<i32>(in.uv * dims), vec2<i32>(0), dims_i - 1);
    let d = load_depth(pix);
    if (d >= 1.0) {
        // Sky/background: fully open.
        return vec4<f32>(1.0, 0.0, 0.0, 1.0);
    }
    let c = view_pos((vec2<f32>(pix) + 0.5) / dims, d);

    // Normal from depth. Per axis, difference against whichever neighbor is closer
    // in depth — the naive one-sided derivative smears normals across silhouettes.
    let px = pos_at(pix + vec2<i32>(1, 0), dims);
    let mx = pos_at(pix - vec2<i32>(1, 0), dims);
    let py = pos_at(pix + vec2<i32>(0, 1), dims);
    let my = pos_at(pix - vec2<i32>(0, 1), dims);
    var ddx = px - c;
    if (abs(mx.z - c.z) < abs(px.z - c.z)) { ddx = c - mx; }
    var ddy = py - c;
    if (abs(my.z - c.z) < abs(py.z - c.z)) { ddy = c - my; }
    // Screen +y is down, so ddy × ddx faces the camera (+z in view space).
    var n = normalize(cross(ddy, ddx));
    if (dot(n, -c) < 0.0) { n = -n; }

    // Tangent basis for the sampling hemisphere.
    var up = vec3<f32>(0.0, 1.0, 0.0);
    if (abs(n.y) > 0.9) { up = vec3<f32>(1.0, 0.0, 0.0); }
    let t = normalize(cross(up, n));
    let b = cross(n, t);

    // White-noise hash rotates the spiral per pixel — stable (no time term) and
    // structure-free, so the blur that follows averages it away cleanly (gradient
    // noise left periodic scallops at contact edges).
    var h3 = fract(vec3<f32>(vec2<f32>(pix).xyx) * 0.1031);
    h3 = h3 + dot(h3, h3.yzx + 33.33);
    let ang0 = fract((h3.x + h3.y) * h3.z) * 6.2831853;

    let radius = max(sp.params.x, 1e-3);
    let bias = sp.params.z;
    let taps = 16;
    // Per-pixel ring jitter (decorrelated from the rotation) so the spiral's
    // radii don't band into stripes that survive the blur.
    let rj = fract(ang0 * 1.6180339);
    var occ = 0.0;
    for (var i = 0; i < taps; i = i + 1) {
        // Golden-angle spiral over the disc (sqrt for even area coverage), lifted
        // along the normal so a flat surface never occludes itself.
        let ang = ang0 + f32(i) * 2.3999632;
        let r = radius * sqrt((f32(i) + rj) / f32(taps));
        let sample_p = c + (t * cos(ang) + b * sin(ang)) * r + n * (r * 0.4);

        let clip = sp.proj * vec4<f32>(sample_p, 1.0);
        if (clip.w <= 0.0) { continue; }
        let sndc = clip.xyz / clip.w;
        let suv = vec2<f32>(sndc.x * 0.5 + 0.5, 0.5 - sndc.y * 0.5);
        if (suv.x < 0.0 || suv.x > 1.0 || suv.y < 0.0 || suv.y > 1.0) { continue; }
        let sd = load_depth(vec2<i32>(suv * dims));
        if (sd >= 1.0) { continue; }

        // The actual scene surface at that screen position. Occluded when it sits
        // in front of our hemisphere sample (view z is negative; nearer = greater).
        // The range check fades hard to zero past the AO radius, so foreground
        // geometry never smears a halo onto far surfaces behind it.
        let sv = view_pos(suv, sd);
        if (sv.z >= sample_p.z + bias) {
            occ = occ + (1.0 - smoothstep(radius * 0.75, radius * 1.5, abs(c.z - sv.z)));
        }
    }
    let ao = clamp(1.0 - sp.params.y * (occ / f32(taps)) * 1.6, 0.0, 1.0);
    return vec4<f32>(ao, 0.0, 0.0, 1.0);
}
