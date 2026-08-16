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
    // Volumetric fog (Lighting.fog_volumetric): a = (density, layer top WORLD y,
    // top falloff, noise amount), b = (noise scale, time, camera WORLD y, on).
    vol_fog_a: vec4<f32>,
    vol_fog_b: vec4<f32>,
    // Baked GI (Matter::LightProbes). Appended at the END so this struct stays
    // byte-identical to the Rust `RaymarchGlobals`.
    gi_meta: vec4<f32>,     // x on, y leak (× spacing), z normal bias (× spacing), w min spacing
    gi_dims: vec4<f32>,     // xyz probe counts
    gi_center: vec4<f32>,   // xyz camera-relative volume center
    gi_half: vec4<f32>,     // xyz volume half-extent
    // Volumetric light injection. Appended at the END so this struct stays
    // byte-identical to the Rust `RaymarchGlobals`.
    // x = amount (0 = the flat fog colour, i.e. exactly the pre-injection look),
    // y = phase anisotropy g (+ = forward, blooms around the sun),
    // z = march steps, w = march the sun shadow at each step (0/1) — the shafts.
    vol_fog_c: vec4<f32>,
    // Area lights: each point light's EMITTER shape and orientation. Appended at
    // the END so this struct stays byte-identical to the Rust `RaymarchGlobals`.
    point_shape: array<vec4<f32>, 16>, // [kind, a, b, flags] — see `area_terms`
    point_rot: array<vec4<f32>, 16>,   // world orientation (xyzw quaternion)
    // Contact shadows: x = on, y = reach in world units, z = steps, w = strength.
    contact: vec4<f32>,
    // Screen-space reflections. Appended at the END so this struct stays
    // byte-identical to the Rust `RaymarchGlobals`.
    //
    // `ssr_prev_vp` maps a point in THIS frame's camera-relative space into the
    // stored scene-colour picture's clip space — the previous view-projection
    // pre-translated by how far the camera moved, because the world is
    // camera-relative and the raw matrix would be about the origin rather than
    // about the scene. See `SceneHistory::prev_view_proj`.
    ssr_prev_vp: mat4x4<f32>,
    // x = on (0/1 — also 0 when there is no stored picture yet), y = how far a
    // reflected ray reaches in world units, z = march steps, w = how thick a
    // surface is assumed to be when deciding whether the ray went behind it or
    // through it.
    ssr: vec4<f32>,
    // Local (point-light) shadows: x = march steps, y = how dark one gets.
    // Whether a given lamp casts at all is its own flag in `point_shape[i].w` —
    // these two are the scene-wide quality knob the Lighting node owns, so a
    // project tunes cost in one place rather than on every lamp it ever places.
    point_steps: vec4<f32>,
    // Reflection probes (Matter::ReflectionProbe). Appended at the END so this
    // struct stays byte-identical to the Rust `RaymarchGlobals`.
    //
    // `probe_meta.y` is the reflection clamp — the most one screen-space bounce
    // may carry. It lives here rather than in `ssr` because that vector is full,
    // and because it is the same question the probes answer: how much of the
    // scene is a reflection allowed to be.
    //
    // `probe_meta.x` is the live count. Each probe is a captured picture of its
    // surroundings PLUS the box it was captured in: the box is what makes a
    // reflected wall land on the wall — an environment map on its own is a
    // picture at infinity and slides with the camera — and it is also the region
    // the probe covers. `probe_pos.w` is intensity; `probe_half.w` is how far
    // outside the box the probe fades before the sky takes over.
    probe_meta: vec4<f32>,
    probe_pos: array<vec4<f32>, 4>,
    probe_half: array<vec4<f32>, 4>,
    // Each lamp's CONE: x = cos(half angle where it reaches zero), y = cos(half
    // angle where it is still at full brightness. **x = -1 is no cone**, which
    // is every light authored before spots existed. Appended at the END so this
    // struct stays byte-identical to the Rust `RaymarchGlobals`.
    point_cone: array<vec4<f32>, 16>,
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

// Volumetric fog media density at camera-relative `p`: a layer filling world
// space below `vol_fog_a.y`, its top softened over `vol_fog_a.z` units and
// broken up by drifting value-noise (reusing the atmosphere's cloud_fbm).
// The fog's noise, at the detail a step of `step` world units can actually
// resolve.
//
// **An octave finer than the sample spacing is not detail, it is aliasing.** The
// march takes a fixed number of steps between the camera and the surface, so a
// step is short up close and long far away — and the far steps were sampling
// three octaves of noise whose finest features were many times smaller than the
// gap between samples. That is the most expensive part of the fog buying the one
// thing fog must not have: a crawl.
//
// A dropped octave contributes its MEAN rather than nothing. Truncating outright
// would make the fog visibly thin with distance, which is a worse artefact than
// the detail being saved.
fn cloud_fbm_lod(p: vec3<f32>, step: f32) -> f32 {
    // Octave i has features 1/2.13^i across; it is worth sampling while those
    // are wider than a step. log(2.13) ≈ 0.7561.
    let octaves = i32(clamp(log(max(1.0 / max(step, 1e-6), 1.0)) / 0.7561, 0.0, 2.0)) + 1;
    var v = 0.0;
    var amp = 0.55;
    var q = p;
    for (var i = 0; i < 3; i = i + 1) {
        if (i < octaves) {
            v += amp * vnoise(q);
        } else {
            v += amp * 0.5;
        }
        q = q * 2.13 + vec3<f32>(11.7, 5.1, 7.3);
        amp *= 0.5;
    }
    return v;
}

// The fog's smooth part: how thick the layer is at this height. Cheap, and
// wanted at every single step — it is what gives a fog layer an edge.
fn vol_fog_layer(p: vec3<f32>) -> f32 {
    let world_y = p.y + G.vol_fog_b.z; // camera-relative → world height
    let falloff = max(G.vol_fog_a.z, 1e-3);
    // 1 inside the layer, fading to 0 across the falloff band above its top.
    let layer = clamp(1.0 - (world_y - G.vol_fog_a.y) / falloff, 0.0, 1.0);
    return G.vol_fog_a.x * layer;
}

// The fog's lumpy part, as a multiplier on the layer. 1 when noise is off.
//
// This is the expensive half — three octaves of value noise, eight hashes each —
// and unlike the layer it does NOT want sampling at every step. See
// [`fog_noise_stride`].
fn vol_fog_noise(p: vec3<f32>, step: f32) -> f32 {
    let noise_amt = G.vol_fog_a.w;
    if (noise_amt <= 0.0) {
        return 1.0;
    }
    let scale = max(G.vol_fog_b.x, 1e-3);
    let drift = vec3<f32>(G.vol_fog_b.y * 0.6, G.vol_fog_b.y * 0.13, G.vol_fog_b.y * 0.45);
    let n = cloud_fbm_lod(p / scale + drift, step / scale);
    return mix(1.0, clamp(n * 1.6, 0.0, 1.0), noise_amt);
}

// How many consecutive steps one noise sample may stand in for.
//
// **The march was sampling a smooth field forty times per feature.** The noise is
// a fixed number of world units across — tens of metres in a typical scene — and
// a step is a fraction of a metre, so consecutive samples differed by far less
// than the dither the march already applies on purpose. Holding a sample for a
// run of steps costs nothing visible and is most of what the noise was charging
// for. Sampling at least eight times across one feature is the fence; a ray
// shorter than a single feature needs one sample and gets it.
fn fog_noise_stride(dt: f32) -> i32 {
    if (G.vol_fog_a.w <= 0.0) {
        return 1;
    }
    let scale = max(G.vol_fog_b.x, 1e-3);
    return i32(clamp(floor(scale / max(dt, 1e-4) * 0.125), 1.0, 8.0));
}

// ---- Area lights ---------------------------------------------------------------
//
// Every placeable light carries the SHAPE it emits from, and `Point` is the
// zero-size case: with no size the whole of this collapses to the plain
// `max(dot(n,l),0)` × range falloff a point light always had — numerically, not
// just in spirit, so a scene of point lights shades exactly as it did.
//
// Two things come out, wanted in two different places and honest about being
// different in kind:
//
//   `ndl`   — what replaces `N·L` for DIFFUSE. Its DIRECTION is exact: the
//             polygon's vector irradiance `w = (1/2π) Σ θᵢ ûᵢ` is linear in the
//             normal, so one loop over the edges gives the direction the emitter
//             actually lights from — which for a wide rect standing close is not
//             the direction of its centre. The terminator is then softened by the
//             emitter's apparent size, and THAT part is a fit, not a derivation.
//
//   `l`,    — where the specular highlight should think the light is: the point
//   `spread`  on the emitter closest to the mirror direction, and how far to
//             smear the lobe. This is the "representative point" approximation.
//             It is why a bar light streaks and a window light broadens, and it
//             is not energy-exact.
//
//   `dist`  — the distance for range falloff, measured to the emitter's SURFACE.
//             A three-metre bar whose centre is out of range still has an end
//             beside you.

struct AreaTerms {
    ndl: f32,
    l: vec3<f32>,
    dist: f32,
    spread: f32,
    // How much of the lamp reaches here at all, 0–1: the CONE, for a lamp that
    // is aimed. 1 for every omnidirectional light, which is what makes a spot
    // cost nothing to a scene that has none. See `spot_atten`.
    atten: f32,
};

fn quat_rot(q: vec4<f32>, v: vec3<f32>) -> vec3<f32> {
    return v + 2.0 * cross(q.xyz, cross(q.xyz, v) + q.w * v);
}

// One edge's contribution to a polygon's vector irradiance: the angle it
// subtends, along the normal of the wedge it sweeps.
fn edge_vector(a: vec3<f32>, b: vec3<f32>) -> vec3<f32> {
    let cr = cross(a, b);
    let len = length(cr);
    if (len < 1e-7) {
        return vec3<f32>(0.0); // degenerate: the corners coincide from here
    }
    return (cr / len) * acos(clamp(dot(a, b), -1.0, 1.0));
}

// The terminator softened by how big the emitter looks from here. `s` is its
// apparent angular half-size; at 0 this is exactly `max(ndl, 0)`, so a point
// light is not a special case that has to be remembered.
fn wrap_ndl(ndl: f32, s: f32) -> f32 {
    return clamp((ndl + s) / (1.0 + s), 0.0, 1.0);
}

// The point on the segment `c ± ax·half` closest to the ray `ro + rd·t`, as a
// position. Used for a tube's representative point.
fn closest_on_segment_to_ray(c: vec3<f32>, ax: vec3<f32>, half: f32, rd: vec3<f32>) -> vec3<f32> {
    // Ray starts at the origin (everything here is relative to the shading point).
    let d = dot(ax, rd);
    let denom = 1.0 - d * d;
    var t = 0.0;
    if (abs(denom) > 1e-5) {
        t = (dot(c, rd) * d - dot(c, ax)) / denom;
    }
    return c + ax * clamp(t, -half, half);
}

// The emitter lane's `w` is a BITMASK, not a boolean: bit 0 = the emitter is
// two-sided, bit 1 = this light casts shadows. It began as a plain 0/1 for
// two-sidedness, and the moment a second flag joined it every `w > 0.5` test
// became a test for "any flag at all" — which would have made every
// shadow-casting disk silently two-sided.
const LIGHT_TWO_SIDED: u32 = 1u;
const LIGHT_SHADOWS: u32 = 2u;

fn light_flag(shape: vec4<f32>, bit: u32) -> bool {
    return (u32(max(shape.w, 0.0) + 0.5) & bit) != 0u;
}

// How much of an aimed lamp reaches a point in direction `to` (which points
// from the shading point AT the light).
//
// `cone.x` is the cosine of the half angle where it reaches zero and `cone.y`
// the cosine of the half angle where it is still full. **`cone.x < -0.999` means
// no cone**, and returns 1 immediately — the branch every light in every scene
// authored before spots existed takes.
//
// Cosines, so this is two dots and a smoothstep with no trigonometry per pixel.
// Note the sense: a NARROWER cone has a LARGER cosine, so `x` (the outer edge)
// is the smaller number and `smoothstep(x, y, c)` runs the right way round.
//
// Squared at the end because a linear ramp across the penumbra reads as a
// visible ring — the eye finds the discontinuity in the *slope*, and squaring
// puts a soft shoulder on both ends of the band for one multiply.
fn spot_atten(cone: vec4<f32>, rot: vec4<f32>, to: vec3<f32>) -> f32 {
    if (cone.x < -0.999) {
        return 1.0;
    }
    // The lamp points down its own -Z, the same axis a camera looks down.
    let aim = quat_rot(rot, vec3<f32>(0.0, 0.0, -1.0));
    // `to` points at the light, so the direction the light travels to get here
    // is its negation. Geometric, and deliberately NOT the representative point
    // an area emitter computes: a cone edge that moved as the camera did would
    // shimmer along every wall it lands on.
    let c = dot(aim, -normalize(to));
    let t = clamp((c - cone.x) / max(cone.y - cone.x, 1e-4), 0.0, 1.0);
    return t * t;
}

fn area_terms(shape: vec4<f32>, rot: vec4<f32>, cone: vec4<f32>, to: vec3<f32>, n: vec3<f32>, v: vec3<f32>) -> AreaTerms {
    var out: AreaTerms;
    out.atten = spot_atten(cone, rot, to);
    let d0 = max(length(to), 1e-4);
    out.l = to / d0;
    out.dist = d0;
    out.spread = 0.0;
    out.ndl = max(dot(n, out.l), 0.0);
    let kind = i32(shape.x + 0.5);
    if (kind == 0) {
        return out; // a point: nothing above this line needed changing
    }
    // The mirror direction — where the highlight wants to find the emitter.
    let r = reflect(-v, n);

    if (kind == 1 || kind == 3) {
        // SPHERE (1) and DISK (3). Both light from their centre and both soften
        // the terminator by their apparent size. The disk additionally has a
        // BACK: a surface behind it gets nothing, and the closer to edge-on you
        // stand the less of it you see, which is the `front` term.
        let radius = max(shape.y, 1e-4);
        out.spread = clamp(radius / d0, 0.0, 1.0);
        var vis = 1.0;
        if (kind == 3) {
            // `to` points AT the light, so a point on the emitting side sees the
            // emitter's forward (-Z) running back toward it.
            let front = dot(quat_rot(rot, vec3<f32>(0.0, 0.0, -1.0)), -out.l);
            vis = select(max(front, 0.0), abs(front), light_flag(shape, LIGHT_TWO_SIDED));
            if (vis <= 0.0) {
                out.ndl = 0.0;
                return out;
            }
        }
        out.ndl = wrap_ndl(dot(n, out.l), out.spread) * vis;
        // Representative point: the point on the sphere nearest the mirror ray.
        let centre_to_ray = r * dot(to, r) - to;
        let closest = to + centre_to_ray * clamp(radius / max(length(centre_to_ray), 1e-4), 0.0, 1.0);
        out.l = normalize(closest);
        out.dist = max(d0 - radius, 1e-3);
        return out;
    }

    if (kind == 2) {
        // RECT. The four corners, relative to the shading point.
        let ax = quat_rot(rot, vec3<f32>(1.0, 0.0, 0.0)) * max(shape.y, 1e-4);
        let ay = quat_rot(rot, vec3<f32>(0.0, 1.0, 0.0)) * max(shape.z, 1e-4);
        let face = normalize(cross(ax, ay)); // the node's +Z; forward is -Z
        let front = dot(face, out.l); // > 0 ⇒ the point is on the emitting side
        if (front <= 0.0 && !light_flag(shape, LIGHT_TWO_SIDED)) {
            out.ndl = 0.0;
            return out;
        }
        let p0 = normalize(to - ax - ay);
        let p1 = normalize(to + ax - ay);
        let p2 = normalize(to + ax + ay);
        let p3 = normalize(to - ax + ay);
        let w = edge_vector(p0, p1) + edge_vector(p1, p2) + edge_vector(p2, p3) + edge_vector(p3, p0);
        let wl = length(w);
        if (wl < 1e-6) {
            out.ndl = 0.0;
            return out;
        }
        // The direction the emitter actually lights from — exact, and not the
        // direction of its centre once it is wide and close.
        //
        // The polygon integral is signed by WINDING, so seen from behind a
        // two-sided emitter it comes out pointing away from the light. Orienting
        // it toward the emitter fixes that, and is true of vector irradiance in
        // general: light arrives FROM the light.
        let dir = normalize(w * select(-1.0, 1.0, dot(w, to) > 0.0));
        let half = max(shape.y, shape.z);
        out.spread = clamp(half / d0, 0.0, 1.0);
        out.ndl = wrap_ndl(dot(n, dir), out.spread);
        // Range falloff measures to the emitter's nearest point, and that has to
        // stay VIEW-INDEPENDENT — a light that dims as you walk around it
        // without moving is not a light anybody can place.
        let nu = clamp(dot(-to, ax) / max(dot(ax, ax), 1e-6), -1.0, 1.0);
        let nv = clamp(dot(-to, ay) / max(dot(ay, ay), 1e-6), -1.0, 1.0);
        out.dist = max(length(to + ax * nu + ay * nv), 1e-3);
        // Representative point: where the mirror ray meets the emitter's plane,
        // pinned inside its extents. This one IS view-dependent — that is the
        // whole idea, and it drives the highlight only.
        let denom = dot(r, face);
        var rp = to;
        if (abs(denom) > 1e-4) {
            let t = dot(to, face) / denom;
            if (t > 0.0) {
                let hit = r * t - to; // the plane hit, in the emitter's own frame
                let u = clamp(dot(hit, ax) / max(dot(ax, ax), 1e-6), -1.0, 1.0);
                let vv = clamp(dot(hit, ay) / max(dot(ay, ay), 1e-6), -1.0, 1.0);
                rp = to + ax * u + ay * vv;
            }
        }
        out.l = normalize(rp);
        return out;
    }

    // TUBE (4): a capsule along the node's local X. It lights from the nearest
    // point on its line, which is what makes a long bar wrap a wall the way a
    // point at its centre never would.
    let ax = quat_rot(rot, vec3<f32>(1.0, 0.0, 0.0));
    let half = max(shape.y, 1e-4);
    let radius = max(shape.z, 1e-4);
    // The point on the tube's AXIS nearest the shading point (which is the
    // origin here): the segment is `to + ax·t`, so |to + ax·t|² is least at
    // t = -to·ax, pinned to the tube's own length.
    let cp = to + ax * clamp(-dot(to, ax), -half, half);
    let cd = max(length(cp), 1e-4);
    // A bar lights from its length as well as its thickness — standing beside a
    // three-metre strip, light arrives from a wide arc, not from one spot.
    out.spread = clamp((radius + half * 0.5) / cd, 0.0, 1.0);
    out.ndl = wrap_ndl(dot(n, cp / cd), out.spread);
    // Representative point: the point on the axis nearest the mirror ray, pulled
    // out to the tube's surface toward us — which is what streaks the highlight
    // along the bar instead of pinning it to the middle.
    let rep = closest_on_segment_to_ray(to, ax, half, r);
    let rl = max(length(rep), 1e-4);
    out.l = normalize(rep * (1.0 - min(radius / rl, 0.9)));
    out.dist = max(cd - radius, 1e-3);
    return out;
}

// ---- Volumetric light injection ------------------------------------------------
//
// Henyey-Greenstein phase, NORMALISED so isotropic (g = 0) reads 1.0 instead of
// the physical 1/4π. Everything else in this engine is lit in "a surface facing
// the light reads 1" units; a phase arriving at 0.08 would make the amount
// slider mean something different from every other lighting knob.
fn fog_phase(cos_t: f32, g_in: f32) -> f32 {
    let g = clamp(g_in, -0.95, 0.95);
    let g2 = g * g;
    let d = max(1.0 + g2 - 2.0 * g * cos_t, 1e-4);
    return (1.0 - g2) / pow(d, 1.5);
}

// The light scattering back toward the camera at camera-relative `p`, for a ray
// travelling along `rd`. `amb` is the ambient/bounce term, sampled ONCE per ray
// by the caller: a probe fetch is 32 texture loads, which per step would cost
// more than the shadow march it sits next to, and the bounce varies far more
// slowly along a ray than the media does.
//
// There is no N·L here and that is not an omission — a mote of fog has no
// facing. What replaces it is the phase function, which is why the anisotropy
// knob does the work the surface normal does everywhere else.
// How far an emitter reaches past its own centre — its radius, or a rect's
// larger half-extent. Conservative on purpose: it only ever widens a rejection
// test.
fn fog_extent(shape: vec4<f32>) -> f32 {
    if (i32(shape.x + 0.5) == 0) {
        return 0.0; // a point has no size
    }
    return max(max(shape.y, shape.z), 0.0);
}

// What a lamp is as far as the AIR is concerned: a direction and a distance.
//
// **Fog was calling the full area-light model and throwing nearly all of it
// away.** There is no surface out here — no `ndl` to integrate over an emitter's
// solid angle, no mirror direction to find a representative point for — and the
// only two numbers used were the ones that come out at the top of it. A rect
// emitter costs four quaternion rotations and an edge integral to produce them,
// and that was being paid per light, per step, per pixel: sixteen steps and four
// ceiling panels is sixty-four of them for one pixel of air.
//
// Softening the distance by the emitter's own size is the one part worth
// keeping — the air beside a long strip light should be lit by the strip and not
// by a point at its middle — and it is one subtraction.
struct FogEmitter {
    l: vec3<f32>,
    dist: f32,
};

fn fog_emitter(shape: vec4<f32>, to: vec3<f32>) -> FogEmitter {
    var o: FogEmitter;
    let d0 = max(length(to), 1e-4);
    o.l = to / d0;
    o.dist = max(d0 - fog_extent(shape), 1e-3);
    return o;
}

fn fog_inscatter(p: vec3<f32>, rd: vec3<f32>, amb: vec3<f32>, pix: vec2<u32>) -> vec3<f32> {
    let g = G.vol_fog_c.y;
    // Shafts cost one shadow march per step per pixel — the single most
    // expensive thing in the fog, and the only thing that draws the beam.
    let march = G.vol_fog_c.w > 0.5 && G.shadow_params.x > 0.5;
    var lit = amb;
    let ns = u32(G.star_meta.x);
    // A key light emitting nothing lights no air either — every interior turns
    // the sun down, so this is the common case rather than the odd one.
    //
    // **It skips the SUN, not the function.** Written as an early return it also
    // skipped the placeable lamps below, and an interior lit entirely by its own
    // ceiling panels — which is every interior — got no light in its air at all.
    let sun_on = dot(G.light_color.rgb, G.light_color.rgb) > 1e-8;
    if (ns == 0u) {
        if (sun_on) {
            let l = sun_dir_at(p);
            var sh = vec3<f32>(1.0);
            if (march) {
                // n = l: no surface to lift off, so the normal-offset term
                // degenerates to a small nudge along the ray, which is exactly
                // what is wanted.
                sh = shadow_post(light_vis(p, l, l), pix);
            }
            lit += G.light_color.rgb * sh * fog_phase(dot(rd, l), g);
        }
    } else {
        for (var i = 0u; i < min(ns, 4u); i++) {
            let l = star_dir_at(i, p);
            var sh = vec3<f32>(1.0);
            if (march) {
                sh = shadow_post(light_vis(p, l, l), pix);
            }
            lit += star_col_at(i, p) * sh * fog_phase(dot(rd, l), g);
        }
    }
    // Placeable point lights: no march (they are unshadowed fill everywhere else
    // in the engine too), so a lamp costs a distance and a phase.
    let pc = min(u32(G.point_count.x), 16u);
    for (var i = 0u; i < pc; i = i + 1u) {
        let range = max(G.point_pos[i].w, 1e-4);
        let to = G.point_pos[i].xyz - p;
        // **Out of range before anything is computed.** This test used to sit
        // after the emitter evaluation, so a lamp at the far end of a level was
        // fully evaluated and then multiplied by zero — once per light, per
        // step, per pixel. Widened by the emitter's own size so it can only ever
        // reject a lamp the evaluation would have rejected too.
        let reach = range + fog_extent(G.point_shape[i]);
        if (dot(to, to) > reach * reach) {
            continue;
        }
        let e = fog_emitter(G.point_shape[i], to);
        let x = clamp(1.0 - e.dist / range, 0.0, 1.0);
        if (x <= 0.0) {
            continue;
        }
        lit += G.point_color[i].rgb * (x * x) * fog_phase(dot(rd, e.l), g);
    }
    return lit;
}

// One volumetric-fog ray: single scattering from the camera to `t_max` along
// `rd`. The caller composites `behind * transmittance + scattered`.
//
// With the amount at 0 the in-scattered radiance is the constant fog colour, and
// then `scattered = fog_color * (1 - transmittance)` exactly, so the composite
// collapses to `mix(behind, fog_color, 1 - T)` — the same expression the flat
// volumetric fog used, not an approximation of it.
struct FogMarch {
    scattered: vec3<f32>,
    transmittance: f32,
};

fn fog_march(rd: vec3<f32>, t_max: f32, pix: vec2<u32>) -> FogMarch {
    var out: FogMarch;
    out.scattered = vec3<f32>(0.0);
    out.transmittance = 1.0;
    if (t_max <= 0.0) {
        return out;
    }
    let steps = u32(clamp(G.vol_fog_c.z, 2.0, 64.0));
    let dt = t_max / f32(steps);
    let jitter = ign(pix); // per-pixel start offset — the banding hider
    let amt = clamp(G.vol_fog_c.x, 0.0, 1.0);
    let gain = max(G.vol_fog_c.x, 1.0); // past 1 the slider brightens rather than blends
    // "Show only the bounce" (the LightProbes tuning view) switches the injected
    // direct light off here, the same way `key_light` does for surfaces.
    let lit_on = amt > 0.0 && G.gi_meta.y < 0.5;
    var amb = G.ambient.rgb;
    if (lit_on) {
        amb = gi_ambient(rd * (t_max * 0.5), -rd, G.ambient.rgb);
    }
    let stride = fog_noise_stride(dt);
    var noise = 1.0;
    for (var i = 0u; i < steps; i = i + 1u) {
        let p = rd * ((f32(i) + jitter) * dt);
        let layer = vol_fog_layer(p);
        if (layer <= 1e-5) {
            continue;
        }
        if (i32(i) % stride == 0) {
            noise = vol_fog_noise(p, dt * f32(stride));
        }
        let sigma = layer * noise;
        if (sigma <= 1e-5) {
            continue;
        }
        var radiance = G.fog_color.rgb;
        if (lit_on) {
            // The fog colour is the media's ALBEDO once light is injected: what
            // it scatters is the light that reaches it, tinted by its own colour.
            radiance = G.fog_color.rgb * mix(vec3<f32>(1.0), fog_inscatter(p, rd, amb, pix) * gain, amt);
        }
        // The fraction of this slab's light that gets out, attenuated by
        // everything already in front of it.
        let a = 1.0 - exp(-sigma * dt);
        out.scattered += out.transmittance * a * radiance;
        out.transmittance *= 1.0 - a;
        // Nothing behind a fog this thick is going to be seen, and neither is
        // anything scattered further along the ray. Dense fog is the case that
        // marches most and gains most.
        if (out.transmittance < 0.003) {
            out.transmittance = 0.0;
            break;
        }
    }
    return out;
}

// Volumetric fog over a ray that hit NOTHING. The depth ramp deliberately leaves
// the sky crisp — it is a stylistic distance ramp, not a medium — but a fog
// LAYER is something the ray really does pass through on the way out of the
// world, and leaving it out is what put a hard seam at the horizon and hid every
// shaft that had sky behind it.
fn fog_sky(color: vec3<f32>, rd: vec3<f32>, pix: vec2<u32>) -> vec3<f32> {
    if (G.fog_params.z < 0.5 || G.vol_fog_b.w < 0.5) {
        return color;
    }
    var t_max = max(G.fog_params.y, 1.0); // the "max distance" fence
    if (rd.y > 1e-3) {
        // An upward ray LEAVES the layer at a known height, so march to there
        // and no further — most sky pixels cost a fraction of the fence.
        let top = G.vol_fog_a.y + G.vol_fog_a.z;
        t_max = min(t_max, max((top - G.vol_fog_b.z) / rd.y, 0.0));
    }
    let m = fog_march(rd, t_max, pix);
    return color * m.transmittance + m.scattered;
}

fn apply_fog(color: vec3<f32>, pos: vec3<f32>, pix: vec2<u32>) -> vec3<f32> {
    // Aerial perspective first: atmosphere + clouds BETWEEN the camera and this
    // surface (haze over a planet seen from orbit, cloud decks over its disc).
    let color2 = atmo_composite(color, pos, length(pos), false);
    if (G.fog_params.z < 0.5) {
        return color2;
    }
    // Optional dither to break up 8-bit banding on slow gradients. Strength rides
    // in the spare fog_color.w lane (0 = off); mode in fog_params.w (0 = Bayer
    // 4×4, 1 = interleaved-gradient noise). A sub-percent nudge is enough.
    let amp = G.fog_color.w;
    let dith = select(bayer4(pix), ign(pix), G.fog_params.w > 0.5);
    if (G.vol_fog_b.w > 0.5) {
        // VOLUMETRIC: march camera → surface, scattering light in as it goes.
        let dist = length(pos);
        let m = fog_march(pos / max(dist, 1e-4), dist, pix);
        // The dither scales the whole result rather than nudging the blend
        // factor: a clear pixel has nothing to break up, and an additive nudge
        // there would darken it (there is no fog colour left to mix toward once
        // the scattered term carries the colour).
        var k = 1.0;
        if (amp > 0.0) {
            k = max(1.0 + (dith - 0.5) * amp * 0.12, 0.0);
        }
        let f = clamp((1.0 - m.transmittance) * k, 0.0, 1.0);
        return color2 * (1.0 - f) + m.scattered * k;
    }
    let denom = max(G.fog_params.y - G.fog_params.x, 1e-4);
    var f = clamp((length(pos) - G.fog_params.x) / denom, 0.0, 1.0);
    if (amp > 0.0) {
        f = clamp(f + (dith - 0.5) * amp * 0.06, 0.0, 1.0);
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
// How far a shadow ray gets before something in the FIELD stops it: terrain,
// blobs, Field Shapes, baked mesh occluder volumes and collider proxies. 1 =
// nothing in the way, 0 = fully blocked, in between = penumbra.
//
// Split out of `light_vis` so a LAMP can march the same set (`point_field_vis`).
// The sun and a lamp differ in exactly four numbers — where the ray ends, how
// soft the penumbra is, and the two lift constants — and in nothing else, so
// two copies of this march would be two copies that drift. What it buys a lamp
// is the half a screen-space trace structurally cannot do: **an occluder the
// camera cannot see**. A wall behind you is not in the depth buffer and is very
// much in the field.
fn field_vis(ro: vec3<f32>, l: vec3<f32>, max_d: f32, k: f32, pen_t0: f32, lift: f32, steps: i32) -> f32 {
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
    // Everything the sweep found was this fragment's OWN proxy, and the field
    // holds nothing else — so there is genuinely nothing to march and the ray
    // is lit. Without this the march below runs with no occluder in it, the
    // field answers "empty" on the first sample, and the out-of-steps rule at
    // the bottom reads that as fully shadowed. It only bites in a scene with no
    // terrain, no blobs and no baked volumes — a plane and a character, which
    // is what an ordinary game scene is and what every probe here had a hill in
    // front of.
    if (vmask == 0u && !blobs && !shapes && march == 0u) {
        return 1.0;
    }
    var t = lift;
    var vis = 1.0;
    // Why the march ended, because the two endings mean opposite things and the
    // rule at the bottom cannot tell them apart from `t` alone.
    var escaped = false; // left everything that could have occluded it — lit
    var reached = false; // marched past the last relevant bound — keep `vis`
    // The k·d/t penumbra estimator is hypersensitive while t is tiny: right at the
    // start surface, sub-voxel noise in the f16-sampled field (worst on the tapered
    // slab walls) reads as near-occluders and stripes the penumbra. Hard hits are
    // tested from the first step, but penumbra only accumulates once the ray has
    // cleared the start surface's own noise floor (keyed to the UNSCALED lift so
    // ordinary ground keeps tight contact shadows).
    for (var s = 0; s < steps; s = s + 1) {
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
        if (d > 1e8) { escaped = true; break; } // nothing along this ray — fully lit
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
        if (vis < 0.01) { break; }
        if (t > t_end) { reached = true; break; }
    }
    if (!escaped && !reached) {
        // Ran out of steps mid-span. With the growing cap that only happens when
        // d stayed pinned small for every step — the ray spent its whole life
        // hugging matter — so it's occluded, not lit-by-default.
        //
        // Keyed to WHY the loop ended, not to `t < t_end`. A ray that broke out
        // because the field went empty had also not reached `t_end`, so the old
        // test caught it too and turned "nothing in the way" into full shadow —
        // scanlines of black across an open floor.
        vis = 0.0;
    }
    return clamp(vis, 0.0, 1.0);
}

fn light_vis(p: vec3<f32>, n: vec3<f32>, l: vec3<f32>) -> f32 {
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
    return field_vis(p + n * lift, l, G.shadow_params.w, G.shadow_params.y, base * 3.0, lift, 64);
}

// Is lamp `i` blocked by something the CAMERA CANNOT SEE?
//
// The screen-space half of `point_vis` reads the depth prepass, so it only
// knows about surfaces that were drawn — turn away from a wall and the shadow
// it was casting stops existing. This marches the field instead, which is where
// the geometry the camera is not looking at lives: terrain, blobs, the baked
// occluder volume a static collider mesh gets, and every collider proxy. The
// two are combined with `min` in `point_vis`; between them a lamp is blocked by
// what is in front of the camera AND by what is behind it.
//
// **Softness comes from the lamp's own size**, not from a knob. A shadow's
// penumbra is set by how big the light looks from the surface — that is what
// the `k` in the k·d/t estimator means — so a wide sphere lamp close up casts a
// soft edge and a bare point casts a hard one, for free and correctly. The one
// number a designer sets is how carefully to look, and that is the same steps
// knob the screen-space march already uses.
fn point_field_vis(p: vec3<f32>, n: vec3<f32>, i: u32) -> f32 {
    let to = G.point_pos[i].xyz - p;
    let dist = length(to);
    if (dist < 1e-4) {
        return 1.0;
    }
    let l = to / dist;
    let base = max(0.03, max(field_eps(p), shadow_vol_eps(p)) * 1.6);
    let lift = min(base * clamp(0.5 / max(dot(n, l), 0.125), 1.0, 4.0), 3.0);
    // The emitter's radius over the distance to it IS its apparent half-size,
    // and `k` is that reciprocal. A point emitter (radius 0) lands on the clamp
    // and casts the hard edge it should.
    let shape = G.point_shape[i];
    let radius = select(0.0, max(shape.y, 0.0), shape.x > 0.5);
    let k = clamp(dist / max(radius, 1e-3), 2.0, 128.0);
    // Stop SHORT of the lamp. The last stretch is the fixture itself and the
    // surfaces it is mounted on, and marching into them makes a bulb shadow its
    // own bracket — the same reason the screen-space march stops at 98%.
    let reach = min(dist, max(G.point_pos[i].w, 1e-4)) * 0.95;
    // One knob, both halves: "how carefully does this lamp look". The field
    // march wants more steps than the screen one because it covers the whole
    // distance to the light rather than a short trace.
    let steps = i32(clamp(G.point_steps.x * 2.0, 8.0, 64.0));
    return field_vis(p + n * lift, l, reach, k, base * 3.0, lift, steps);
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

// ---- Baked global illumination (Matter::LightProbes) ---------------------------
//
// A lattice of probes over a box, each holding the light arriving from every
// direction as SH-L1. The four coefficients of a probe sit side by side along x
// in one 3D texture (`gi_tex`, declared by each host module), read with
// `textureLoad` only: the eight-probe blend below applies its own weights, and
// hardware trilinear cannot apply a leak test, so there is no filtering to lose.
//
// This is a transliteration of `BakedGi::sample` in the floptle-gi crate, which
// is where the weighting is unit-tested. If you change one, change both — the
// Rust side is the one with the tests that say what "does not leak through a
// wall" means.

fn gi_texel(ix: vec3<i32>, c: i32) -> vec4<f32> {
    return textureLoad(gi_tex, vec3<i32>(ix.x * 4 + c, ix.y, ix.z), 0);
}

// The baked bounce at camera-relative point `p` on a surface facing `n`:
// rgb = the value that multiplies albedo, a = coverage (0 outside the volume,
// fading in over the box's outer tenth so its edge is not a visible seam).
fn gi_bounce(p: vec3<f32>, n: vec3<f32>) -> vec4<f32> {
    if (G.gi_meta.x < 0.5) {
        return vec4<f32>(0.0);
    }
    let sp = max(G.gi_meta.w, 1e-4);
    // Step off the surface first. A shading point sits exactly ON the geometry,
    // which is the one place where "which side of this wall am I on" is
    // genuinely ambiguous; half a cell along the normal is not.
    let bp = p + n * (G.gi_meta.z * sp);
    let h = max(G.gi_half.xyz, vec3<f32>(1e-4));
    let local = (bp - G.gi_center.xyz) / h;
    let m = max(max(abs(local.x), abs(local.y)), abs(local.z));
    let coverage = 1.0 - clamp((m - 0.9) / 0.1, 0.0, 1.0);
    if (coverage <= 0.0) {
        return vec4<f32>(0.0);
    }
    let dims = max(G.gi_dims.xyz, vec3<f32>(2.0));
    let t = clamp(local * 0.5 + 0.5, vec3<f32>(0.0), vec3<f32>(1.0)) * (dims - 1.0);
    let base = floor(t);
    let frac = t - base;

    var c0 = vec3<f32>(0.0);
    var c1 = vec3<f32>(0.0);
    var c2 = vec3<f32>(0.0);
    var c3 = vec3<f32>(0.0);
    var wsum = 0.0;
    for (var k = 0u; k < 8u; k++) {
        let off = vec3<f32>(f32(k & 1u), f32((k >> 1u) & 1u), f32((k >> 2u) & 1u));
        let ixf = min(base + off, dims - 1.0);
        let ix = vec3<i32>(ixf);
        let tri = mix(1.0 - frac.x, frac.x, off.x)
            * mix(1.0 - frac.y, frac.y, off.y)
            * mix(1.0 - frac.z, frac.z, off.z);
        // Where that probe actually is, so the surface can ask whether it is
        // even on the right side to be lighting it.
        let pw = G.gi_center.xyz - h + (ixf / (dims - 1.0)) * 2.0 * h;
        let to = pw - bp;
        let l2 = dot(to, to);
        var dir = n;
        if (l2 > 1e-12) {
            dir = to * inverseSqrt(l2);
        }
        // Wrap shading, squared: a probe behind the surface cannot be lighting
        // it. Softened rather than cut off, so a wall sliding past a probe plane
        // does not pop.
        let facing = max(dot(n, dir) * 0.5 + 0.5, 0.0);
        let wrap = facing * facing + 0.05;
        let e0 = gi_texel(ix, 0);
        // `.w` is the probe's validity, already resolved against the volume's
        // leak setting when the texture was uploaded — a probe with no clearance
        // around it is inside geometry and lights nothing.
        let w = tri * wrap * e0.w;
        if (w > 0.0) {
            c0 += e0.rgb * w;
            c1 += gi_texel(ix, 1).rgb * w;
            c2 += gi_texel(ix, 2).rgb * w;
            c3 += gi_texel(ix, 3).rgb * w;
            wsum += w;
        }
    }
    if (wsum <= 1e-6) {
        return vec4<f32>(0.0);
    }
    // The cosine convolution (π for band 0, 2π/3 for band 1) and the Lambert
    // 1/π, folded into two constants. Clamped, because a truncated SH fit dips
    // negative opposite a strong lobe and negative ambient is a black smear.
    let inv = 1.0 / wsum;
    let e = 0.28209479 * c0 * inv
        + (0.48860251 * 2.0 / 3.0) * (n.x * c1 + n.y * c2 + n.z * c3) * inv;
    return vec4<f32>(max(e, vec3<f32>(0.0)), coverage);
}

// The ambient term a surface actually gets: the baked bounce where a probe
// volume covers it, the scene's flat ambient where none does.
//
// REPLACES rather than adds. A flat ambient is a stand-in for the bounce, so
// keeping both double-counts exactly the light the bake just measured — and the
// tell is that a scene looks *washed out* after baking, which reads as the bake
// being wrong rather than as the old patch still being applied.
fn gi_ambient(p: vec3<f32>, n: vec3<f32>, flat_ambient: vec3<f32>) -> vec3<f32> {
    let gi = gi_bounce(p, n);
    return mix(flat_ambient, gi.rgb, gi.a);
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
    let l = star_dir_at(i, p);
    return shadow_post(min(light_vis(p, n, l), contact_vis(p, n, l, pix)), pix);
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
    // "Show only the bounce" (the LightProbes node's tuning view). Killing the
    // key light HERE rather than at each shading site is what makes it one line:
    // raster meshes, terrain, blobs, field shapes and .flsl materials all shade
    // through this function, so they all go dark together and what is left on
    // screen is exactly the baked light and the things that emit.
    if (G.gi_meta.y > 0.5) {
        return out;
    }
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

// ---- Contact shadows -----------------------------------------------------------
//
// The marched field shadow knows about terrain, blobs, baked level meshes and
// collider PROXIES — and a proxy is a box or a capsule, so a character casts the
// shadow of a capsule. At arm's length that reads as an object floating: the
// contact between a foot and the floor is exactly where the proxy is least like
// the thing it stands for.
//
// This closes that gap from the other end. It marches the opaque depth prepass
// in SCREEN space over a short distance toward the light, so anything on screen
// occludes with its true silhouette, whatever it is made of and however it is
// posed — no proxy, no bake, no second gather. What it cannot do is shadow from
// something off-screen or hidden behind something else, which is why it is a
// short-range companion to the field march and not a replacement for it.
//
// The prepass's own 1×1 fallback is the "no prepass this frame" signal: reading
// the bound texture's size beats a uniform flag that a resize path could forget
// to clear.
fn contact_vis(p: vec3<f32>, n: vec3<f32>, l: vec3<f32>, pix: vec2<u32>) -> f32 {
    if (G.contact.x < 0.5) {
        return 1.0;
    }
    let dims = textureDimensions(prime_tex, 0);
    if (dims.x <= 1u || dims.y <= 1u) {
        return 1.0; // offscreen previews and probes: no prepass, no contact
    }
    let reach = max(G.contact.y, 1e-3);
    let steps = u32(clamp(G.contact.z, 2.0, 32.0));
    let dt = reach / f32(steps);
    // Start off the surface, or every pixel shadows itself. A FIXED lift, not one
    // scaled by the reach: tie it to the reach and turning the reach up lifts the
    // ray's start over the very thing it was meant to find, so the knob stops
    // being monotonic — a longer trace finds LESS.
    let bias = 0.02;
    let ro = p + n * bias;
    // How far behind a visible surface still counts as "inside it".
    //
    // Depth alone cannot tell "I am inside a thick pillar" from "I am in front of
    // a wall on the far side of the room", so this number is the whole judgement
    // call. It scales with the REACH, which is short by design: a trace that only
    // looks 35 cm ahead can afford to believe that anything within 35 cm behind
    // what it crossed is the same object, and that is what lets a solid pillar
    // cast rather than being written off as scenery.
    let thickness = max(reach, dt * 3.0) + 0.05;
    let jitter = ign(pix); // the step offset, so banding becomes noise
    let fdims = vec2<f32>(dims);
    for (var i = 0u; i < steps; i = i + 1u) {
        let q = ro + l * ((f32(i) + jitter) * dt);
        let clip = G.view_proj * vec4<f32>(q, 1.0);
        if (clip.w <= 0.0) {
            break; // behind the eye
        }
        let ndc = clip.xyz / clip.w;
        if (abs(ndc.x) > 1.0 || abs(ndc.y) > 1.0) {
            break; // off screen: no evidence either way, and guessing is worse
        }
        let uv = vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
        let texel = vec2<i32>(clamp(uv * fdims, vec2<f32>(0.0), fdims - vec2<f32>(1.0)));
        let depth = textureLoad(prime_tex, texel, 0).x;
        if (depth >= 1.0) {
            continue; // nothing was drawn here
        }
        // Compare in WORLD distance, not in depth: the depth buffer's units are
        // nonlinear, so a thickness expressed in them means a different number of
        // metres at every distance — which is how a screen-space shadow ends up
        // tuned for one part of a level and broken in the next.
        let sp = G.inv_view_proj * vec4<f32>(ndc.xy, depth, 1.0);
        let surface = sp.xyz / sp.w;
        let behind = length(q) - length(surface);
        // `behind > bias`: the ray has passed BEHIND whatever is drawn here.
        // `behind < thickness`: and not so far behind that it is a different
        // object entirely — without that, a wall in the distance would shadow
        // everything standing in front of it. This pair is the whole art of a
        // screen-space trace: too tight and the shadow breaks into stripes, too
        // loose and every surface shadows the room behind it.
        if (behind > bias && behind < thickness) {
            return 1.0 - clamp(G.contact.w, 0.0, 1.0);
        }
    }
    return 1.0;
}

// Is point light `i` visible from `p`? 1 = lit, 0 = fully shadowed.
//
// **Why a screen-space trace and not the field march the sun uses.** The sun is
// one light, infinitely far away, and its march is bounded by a distance the
// scene sets. A lamp is one of sixteen, sits inside the level, and the thing
// that has to block it is almost always ordinary polygon geometry — a wall, a
// crate, a character — none of which exists in the SDF field at all. The field
// knows terrain, blobs and collider proxies; a room does not.
//
// So this reads the same depth prepass contact shadows do, and it works on the
// real silhouette of whatever is on screen — every bolt and railing of it, at no
// authoring cost. What it cannot do is see an occluder the camera cannot: turn
// away from a wall and the shadow it was casting stops existing.
//
// That half is [`point_field_vis`], which marches the field — where the geometry
// off screen actually lives. The two are combined below with `min`, and the
// division of labour is exact: the screen-space trace has the real silhouette
// and only what is in frame; the field has everything, at the resolution of a
// collider proxy or a baked occluder volume. Neither alone is a local shadow.
//
// The march ends AT THE LIGHT rather than at a fixed reach — a lamp two metres
// away and one twenty metres away need completely different distances, and a
// single tuned number would be wrong for one of them. The step count is fixed,
// so a distant lamp simply samples more coarsely.
fn point_screen_vis(p: vec3<f32>, n: vec3<f32>, i: u32, pix: vec2<u32>) -> f32 {
    let dims = textureDimensions(prime_tex, 0);
    if (dims.x <= 1u || dims.y <= 1u) {
        return 1.0; // no prepass this frame: offscreen previews and probes
    }
    let to = G.point_pos[i].xyz - p;
    let dist = length(to);
    if (dist < 1e-4) {
        return 1.0;
    }
    let l = to / dist;
    // Never march past the light itself, and never past the light's own range —
    // beyond it the lamp contributes nothing, so what is out there cannot matter.
    let reach = min(dist, max(G.point_pos[i].w, 1e-4));
    let steps = u32(clamp(G.point_steps.x, 4.0, 32.0));
    let dt = reach / f32(steps);
    // A FIXED lift, for the reason spelled out in `contact_vis`: scale it by the
    // reach and a lamp further away starts its ray higher, so moving a light AWAY
    // makes its shadows weaker rather than softer.
    let ro = p + n * 0.02;
    // Proportional to the step here, not to the reach. A lamp across a room takes
    // long steps, and a window sized for the short trace would let the ray tunnel
    // between two samples that both sit inside the same wall.
    let thickness = max(dt * 2.0, 0.05) + reach * 0.02;
    let jitter = ign(pix);
    let fdims = vec2<f32>(dims);
    for (var s = 0u; s < steps; s = s + 1u) {
        // Stop short of the light: the last stretch is the lamp's own fixture and
        // the surfaces right around it, and marching into them makes a bulb
        // shadow the wall it is mounted on.
        let t = (f32(s) + jitter) * dt;
        if (t >= reach * 0.98) {
            break;
        }
        let q = ro + l * t;
        let clip = G.view_proj * vec4<f32>(q, 1.0);
        if (clip.w <= 0.0) {
            break; // behind the eye
        }
        let ndc = clip.xyz / clip.w;
        if (abs(ndc.x) > 1.0 || abs(ndc.y) > 1.0) {
            break; // off screen: no evidence either way, and guessing is worse
        }
        let uv = vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
        let texel = vec2<i32>(clamp(uv * fdims, vec2<f32>(0.0), fdims - vec2<f32>(1.0)));
        let depth = textureLoad(prime_tex, texel, 0).x;
        if (depth >= 1.0) {
            continue; // nothing was drawn here
        }
        let sp = G.inv_view_proj * vec4<f32>(ndc.xy, depth, 1.0);
        let surface = sp.xyz / sp.w;
        let behind = length(q) - length(surface);
        if (behind > 0.02 && behind < thickness) {
            return 0.0;
        }
    }
    return 1.0;
}

// The lamp's visibility a shading point actually uses: blocked by what is in
// front of the camera and by what is behind it, darkened by the scene's one
// strength knob.
//
// `min`, and the strength applied ONCE at the end: two shadow terms scaled
// separately and multiplied would make a surface both halves agree on twice as
// dark as one either half found, which reads as a seam wherever a wall leaves
// the frame.
fn point_vis(p: vec3<f32>, n: vec3<f32>, i: u32, pix: vec2<u32>) -> f32 {
    if (!light_flag(G.point_shape[i], LIGHT_SHADOWS)) {
        return 1.0;
    }
    var vis = point_field_vis(p, n, i);
    if (vis > 0.0) {
        vis = min(vis, point_screen_vis(p, n, i, pix));
    }
    return mix(1.0, vis, clamp(G.point_steps.y, 0.0, 1.0));
}

// How far it is from `p` to the SOLID surface drawn behind it, in world units.
//
// This is what a transparent surface needs and could never ask for: the SDF
// field (`map_d`) knows about terrain and blobs, and nothing at all about the
// ordinary polygon geometry most of a level is made of. Shoreline foam, soft
// particles and contact glow are all the same measurement — "how much room is
// there between me and whatever is behind me" — and all of them used to be
// impossible against a mesh.
//
// Reads the opaque depth prepass, and reprojects `p` itself to find the texel
// rather than taking a screen position, so it is exact for the fragment that
// asks and needs no extra plumbing to reach a shader.
//
// A very large number means "nothing behind this at all": no prepass this frame
// (offscreen previews, probes), off screen, or the sky. That is the value that
// makes `saturate(gap / width)` come out as "wide open", which is the right
// answer in every one of those cases.
fn flsl_surface_gap(p: vec3<f32>) -> f32 {
    let dims = textureDimensions(prime_tex, 0);
    if (dims.x <= 1u || dims.y <= 1u) {
        return 1e9;
    }
    let clip = G.view_proj * vec4<f32>(p, 1.0);
    if (clip.w <= 0.0) {
        return 1e9;
    }
    let ndc = clip.xyz / clip.w;
    if (abs(ndc.x) > 1.0 || abs(ndc.y) > 1.0) {
        return 1e9;
    }
    let uv = vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
    let fd = vec2<f32>(dims);
    let texel = vec2<i32>(clamp(uv * fd, vec2<f32>(0.0), fd - vec2<f32>(1.0)));
    let d = textureLoad(prime_tex, texel, 0).x;
    if (d >= 1.0) {
        return 1e9; // sky: nothing was drawn here
    }
    // World distance, not depth: the buffer's units are nonlinear, so a foam
    // width expressed in them would mean a different number of metres at every
    // distance — the shoreline would fatten as the camera backed away.
    let sp = G.inv_view_proj * vec4<f32>(ndc.xy, d, 1.0);
    let surface = sp.xyz / sp.w;
    return max(length(surface) - length(p), 0.0);
}

fn sun_shadow(p: vec3<f32>, n: vec3<f32>, pix: vec2<u32>) -> vec3<f32> {
    if (G.shadow_params.x < 0.5) {
        return vec3<f32>(1.0);
    }
    let l = sun_dir_at(p);
    // `min`, and BEFORE the styling: a contact shadow is the same shadow seen
    // from closer up, so it takes the same tint, strength and posterize the
    // marched one does. Two shadow terms with two different looks would read as
    // two shadows.
    var vis = min(light_vis(p, n, l), contact_vis(p, n, l, pix));
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
