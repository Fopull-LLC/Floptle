// Raymarched SDF matter, composited with the raster meshes.
//
// Two kinds of matter are folded into one field with smin (smooth union): the
// analytic morphing blob, and a BAKED MESH VOLUME (a 3D distance texture + a
// co-located color texture produced by mesh2sdf). Distance AND color blend by the
// same smin weight, so where the two fuse the textures crossfade across the seam.
// Rays come from inverse(view_proj) (camera-relative, ADR-0015) and the fragment
// writes frag_depth, so this shares one depth buffer with the raster meshes.
//
// The `Globals` struct and all distance-only field machinery (`map_d`, blob/
// volume distances, SDF AO, `sun_shadow`) live in `field.wgsl`, which is
// concatenated onto this module at creation — this file keeps the COLOR-carrying
// surface path (what the hit looks like) and the primary march.

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
// The SAME terrain palette, sampled nearest. A texture_2d_array has one sampler for
// all 16 slots, so per-texture filtering can't come from the sampler alone — both are
// bound and `triplanar` picks per slot from the mask in G.terrain_tint.w. Without this
// every terrain texture was hardcoded Linear (blurry) while the identical image on a
// mesh honoured its Pixelated setting.
@group(0) @binding(8) var terrain_samp_nearest: sampler;
@group(0) @binding(6) var sky_tex: texture_2d<f32>;
// The opaque-mesh depth prepass (screen-sized when primed, a 1×1 "off" fallback
// otherwise): caps the march per pixel so rays stop at the nearest mesh instead
// of marching the field behind it. Depth32Float binds as unfilterable float.
@group(0) @binding(7) var prime_tex: texture_2d<f32>;

const PI: f32 = 3.14159265359;

// The environment color along world ray direction `dir`: a flat color, or the equirect
// sky texture (rotated by the skybox node so a script can spin it) times its tint.
//[flsl-sky-custom-begin] — the renderer splices a generated Sky shader over this block;
// the stub returns a sentinel so `sky_color` falls through to the built-in path.
fn flsl_sky(dir: vec3<f32>, t: f32) -> vec3<f32> {
    return vec3<f32>(-1.0);
}
//[flsl-sky-custom-end]

fn sky_color(dir: vec3<f32>) -> vec3<f32> {
    // A Sky shader (sky_meta.x = 1) overrides everything: it computes the environment color
    // per ray direction. `flsl_sky` is the spliced procedural sky (a stub otherwise).
    if (G.sky_meta.x > 0.5) {
        return flsl_sky(normalize(dir), G.params.x);
    }
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

// smin that blends distance AND color by the same weight h (texture crossfade).
fn smin_matter(a: Matter, b: Matter, k: f32) -> Matter {
    let h = clamp(0.5 + 0.5 * (b.d - a.d) / k, 0.0, 1.0);
    let d = mix(b.d, a.d, h) - k * h * (1.0 - h);
    let col = mix(b.col, a.col, h);
    return Matter(d, col);
}

// The analytic blob: a STATIC smooth metaball (geometry in `blob_d`, field.wgsl —
// no time morph) so it is a predictable, placeable shape whose size comes only
// from the transform — `center.w` is the scale, and the blob's radius is ≈ 0.85 *
// scale (comparable to the unit sphere). A low-frequency iridescent tint (its
// spatial period spans the whole blob, so it never aliases) gives the otherworldly
// look; the close-up "ring" artifacts came from the specular highlight (see `fs`),
// not from this color. Animation, if wanted, belongs to scripts, not the shape.
fn blob_one(p: vec3<f32>, center: vec3<f32>, s: f32) -> Matter {
    let q = (p - center) / s;
    let iri = 0.5 + 0.5 * cos(6.2831 * (q.y * 0.5 + vec3<f32>(0.0, 0.33, 0.67)));
    let col = mix(vec3<f32>(0.35, 0.16, 0.55), iri, 0.55);
    return Matter(blob_d(p, center, s), col);
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

// March bound from all blobs + every volume box (replaces the old single-blob reach).
fn march_bound() -> f32 {
    let count = min(u32(G.params.y), 16u);
    var reach = 0.0;
    var maxc = 0.0;
    let vols = min(u32(G.params.w), 16u);
    for (var i = 0u; i < vols; i = i + 1u) {
        if (G.vol_center[i].w < 0.5 || G.vol_center[i].w > 1.5) { continue; }
        reach = max(reach, length(G.vol_half[i].xyz));
        maxc = max(maxc, length(G.vol_center[i].xyz));
    }
    for (var i = 0u; i < count; i = i + 1u) {
        reach = max(reach, G.blobs[i].w);
        maxc = max(maxc, length(G.blobs[i].xyz));
    }
    let shapes = min(u32(G.shape_meta.x), 4u);
    for (var i = 0u; i < shapes; i = i + 1u) {
        reach = max(reach, G.shape_aux[i].x);
        maxc = max(maxc, length(G.shape_pos[i].xyz));
    }
    return reach * 60.0 + maxc + 100.0;
}

// One baked volume, RAW (untapered): a box SDF outside (to march toward), the
// sampled distance + albedo inside, read from this volume's slot of the shared 3D
// atlas at its NATIVE voxel resolution. The slab-edge taper is applied ONCE, after
// the fold (see `volumes`) — tapering before the smin would bulge/crease the
// under-slope at seams.
fn volume_at(i: u32, p: vec3<f32>) -> Matter {
    let vh = G.vol_half[i].xyz;
    let rel = p - G.vol_center[i].xyz;
    let q = abs(rel) - vh;
    let box_d = length(max(q, vec3<f32>(0.0))) + min(max(q.x, max(q.y, q.z)), 0.0);
    // Far from the brick: the box distance alone is a valid conservative bound
    // and the color can't reach any blend — skip both texture fetches (mirrors
    // `volume_d` in field.wgsl, including why the tight content box is NOT the
    // bound here; the cutoff scales with the fuse radius).
    if (box_d > 4.0 + 2.0 * G.vol_half[i].w) {
        return Matter(box_d, vec3<f32>(1.0));
    }
    if (box_d > 0.0) {
        // Outside the brick: CONTINUE the field, exactly like `Terrain::sample` —
        // box distance PLUS the clamped edge sample's air gap. A constant floor here
        // would read as "surface just outside the box" to the smin fuse and raise a
        // bright ridge along the face wherever another volume overlaps (the same
        // near-zero-shell lesson as the CPU combine). The 0.08 floor is still kept
        // so a ray never crawls on (or reports) the box face itself — it jumps
        // across into the volume, where the real sampled SDF takes over.
        let euvw = atlas_uvw(i, rel);
        let ed = textureSampleLevel(dist_tex, vol_samp, euvw, 0.0).r;
        let ecol = textureSampleLevel(color_tex, vol_samp, euvw, 0.0).rgb;
        return Matter(max(box_d + max(ed, 0.0), 0.08), ecol);
    }
    let uvw = atlas_uvw(i, rel);
    let d = textureSampleLevel(dist_tex, vol_samp, uvw, 0.0).r;
    let col = textureSampleLevel(color_tex, vol_samp, uvw, 0.0).rgb;
    return Matter(d, col);
}

// Every present volume folded together with smin (the SAME polynomial fuse the old
// CPU combine used, so overlapping terrains still melt into one seamless surface).
// `any` is false when no volume contributed — the caller must NOT smin the sentinel
// against a blob (f32 cancellation collapses the field; see `map`).
struct VolFold { m: Matter, any: bool };
fn volumes(p: vec3<f32>) -> VolFold {
    var m = Matter(1e9, vec3<f32>(1.0));
    var any = false;
    let vols = min(u32(G.params.w), 16u);
    for (var i = 0u; i < vols; i = i + 1u) {
        // Skip absent slots AND shadow-only occluder bakes (w = 2) — those exist
        // for `light_vis` alone; the drawn field must not contain them.
        if (G.vol_center[i].w < 0.5 || G.vol_center[i].w > 1.5) { continue; }
        let v = volume_at(i, p);
        if (!any) {
            m = v;
            any = true;
        } else {
            m = smin_matter(m, v, max(G.vol_half[i].w, 0.0001));
        }
    }
    // Taper the finite slabs' SIDE + BOTTOM faces up to air — applied ONCE to the
    // folded field, measured against the UNION of the volume boxes, so a terrain
    // doesn't render as hard dirt walls / a shell at its outer faces, while interior
    // seams (faces a neighbor continues through) stay perfectly seamless. The TOP
    // ground surface is untouched. (This mirrors what the old CPU combine + single-
    // volume taper produced, including for one isolated volume.)
    let uedge = union_edge(p);
    if (any && uedge > -1e8) {
        m.d = max(m.d, 2.0 - uedge);
    }
    return VolFold(m, any);
}

// A threshold-crossing is a REAL surface (not the shell) when there are no volumes,
// or we are strictly inside some volume's box, or an analytic blob is the matter
// here. The box test must stay STRICT: outside the bricks `volumes()` returns a
// constant floor (the box-approach step), and at a far camera the distance-relaxed
// `thr` grows past that floor — any slack here would accept a box face itself and
// re-draw the shell. Genuine terrain hits are inside a box; the grazing-gap fill
// below only records/accepts points inside a box, so it needs no slack here.
fn real_surface(p: vec3<f32>, thr: f32) -> bool {
    // Field Shapes are always real surface (their own bounding-sphere march
    // keeps rings away); checked first so shape-only scenes hit. Zero shapes
    // short-circuits before evaluating the field.
    if (G.shape_meta.x >= 0.5 && custom_d(p) < thr) { return true; }
    if (G.params.w < 0.5) { return true; }
    if (inside_volume_box_eps(p, 0.0)) { return true; }
    return G.params.y >= 0.5 && analytic_d(p) < thr;
}

//[flsl-color-custom-begin] — the renderer splices generated Field Shape color
// functions over this block; the stubs keep the surface pass unchanged.
fn custom_col(p: vec3<f32>) -> Matter {
    return Matter(1e9, vec3<f32>(0.0));
}
fn nearest_shape(p: vec3<f32>) -> i32 {
    return 0;
}
//[flsl-color-custom-end]

// The whole field: every piece of matter folded together with smin.
fn map(p: vec3<f32>) -> Matter {
    let a = analytic(p);
    let v = volumes(p);
    // Fold only the parts that exist. Blending against an "absent" sentinel (1e9)
    // is not just wasteful: f32 `mix(1e9, d, 1.0)` is evaluated as
    // `1e9 + 1.0*(d - 1e9)`, and `d - 1e9` loses `d` entirely, so the field would
    // collapse to ~0 everywhere — a surface at the camera, every ray a false hit.
    // (This was the "glitchy giant sphere".)
    var base: Matter;
    if (!v.any) {
        base = a;
    } else if (u32(G.params.y) == 0u) {
        base = v.m;
    } else {
        base = smin_matter(a, v.m, max(G.params.z, 0.0001));
    }
    // Field Shapes union in hard (min picks the nearer surface + its color;
    // exact against the 1e9 stub — no f32 cancellation).
    let c = custom_col(p);
    if (c.d < base.d) {
        return c;
    }
    return base;
}

// Triplanar-sample a terrain palette layer at a box-relative position (world-stable,
// since `rel` cancels the camera offset), blended by the surface normal.
fn triplanar(slot: i32, rel: vec3<f32>, n: vec3<f32>) -> vec3<f32> {
    let scale = 0.22; // ~4.5 world units per tile
    let an = abs(n) + vec3<f32>(0.0001);
    let w = an / (an.x + an.y + an.z);
    // Bit `slot` of the mask = this slot's texture asked for Pixelated/Nearest filtering
    // (the Assets panel setting). Both samplers are read and one result is selected:
    // WGSL has no sampler arrays, and `select` keeps this uniform-safe.
    let nearest = (u32(G.terrain_tint.w) & (1u << u32(slot))) != 0u;
    let lx = textureSampleLevel(terrain_tex, terrain_samp, rel.zy * scale, slot, 0.0).rgb;
    let ly = textureSampleLevel(terrain_tex, terrain_samp, rel.xz * scale, slot, 0.0).rgb;
    let lz = textureSampleLevel(terrain_tex, terrain_samp, rel.xy * scale, slot, 0.0).rgb;
    let nx = textureSampleLevel(terrain_tex, terrain_samp_nearest, rel.zy * scale, slot, 0.0).rgb;
    let ny = textureSampleLevel(terrain_tex, terrain_samp_nearest, rel.xz * scale, slot, 0.0).rgb;
    let nz = textureSampleLevel(terrain_tex, terrain_samp_nearest, rel.xy * scale, slot, 0.0).rgb;
    let cx = select(lx, nx, nearest);
    let cy = select(ly, ny, nearest);
    let cz = select(lz, nz, nearest);
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
    // The DOMINANT volume at p (nearest surface) provides the painted slot — same
    // rule as the old CPU combine ("slot from the nearer surface, never blended").
    let ci = containing_volume(p, 0.0);
    if (ci < 0) {
        return tint; // not inside any terrain box
    }
    let i = u32(ci);
    let rel = p - G.vol_center[i].xyz;
    // The slot only transitions over ONE voxel, so a single linear tap gives a narrow
    // seam. Average a few taps in the surface PLANE (so we widen along the ground, not
    // into it) → a soft, several-voxel crossfade. `a` is the 1-based slot (0 = untextured).
    var t1 = cross(n, vec3<f32>(0.0, 1.0, 0.0));
    if (dot(t1, t1) < 0.01) { t1 = cross(n, vec3<f32>(1.0, 0.0, 0.0)); }
    t1 = normalize(t1);
    let t2 = normalize(cross(n, t1));
    // Offsets of 1.5 voxels along each tangent, expressed in world units per axis.
    let voxel = 2.0 * G.vol_half[i].xyz / max(G.vol_dims[i].xyz, vec3<f32>(1.0));
    let o1 = t1 * voxel * 1.5;
    let o2 = t2 * voxel * 1.5;
    let a = (
        textureSampleLevel(color_tex, vol_samp, atlas_uvw(i, rel), 0.0).a
        + textureSampleLevel(color_tex, vol_samp, atlas_uvw(i, rel + o1), 0.0).a
        + textureSampleLevel(color_tex, vol_samp, atlas_uvw(i, rel - o1), 0.0).a
        + textureSampleLevel(color_tex, vol_samp, atlas_uvw(i, rel + o2), 0.0).a
        + textureSampleLevel(color_tex, vol_samp, atlas_uvw(i, rel - o2), 0.0).a
    ) * (255.0 / 5.0);
    if (a < 0.5 || a > 254.5) {
        return tint; // fully untextured here (255 = the legacy no-slot sentinel)
    }
    let lo = floor(a);
    let f = a - lo;
    let c_lo = terrain_slot_color(i32(lo) - 1, rel, n, tint);
    let c_hi = terrain_slot_color(i32(ceil(a)) - 1, rel, n, tint);
    return mix(c_lo, c_hi, f);
}

fn calc_normal(p: vec3<f32>) -> vec3<f32> {
    // For TERRAIN (a sampled voxel field) `field_eps` is ~one voxel, so the central
    // difference spans cell boundaries and low-passes residual grid/f16 noise instead
    // of reporting a single cell's facet (the grain). Analytic blobs have a continuous
    // gradient and want a small epsilon for crisp edges.
    let h = field_eps(p);
    // Tetrahedron offsets: 4 taps, isotropic (no axis-aligned facet bias), cheaper
    // than the 6-tap central cross.
    let k0 = vec3<f32>(1.0, -1.0, -1.0);
    let k1 = vec3<f32>(-1.0, -1.0, 1.0);
    let k2 = vec3<f32>(-1.0, 1.0, -1.0);
    let k3 = vec3<f32>(1.0, 1.0, 1.0);
    return normalize(
        k0 * map_d(p + k0 * h) + k1 * map_d(p + k1 * h) + k2 * map_d(p + k2 * h)
            + k3 * map_d(p + k3 * h),
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
    // Everything drawable lives inside the volume boxes + blob spheres: march
    // only the ray's span through those bounds. Rays that miss every bound are
    // provably sky (zero steps); rays toward distant matter skip the empty air.
    var span = field_span(ro, rd, max_t);
    // Opaque-mesh cap (the depth prepass): never march past the nearest mesh —
    // its fragment would lose the depth test anyway. A small forward bias keeps
    // mesh↔field contact edges hole-free (those hits still depth-resolve right).
    let pdims = textureDimensions(prime_tex);
    if (pdims.x > 1u) {
        let pix = min(vec2<u32>(in.clip.xy), pdims - vec2<u32>(1u));
        let dmesh = textureLoad(prime_tex, pix, 0).x;
        if (dmesh < 1.0) {
            let wp = G.inv_view_proj * vec4<f32>(in.ndc, dmesh, 1.0);
            let t_mesh = dot(wp.xyz / wp.w - ro, rd);
            span.y = min(span.y, t_mesh + max(0.05, 0.005 * t_mesh));
        }
    }
    var t = span.x;
    var prev_t = span.x;
    // The field value at `prev_t` / at the coarse hit — carried out of the loop so
    // the refine below starts from an already-sampled bracket instead of paying
    // fresh evaluations for values the march just computed.
    var prev_d = 1e9;
    var hit_d = 0.0;
    var hit = false;
    // Closest approach to a real surface, so a grazing ray that never quite trips the
    // coarse threshold — the silhouette of a hill/ravine, where the step shrinks and
    // the iteration budget runs out — can be accepted below instead of leaving a
    // transparent hole. (Those holes are what the low-res retro filter blew up into
    // visible blocky gaps along terrain edges.)
    var min_d = 1e9;
    var min_t = 0.0;
    var min_prev = 0.0;
    var min_prev_d = 1e9;
    for (var i = 0; i < 256; i = i + 1) {
        if (t > span.y) {
            break;
        }
        let p = ro + rd * t;
        // Distance-only sampling in the hot loop (map_d, field.wgsl) — the color
        // atlas is read once, at the refined hit, not per step.
        let d = map_d(p);
        // Distance-relaxed threshold for the COARSE hit (the precise surface is then
        // found by the refine below). A gentle t-growth still helps grazing rays
        // converge without exhausting the step budget, but it's kept small so the far
        // silhouette stays sharp (the old larger growth left a fuzzy wispy horizon).
        let thr = 0.0006 * t + 0.002;
        if (d < min_d && real_surface(p, 0.08)) {
            min_d = d;
            min_t = t;
            min_prev = prev_t;
            min_prev_d = prev_d;
        }
        if (d < thr && real_surface(p, thr)) {
            hit_d = d;
            hit = true;
            break;
        }
        prev_t = t;
        prev_d = d;
        // Conservative step (0.85): the smin-blended + trilinear-sampled field is
        // not a perfectly exact SDF, so understep to avoid overshoot cracks when
        // the camera is close to the surface.
        t = t + max(d, 0.003) * 0.85;
    }
    // Grazing-silhouette fill: no clean hit, but the ray passed within ~a voxel of a
    // real surface → accept that closest approach (refined below).
    if (!hit && min_d < 0.06 + 0.0015 * min_t) {
        hit = true;
        t = min_t;
        hit_d = min_d;
        prev_t = min_prev;
        prev_d = min_prev_d;
    }

    // Refine the loose threshold hit to the TRUE surface (where the field crosses
    // zero). The relaxed threshold above hits at a t that varies with distance,
    // which on a grazing surface produced visible depth BANDING; refining to d≈0
    // gives a consistent surface depth + cleaner normals (no banding/grain).
    if (hit) {
        var a = prev_t; // outside (d > 0)
        var da = prev_d;
        var b = t;      // at/just inside the threshold
        var db = hit_d;
        // Walk `b` until it's truly inside (d < 0) so [a,b] brackets the crossing.
        var bracketed = db < 0.0;
        for (var k = 0; k < 10; k = k + 1) {
            if (bracketed) { break; }
            a = b;
            da = db;
            b = b + 0.02;
            db = map_d(ro + rd * b);
            bracketed = db < 0.0;
        }
        // Only refine when we actually bracket a zero crossing. A grazing silhouette
        // ray that never goes inside keeps its (smooth) threshold hit instead of a
        // bogus refined result — that was the wispy far-horizon edge.
        if (bracketed) {
            // Bracketed secant (regula falsi, Illinois-damped) instead of plain
            // bisection: the field is close to linear across the tiny bracket, so
            // a couple of evaluations land within ~1e-4 — the consistency the old
            // 14-step bisection bought with 14 evaluations. This refine runs for
            // EVERY hit pixel (it was ~a third of the terrain frame); it must
            // stay cheap.
            var tm = b;
            for (var j = 0; j < 5; j = j + 1) {
                tm = a + da * (b - a) / max(da - db, 1e-9);
                let dm = map_d(ro + rd * tm);
                if (abs(dm) < 3e-4) {
                    break;
                }
                if (dm < 0.0) {
                    b = tm;
                    db = dm;
                    da = da * 0.5; // damp the kept side so the secant keeps moving
                } else {
                    a = tm;
                    da = dm;
                    db = db * 0.5;
                }
            }
            t = tm;
        }
    }

    var out: FsOut;
    var drawn = false;
    if (hit) {
        let p = ro + rd * t;
        let m = map(p); // the hit's blended surface color (one color fetch per ray)
        let clip = G.view_proj * vec4<f32>(p, 1.0);
        let ndc_z = clip.z / clip.w;
        if (clip.w > 0.0 && ndc_z >= 0.0 && ndc_z <= 1.0) {
            let n = calc_normal(p);
            let l = normalize(G.light_dir.xyz);
            let v = -rd; // toward the camera (the camera sits at the ray origin)
            let diff = max(dot(n, l), 0.0);
            // The terrain palette texture (if painted) modulates the per-voxel tint.
            let albedo = terrain_albedo(p, n, m.col);
            // SDF AO (PostProcess node, `Sdf` mode): darkens all received light
            // below, but never emissive — a glowing surface stays glowing.
            var occ = 1.0;
            if (G.ao_params.x > 0.5) {
                occ = sdf_ao(p, n);
            }
            // Sun shadows (Lighting node): a field march toward the light. Only
            // multiplies the DIRECTIONAL diffuse + specular — ambient and point
            // lights are the unshadowed fill. Skipped on sun-averted surfaces
            // (diff = 0 already zeroes the terms it would scale).
            let pix = vec2<u32>(u32(in.clip.x), u32(in.clip.y));
            var sh = vec3<f32>(1.0);
            if (diff > 0.0) {
                sh = sun_shadow(p, n, pix);
            }
            var col: vec3<f32>;
            if (G.shape_meta.x >= 0.5 && custom_d(p) <= m.d + 1e-4) {
                // FIELD SHAPE: the hit is on an authored SDF shader — shade its
                // albedo (`output color`) with the node Material's response,
                // the same lighting model as terrain/blobs/meshes.
                let si = clamp(nearest_shape(p), 0, 3);
                let tinted = m.col * G.shape_tint[si].rgb;
                let emissive = G.shape_emissive[si].rgb * G.shape_emissive[si].a;
                let spar = G.shape_params[si];
                if (spar.z > 0.5) {
                    col = tinted + emissive; // unlit / fullbright
                } else {
                    let ambient = G.ambient.rgb * spar.w;
                    col = tinted * (ambient + G.light_color.rgb * diff * sh);
                    col = col + tinted * point_diffuse(p, n);
                    let h = normalize(l + v);
                    let shininess = max(spar.x, 1.0);
                    let spec = pow(max(dot(n, h), 0.0), shininess) * G.shape_specular[si].a * select(0.0, 1.0, diff > 0.0);
                    col = col + G.shape_specular[si].rgb * spec * G.light_color.rgb * sh;
                    let rim_f = pow(1.0 - max(dot(n, v), 0.0), 2.0) * spar.y;
                    col = (col + G.shape_rim[si].rgb * rim_f) * occ + emissive;
                }
            } else if (inside_volume_box_eps(p, 0.06)) {
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
                    col = tinted * (ambient + G.light_color.rgb * diff * sh);
                    col = col + tinted * point_diffuse(p, n); // placeable point lights
                    let h = normalize(l + v);
                    let shininess = max(G.terrain_params.x, 1.0);
                    let spec = pow(max(dot(n, h), 0.0), shininess) * G.terrain_specular.a * select(0.0, 1.0, diff > 0.0);
                    col = col + G.terrain_specular.rgb * spec * G.light_color.rgb * sh;
                    let rim_f = pow(1.0 - max(dot(n, v), 0.0), 2.0) * G.terrain_params.y;
                    col = (col + G.terrain_rim.rgb * rim_f) * occ + emissive;
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
                    col = tinted * (ambient + G.light_color.rgb * diff * sh);
                    col = col + tinted * point_diffuse(p, n); // placeable point lights
                    let h = normalize(l + v);
                    let shininess = max(bpar.x, 1.0);
                    let spec = pow(max(dot(n, h), 0.0), shininess) * G.blob_specular[bi].a * select(0.0, 1.0, diff > 0.0);
                    col = col + G.blob_specular[bi].rgb * spec * G.light_color.rgb * sh;
                    let rim_f = pow(1.0 - max(dot(n, v), 0.0), 2.0) * bpar.y;
                    col = (col + G.blob_rim[bi].rgb * rim_f) * occ + emissive;
                }
            }
            // Depth fog by camera-relative distance (p is camera-relative). The sky
            // branch below stays UNFOGGED so a textured skybox reads crisp.
            let fogged = apply_fog(clamp(col, vec3<f32>(0.0), vec3<f32>(1.0)), p, pix);
            out.color = vec4<f32>(fogged, 1.0);
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
    let span = field_span(ro, rd, max_t);
    var t = span.x;
    var masked = 0.0;
    for (var i = 0; i < 160; i = i + 1) {
        if (t > span.y) {
            break;
        }
        let p = ro + rd * t;
        let d = map_d(p);
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
    }
    if (masked < 0.5) {
        discard;
    }
    return vec4<f32>(1.0, 0.0, 0.0, 1.0);
}
