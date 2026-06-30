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
    light_color: vec4<f32>,
    ambient: vec4<f32>,
    bg: vec4<f32>,
    center: vec4<f32>,      // (unused legacy field; blobs now live in `blobs`)
    params: vec4<f32>,      // x = time, y = blob count
    vol_center: vec4<f32>,  // baked volume: xyz camera-relative box center, w = present
    vol_half: vec4<f32>,    // xyz half-extent, w = blend radius k
    // Terrain surface material (same model as the raster meshes). Ignored by blobs.
    terrain_tint: vec4<f32>,     // rgb tint (× painted albedo), a unused
    terrain_emissive: vec4<f32>, // rgb, a = strength
    terrain_specular: vec4<f32>, // rgb, a = strength
    terrain_params: vec4<f32>,   // x shininess, y rim_strength, z unlit, w ambient_mul
    terrain_rim: vec4<f32>,      // rgb, a unused
    blobs: array<vec4<f32>, 16>, // each: xyz camera-relative center, w = scale
    point_count: vec4<f32>,            // x = active point-light count
    point_pos: array<vec4<f32>, 16>,   // xyz camera-relative pos, w = range
    point_color: array<vec4<f32>, 16>, // rgb = color * intensity
    // Per-blob material (same model as terrain_*), indexed by blob.
    blob_tint: array<vec4<f32>, 16>,     // rgb tint (× procedural color), a unused
    blob_emissive: array<vec4<f32>, 16>, // rgb, a = strength
    blob_specular: array<vec4<f32>, 16>, // rgb, a = strength
    blob_params: array<vec4<f32>, 16>,   // x shininess, y rim_strength, z unlit, w ambient_mul
    blob_rim: array<vec4<f32>, 16>,      // rgb, a unused
    sky_params: vec4<f32>,               // x = mode (0 solid, 1 texture), y = size
    sky_tint: vec4<f32>,                 // rgb tint × sampled texel
    sky_rot0: vec4<f32>,                 // inverse skybox rotation, column 0 (xyz)
    sky_rot1: vec4<f32>,                 // column 1
    sky_rot2: vec4<f32>,                 // column 2
};
@group(0) @binding(0) var<uniform> G: Globals;
@group(0) @binding(1) var dist_tex: texture_3d<f32>;
@group(0) @binding(2) var color_tex: texture_3d<f32>;
@group(0) @binding(3) var vol_samp: sampler;
// Terrain texture palette (triplanar-mapped). The volume color's alpha selects a
// slot: 0 = untextured (flat tint), n = palette layer n-1.
@group(0) @binding(4) var terrain_tex: texture_2d_array<f32>;
// A REPEAT sampler so triplanar terrain textures tile across the surface.
@group(0) @binding(5) var terrain_samp: sampler;
// The equirectangular skybox texture (sampled for background pixels, reusing the
// REPEAT terrain sampler so it wraps seamlessly).
@group(0) @binding(6) var sky_tex: texture_2d<f32>;

const PI: f32 = 3.14159265359;

// The environment color along world ray direction `dir`: a flat color, or the equirect
// sky texture (rotated by the skybox node so a script can spin it) times its tint.
fn sky_color(dir: vec3<f32>) -> vec3<f32> {
    if (G.sky_params.x < 0.5) {
        return G.bg.rgb;
    }
    // Rotate the ray into the skybox's local frame (inverse rotation, as 3 columns).
    let r = mat3x3<f32>(G.sky_rot0.xyz, G.sky_rot1.xyz, G.sky_rot2.xyz);
    let d = normalize(r * dir);
    let u = atan2(d.z, d.x) / (2.0 * PI) + 0.5; // longitude → [0,1]
    let v = acos(clamp(d.y, -1.0, 1.0)) / PI;   // latitude  → [0,1] (top→bottom)
    let texel = textureSampleLevel(sky_tex, terrain_samp, vec2<f32>(u, v), 0.0).rgb;
    return texel * G.sky_tint.rgb;
}

// Accumulated diffuse from the point lights at camera-relative position `p` with
// surface normal `n`. Smooth falloff to 0 at each light's range.
fn point_diffuse(p: vec3<f32>, n: vec3<f32>) -> vec3<f32> {
    var acc = vec3<f32>(0.0);
    let count = min(u32(G.point_count.x), 16u);
    for (var i = 0u; i < count; i = i + 1u) {
        let lp = G.point_pos[i];
        let to = lp.xyz - p;
        let dist = length(to);
        let range = max(lp.w, 0.0001);
        let ndl = max(dot(n, to / max(dist, 1e-4)), 0.0);
        let x = clamp(1.0 - dist / range, 0.0, 1.0);
        acc = acc + G.point_color[i].rgb * (ndl * x * x);
    }
    return acc;
}

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

// The analytic blob: a STATIC smooth metaball (fixed-offset smin-blended spheres,
// no time morph) so it is a predictable, placeable shape whose size comes only
// from the transform — `center.w` is the scale, and the blob's radius is ≈ 0.85 *
// scale (comparable to the unit sphere). A low-frequency iridescent tint (its
// spatial period spans the whole blob, so it never aliases) gives the otherworldly
// look; the close-up "ring" artifacts came from the specular highlight (see `fs`),
// not from this color. Animation, if wanted, belongs to scripts, not the shape.
fn blob_one(p: vec3<f32>, center: vec3<f32>, s: f32) -> Matter {
    let q = (p - center) / s;
    var d = sd_sphere(q - vec3<f32>(0.26, 0.10, 0.0), 0.55);
    d = smin(d, sd_sphere(q - vec3<f32>(-0.24, 0.16, 0.12), 0.50), 0.30);
    d = smin(d, sd_sphere(q - vec3<f32>(0.06, -0.22, -0.14), 0.50), 0.30);
    d = smin(d, sd_sphere(q - vec3<f32>(-0.10, -0.06, 0.24), 0.48), 0.30);
    let iri = 0.5 + 0.5 * cos(6.2831 * (q.y * 0.5 + vec3<f32>(0.0, 0.33, 0.67)));
    let col = mix(vec3<f32>(0.35, 0.16, 0.55), iri, 0.55);
    return Matter(d * s, col);
}

// Every blob folded together with smin. Seeded with blob 0 (never blended against
// the 1e9 sentinel — that f32 cancellation collapses the field). Blobs far apart
// stay distinct (small smin k); close ones fuse.
fn analytic(p: vec3<f32>) -> Matter {
    let count = min(u32(G.params.y), 16u);
    if (count == 0u) {
        return Matter(1e9, vec3<f32>(0.0));
    }
    var m = blob_one(p, G.blobs[0].xyz, max(G.blobs[0].w, 0.02));
    for (var i = 1u; i < count; i = i + 1u) {
        let b = blob_one(p, G.blobs[i].xyz, max(G.blobs[i].w, 0.02));
        m = smin_matter(m, b, 0.3 * max(G.blobs[i].w, 0.05));
    }
    return m;
}

// Index of the blob whose surface is nearest `p` — so the hit point shades with that
// blob's material (the dominant one in the smin blend at a surface point).
fn nearest_blob(p: vec3<f32>) -> i32 {
    let count = min(u32(G.params.y), 16u);
    var bi = 0;
    var bd = 1e9;
    for (var i = 0u; i < count; i = i + 1u) {
        let d = blob_one(p, G.blobs[i].xyz, max(G.blobs[i].w, 0.02)).d;
        if (d < bd) { bd = d; bi = i32(i); }
    }
    return bi;
}

// March bound from all blobs + the volume box (replaces the old single-blob reach).
fn march_bound() -> f32 {
    let count = min(u32(G.params.y), 16u);
    var reach = length(G.vol_half.xyz);
    var maxc = length(G.vol_center.xyz);
    for (var i = 0u; i < count; i = i + 1u) {
        reach = max(reach, G.blobs[i].w);
        maxc = max(maxc, length(G.blobs[i].xyz));
    }
    return reach * 60.0 + maxc + 100.0;
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
        // Outside the brick: step toward it, but never report the box face itself as
        // a surface — keep the step above the hit threshold so the ray crosses into
        // the volume, where the real (sampled) SDF takes over. (Without this, a
        // volume whose matter doesn't fill its box renders the box as a shell.)
        return Matter(max(box_d, 0.08), vec3<f32>(1.0));
    }
    let local = clamp(rel / (2.0 * G.vol_half.xyz) + 0.5, vec3<f32>(0.0), vec3<f32>(1.0));
    let d = textureSampleLevel(dist_tex, vol_samp, local, 0.0).r;
    let col = textureSampleLevel(color_tex, vol_samp, local, 0.0).rgb;
    // Taper the finite slab's SIDE + BOTTOM faces up to air, so a terrain that fills
    // its box doesn't render as hard dirt walls / a visible shell — the surface
    // slopes off gently to meet the air at the edges (a gentle slope avoids the
    // grazing-angle aliasing a steep rounded lip would cause). The TOP ground
    // surface is untouched.
    let margin = 2.0;
    let edge = min(min(G.vol_half.x - abs(rel.x), G.vol_half.z - abs(rel.z)), rel.y + G.vol_half.y);
    return Matter(max(d, margin - edge), col);
}

// True when `p` is inside the volume box expanded by `e` — used to reject false hits
// on the box's bounding faces (the box-approach distance is never a real surface),
// while a small `e` still admits genuine terrain hits sitting right at a face.
fn inside_volume_box_eps(p: vec3<f32>, e: f32) -> bool {
    let q = abs(p - G.vol_center.xyz) - G.vol_half.xyz;
    return max(q.x, max(q.y, q.z)) < e;
}
fn inside_volume_box(p: vec3<f32>) -> bool {
    return inside_volume_box_eps(p, 0.0);
}

// A threshold-crossing is a REAL surface (not the shell) when there is no volume,
// or we are strictly inside the volume box, or an analytic blob is the matter here.
// The box test must stay STRICT: outside the brick `volume()` returns a constant
// floor (the box-approach step), and at a far camera the distance-relaxed `thr` grows
// past that floor — any slack here would accept the box face itself and re-draw the
// shell. Genuine terrain hits are inside the box; the grazing-gap fill below only
// records/accepts points inside the box, so it needs no slack here.
fn real_surface(p: vec3<f32>, thr: f32) -> bool {
    if (G.vol_center.w < 0.5) { return true; }
    if (inside_volume_box(p)) { return true; }
    return G.params.y >= 0.5 && analytic(p).d < thr;
}

// The whole field: every piece of matter folded together with smin.
fn map(p: vec3<f32>) -> Matter {
    let a = analytic(p);
    // No baked volume bound → return the blob directly. Blending against the
    // "absent" sentinel (1e9) is not just wasteful: f32 `mix(1e9, d, 1.0)` is
    // evaluated as `1e9 + 1.0*(d - 1e9)`, and `d - 1e9` loses `d` entirely, so the
    // field would collapse to ~0 everywhere — a surface at the camera, every ray a
    // false hit. (This was the "glitchy giant sphere".)
    if (G.vol_center.w < 0.5) {
        return a;
    }
    let v = volume(p);
    return smin_matter(a, v, max(G.vol_half.w, 0.0001));
}

// Triplanar-sample a terrain palette layer at a box-relative position (world-stable,
// since `rel` cancels the camera offset), blended by the surface normal.
fn triplanar(slot: i32, rel: vec3<f32>, n: vec3<f32>) -> vec3<f32> {
    let scale = 0.22; // ~4.5 world units per tile
    let an = abs(n) + vec3<f32>(0.0001);
    let w = an / (an.x + an.y + an.z);
    let cx = textureSampleLevel(terrain_tex, terrain_samp, rel.zy * scale, slot, 0.0).rgb;
    let cy = textureSampleLevel(terrain_tex, terrain_samp, rel.xz * scale, slot, 0.0).rgb;
    let cz = textureSampleLevel(terrain_tex, terrain_samp, rel.xy * scale, slot, 0.0).rgb;
    return cx * w.x + cy * w.y + cz * w.z;
}

// One painted slot's contribution at `rel` (slot index is 0-based; < 0 = untextured,
// which is just the flat tint).
fn terrain_slot_color(slot: i32, rel: vec3<f32>, n: vec3<f32>, tint: vec3<f32>) -> vec3<f32> {
    if (slot < 0) {
        return tint;
    }
    return triplanar(slot, rel, n) * tint * 1.6; // texture modulates the painted tint
}

// The terrain texture (if any) at a hit point `p`, multiplied into the tint. The painted
// slot lives in the volume color's alpha (0 = untextured). The alpha is sampled LINEARLY,
// so at a boundary between two painted slots it reads a fractional value — we blend the
// two neighbouring slots by that fraction instead of snapping (round()), which gives a
// smooth crossfade between textures instead of a hard, jarring seam.
fn terrain_albedo(p: vec3<f32>, n: vec3<f32>, tint: vec3<f32>) -> vec3<f32> {
    if (G.vol_center.w < 0.5) {
        return tint;
    }
    let rel = p - G.vol_center.xyz;
    let q = abs(rel) - G.vol_half.xyz;
    if (max(q.x, max(q.y, q.z)) > 0.0) {
        return tint; // not inside the terrain box
    }
    let inv = 1.0 / (2.0 * G.vol_half.xyz);
    let local = clamp(rel * inv + 0.5, vec3<f32>(0.0), vec3<f32>(1.0));
    // The slot only transitions over ONE voxel, so a single linear tap gives a narrow
    // seam. Average a few taps in the surface PLANE (so we widen along the ground, not
    // into it) → a soft, several-voxel crossfade. `a` is the 1-based slot (0 = untextured).
    let voxel = 2.0 * G.vol_half.xyz / max(vec3<f32>(textureDimensions(color_tex)), vec3<f32>(1.0));
    var t1 = cross(n, vec3<f32>(0.0, 1.0, 0.0));
    if (dot(t1, t1) < 0.01) { t1 = cross(n, vec3<f32>(1.0, 0.0, 0.0)); }
    t1 = normalize(t1);
    let t2 = normalize(cross(n, t1));
    let o1 = t1 * voxel * 1.5 * inv;
    let o2 = t2 * voxel * 1.5 * inv;
    let a = (
        textureSampleLevel(color_tex, vol_samp, local, 0.0).a
        + textureSampleLevel(color_tex, vol_samp, clamp(local + o1, vec3<f32>(0.0), vec3<f32>(1.0)), 0.0).a
        + textureSampleLevel(color_tex, vol_samp, clamp(local - o1, vec3<f32>(0.0), vec3<f32>(1.0)), 0.0).a
        + textureSampleLevel(color_tex, vol_samp, clamp(local + o2, vec3<f32>(0.0), vec3<f32>(1.0)), 0.0).a
        + textureSampleLevel(color_tex, vol_samp, clamp(local - o2, vec3<f32>(0.0), vec3<f32>(1.0)), 0.0).a
    ) * (255.0 / 5.0);
    if (a < 0.5) {
        return tint; // fully untextured here
    }
    let lo = floor(a);
    let f = a - lo;
    let c_lo = terrain_slot_color(i32(lo) - 1, rel, n, tint);
    let c_hi = terrain_slot_color(i32(ceil(a)) - 1, rel, n, tint);
    return mix(c_lo, c_hi, f);
}

fn calc_normal(p: vec3<f32>) -> vec3<f32> {
    // For TERRAIN (a sampled voxel field) use an epsilon of ~one voxel so the central
    // difference spans cell boundaries and low-passes residual grid/f16 noise instead
    // of reporting a single cell's facet (the grain). Analytic blobs have a continuous
    // gradient and want a small epsilon for crisp edges.
    var h = 0.012;
    if (G.vol_center.w >= 0.5 && inside_volume_box_eps(p, 0.08)) {
        let voxel = 2.0 * G.vol_half.xyz / max(vec3<f32>(textureDimensions(dist_tex)), vec3<f32>(1.0));
        h = clamp(max(voxel.x, max(voxel.y, voxel.z)), 0.02, 1.0);
    }
    // Tetrahedron offsets: 4 taps, isotropic (no axis-aligned facet bias), cheaper
    // than the 6-tap central cross.
    let k0 = vec3<f32>(1.0, -1.0, -1.0);
    let k1 = vec3<f32>(-1.0, -1.0, 1.0);
    let k2 = vec3<f32>(-1.0, 1.0, -1.0);
    let k3 = vec3<f32>(1.0, 1.0, 1.0);
    return normalize(
        k0 * map(p + k0 * h).d + k1 * map(p + k1 * h).d + k2 * map(p + k2 * h).d
            + k3 * map(p + k3 * h).d,
    );
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

    let max_t = march_bound();
    var t = 0.0;
    var prev_t = 0.0;
    var hit = false;
    var m: Matter;
    // Closest approach to a real surface, so a grazing ray that never quite trips the
    // coarse threshold — the silhouette of a hill/ravine, where the step shrinks and
    // the iteration budget runs out — can be accepted below instead of leaving a
    // transparent hole. (Those holes are what the low-res retro filter blew up into
    // visible blocky gaps along terrain edges.)
    var min_d = 1e9;
    var min_t = 0.0;
    var min_prev = 0.0;
    for (var i = 0; i < 256; i = i + 1) {
        let p = ro + rd * t;
        m = map(p);
        // Distance-relaxed threshold for the COARSE hit (the precise surface is then
        // found by bisection below). A gentle t-growth still helps grazing rays
        // converge without exhausting the step budget, but it's kept small so the far
        // silhouette stays sharp (the old larger growth left a fuzzy wispy horizon).
        let thr = 0.0006 * t + 0.002;
        if (m.d < min_d && real_surface(p, 0.08)) {
            min_d = m.d;
            min_t = t;
            min_prev = prev_t;
        }
        if (m.d < thr && real_surface(p, thr)) {
            hit = true;
            break;
        }
        prev_t = t;
        // Conservative step (0.85): the smin-blended + trilinear-sampled field is
        // not a perfectly exact SDF, so understep to avoid overshoot cracks when
        // the camera is close to the surface.
        t = t + max(m.d, 0.003) * 0.85;
        if (t > max_t) {
            break;
        }
    }
    // Grazing-silhouette fill: no clean hit, but the ray passed within ~a voxel of a
    // real surface → accept that closest approach (refined by the bisection below).
    if (!hit && min_d < 0.06 + 0.0015 * min_t) {
        hit = true;
        t = min_t;
        prev_t = min_prev;
        m = map(ro + rd * t);
    }

    // Refine the loose threshold hit to the TRUE surface (where the field crosses
    // zero) by bisection. The relaxed threshold above hits at a t that varies with
    // distance, which on a grazing surface produced visible depth BANDING; bisecting
    // to d≈0 gives a consistent surface depth + cleaner normals (no banding/grain).
    if (hit) {
        var a = prev_t; // outside (d > 0)
        var b = t;      // at/just inside the threshold
        // Walk `b` until it's truly inside (d < 0) so [a,b] brackets the crossing.
        var bracketed = false;
        for (var k = 0; k < 10; k = k + 1) {
            if (map(ro + rd * b).d < 0.0) { bracketed = true; break; }
            a = b;
            b = b + 0.02;
        }
        // Only refine when we actually bracket a zero crossing. A grazing silhouette
        // ray that never goes inside keeps its (smooth) threshold hit instead of a
        // bogus bisection result — that was the wispy far-horizon edge.
        if (bracketed) {
            for (var j = 0; j < 14; j = j + 1) {
                let tm = 0.5 * (a + b);
                if (map(ro + rd * tm).d < 0.0) { b = tm; } else { a = tm; }
            }
            t = 0.5 * (a + b);
        }
        m = map(ro + rd * t);
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
            let v = -rd; // toward the camera (the camera sits at the ray origin)
            let diff = max(dot(n, l), 0.0);
            // The terrain palette texture (if painted) modulates the per-voxel tint.
            let albedo = terrain_albedo(p, n, m.col);
            var col: vec3<f32>;
            if (G.vol_center.w >= 0.5 && inside_volume_box_eps(p, 0.06)) {
                // TERRAIN: shade with its Material using the SAME model as the raster
                // meshes (ambient×mul + diffuse, Blinn-Phong specular, rim, emissive,
                // unlit), so terrain sits consistently next to everything else instead
                // of the old hardcoded look with a fixed blue rim. Defaults (no/neutral
                // material) give plain matte — no specular, no rim.
                let tinted = albedo * G.terrain_tint.rgb;
                let emissive = G.terrain_emissive.rgb * G.terrain_emissive.a;
                if (G.terrain_params.z > 0.5) {
                    col = tinted + emissive; // unlit / fullbright
                } else {
                    let ambient = G.ambient.rgb * G.terrain_params.w;
                    col = tinted * (ambient + G.light_color.rgb * diff);
                    col = col + tinted * point_diffuse(p, n); // placeable point lights
                    let h = normalize(l + v);
                    let shininess = max(G.terrain_params.x, 1.0);
                    let spec = pow(max(dot(n, h), 0.0), shininess) * G.terrain_specular.a * select(0.0, 1.0, diff > 0.0);
                    col = col + G.terrain_specular.rgb * spec * G.light_color.rgb;
                    let rim_f = pow(1.0 - max(dot(n, v), 0.0), 2.0) * G.terrain_params.y;
                    col = col + G.terrain_rim.rgb * rim_f + emissive;
                }
            } else {
                // BLOB: shade with the hit blob's Material (same lighting model as the
                // meshes/terrain) so an assigned Material actually changes its look.
                // The default (material-less) blob is fed neutral tint + a subtle blue
                // rim by the editor, reproducing the old matte-with-rim appearance.
                let bi = clamp(nearest_blob(p), 0, 15);
                let tinted = albedo * G.blob_tint[bi].rgb;
                let emissive = G.blob_emissive[bi].rgb * G.blob_emissive[bi].a;
                let bpar = G.blob_params[bi];
                if (bpar.z > 0.5) {
                    col = tinted + emissive; // unlit / fullbright
                } else {
                    let ambient = G.ambient.rgb * bpar.w;
                    col = tinted * (ambient + G.light_color.rgb * diff);
                    col = col + tinted * point_diffuse(p, n); // placeable point lights
                    let h = normalize(l + v);
                    let shininess = max(bpar.x, 1.0);
                    let spec = pow(max(dot(n, h), 0.0), shininess) * G.blob_specular[bi].a * select(0.0, 1.0, diff > 0.0);
                    col = col + G.blob_specular[bi].rgb * spec * G.light_color.rgb;
                    let rim_f = pow(1.0 - max(dot(n, v), 0.0), 2.0) * bpar.y;
                    col = col + G.blob_rim[bi].rgb * rim_f + emissive;
                }
            }
            out.color = vec4<f32>(clamp(col, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
            out.depth = ndc_z;
            drawn = true;
        }
    }
    if (!drawn) {
        out.color = vec4<f32>(sky_color(rd), 1.0);
        out.depth = 1.0;
    }
    return out;
}

// Silhouette mask: 1.0 where the matter field is hit and in front of the camera,
// discarded elsewhere. A post-pass edge-detects this into a selection outline that
// hugs the true SDF silhouette (not a bounding circle).
@fragment
fn fs_mask(in: VOut) -> @location(0) vec4<f32> {
    let near = G.inv_view_proj * vec4<f32>(in.ndc, 0.0, 1.0);
    let far = G.inv_view_proj * vec4<f32>(in.ndc, 1.0, 1.0);
    let ro = near.xyz / near.w;
    let rd = normalize(far.xyz / far.w - ro);

    let max_t = march_bound();
    var t = 0.0;
    var masked = 0.0;
    for (var i = 0; i < 160; i = i + 1) {
        let p = ro + rd * t;
        let d = map(p).d;
        let thr = 0.0015 * t + 0.0025;
        if (d < thr && real_surface(p, thr)) {
            let clip = G.view_proj * vec4<f32>(p, 1.0);
            let ndc_z = clip.z / clip.w;
            if (clip.w > 0.0 && ndc_z >= 0.0 && ndc_z <= 1.0) {
                masked = 1.0;
            }
            break;
        }
        t = t + max(d, 0.003) * 0.8;
        if (t > max_t) {
            break;
        }
    }
    if (masked < 0.5) {
        discard;
    }
    return vec4<f32>(1.0, 0.0, 0.0, 1.0);
}
