// Raymarched SDF matter, composited with the raster meshes.
//
// Rays are reconstructed from the engine camera's inverse(view_proj) so they line
// up exactly with the rasterized meshes (no "swim"), in CAMERA-RELATIVE space (the
// camera is the origin, ADR-0015). The fragment writes @builtin(frag_depth) from
// the projected hit point, so this shares the one depth buffer with the meshes —
// matter and Blender geometry genuinely occlude/intersect.
//
// The matter itself is a few spheres blended with smin (smooth union): gooey
// "merging matter", the first proof of the unified-field thesis. Swapping `map`
// for a Mandelbox/Menger estimator gives fractal matter through the same pass.

struct Globals {
    view_proj: mat4x4<f32>,
    inv_view_proj: mat4x4<f32>,
    light_dir: vec4<f32>,   // xyz = world-space dir toward the light
    bg: vec4<f32>,          // background color where rays miss
    center: vec4<f32>,      // xyz = camera-relative matter center, w = scale
    params: vec4<f32>,      // x = time
};
@group(0) @binding(0) var<uniform> G: Globals;

struct VOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) ndc: vec2<f32>,
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
    o.ndc = xy; // already in [-1,1] clip space
    return o;
}

fn sd_sphere(p: vec3<f32>, r: f32) -> f32 {
    return length(p) - r;
}

// Smooth union (the "merge" operator that makes matter blend).
fn smin(a: f32, b: f32, k: f32) -> f32 {
    let h = clamp(0.5 + 0.5 * (b - a) / k, 0.0, 1.0);
    return mix(b, a, h) - k * h * (1.0 - h);
}

// Distance to the matter at camera-relative point `p`.
fn map(p: vec3<f32>) -> f32 {
    let s = G.center.w;
    let q = (p - G.center.xyz) / s; // local space, ~unit radius
    let t = G.params.x;
    var d = sd_sphere(q - vec3<f32>(sin(t * 0.7) * 0.5, cos(t * 0.5) * 0.4, 0.0), 0.6);
    d = smin(d, sd_sphere(q - vec3<f32>(cos(t * 0.6) * 0.5, sin(t * 0.8) * 0.5, sin(t * 0.4) * 0.4), 0.55), 0.35);
    d = smin(d, sd_sphere(q - vec3<f32>(-sin(t * 0.5) * 0.5, 0.3, cos(t * 0.7) * 0.5), 0.5), 0.35);
    d = smin(d, sd_sphere(q - vec3<f32>(0.0, -sin(t * 0.6) * 0.5, -0.4), 0.5), 0.35);
    return d * s; // back to world-scale distance
}

fn calc_normal(p: vec3<f32>) -> vec3<f32> {
    let e = vec2<f32>(0.0009, 0.0) * G.center.w;
    return normalize(vec3<f32>(
        map(p + e.xyy) - map(p - e.xyy),
        map(p + e.yxy) - map(p - e.yxy),
        map(p + e.yyx) - map(p - e.yyx),
    ));
}

struct FsOut {
    @location(0) color: vec4<f32>,
    @builtin(frag_depth) depth: f32,
};

@fragment
fn fs(in: VOut) -> FsOut {
    // Reconstruct the ray for this pixel from inverse(view_proj) (camera-relative).
    let near = G.inv_view_proj * vec4<f32>(in.ndc, 0.0, 1.0);
    let far = G.inv_view_proj * vec4<f32>(in.ndc, 1.0, 1.0);
    let ro = near.xyz / near.w;
    let rd = normalize(far.xyz / far.w - ro);

    let max_t = G.center.w * 60.0 + length(G.center.xyz) + 50.0;
    var t = 0.0;
    var hit = false;
    for (var i = 0; i < 96; i = i + 1) {
        let p = ro + rd * t;
        let d = map(p);
        if (d < 0.001 * t + 0.0006) {
            hit = true;
            break;
        }
        t = t + d;
        if (t > max_t) {
            break;
        }
    }

    var out: FsOut;
    var drawn = false;
    if (hit) {
        let p = ro + rd * t;
        // project through the shared view_proj → depth matching the meshes
        let clip = G.view_proj * vec4<f32>(p, 1.0);
        let ndc_z = clip.z / clip.w;
        // reject hits beyond the far plane / behind near: the GPU would clamp
        // frag_depth into [0,1] and a real far hit could masquerade as a near one
        if (clip.w > 0.0 && ndc_z >= 0.0 && ndc_z <= 1.0) {
            let n = calc_normal(p);
            let l = normalize(G.light_dir.xyz);
            let diff = max(dot(n, l), 0.0);
            let fres = pow(1.0 - max(dot(n, -rd), 0.0), 3.0);
            // an iridescent, otherworldly matter color shifting over the surface
            let iri = 0.5 + 0.5 * cos(6.2831 * (n.y * 0.5 + vec3<f32>(0.0, 0.33, 0.67)) + G.params.x * 0.2);
            let base = mix(vec3<f32>(0.35, 0.16, 0.55), iri, 0.55);
            var col = base * (0.18 + 0.82 * diff) + vec3<f32>(0.55, 0.75, 1.0) * fres * 0.6;
            col = col + pow(max(dot(reflect(rd, n), l), 0.0), 40.0) * vec3<f32>(0.7); // spec
            out.color = vec4<f32>(clamp(col, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
            out.depth = ndc_z;
            drawn = true;
        }
    }
    if (!drawn) {
        out.color = vec4<f32>(G.bg.rgb, 1.0);
        out.depth = 1.0; // far → meshes draw freely over the background
    }
    return out;
}
