// Raymarched SDF matter, composited with the raster meshes.
//
// Two kinds of matter are folded into one field with smin (smooth union): the
// analytic morphing blob, and a BAKED MESH VOLUME (a 3D distance texture + a
// co-located color texture produced by mesh2sdf). Distance AND color blend by the
// same smin weight, so where the two fuse the textures crossfade across the seam.
// Rays come from inverse(view_proj) (camera-relative, ADR-0015) and the fragment
// writes frag_depth, so this shares one depth buffer with the raster meshes.

struct Globals {
    view_proj: mat4x4<f32>,
    inv_view_proj: mat4x4<f32>,
    light_dir: vec4<f32>,
    bg: vec4<f32>,
    center: vec4<f32>,      // analytic blob: xyz camera-relative center, w = scale
    params: vec4<f32>,      // x = time
    vol_center: vec4<f32>,  // baked volume: xyz camera-relative box center, w = present
    vol_half: vec4<f32>,    // xyz half-extent, w = blend radius k
};
@group(0) @binding(0) var<uniform> G: Globals;
@group(0) @binding(1) var dist_tex: texture_3d<f32>;
@group(0) @binding(2) var color_tex: texture_3d<f32>;
@group(0) @binding(3) var vol_samp: sampler;

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
    o.ndc = xy;
    return o;
}

struct Matter { d: f32, col: vec3<f32> };

fn sd_sphere(p: vec3<f32>, r: f32) -> f32 {
    return length(p) - r;
}

fn smin(a: f32, b: f32, k: f32) -> f32 {
    let h = clamp(0.5 + 0.5 * (b - a) / k, 0.0, 1.0);
    return mix(b, a, h) - k * h * (1.0 - h);
}

// smin that blends distance AND color by the same weight h (texture crossfade).
fn smin_matter(a: Matter, b: Matter, k: f32) -> Matter {
    let h = clamp(0.5 + 0.5 * (b.d - a.d) / k, 0.0, 1.0);
    let d = mix(b.d, a.d, h) - k * h * (1.0 - h);
    let col = mix(b.col, a.col, h);
    return Matter(d, col);
}

// The analytic blob: smin-blended morphing spheres, iridescent.
fn analytic(p: vec3<f32>) -> Matter {
    let s = G.center.w;
    let q = (p - G.center.xyz) / s;
    let t = G.params.x;
    var d = sd_sphere(q - vec3<f32>(sin(t * 0.7) * 0.5, cos(t * 0.5) * 0.4, 0.0), 0.6);
    d = smin(d, sd_sphere(q - vec3<f32>(cos(t * 0.6) * 0.5, sin(t * 0.8) * 0.5, sin(t * 0.4) * 0.4), 0.55), 0.35);
    d = smin(d, sd_sphere(q - vec3<f32>(-sin(t * 0.5) * 0.5, 0.3, cos(t * 0.7) * 0.5), 0.5), 0.35);
    d = smin(d, sd_sphere(q - vec3<f32>(0.0, -sin(t * 0.6) * 0.5, -0.4), 0.5), 0.35);
    let iri = 0.5 + 0.5 * cos(6.2831 * (q.y * 0.5 + vec3<f32>(0.0, 0.33, 0.67)) + t * 0.2);
    let col = mix(vec3<f32>(0.35, 0.16, 0.55), iri, 0.55);
    return Matter(d * s, col);
}

// The baked mesh volume: a box SDF outside (to march toward), the sampled mesh
// distance + albedo inside.
fn volume(p: vec3<f32>) -> Matter {
    if (G.vol_center.w < 0.5) {
        return Matter(1e9, vec3<f32>(1.0)); // no volume bound
    }
    let rel = p - G.vol_center.xyz;
    let q = abs(rel) - G.vol_half.xyz;
    let box_d = length(max(q, vec3<f32>(0.0))) + min(max(q.x, max(q.y, q.z)), 0.0);
    if (box_d > 0.0) {
        return Matter(box_d, vec3<f32>(1.0)); // outside the brick: step toward it
    }
    let local = clamp(rel / (2.0 * G.vol_half.xyz) + 0.5, vec3<f32>(0.0), vec3<f32>(1.0));
    let d = textureSampleLevel(dist_tex, vol_samp, local, 0.0).r;
    let col = textureSampleLevel(color_tex, vol_samp, local, 0.0).rgb;
    return Matter(d, col);
}

// The whole field: every piece of matter folded together with smin.
fn map(p: vec3<f32>) -> Matter {
    let a = analytic(p);
    let v = volume(p);
    return smin_matter(a, v, max(G.vol_half.w, 0.0001));
}

fn calc_normal(p: vec3<f32>) -> vec3<f32> {
    let e = vec2<f32>(0.0028, 0.0);
    return normalize(vec3<f32>(
        map(p + e.xyy).d - map(p - e.xyy).d,
        map(p + e.yxy).d - map(p - e.yxy).d,
        map(p + e.yyx).d - map(p - e.yyx).d,
    ));
}

struct FsOut {
    @location(0) color: vec4<f32>,
    @builtin(frag_depth) depth: f32,
};

@fragment
fn fs(in: VOut) -> FsOut {
    let near = G.inv_view_proj * vec4<f32>(in.ndc, 0.0, 1.0);
    let far = G.inv_view_proj * vec4<f32>(in.ndc, 1.0, 1.0);
    let ro = near.xyz / near.w;
    let rd = normalize(far.xyz / far.w - ro);

    let reach = max(G.center.w, length(G.vol_half.xyz));
    let max_t = reach * 60.0 + length(G.center.xyz) + length(G.vol_center.xyz) + 100.0;
    var t = 0.0;
    var hit = false;
    var m: Matter;
    for (var i = 0; i < 160; i = i + 1) {
        let p = ro + rd * t;
        m = map(p);
        // Distance-relaxed threshold: grows with t so grazing rays near the
        // silhouette converge instead of exhausting the step budget (holes).
        if (m.d < 0.0015 * t + 0.0025) {
            hit = true;
            break;
        }
        // Conservative step (0.8): the smin-blended + trilinear-sampled field is
        // not a perfectly exact SDF, so understep to avoid overshoot cracks when
        // the camera is close to the surface.
        t = t + max(m.d, 0.003) * 0.8;
        if (t > max_t) {
            break;
        }
    }

    var out: FsOut;
    var drawn = false;
    if (hit) {
        let p = ro + rd * t;
        let clip = G.view_proj * vec4<f32>(p, 1.0);
        let ndc_z = clip.z / clip.w;
        if (clip.w > 0.0 && ndc_z >= 0.0 && ndc_z <= 1.0) {
            let n = calc_normal(p);
            let l = normalize(G.light_dir.xyz);
            let diff = max(dot(n, l), 0.0);
            let fres = pow(1.0 - max(dot(n, -rd), 0.0), 3.0);
            let albedo = m.col;
            var col = albedo * (0.18 + 0.82 * diff) + vec3<f32>(0.5, 0.7, 1.0) * fres * 0.4;
            col = col + pow(max(dot(reflect(rd, n), l), 0.0), 40.0) * vec3<f32>(0.6);
            out.color = vec4<f32>(clamp(col, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
            out.depth = ndc_z;
            drawn = true;
        }
    }
    if (!drawn) {
        out.color = vec4<f32>(G.bg.rgb, 1.0);
        out.depth = 1.0;
    }
    return out;
}
