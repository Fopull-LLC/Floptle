// The SHARED distance-field module: the fused SDF field (terrain volumes +
// blobs), its distance-only sampling, and the two field-lighting effects built
// on it — SDF ambient occlusion and marched sun shadows (iq's `min(k·d/t)`
// analytic penumbra, plus proxy occluders so raster meshes cast too).
//
// This file is CONCATENATED onto both render passes' shaders at module-creation
// time (WGSL module-scope declarations are order-independent):
//   - `raymarch.wgsl` — declares `G`/`dist_tex`/`vol_samp` at group(0) and keeps
//     the color-carrying surface path (`map`, `volume_at`, …) for drawing.
//   - `raster.wgsl`  — declares the same three names at group(2) (bound to the
//     raymarch pass's own globals buffer + distance atlas), so mesh fragments
//     march the very same field: meshes RECEIVE field shadows and true SDF AO.
// Everything here reads only distances (never the color atlas), so the raster
// pass binds just the uniform + distance texture + sampler.

struct Globals {
    view_proj: mat4x4<f32>,
    inv_view_proj: mat4x4<f32>,
    light_dir: vec4<f32>,
    light_color: vec4<f32>,
    ambient: vec4<f32>,
    bg: vec4<f32>,
    center: vec4<f32>,      // (unused legacy field; blobs now live in `blobs`)
    params: vec4<f32>,      // x = time, y = blob count, z = blob↔volume blend k, w = volume count
    // Up to 16 baked volumes, EACH at its native voxel resolution inside one shared
    // 3D atlas (no combined-grid resolution spread — ADR-0015 / multi-volume terrain).
    vol_center: array<vec4<f32>, 16>, // xyz camera-relative box center, w = KIND (see `vol_drawn` & co.)
    vol_half: array<vec4<f32>, 16>,   // xyz half-extent, w = volume↔volume fuse k
    vol_atlas: array<vec4<f32>, 16>,  // xyz voxel offset in the atlas (renderer-patched)
    vol_dims: array<vec4<f32>, 16>,   // xyz voxel dims of this volume (renderer-patched)
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
    ao_params: vec4<f32>,                // SDF AO: x on, y strength, z radius (world)
    // Sun shadows (the Lighting node's knobs).
    shadow_params: vec4<f32>,            // x on, y penumbra k, z strength, w max march dist
    shadow_tint: vec4<f32>,              // rgb tint, w quantize bands (0 = smooth)
    shadow_extra: vec4<f32>,             // x = Bayer-dither the penumbra
    // Proxy occluders: collider shapes standing in for raster meshes in the shadow
    // march only (meshes aren't in the field). See `prox_d`.
    prox_count: vec4<f32>,               // x = active proxy count
    prox_a: array<vec4<f32>, 32>,        // xyz center / capsule end A (camera-relative), w = radius
    prox_b: array<vec4<f32>, 32>,        // xyz capsule end B / box half-extents, w = kind (0 sphere, 1 capsule, 2 box)
    prox_rot: array<vec4<f32>, 32>,      // box orientation quat (xyzw)
    // Depth fog (the Lighting node). Appended at the END so this struct stays
    // byte-identical to the Rust `RaymarchGlobals` that feeds it.
    fog_color: vec4<f32>,                // rgb = fog color (w unused)
    fog_params: vec4<f32>,               // x start dist, y end dist, z on (0/1), w unused
    // Per volume: the tight CONTENT box (camera-relative center + half-extent),
    // scanned from the baked voxels at upload — the sub-box of the brick that
    // actually holds surface. All march bounds use it instead of the full brick:
    // a generous terrain box is mostly empty air above the hills, and a camera
    // standing inside the brick must not pay to march (and fetch) through it.
    vol_tight_c: array<vec4<f32>, 16>,
    vol_tight_h: array<vec4<f32>, 16>,
    // ---- Field Shapes (ADR-0007 Sdf stage): up to 4 authored SDF shaders in
    // the scene, each contributing a distance (`custom_d`) min-folded into the
    // field. Shader code is SPLICED into this module by the renderer; per-shape
    // transform/params live here so edits are uniform writes, not recompiles.
    shape_meta: vec4<f32>,             // x = active shape count
    shape_pos: array<vec4<f32>, 4>,    // xyz camera-relative position, w = uniform scale
    shape_rot: array<vec4<f32>, 4>,    // INVERSE rotation quat (xyzw)
    shape_aux: array<vec4<f32>, 4>,    // x = bounding radius (world units)
    shape_uniforms: array<vec4<f32>, 64>, // 16 slots per shape (shader-exposed knobs)
    // Per-shape surface material, same model as terrain_*/blob_*.
    shape_tint: array<vec4<f32>, 4>,
    shape_emissive: array<vec4<f32>, 4>,
    shape_specular: array<vec4<f32>, 4>,
    shape_params: array<vec4<f32>, 4>,
    shape_rim: array<vec4<f32>, 4>,
    // Sky shader (ADR-0007 Sky stage): x = active (0/1). The shader's exposed uniforms ride
    // `sky_uniforms`. Appended at the END so the Rust `RaymarchGlobals` stays byte-identical.
    sky_meta: vec4<f32>,
    sky_uniforms: array<vec4<f32>, 16>,
    // S8 atmospheres (meta.x = count): per body color.rgb+density.w, camera-
    // relative center + surface radius, params = (shell height, clouds, -, -).
    atmo_meta: vec4<f32>,
    atmo_color: array<vec4<f32>, 4>,
    atmo_body: array<vec4<f32>, 4>,
    atmo_params: array<vec4<f32>, 4>,
    // Stars mode: meta.x = count (0 = legacy light_dir single light); per star
    // camera-relative position + (color.rgb, K) with irradiance = K / d².
    star_meta: vec4<f32>,
    star_pos: array<vec4<f32>, 4>,
    star_color: array<vec4<f32>, 4>,
};

// A point mapped into Field Shape `i`'s local frame: un-translate (positions
// are camera-relative on both sides), un-rotate by the stored INVERSE quat,
// un-scale. Sdf shader code (spliced below) authors in this space.
fn shape_local(i: u32, p: vec3<f32>) -> vec3<f32> {
    let q = G.shape_rot[i];
    let rel = p - G.shape_pos[i].xyz;
    let r = rel + 2.0 * cross(q.xyz, cross(q.xyz, rel) + q.w * rel);
    return r / max(G.shape_pos[i].w, 1e-6);
}

//[flsl-field-custom-begin] — the renderer splices generated Field Shape
// distance functions over this block; the stub keeps the field unchanged.
fn custom_d(p: vec3<f32>) -> f32 {
    return 1e9;
}
//[flsl-field-custom-end]

fn sd_sphere(p: vec3<f32>, r: f32) -> f32 {
    return length(p) - r;
}

// ---- Ray/bounds intersection helpers -----------------------------------------
// Everything in the field is bounded (volume boxes, blob spheres, proxy shapes),
// so a ray can compute ONCE where field content can possibly live along it and
// march only that span. This is the engine's central raymarch optimization: sky
// rays never march, distant terrain skips all the empty air in front of it, and
// shadow rays that leave the bounds stop immediately.

// 1/dir with zero components clamped away (keeps the slab test finite; the tiny
// epsilon direction is equivalent to nudging the ray, never wrong by > 1e-8).
fn safe_inv(d: vec3<f32>) -> vec3<f32> {
    let s = select(vec3<f32>(1.0), vec3<f32>(-1.0), d < vec3<f32>(0.0));
    return s / max(abs(d), vec3<f32>(1e-8));
}

// Entry/exit of ray `ro + t*inv⁻¹` through the box (center c, half-extent h):
// returns (t_in, t_out); a miss has t_in > t_out.
fn slab_span(ro: vec3<f32>, inv: vec3<f32>, c: vec3<f32>, h: vec3<f32>) -> vec2<f32> {
    let t1 = (c - h - ro) * inv;
    let t2 = (c + h - ro) * inv;
    let tmin = min(t1, t2);
    let tmax = max(t1, t2);
    return vec2<f32>(max(max(tmin.x, tmin.y), tmin.z), min(min(tmax.x, tmax.y), tmax.z));
}

// Entry/exit of ray `ro + t*rd` (rd normalized) through the sphere (c, r):
// returns (t_in, t_out); a miss has t_in > t_out.
fn sphere_span(ro: vec3<f32>, rd: vec3<f32>, c: vec3<f32>, r: f32) -> vec2<f32> {
    let oc = ro - c;
    let b = dot(oc, rd);
    let disc = b * b - (dot(oc, oc) - r * r);
    if (disc < 0.0) {
        return vec2<f32>(1.0, -1.0);
    }
    let s = sqrt(disc);
    return vec2<f32>(-b - s, -b + s);
}

// A volume's bound margin: the smin fuse can bulge the surface at most k/4
// outside the pieces' own bounds — 2k is a generous cover for both the
// volume↔volume fuse and the blob↔volume blend (G.params.z).
fn vol_pad(i: u32) -> f32 {
    return 0.5 + 2.0 * max(G.vol_half[i].w, G.params.z);
}

// A blob's bounding radius: the metaball geometry reaches ≈0.83·s from its
// center; margins cover the blob↔blob (0.3·s) and blob↔volume (params.z) fuses.
fn blob_bound(i: u32) -> f32 {
    let s = max(G.blobs[i].w, 0.02);
    return s + max(0.3 * s, G.params.z);
}

// ---- Volume kinds (`vol_center.w`) ---------------------------------------------
//
//   0 = absent
//   1 = render        — drawn by the raymarch, in the AO field, casts shadows
//   2 = occluder bake — casts shadows ONLY: a baked static level mesh whose real
//                       triangles the raster pass draws. Deliberately outside the AO
//                       field (it would double-occlude its own triangles).
//   3 = shadow + AO, NOT drawn — MESHED TERRAIN (ADR terrain 2.0 / P2). The raster
//                       pass draws its extracted chunk meshes, while the field keeps
//                       casting its sun shadows AND darkening props that stand on it.
//
// Kind 3 exists rather than re-using kind 2 for one reason: `map_d` skips kind 2, so
// terrain-as-2 would silently strip the SDF contact AO out from under every prop in
// the scene. Kind 3 is therefore identical to kind 1 in every FIELD-MATH site
// (`volumes_d`, `union_edge_m`, `field_eps`) and differs only in the DRAW sites
// (`field_span`, `volumes`, `real_surface`, `containing_volume`) — which is what makes
// the render swap a change to visibility alone, with shadows and AO untouched.
fn vol_absent(i: u32) -> bool { return G.vol_center[i].w < 0.5; }
// Kind 1 only: the raymarch draws this volume.
fn vol_drawn(i: u32) -> bool { return abs(G.vol_center[i].w - 1.0) < 0.5; }
// Kinds 1 and 3: this volume is matter as far as normals / AO / the fused smin go.
fn vol_in_field(i: u32) -> bool {
    let w = G.vol_center[i].w;
    return w > 0.5 && (w < 1.5 || w > 2.5);
}
// Kind 2 only: a cast-only occluder bake, folded into the shadow march with a plain min.
fn vol_occluder(i: u32) -> bool { return abs(G.vol_center[i].w - 2.0) < 0.5; }

// The span of the whole DRAWN field (render volumes + blobs) along a ray — the
// primary march runs only inside it. Returns (t0, t1); t0 > t1 = provably sky.
fn field_span(ro: vec3<f32>, rd: vec3<f32>, max_t: f32) -> vec2<f32> {
    var t0 = 1e30;
    var t1 = -1e30;
    let inv = safe_inv(rd);
    let vols = min(u32(G.params.w), 16u);
    for (var i = 0u; i < vols; i = i + 1u) {
        if (!vol_drawn(i)) { continue; }        // DRAW: kind 3 terrain is meshed, not marched
        // The TIGHT content box, not the brick: rays over the hills toward the
        // sky must exit at the terrain's true top, not the brick's.
        let s = slab_span(ro, inv, G.vol_tight_c[i].xyz, G.vol_tight_h[i].xyz + vec3<f32>(vol_pad(i)));
        if (s.x <= s.y && s.y > 0.0) {
            t0 = min(t0, s.x);
            t1 = max(t1, s.y);
        }
    }
    let count = min(u32(G.params.y), 16u);
    for (var i = 0u; i < count; i = i + 1u) {
        let s = sphere_span(ro, rd, G.blobs[i].xyz, blob_bound(i));
        if (s.x <= s.y && s.y > 0.0) {
            t0 = min(t0, s.x);
            t1 = max(t1, s.y);
        }
    }
    // Field Shapes: their authored bounding spheres join the span.
    let shapes = min(u32(G.shape_meta.x), 4u);
    for (var i = 0u; i < shapes; i = i + 1u) {
        let s = sphere_span(ro, rd, G.shape_pos[i].xyz, G.shape_aux[i].x);
        if (s.x <= s.y && s.y > 0.0) {
            t0 = min(t0, s.x);
            t1 = max(t1, s.y);
        }
    }
    return vec2<f32>(max(t0, 0.0), min(t1, max_t));
}

fn smin(a: f32, b: f32, k: f32) -> f32 {
    let h = clamp(0.5 + 0.5 * (b - a) / k, 0.0, 1.0);
    return mix(b, a, h) - k * h * (1.0 - h);
}

// The analytic blob's GEOMETRY (fixed-offset smin-blended spheres; see
// `blob_one` in raymarch.wgsl for the look/why): the distance-only half, shared
// so shadows/AO see blobs exactly as the surface pass draws them.
fn blob_d(p: vec3<f32>, center: vec3<f32>, s: f32) -> f32 {
    let q = (p - center) / s;
    var d = sd_sphere(q - vec3<f32>(0.26, 0.10, 0.0), 0.55);
    d = smin(d, sd_sphere(q - vec3<f32>(-0.24, 0.16, 0.12), 0.50), 0.30);
    d = smin(d, sd_sphere(q - vec3<f32>(0.06, -0.22, -0.14), 0.50), 0.30);
    d = smin(d, sd_sphere(q - vec3<f32>(-0.10, -0.06, 0.24), 0.48), 0.30);
    return d * s;
}

// Every blob folded together with smin — the distance mirror of `analytic` in
// raymarch.wgsl (same seeding rule: never blend against the 1e9 sentinel).
fn analytic_d(p: vec3<f32>) -> f32 {
    let count = min(u32(G.params.y), 16u);
    if (count == 0u) {
        return 1e9;
    }
    var d = blob_d(p, G.blobs[0].xyz, max(G.blobs[0].w, 0.02));
    for (var i = 1u; i < count; i = i + 1u) {
        let b = blob_d(p, G.blobs[i].xyz, max(G.blobs[i].w, 0.02));
        d = smin(d, b, 0.3 * max(G.blobs[i].w, 0.05));
    }
    return d;
}

// Map a box-relative position to atlas texture coords for volume `i`. The voxel
// coordinate is clamped half a voxel inside the slot — the per-volume equivalent of
// ClampToEdge, which also stops trilinear taps bleeding into the neighbouring slot.
fn atlas_uvw(i: u32, rel: vec3<f32>) -> vec3<f32> {
    let dims = G.vol_dims[i].xyz;
    let frac = clamp(rel / (2.0 * G.vol_half[i].xyz) + 0.5, vec3<f32>(0.0), vec3<f32>(1.0));
    let vox = clamp(frac * dims, vec3<f32>(0.5), dims - 0.5);
    return (G.vol_atlas[i].xyz + vox) / vec3<f32>(textureDimensions(dist_tex));
}

// One baked volume's DISTANCE (the distance mirror of `volume_at` in
// raymarch.wgsl — same outside-the-brick continuation + 0.08 floor; see the
// comments there for why).
fn volume_d(i: u32, p: vec3<f32>) -> f32 {
    let vh = G.vol_half[i].xyz;
    let rel = p - G.vol_center[i].xyz;
    let q = abs(rel) - vh;
    let box_d = length(max(q, vec3<f32>(0.0))) + min(max(q.x, max(q.y, q.z)), 0.0);
    // Far from the brick the box distance alone is a valid (conservative) lower
    // bound and the edge-continuation can't influence any nearby surface — skip
    // the 3D-texture fetch entirely. The cutoff scales with the fuse radius so a
    // wide smin still sees the continued field where it actually blends. (The
    // TIGHT content box is deliberately NOT used here: in the brick's air its
    // distance is a much weaker bound than the fetched true distance, so trading
    // the fetch for it costs more small steps than it saves — measured slower.
    // The tight box pays off where whole marches are skipped or ended early:
    // `field_span` and the `light_vis` relevance sweep.)
    if (box_d > 4.0 + 2.0 * G.vol_half[i].w) {
        return box_d;
    }
    let d = textureSampleLevel(dist_tex, vol_samp, atlas_uvw(i, rel), 0.0).r;
    if (box_d > 0.0) {
        return max(box_d + max(d, 0.0), 0.08);
    }
    return d;
}

// Distance from `p` INWARD from volume `i`'s side/bottom faces (positive inside;
// the top face never tapers — it's the ground surface).
fn box_edge(i: u32, p: vec3<f32>) -> f32 {
    let vh = G.vol_half[i].xyz;
    let rel = p - G.vol_center[i].xyz;
    return min(min(vh.x - abs(rel.x), vh.z - abs(rel.z)), rel.y + vh.y);
}

// Edge distance to the UNION of all present volume boxes at `p` — the max of the
// containing boxes' individual edge distances. This is what makes seams seamless:
// near volume A's face but deep inside overlapping volume B, the union edge is B's
// (large), so no taper; on a face no neighbor continues past, it's small → taper.
// A single isolated volume reduces exactly to its own edge (the original look).
fn union_edge_m(p: vec3<f32>, mask: u32) -> f32 {
    var e = -1e9;
    let vols = min(u32(G.params.w), 16u);
    for (var i = 0u; i < vols; i = i + 1u) {
        if ((mask & (1u << i)) == 0u) { continue; }
        if (!vol_in_field(i)) { continue; }
        let q = abs(p - G.vol_center[i].xyz) - G.vol_half[i].xyz;
        if (max(q.x, max(q.y, q.z)) < 0.0) {
            e = max(e, box_edge(i, p));
        }
    }
    return e;
}

fn union_edge(p: vec3<f32>) -> f32 {
    return union_edge_m(p, 0xffffu);
}

// True when `p` is inside ANY volume's box expanded by `e` — used to reject false
// hits on the boxes' bounding faces (the box-approach distance is never a real
// surface), while a small `e` still admits genuine terrain hits right at a face.
fn inside_volume_box_eps(p: vec3<f32>, e: f32) -> bool {
    let vols = min(u32(G.params.w), 16u);
    for (var i = 0u; i < vols; i = i + 1u) {
        if (!vol_drawn(i)) { continue; }        // DRAW: only a drawn box can produce a false hit
        let q = abs(p - G.vol_center[i].xyz) - G.vol_half[i].xyz;
        if (max(q.x, max(q.y, q.z)) < e) { return true; }
    }
    return false;
}

// The volume containing `p` (smallest sampled distance among boxes it's inside,
// expanded by `e`) — for per-volume voxel size / texture-slot decisions. −1 = none.
fn containing_volume(p: vec3<f32>, e: f32) -> i32 {
    var best = -1;
    var bd = 1e9;
    let vols = min(u32(G.params.w), 16u);
    for (var i = 0u; i < vols; i = i + 1u) {
        if (!vol_drawn(i)) { continue; }        // DRAW: picks the texture slot the march shades with
        let q = abs(p - G.vol_center[i].xyz) - G.vol_half[i].xyz;
        if (max(q.x, max(q.y, q.z)) < e) {
            let d = volume_d(i, p);
            if (d < bd) { bd = d; best = i32(i); }
        }
    }
    return best;
}

// Every present volume's distance folded with smin + the union-edge taper — the
// distance mirror of `volumes` in raymarch.wgsl (see there for the taper rationale).
struct VolFoldD { d: f32, any: bool };
fn volumes_d(p: vec3<f32>) -> VolFoldD {
    var d = 1e9;
    var any = false;
    let vols = min(u32(G.params.w), 16u);
    for (var i = 0u; i < vols; i = i + 1u) {
        if (!vol_in_field(i)) { continue; }
        let v = volume_d(i, p);
        if (!any) {
            d = v;
            any = true;
        } else {
            d = smin(d, v, max(G.vol_half[i].w, 0.0001));
        }
    }
    let uedge = union_edge(p);
    if (any && uedge > -1e8) {
        d = max(d, 2.0 - uedge);
    }
    return VolFoldD(d, any);
}

// The whole field's DISTANCE: every piece of matter folded together with smin.
// Identical math to `map` in raymarch.wgsl minus the color fetches — this is what
// normals, AO and shadow rays march (they never need color). The sentinel rules
// are the same: never smin against an absent part (f32 cancellation collapses it).
fn map_d(p: vec3<f32>) -> f32 {
    let a = analytic_d(p);
    let v = volumes_d(p);
    var base: f32;
    if (!v.any) {
        base = a;
    } else if (u32(G.params.y) == 0u) {
        base = v.d;
    } else {
        base = smin(a, v.d, max(G.params.z, 0.0001));
    }
    // Field Shapes union in hard (min is exact against the 1e9 stub — no f32
    // cancellation, unlike smin against an absent part).
    return min(base, custom_d(p));
}

// The field's sampling granularity at `p`: ~one voxel inside a baked volume (the
// central difference / shadow lift must span cell boundaries to low-pass residual
// grid+f16 noise), a small fixed epsilon on the analytic blobs.
fn field_eps(p: vec3<f32>) -> f32 {
    var h = 0.012;
    let vols = min(u32(G.params.w), 16u);
    for (var i = 0u; i < vols; i = i + 1u) {
        if (!vol_in_field(i)) { continue; }
        let q = abs(p - G.vol_center[i].xyz) - G.vol_half[i].xyz;
        if (max(q.x, max(q.y, q.z)) < 0.08) {
            // Where boxes overlap the LARGEST voxel wins — pure box tests, no
            // texture fetch (this runs per shadow ray, it must stay cheap).
            let voxel = 2.0 * G.vol_half[i].xyz / max(G.vol_dims[i].xyz, vec3<f32>(1.0));
            h = max(h, clamp(max(voxel.x, max(voxel.y, voxel.z)), 0.02, 1.0));
        }
    }
    return h;
}

// SDF ("true") ambient occlusion: step outward along the normal and measure how
// much the fused field (volumes + blobs) pinches in versus open space — iq's
// exponentially-weighted AO. Because it reads the real distance field it shades
// creases, overhangs and contact points regardless of the camera, with none of
// SSAO's screen-space artifacts. Driven by the scene PostProcess node's `Sdf` AO
// mode (ao_params: y = strength, z = radius in world units). Mesh fragments call
// this too (the raster pass binds the field), so meshes RECEIVE field AO — they
// just don't occlude, not being in the field themselves.
// Depth fog: blend `color` toward the fog color by camera-relative distance. `pos`
// is the camera-relative fragment position — the camera is the origin (ADR-0015), so
// `length(pos)` is the view distance, a small number even at world 1e7 (no depth
// reconstruction, no precision loss). Off (returns `color`) when fog_params.z == 0.
// ---- S8 atmospheres: shell scattering shared by SKY rays and GEOMETRY rays.
// Cheap value-noise fbm for the cloud layer (3 octaves on the shell sphere).
fn hash31(p3in: vec3<f32>) -> f32 {
    var p3 = fract(p3in * 0.1031);
    p3 += dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

fn vnoise(p: vec3<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * (3.0 - 2.0 * f);
    let a = hash31(i);
    let b = hash31(i + vec3<f32>(1.0, 0.0, 0.0));
    let c = hash31(i + vec3<f32>(0.0, 1.0, 0.0));
    let d = hash31(i + vec3<f32>(1.0, 1.0, 0.0));
    let e = hash31(i + vec3<f32>(0.0, 0.0, 1.0));
    let f1 = hash31(i + vec3<f32>(1.0, 0.0, 1.0));
    let g1 = hash31(i + vec3<f32>(0.0, 1.0, 1.0));
    let h1 = hash31(i + vec3<f32>(1.0, 1.0, 1.0));
    let lo = mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
    let hi = mix(mix(e, f1, u.x), mix(g1, h1, u.x), u.y);
    return mix(lo, hi, u.z);
}

fn cloud_fbm(p: vec3<f32>) -> f32 {
    var v = 0.0;
    var amp = 0.55;
    var q = p;
    for (var i = 0; i < 3; i = i + 1) {
        v += amp * vnoise(q);
        q = q * 2.13 + vec3<f32>(11.7, 5.1, 7.3);
        amp *= 0.5;
    }
    return v;
}

// Atmosphere + clouds composited over `base` along the ray `rd` (camera at the
// origin), stopping at geometry `tmax` (1e9 for sky rays). Every listed body's
// shell is intersected analytically: the chord length through the shell sets
// the optical depth, so the SAME math gives a tinted sky from inside, the limb
// halo seen from space, aerial haze over a planet's disc, and cloud decks both
// overhead and from orbit. `is_sky` adds the scattered star glow.
fn atmo_composite(base: vec3<f32>, rd_in: vec3<f32>, tmax: f32, is_sky: bool) -> vec3<f32> {
    var out = base;
    let count = min(u32(G.atmo_meta.x), 4u);
    if (count == 0u) {
        return out;
    }
    let rd = normalize(rd_in);
    // The glow/scatter color: star 0 in stars mode, the legacy light otherwise.
    var gcol = G.light_color.rgb;
    if (G.star_meta.x > 0.5) {
        gcol = G.star_color[0].rgb;
    }
    for (var i = 0u; i < count; i = i + 1u) {
        let c = G.atmo_body[i].xyz;
        let R = G.atmo_body[i].w;
        let H = G.atmo_params[i].x;
        let density = G.atmo_color[i].w;
        if (H < 0.001 || density < 0.001) {
            continue;
        }
        let Ra = R + H;
        let b = dot(rd, c);
        // Perpendicular distance² from the body centre to the ray. Computed as the
        // squared length of c's REJECTION onto rd (c - b·rd), NOT `dot(c,c)-b·b`:
        // at orbital scale |c| reaches ~6e5, so dot(c,c) and b·b are both ~3.6e11
        // and their difference loses all precision (catastrophic f32 cancellation),
        // jittering NaNs into the sky every frame as the camera moves. The
        // rejection form is stable at any distance.
        let perp = c - rd * b;
        let d2 = dot(perp, perp);
        if (d2 > Ra * Ra || (b < 0.0 && dot(c, c) > Ra * Ra)) {
            continue; // misses the shell, or the shell is entirely behind us
        }
        let hh = sqrt(max(Ra * Ra - d2, 0.0));
        let t0 = max(b - hh, 0.0);
        let t1 = min(b + hh, tmax);
        if (t1 <= t0) {
            continue;
        }
        // Optical depth: EXPONENTIAL extinction over the chord through the
        // shell (denser near the surface). Beer-Lambert keeps the planet's
        // disc readable from orbit — the old linear chord saturated the whole
        // face to a solid ball of sky color — while grazing limb rays still
        // build up into the halo.
        let midp = rd * (0.5 * (t0 + t1));
        let midalt = length(midp - c) - R;
        let densf = clamp(1.0 - midalt / H, 0.05, 1.0);
        let a = (1.0 - exp(-(t1 - t0) / (H * 6.0) * densf)) * density;
        // Day side: how high the star stands over the chord point's horizon.
        // The twilight band is WIDE (scattering wraps well past the terminator)
        // so the halo fades smoothly around the limb instead of cutting off at
        // a hard day/night edge, and a faint airglow floor keeps the ring
        // readable all the way around — dim, but never invisible, on the night
        // side.
        let zen = normalize(midp - c);
        let sdir = sun_dir_at(midp);
        let sda = dot(zen, sdir);
        let daylight = smoothstep(-0.45, 0.35, sda);
        let scatter = max(daylight, 0.12);
        let scol = G.atmo_color[i].rgb * scatter;
        out = mix(out, scol, a);
        // Cloud deck: a drifting noise shell at ~1/3 of the atmosphere height.
        let cov = G.atmo_params[i].y;
        if (cov > 0.01) {
            let rc = R + H * 0.35;
            if (d2 < rc * rc) {
                let hc = sqrt(rc * rc - d2);
                var tc = b - hc;
                if (tc < 0.0) {
                    tc = b + hc; // camera inside the deck sphere: use the far hit
                }
                if (tc > 0.0 && tc < tmax) {
                    let cp = normalize(rd * tc - c);
                    let drift = G.params.x * 0.004;
                    let nse = cloud_fbm(cp * 14.0 + vec3<f32>(drift, 0.0, drift * 0.7));
                    let edge = 1.0 - cov * 0.9;
                    let cl = smoothstep(edge, edge + 0.22, nse);
                    let ccol = mix(scol, vec3<f32>(daylight * 0.95), 0.75);
                    // Cloud opacity rides its own gentle curve — never the
                    // saturated atmosphere alpha (that whited out the disc).
                    out = mix(out, ccol, cl * 0.85 * clamp(density + 0.25, 0.0, 1.0) * smoothstep(0.02, 0.2, a));
                }
            }
        }
        if (is_sky) {
            let sd = max(dot(rd, sun_dir_at(vec3<f32>(0.0))), 0.0);
            out += gcol * (pow(sd, 180.0) * 1.4 + pow(sd, 10.0) * 0.12) * a * daylight;
        }
    }
    // Never emit a NaN: a single bad component resolves to 0 (black) in the
    // attachment and flickers the sky. Fall back to the un-scattered base.
    if (!all(out == out)) {
        return base;
    }
    return out;
}

fn apply_fog(color: vec3<f32>, pos: vec3<f32>, pix: vec2<u32>) -> vec3<f32> {
    // Aerial perspective first: atmosphere + clouds BETWEEN the camera and this
    // surface (haze over a planet seen from orbit, cloud decks over its disc).
    let color2 = atmo_composite(color, pos, length(pos), false);
    if (G.fog_params.z < 0.5) {
        return color2;
    }
    let denom = max(G.fog_params.y - G.fog_params.x, 1e-4);
    var f = clamp((length(pos) - G.fog_params.x) / denom, 0.0, 1.0);
    // Optional dither of the fog factor to break up 8-bit banding on slow gradients.
    // Strength rides in the spare fog_color.w lane (0 = off); mode in fog_params.w
    // (0 = Bayer 4×4, 1 = interleaved-gradient noise). A sub-percent nudge is enough.
    let amp = G.fog_color.w;
    if (amp > 0.0) {
        let d = select(bayer4(pix), ign(pix), G.fog_params.w > 0.5);
        f = clamp(f + (d - 0.5) * amp * 0.06, 0.0, 1.0);
    }
    return mix(color2, G.fog_color.rgb, f);
}

// The UNCLAMPED voxel edge of the coarsest in-field volume containing `p` —
// unlike `field_eps` (clamped to 1.0 for step sizing), this reports the truth,
// so consumers can judge how much detail the field can actually resolve (a
// planet's 192-cap shadow proxy runs 4+ units per voxel).
fn vol_voxel_at(p: vec3<f32>) -> f32 {
    var h = 0.02;
    let vols = min(u32(G.params.w), 16u);
    for (var i = 0u; i < vols; i = i + 1u) {
        if (!vol_in_field(i)) { continue; }
        let q = abs(p - G.vol_center[i].xyz) - G.vol_half[i].xyz;
        if (max(q.x, max(q.y, q.z)) < 0.08) {
            let voxel = 2.0 * G.vol_half[i].xyz / max(G.vol_dims[i].xyz, vec3<f32>(1.0));
            h = max(h, max(voxel.x, max(voxel.y, voxel.z)));
        }
    }
    return h;
}

fn sdf_ao(p: vec3<f32>, n: vec3<f32>) -> f32 {
    let radius = max(G.ao_params.z, 1e-3);
    var occ = 0.0;
    var sca = 1.0;
    for (var i = 1; i <= 5; i = i + 1) {
        let h = radius * f32(i) / 5.0;
        occ = occ + (h - map_d(p + n * h)) * sca;
        sca = sca * 0.6;
    }
    let ao = clamp(1.0 - 1.5 * occ / radius, 0.0, 1.0);
    // TRUST falls with field coarseness: sampling a 1.5-unit AO radius out of a
    // 4-unit-voxel planet proxy reads trilinear mush, not occlusion — it painted
    // blobby light/dark patches over night-side terrain (2026-07-20). When the
    // voxel can't resolve the radius, fade toward flat (SSAO covers fine detail).
    let trust = clamp(radius / vol_voxel_at(p), 0.0, 1.0);
    return mix(1.0, ao, clamp(G.ao_params.y, 0.0, 1.0) * trust);
}

// SHADOW-ONLY occluder volumes (vol_center.w = 2): baked static level meshes that
// cast sun shadows with their true silhouette (dark interiors!) but are never
// drawn — the raster pass renders the actual triangles. Folded into the shadow
// march only; the render/AO field (`map_d`) skips them.
fn shadow_volumes_d(p: vec3<f32>) -> f32 {
    var d = 1e9;
    let vols = min(u32(G.params.w), 16u);
    for (var i = 0u; i < vols; i = i + 1u) {
        if (!vol_occluder(i)) { continue; }
        d = min(d, volume_d(i, p));
    }
    return d;
}

// The voxel size of the shadow-only volume containing `p` (0 when none) — the
// shadow-ray lift must clear the occluder bake's own fattened sheet, or a mesh
// standing in its bake would blanket self-shadow.
fn shadow_vol_eps(p: vec3<f32>) -> f32 {
    var h = 0.0;
    let vols = min(u32(G.params.w), 16u);
    for (var i = 0u; i < vols; i = i + 1u) {
        if (!vol_occluder(i)) { continue; }
        let q = abs(p - G.vol_center[i].xyz) - G.vol_half[i].xyz;
        if (max(q.x, max(q.y, q.z)) < 0.08) {
            let voxel = 2.0 * G.vol_half[i].xyz / max(G.vol_dims[i].xyz, vec3<f32>(1.0));
            h = max(h, max(voxel.x, max(voxel.y, voxel.z)));
        }
    }
    return h;
}

// Rotate `v` by the CONJUGATE (inverse, for unit quats) of quaternion `q` —
// world → box-local for the oriented box proxy.
fn quat_unrotate(q: vec4<f32>, v: vec3<f32>) -> vec3<f32> {
    let u = -q.xyz;
    return v + 2.0 * cross(u, cross(u, v) + q.w * v);
}

// Proxy occluder `i`'s distance at `p`: the cheap analytic stand-in (sphere /
// capsule / oriented box, harvested from the node's collider) that lets a raster
// mesh CAST shadows without being in the field. Folded into the shadow march
// only — proxies never affect the drawn surface or AO.
fn prox_d(i: u32, p: vec3<f32>) -> f32 {
    let a = G.prox_a[i];
    let b = G.prox_b[i];
    if (b.w < 0.5) { // sphere
        return length(p - a.xyz) - a.w;
    }
    if (b.w < 1.5) { // capsule from a to b
        let ba = b.xyz - a.xyz;
        let t = clamp(dot(p - a.xyz, ba) / max(dot(ba, ba), 1e-6), 0.0, 1.0);
        return length(p - a.xyz - ba * t) - a.w;
    }
    // Oriented box: half-extents in b.xyz, orientation quat in prox_rot.
    let q = quat_unrotate(G.prox_rot[i], p - a.xyz);
    let d = abs(q) - b.xyz;
    return length(max(d, vec3<f32>(0.0))) + min(max(d.x, max(d.y, d.z)), 0.0);
}

// The bounding sphere of proxy occluder `i` — (center, radius) covering the
// sphere/capsule/oriented-box exactly (used for the shadow ray's relevance sweep).
fn prox_bound(i: u32) -> vec4<f32> {
    let a = G.prox_a[i];
    let b = G.prox_b[i];
    if (b.w < 0.5) { // sphere
        return vec4<f32>(a.xyz, a.w);
    }
    if (b.w < 1.5) { // capsule from a to b
        let c = 0.5 * (a.xyz + b.xyz);
        return vec4<f32>(c, 0.5 * length(b.xyz - a.xyz) + a.w);
    }
    // Oriented box: half-extents in b.xyz around center a.
    return vec4<f32>(a.xyz, length(b.xyz));
}

// The masked shadow-march field: the same fold as `min(map_d, shadow_volumes_d)`
// but touching ONLY the pieces whose bounds the shadow ray actually crosses
// (`vmask` bits over volumes of both kinds, `blobs` for the analytic part).
// Skipped pieces sit ≥ their bound margin from every point on the ray, where
// their contribution to the fold provably cannot move the surface.
fn shadow_field_d(p: vec3<f32>, vmask: u32, blobs: bool) -> f32 {
    var vd = 1e9;   // render volumes (w = 1): smin fold + union-edge taper
    var any = false;
    var sd = 1e9;   // shadow-only occluder bakes (w = 2): plain min
    let vols = min(u32(G.params.w), 16u);
    for (var i = 0u; i < vols; i = i + 1u) {
        if ((vmask & (1u << i)) == 0u) { continue; }
        if (vol_occluder(i)) {
            sd = min(sd, volume_d(i, p));
        } else {
            let v = volume_d(i, p);
            if (!any) {
                vd = v;
                any = true;
            } else {
                vd = smin(vd, v, max(G.vol_half[i].w, 0.0001));
            }
        }
    }
    if (any) {
        let uedge = union_edge_m(p, vmask);
        if (uedge > -1e8) {
            vd = max(vd, 2.0 - uedge);
        }
    }
    var field = 1e9;
    let has_blobs = blobs && G.params.y >= 0.5;
    if (any && has_blobs) {
        field = smin(analytic_d(p), vd, max(G.params.z, 0.0001));
    } else if (any) {
        field = vd;
    } else if (has_blobs) {
        field = analytic_d(p);
    }
    return min(field, sd);
}

// Visibility of the sun from surface point `p` with normal `n`: 1 = fully lit,
// 0 = fully shadowed, in between = analytic penumbra. Marches the fused field
// PLUS the proxy occluders toward the light, tracking iq's `min(k·d/t)` — the
// single `k` sweeps razor-hard (large) to dreamy-soft (small) with no kernels.
//
// Before marching, one cheap relevance sweep intersects the ray with every
// piece's bound: pieces the ray can't touch are skipped in the march entirely,
// a ray that touches nothing returns lit with no march at all (the common case
// for open ground / sky-facing walls), and the march stops at the LAST bound
// exit instead of crawling to the full shadow distance.
fn light_vis(p: vec3<f32>, n: vec3<f32>, l: vec3<f32>) -> f32 {
    let k = G.shadow_params.y;
    let max_d = G.shadow_params.w;

    // Lift off the surface along the normal (voxel-aware) so the ray doesn't
    // immediately re-hit the surface it starts on (shadow acne on the noisy
    // f16-sampled terrain field). When the sun GRAZES the surface (n·l → 0) the
    // ray hugs that noisy shell for a long stretch and grazing walls stripe —
    // boost the lift there, but leave ordinary sun angles alone so contact
    // shadows stay tight. (Computed before the relevance sweep: the sweep must
    // test the ACTUAL march ray, which starts at `ro`, not at the surface.)
    let base = max(0.03, max(field_eps(p), shadow_vol_eps(p)) * 1.6);
    // Absolute cap: the voxel-scaled grazing boost reached 6+ units on coarse
    // planet proxies — far enough to START the ray past a cave roof or a whole
    // terrain feature, which lit sealed caves from the inside (2026-07-20).
    let lift = min(base * clamp(0.5 / max(dot(n, l), 0.125), 1.0, 4.0), 3.0);
    let ro = p + n * lift;

    // ---- Relevance sweep: which pieces can this ray possibly matter for?
    // "Matter" is wider than "hit": the k*d/t penumbra estimator dims for pieces
    // the ray merely passes NEAR — within t/k at range t — so every bound is
    // expanded by (distance along the ray to the piece)/k, the estimator's exact
    // reach at that range. Without this, shadows clip to their caster's raw
    // bound (a box shadow rounds into its bounding-sphere's ellipse).
    let inv = safe_inv(l);
    let pen_k = max(k, 1.0);
    var vmask = 0u;
    var pmask = 0u;
    var blobs = false;
    var t_end = 0.0;
    let vols = min(u32(G.params.w), 16u);
    for (var i = 0u; i < vols; i = i + 1u) {
        if (vol_absent(i)) { continue; } // every present kind casts (1, 2 and 3 alike)
        // Tight content box, not the brick: a sun ray from open ground exits the
        // terrain at its true top, so t_end stops just past the hills instead of
        // marching to the brick's roof.
        let pen = max(dot(G.vol_tight_c[i].xyz - ro, l), 0.0) / pen_k;
        let s = slab_span(ro, inv, G.vol_tight_c[i].xyz, G.vol_tight_h[i].xyz + vec3<f32>(vol_pad(i) + pen));
        if (s.x <= s.y && s.y > 0.0 && s.x < max_d) {
            vmask = vmask | (1u << i);
            t_end = max(t_end, min(s.y, max_d));
        }
    }
    let bc = min(u32(G.params.y), 16u);
    for (var i = 0u; i < bc; i = i + 1u) {
        let pen = max(dot(G.blobs[i].xyz - ro, l), 0.0) / pen_k;
        let s = sphere_span(ro, l, G.blobs[i].xyz, blob_bound(i) + pen);
        if (s.x <= s.y && s.y > 0.0 && s.x < max_d) {
            blobs = true;
            t_end = max(t_end, min(s.y, max_d));
        }
    }
    let pc = min(u32(G.prox_count.x), 32u);
    for (var i = 0u; i < pc; i = i + 1u) {
        let bnd = prox_bound(i);
        let pen = max(dot(bnd.xyz - ro, l), 0.0) / pen_k;
        let s = sphere_span(ro, l, bnd.xyz, bnd.w + pen);
        if (s.x <= s.y && s.y > 0.0 && s.x < max_d) {
            pmask = pmask | (1u << i);
            t_end = max(t_end, min(s.y, max_d));
        }
    }
    // Field Shapes cast too: penumbra-expanded bounding spheres, exactly like
    // blobs (the k·d/t estimator's reach — see the sweep comment above).
    var shapes = false;
    let sc = min(u32(G.shape_meta.x), 4u);
    for (var i = 0u; i < sc; i = i + 1u) {
        let pen = max(dot(G.shape_pos[i].xyz - ro, l), 0.0) / pen_k;
        let s = sphere_span(ro, l, G.shape_pos[i].xyz, G.shape_aux[i].x + pen);
        if (s.x <= s.y && s.y > 0.0 && s.x < max_d) {
            shapes = true;
            t_end = max(t_end, min(s.y, max_d));
        }
    }
    if (vmask == 0u && pmask == 0u && !blobs && !shapes) {
        return 1.0; // nothing anywhere along this ray — fully lit, no march
    }
    // A proxy containing the start point is the caster this fragment belongs to
    // (a character standing inside its own capsule) — skip it so meshes don't
    // blanket-shadow themselves; it still casts on everything else.
    var skip = 0u;
    for (var i = 0u; i < pc; i = i + 1u) {
        if ((pmask & (1u << i)) != 0u && prox_d(i, ro) < lift) { skip = skip | (1u << i); }
    }
    let march = pmask & ~skip;
    var t = lift;
    var vis = 1.0;
    // The k·d/t penumbra estimator is hypersensitive while t is tiny: right at the
    // start surface, sub-voxel noise in the f16-sampled field (worst on the tapered
    // slab walls) reads as near-occluders and stripes the penumbra. Hard hits are
    // tested from the first step, but penumbra only accumulates once the ray has
    // cleared the start surface's own noise floor (keyed to the UNSCALED lift so
    // ordinary ground keeps tight contact shadows).
    let pen_t0 = base * 3.0;
    for (var s = 0; s < 64; s = s + 1) {
        let q = ro + l * t;
        var d = shadow_field_d(q, vmask, blobs);
        if (shapes) {
            d = min(d, custom_d(q));
        }
        for (var i = 0u; i < pc; i = i + 1u) {
            if ((march & (1u << i)) != 0u) {
                d = min(d, prox_d(i, q));
            }
        }
        if (d < 0.001) { return 0.0; }   // hard hit — fully occluded
        if (d > 1e8) { break; }          // nothing along this ray — fully lit
        if (t > pen_t0) {
            vis = min(vis, clamp(k * d / t, 0.0, 1.0));
        }
        // Step cap GROWS with distance: a flat 4-unit cap gave the 64-step march
        // a 256-unit total reach — any longer relevance span (a planet's volume
        // is 600+) exhausted the loop and returned mostly-LIT, so starlight
        // leaked through hundreds of units of rock in soft blobs (2026-07-20).
        // The k·d/t penumbra needs no dense sampling far out (its scale ~t/k
        // grows too), so geometric growth loses nothing.
        t = t + clamp(d, 0.02, max(4.0, t * 0.12));
        if (vis < 0.01 || t > t_end) { break; }
    }
    if (t < t_end) {
        // Ran out of steps mid-span. With the growing cap that only happens when
        // d stayed pinned small for all 64 steps — the ray spent its whole life
        // hugging matter — so it's occluded, not lit-by-default.
        vis = 0.0;
    }
    return clamp(vis, 0.0, 1.0);
}

// Bayer 4×4 ordered-dither threshold for pixel `pix`, in (0,1) — the classic
// crosshatch pattern; at retro internal resolutions the cells go chunky with
// the pixels, which is the point.
fn bayer4(pix: vec2<u32>) -> f32 {
    var m = array<u32, 16>(0u, 8u, 2u, 10u, 12u, 4u, 14u, 6u, 3u, 11u, 1u, 9u, 15u, 7u, 13u, 5u);
    return (f32(m[(pix.y % 4u) * 4u + (pix.x % 4u)]) + 0.5) / 16.0;
}

// Interleaved-gradient noise threshold in (0,1) — a finer, less grid-like dither
// than 4×4 Bayer, well suited to the very slow gradients of distance fog.
fn ign(pix: vec2<u32>) -> f32 {
    let p = vec2<f32>(f32(pix.x), f32(pix.y));
    return fract(52.9829189 * fract(dot(p, vec2<f32>(0.06711056, 0.00583715))));
}

// The sun-shadow multiplier for the DIRECTIONAL light at `p` (screen pixel `pix`
// drives the optional dither): vec3(1) when lit, darkening toward the shadow
// tint with the configured strength when occluded. Multiplies the directional
// diffuse + specular only — ambient and point lights are unshadowed fill.
// Direction TO the key light from `p` (camera-relative). `light_dir.w` picks
// the model: 0 = classic directional sun (xyz = one global direction), 1 = a
// POSITIONAL star (xyz = the star's camera-relative position) — light then
// radiates from that point, so terminators and shadow directions line up
// radially the way a real sun's do. In STARS mode the editor also writes the
// brightest star here, so single-light consumers (atmosphere daylight, sky
// glow) keep working unchanged.
fn sun_dir_at(p: vec3<f32>) -> vec3<f32> {
    if (G.light_dir.w > 0.5) {
        return normalize(G.light_dir.xyz - p);
    }
    return normalize(G.light_dir.xyz);
}

// ---- Stars mode (Lighting `stars`): luminous celestial bodies ARE the key
// lights. Up to 4 reach the uniforms; irradiance falls off with the inverse
// square of the distance (capped near the star), so far sides of planets go
// genuinely dark and a second sun genuinely double-lights.
fn star_dir_at(i: u32, p: vec3<f32>) -> vec3<f32> {
    return normalize(G.star_pos[i].xyz - p);
}

fn star_col_at(i: u32, p: vec3<f32>) -> vec3<f32> {
    let sv = G.star_pos[i].xyz - p;
    let d2 = max(dot(sv, sv), 1.0);
    return G.star_color[i].rgb * min(G.star_color[i].w / d2, 4.0);
}

// The shadow march's retro post (quantize bands + Bayer dither) + tint mix,
// shared by the legacy sun shadow and the per-star shadows.
fn shadow_post(vis_in: f32, pix: vec2<u32>) -> vec3<f32> {
    var vis = vis_in;
    let bands = G.shadow_tint.w;
    if (bands >= 2.0) {
        var v = vis * (bands - 1.0);
        if (G.shadow_extra.x > 0.5) {
            v = floor(v + bayer4(pix));
        } else {
            v = round(v);
        }
        vis = clamp(v / (bands - 1.0), 0.0, 1.0);
    }
    return mix(vec3<f32>(1.0), G.shadow_tint.rgb, G.shadow_params.z * (1.0 - vis));
}

// Marched shadow toward star `i`.
fn star_shadow(i: u32, p: vec3<f32>, n: vec3<f32>, pix: vec2<u32>) -> vec3<f32> {
    if (G.shadow_params.x < 0.5) {
        return vec3<f32>(1.0);
    }
    return shadow_post(light_vis(p, n, star_dir_at(i, p)), pix);
}

// The full key-light response at a point: Σ over stars (or the one legacy
// light) of color·NdotL·shadow, plus the matching Blinn-Phong specular energy
// for a surface with `shininess`. Every lit surface — raster meshes, .flsl
// materials, raymarched terrain/blobs/shapes — shades through this, so a new
// light model lands everywhere at once.
struct KeyLight {
    diffuse: vec3<f32>,
    spec: vec3<f32>,
}

fn key_light(p: vec3<f32>, n: vec3<f32>, v: vec3<f32>, shininess: f32, pix: vec2<u32>) -> KeyLight {
    var out: KeyLight;
    out.diffuse = vec3<f32>(0.0);
    out.spec = vec3<f32>(0.0);
    let ns = u32(G.star_meta.x);
    if (ns == 0u) {
        let l = sun_dir_at(p);
        let ndl = max(dot(n, l), 0.0);
        var sh = vec3<f32>(1.0);
        if (ndl > 0.0) {
            sh = sun_shadow(p, n, pix);
        }
        out.diffuse = G.light_color.rgb * ndl * sh;
        let h = normalize(l + v);
        let sp = pow(max(dot(n, h), 0.0), shininess) * select(0.0, 1.0, ndl > 0.0);
        out.spec = G.light_color.rgb * sp * sh;
        return out;
    }
    for (var i = 0u; i < min(ns, 4u); i++) {
        let l = star_dir_at(i, p);
        let scol = star_col_at(i, p);
        let ndl = max(dot(n, l), 0.0);
        var sh = vec3<f32>(1.0);
        if (ndl > 0.0) {
            sh = star_shadow(i, p, n, pix);
        }
        out.diffuse += scol * ndl * sh;
        let h = normalize(l + v);
        let sp = pow(max(dot(n, h), 0.0), shininess) * select(0.0, 1.0, ndl > 0.0);
        out.spec += scol * sp * sh;
    }
    return out;
}

fn sun_shadow(p: vec3<f32>, n: vec3<f32>, pix: vec2<u32>) -> vec3<f32> {
    if (G.shadow_params.x < 0.5) {
        return vec3<f32>(1.0);
    }
    let l = sun_dir_at(p);
    var vis = light_vis(p, n, l);
    // Retro styling: posterize the penumbra into N bands; Bayer-dither between
    // adjacent bands when dither is on (quantize 2 + dither ≈ the PS1 edge).
    let bands = G.shadow_tint.w;
    if (bands >= 2.0) {
        var v = vis * (bands - 1.0);
        if (G.shadow_extra.x > 0.5) {
            v = floor(v + bayer4(pix));
        } else {
            v = round(v);
        }
        vis = clamp(v / (bands - 1.0), 0.0, 1.0);
    }
    // Full shadow multiplies the sun toward `tint` (black = plain darkness, a
    // color = tinted "transparent" shadows), scaled by how dark shadows may get.
    return mix(vec3<f32>(1.0), G.shadow_tint.rgb, G.shadow_params.z * (1.0 - vis));
}
