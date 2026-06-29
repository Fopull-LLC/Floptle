// Raymarched SDF matter, composited with the raster meshes.
//
// Folds analytic matter (the morphing blob) and a BAKED MESH VOLUME (a 3D distance
// texture + a color texture from mesh2sdf) into one field with smin — distance AND
// color blend by the same weight, so textures crossfade across merge seams. Rays
// come from inverse(view_proj) (camera-relative, ADR-0015); the fragment writes
// frag_depth so this shares one depth buffer with the raster meshes.
//
// The volume is bounded by an AABB: rays march TO the box (ray_box) and only sample
// the field INSIDE it — the box itself is never a surface. Distance is sampled
// trilinearly (smooth), color nearest-neighbor (crisp pixel-art).

struct Globals {
    view_proj: mat4x4<f32>,
    inv_view_proj: mat4x4<f32>,
    light_dir: vec4<f32>,
    bg: vec4<f32>,
    center: vec4<f32>,      // analytic blob: xyz camera-relative center, w = scale
    params: vec4<f32>,      // x = time, y = volume voxel size
    vol_center: vec4<f32>,  // baked volume: xyz camera-relative box center, w = present
    vol_half: vec4<f32>,    // xyz half-extent, w = blend radius k
};
@group(0) @binding(0) var<uniform> G: Globals;
@group(0) @binding(1) var dist_tex: texture_3d<f32>;
@group(0) @binding(2) var color_tex: texture_3d<f32>;
@group(0) @binding(3) var samp_lin: sampler;  // trilinear — distance
@group(0) @binding(4) var samp_pt: sampler;   // nearest  — color (pixel-art)

// BVH backend (exact triangle distance → sharp matter).
struct BvhNode { aabb_min: vec3<f32>, lo: u32, aabb_max: vec3<f32>, hi: u32 };
struct Tri { a: vec4<f32>, b: vec4<f32>, c: vec4<f32> };
struct TriData { uv_a: vec2<f32>, uv_b: vec2<f32>, uv_c: vec2<f32>, pad: vec2<f32>, rect: vec4<f32> };
@group(0) @binding(5) var<storage, read> bvh_nodes: array<BvhNode>;
@group(0) @binding(6) var<storage, read> triangles: array<Tri>;
@group(0) @binding(7) var<storage, read> tri_data: array<TriData>;
@group(0) @binding(8) var atlas_tex: texture_2d<f32>;
@group(0) @binding(9) var atlas_samp: sampler;

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

fn smin_matter(a: Matter, b: Matter, k: f32) -> Matter {
    let h = clamp(0.5 + 0.5 * (b.d - a.d) / k, 0.0, 1.0);
    let d = mix(b.d, a.d, h) - k * h * (1.0 - h);
    let col = mix(b.col, a.col, h);
    return Matter(d, col);
}

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

// The baked mesh volume. OUTSIDE the AABB it contributes nothing (the box is not a
// surface); INSIDE it returns the sampled signed distance + albedo.
fn volume(p: vec3<f32>) -> Matter {
    if (G.vol_center.w < 0.5) {
        return Matter(1e9, vec3<f32>(1.0));
    }
    let rel = p - G.vol_center.xyz;
    let q = abs(rel) - G.vol_half.xyz;
    if (max(q.x, max(q.y, q.z)) > 0.0) {
        return Matter(1e9, vec3<f32>(1.0)); // outside the brick
    }
    let local = clamp(rel / (2.0 * G.vol_half.xyz) + 0.5, vec3<f32>(0.0), vec3<f32>(1.0));
    let d = textureSampleLevel(dist_tex, samp_lin, local, 0.0).r;
    let col = textureSampleLevel(color_tex, samp_pt, local, 0.0).rgb;
    return Matter(d, col);
}

struct TriHit { cp: vec3<f32>, u: f32, v: f32, w: f32 };

// Closest point on triangle abc to p + barycentric (Ericson) — WGSL port of
// mesh2sdf::closest_point_on_triangle.
fn closest_on_tri(p: vec3<f32>, a: vec3<f32>, b: vec3<f32>, c: vec3<f32>) -> TriHit {
    let ab = b - a;
    let ac = c - a;
    let ap = p - a;
    let d1 = dot(ab, ap);
    let d2 = dot(ac, ap);
    if (d1 <= 0.0 && d2 <= 0.0) { return TriHit(a, 1.0, 0.0, 0.0); }
    let bp = p - b;
    let d3 = dot(ab, bp);
    let d4 = dot(ac, bp);
    if (d3 >= 0.0 && d4 <= d3) { return TriHit(b, 0.0, 1.0, 0.0); }
    let vc = d1 * d4 - d3 * d2;
    if (vc <= 0.0 && d1 >= 0.0 && d3 <= 0.0) {
        let v = d1 / (d1 - d3);
        return TriHit(a + ab * v, 1.0 - v, v, 0.0);
    }
    let cp = p - c;
    let d5 = dot(ab, cp);
    let d6 = dot(ac, cp);
    if (d6 >= 0.0 && d5 <= d6) { return TriHit(c, 0.0, 0.0, 1.0); }
    let vb = d5 * d2 - d1 * d6;
    if (vb <= 0.0 && d2 >= 0.0 && d6 <= 0.0) {
        let w = d2 / (d2 - d6);
        return TriHit(a + ac * w, 1.0 - w, 0.0, w);
    }
    let va = d3 * d6 - d5 * d4;
    if (va <= 0.0 && (d4 - d3) >= 0.0 && (d5 - d6) >= 0.0) {
        let w = (d4 - d3) / ((d4 - d3) + (d5 - d6));
        return TriHit(b + (c - b) * w, 0.0, 1.0 - w, w);
    }
    let denom = 1.0 / (va + vb + vc);
    let v = vb * denom;
    let w = vc * denom;
    return TriHit(a + ab * v + ac * w, 1.0 - v - w, v, w);
}

// Exact distance to the mesh via stack-based BVH traversal (triangles are in local
// space; the box center maps to vol_center). Returns a thin-shell SDF + atlas color.
fn bvh_query(p: vec3<f32>) -> Matter {
    let lp = p - G.vol_center.xyz;
    let q0 = abs(lp) - G.vol_half.xyz;
    if (max(q0.x, max(q0.y, q0.z)) > 0.0) {
        return Matter(1e9, vec3<f32>(1.0)); // outside the brick
    }

    var stack: array<u32, 32>;
    stack[0] = 0u;
    var sp: i32 = 1;
    var best = 1e9;
    var best_tri = 0u;
    var bu = 1.0;
    var bv = 0.0;
    var bw = 0.0;
    loop {
        if (sp == 0) { break; }
        sp = sp - 1;
        let ni = stack[sp];
        let n = bvh_nodes[ni];
        let qq = max(max(n.aabb_min - lp, lp - n.aabb_max), vec3<f32>(0.0));
        if (dot(qq, qq) >= best * best) { continue; }
        if (n.hi == 0u) {
            if (sp < 30) {
                stack[sp] = ni + 1u;
                sp = sp + 1;
                stack[sp] = n.lo;
                sp = sp + 1;
            }
        } else {
            for (var k = 0u; k < n.hi; k = k + 1u) {
                let t = triangles[n.lo + k];
                let h = closest_on_tri(lp, t.a.xyz, t.b.xyz, t.c.xyz);
                let d = distance(lp, h.cp);
                if (d < best) {
                    best = d;
                    best_tri = n.lo + k;
                    bu = h.u;
                    bv = h.v;
                    bw = h.w;
                }
            }
        }
    }

    let td = tri_data[best_tri];
    let uv = bu * td.uv_a + bv * td.uv_b + bw * td.uv_c;
    let auv = td.rect.xy + fract(uv) * td.rect.zw;
    let col = textureSampleLevel(atlas_tex, atlas_samp, auv, 0.0).rgb;
    return Matter(best - G.params.z, col); // thin shell (params.z = thickness)
}

fn map(p: vec3<f32>) -> Matter {
    let a = analytic(p);
    var v: Matter;
    if (G.vol_center.w > 1.5) {
        v = bvh_query(p); // exact triangle distance — sharp
    } else {
        v = volume(p); // voxel volume — fast, rounded
    }
    return smin_matter(a, v, max(G.vol_half.w, 0.0001));
}

fn calc_normal(p: vec3<f32>) -> vec3<f32> {
    // BVH (exact) reads sharp normals with a tiny epsilon ~ shell thickness; the
    // voxel field needs ~one voxel apart or it reads constant within a cell.
    var eps: f32;
    if (G.vol_center.w > 1.5) {
        eps = clamp(G.params.z * 0.7, 0.01, 0.3);
    } else {
        eps = clamp(G.params.y * 0.9, 0.004, 0.6);
    }
    let e = vec2<f32>(eps, 0.0);
    return normalize(vec3<f32>(
        map(p + e.xyy).d - map(p - e.xyy).d,
        map(p + e.yxy).d - map(p - e.yxy).d,
        map(p + e.yyx).d - map(p - e.yyx).d,
    ));
}

// Slab ray/AABB: returns (t_near, t_far); miss if near > far.
fn ray_box(ro: vec3<f32>, rd: vec3<f32>, center: vec3<f32>, half: vec3<f32>) -> vec2<f32> {
    let inv = 1.0 / rd;
    let t0 = (center - half - ro) * inv;
    let t1 = (center + half - ro) * inv;
    let tn = min(t0, t1);
    let tf = max(t0, t1);
    return vec2<f32>(max(max(tn.x, tn.y), tn.z), min(min(tf.x, tf.y), tf.z));
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

    // March to the volume's AABB so empty space before it isn't skipped past.
    let bt = ray_box(ro, rd, G.vol_center.xyz, G.vol_half.xyz);
    let has_box = G.vol_center.w > 0.5 && bt.y > max(bt.x, 0.0);

    let reach = max(G.center.w, length(G.vol_half.xyz));
    let max_t = reach * 60.0 + length(G.center.xyz) + length(G.vol_center.xyz) + 100.0;
    var t = 0.0;
    var hit = false;
    var m: Matter;
    for (var i = 0; i < 160; i = i + 1) {
        let p = ro + rd * t;
        m = map(p);
        if (m.d < 0.001 * t + 0.0006) {
            hit = true;
            break;
        }
        var step = max(m.d, 0.002) * 0.9;
        if (has_box && t < bt.x) {
            step = min(step, bt.x - t + 0.02); // land just inside the box entry
        }
        t = t + step;
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
            var col = m.col * (0.2 + 0.8 * diff) + vec3<f32>(0.5, 0.7, 1.0) * fres * 0.3;
            col = col + pow(max(dot(reflect(rd, n), l), 0.0), 40.0) * vec3<f32>(0.5);
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
