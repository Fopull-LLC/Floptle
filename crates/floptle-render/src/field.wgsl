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
    vol_center: array<vec4<f32>, 16>, // xyz camera-relative box center, w: 0 absent, 1 render, 2 shadow-only
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
};

fn sd_sphere(p: vec3<f32>, r: f32) -> f32 {
    return length(p) - r;
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
fn union_edge(p: vec3<f32>) -> f32 {
    var e = -1e9;
    let vols = min(u32(G.params.w), 16u);
    for (var i = 0u; i < vols; i = i + 1u) {
        if (G.vol_center[i].w < 0.5 || G.vol_center[i].w > 1.5) { continue; }
        let q = abs(p - G.vol_center[i].xyz) - G.vol_half[i].xyz;
        if (max(q.x, max(q.y, q.z)) < 0.0) {
            e = max(e, box_edge(i, p));
        }
    }
    return e;
}

// True when `p` is inside ANY volume's box expanded by `e` — used to reject false
// hits on the boxes' bounding faces (the box-approach distance is never a real
// surface), while a small `e` still admits genuine terrain hits right at a face.
fn inside_volume_box_eps(p: vec3<f32>, e: f32) -> bool {
    let vols = min(u32(G.params.w), 16u);
    for (var i = 0u; i < vols; i = i + 1u) {
        if (G.vol_center[i].w < 0.5 || G.vol_center[i].w > 1.5) { continue; }
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
        if (G.vol_center[i].w < 0.5 || G.vol_center[i].w > 1.5) { continue; }
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
        if (G.vol_center[i].w < 0.5 || G.vol_center[i].w > 1.5) { continue; }
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
    if (!v.any) {
        return a;
    }
    if (u32(G.params.y) == 0u) {
        return v.d;
    }
    return smin(a, v.d, max(G.params.z, 0.0001));
}

// The field's sampling granularity at `p`: ~one voxel inside a baked volume (the
// central difference / shadow lift must span cell boundaries to low-pass residual
// grid+f16 noise), a small fixed epsilon on the analytic blobs.
fn field_eps(p: vec3<f32>) -> f32 {
    var h = 0.012;
    let ci = containing_volume(p, 0.08);
    if (ci >= 0) {
        let i = u32(ci);
        let voxel = 2.0 * G.vol_half[i].xyz / max(G.vol_dims[i].xyz, vec3<f32>(1.0));
        h = clamp(max(voxel.x, max(voxel.y, voxel.z)), 0.02, 1.0);
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
fn apply_fog(color: vec3<f32>, pos: vec3<f32>) -> vec3<f32> {
    if (G.fog_params.z < 0.5) {
        return color;
    }
    let denom = max(G.fog_params.y - G.fog_params.x, 1e-4);
    let f = clamp((length(pos) - G.fog_params.x) / denom, 0.0, 1.0);
    return mix(color, G.fog_color.rgb, f);
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
    return mix(1.0, ao, clamp(G.ao_params.y, 0.0, 1.0));
}

// SHADOW-ONLY occluder volumes (vol_center.w = 2): baked static level meshes that
// cast sun shadows with their true silhouette (dark interiors!) but are never
// drawn — the raster pass renders the actual triangles. Folded into the shadow
// march only; the render/AO field (`map_d`) skips them.
fn shadow_volumes_d(p: vec3<f32>) -> f32 {
    var d = 1e9;
    let vols = min(u32(G.params.w), 16u);
    for (var i = 0u; i < vols; i = i + 1u) {
        if (G.vol_center[i].w < 1.5) { continue; }
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
        if (G.vol_center[i].w < 1.5) { continue; }
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

// Visibility of the sun from surface point `p` with normal `n`: 1 = fully lit,
// 0 = fully shadowed, in between = analytic penumbra. Marches the fused field
// PLUS the proxy occluders toward the light, tracking iq's `min(k·d/t)` — the
// single `k` sweeps razor-hard (large) to dreamy-soft (small) with no kernels.
fn light_vis(p: vec3<f32>, n: vec3<f32>, l: vec3<f32>) -> f32 {
    let k = G.shadow_params.y;
    let max_d = G.shadow_params.w;
    // Lift off the surface along the normal (voxel-aware) so the ray doesn't
    // immediately re-hit the surface it starts on (shadow acne on the noisy
    // f16-sampled terrain field). When the sun GRAZES the surface (n·l → 0) the
    // ray hugs that noisy shell for a long stretch and grazing walls stripe —
    // boost the lift there, but leave ordinary sun angles alone so contact
    // shadows stay tight.
    let base = max(0.03, max(field_eps(p), shadow_vol_eps(p)) * 1.6);
    let lift = base * clamp(0.5 / max(dot(n, l), 0.125), 1.0, 4.0);
    let ro = p + n * lift;
    // A proxy containing the start point is the caster this fragment belongs to
    // (a character standing inside its own capsule) — skip it so meshes don't
    // blanket-shadow themselves; it still casts on everything else.
    var skip = 0u;
    let pc = min(u32(G.prox_count.x), 32u);
    for (var i = 0u; i < pc; i = i + 1u) {
        if (prox_d(i, ro) < lift) { skip = skip | (1u << i); }
    }
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
        var d = min(map_d(q), shadow_volumes_d(q));
        for (var i = 0u; i < pc; i = i + 1u) {
            if ((skip & (1u << i)) == 0u) {
                d = min(d, prox_d(i, q));
            }
        }
        if (d < 0.001) { return 0.0; }   // hard hit — fully occluded
        if (d > 1e8) { break; }          // empty scene along this ray — fully lit
        if (t > pen_t0) {
            vis = min(vis, clamp(k * d / t, 0.0, 1.0));
        }
        t = t + clamp(d, 0.02, 4.0);
        if (vis < 0.01 || t > max_d) { break; }
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

// The sun-shadow multiplier for the DIRECTIONAL light at `p` (screen pixel `pix`
// drives the optional dither): vec3(1) when lit, darkening toward the shadow
// tint with the configured strength when occluded. Multiplies the directional
// diffuse + specular only — ambient and point lights are unshadowed fill.
fn sun_shadow(p: vec3<f32>, n: vec3<f32>, pix: vec2<u32>) -> vec3<f32> {
    if (G.shadow_params.x < 0.5) {
        return vec3<f32>(1.0);
    }
    let l = normalize(G.light_dir.xyz);
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
