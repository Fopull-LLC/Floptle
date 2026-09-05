// Forward raster: instanced, depth-tested meshes with directional diffuse light
// and a per-material base-color texture.
//
// Group 0 (shared, set once per frame): the camera/light globals.
// Group 1 (per mesh/material): the base-color texture + its sampler (so each texture
// chooses its own filtering / wrap mode). Group 2 (shared): the raymarch pass's OWN
// globals + distance atlas — the fused SDF field (see field.wgsl, concatenated onto
// this module), so mesh fragments RECEIVE field sun-shadows and true SDF AO by
// marching the very field the raymarch pass draws. Per-vertex stream (buffer 0):
// pos/normal/uv. Per-instance stream (buffer 1): camera-relative model matrix
// (locations 3..6), inverse-transpose normal matrix columns (7..9), tint (10).

struct RasterGlobals {
    view_proj: mat4x4<f32>,
    light_dir: vec4<f32>,    // xyz = normalized world-space direction TO the light
    light_color: vec4<f32>,
    ambient: vec4<f32>,
    point_count: vec4<f32>,            // x = active point-light count
    point_pos: array<vec4<f32>, 16>,   // xyz camera-relative pos, w = range
    point_color: array<vec4<f32>, 16>, // rgb = color * intensity
    point_shape: array<vec4<f32>, 16>, // the EMITTER: [kind, a, b, flags] — see area_terms()
    point_rot: array<vec4<f32>, 16>,   // the emitter's world orientation (xyzw quaternion)
    terrain_mask: vec4<f32>,           // y = triplanar scale (bitmasks moved to terrain_bits)
    terrain_bits: vec4<u32>,           // x = per-slot NEAREST bitmask, y = per-slot GLOW bitmask (bit-exact at 32 slots)
    // Each lamp's CONE: x = cos(half angle where it reaches zero), y = cos(half
    // angle where it is still full. x = -1 is NO cone. Appended at the END so
    // this struct stays byte-identical to the Rust `Globals`.
    point_cone: array<vec4<f32>, 16>,
};

@group(0) @binding(0) var<uniform> g: RasterGlobals;
// Vertex paint: every painted mesh's RGBA8 colors packed back to back, read as
// `vpaint[paint_base + vertex_index]`. A storage buffer rather than a vertex
// attribute because locations 0..15 are FULL (see VsIn) — and because one global
// buffer + a per-instance base offset lets painted nodes stay in their instanced
// batches. Index 0 is a reserved dummy: paint_base == 0 means "unpainted".
@group(0) @binding(1) var<storage, read> vpaint: array<u32>;
// Terrain chunk colors, read as `tpaint[n0.w + vertex_index]`. Its own store rather
// than a region of `vpaint` because chunk meshes are re-extracted constantly (every
// sculpt dab, every LOD change): their blocks must be freeable, and `vpaint`'s never
// are. See `Raster::tpaint_buf`. Index 0 is the reserved dummy = "no terrain color".
@group(0) @binding(2) var<storage, read> tpaint: array<u32>;
// The terrain texture palette (a layer array) + its REPEAT samplers (linear + nearest),
// for meshed-terrain triplanar splatting — the raster mirror of the raymarch's palette.
//
// **The SAME texture is bound twice**, once per sampler, and that is not
// redundancy — it is what makes this shader expressible on OpenGL. GLSL has no
// separate image and sampler: `sampler2DArray` is the pair, so one image used
// with two samplers cannot be written at all, and naga refuses the whole module
// rather than the one function. Binding 11 is the same view as binding 3; the
// cost is one descriptor and the alternative was a pipeline that could not be
// created on that backend.
@group(0) @binding(3) var terrain_pal: texture_2d_array<f32>;
@group(0) @binding(4) var terrain_pal_samp: sampler;
@group(0) @binding(11) var terrain_pal_nearest: texture_2d_array<f32>;
@group(0) @binding(5) var terrain_pal_samp_nearest: sampler;
// GPU skinning (`floptle/0080`). Three stores, all read in `vs_skin` and nowhere
// else, all following the `vpaint` pattern: ONE buffer per scene, indexed by a
// per-instance base, so skinned characters stay ordinary instanced draws instead
// of each needing a bind group of its own.
//
//   skin_joints[skin_base + vertex_index]  → the four palette slots this vertex
//   skin_weights[skin_base + vertex_index]   is weighted to, and by how much.
//   skin_palette[palette_base]             → the FALLBACK matrix (the part's own
//                                            node, for zero-weight vertices).
//   skin_palette[palette_base + 1 + slot]  → that slot's `nodeWorld · inverseBind`.
//
// Both bases arrive through ONE instance lane — `n0.w`, which carries the terrain
// color base for `vs` and is free here because terrain is never skinned. It holds
// an index into `skin_meta`, whose entry says where this instance's vertex
// attributes and bone palette begin. One lane rather than two because the raster
// attribute budget is FULL at 16/16: there was no second lane to spend, and an
// indirection costs one buffer read against a table sized by the frame's skinned
// draws.
//
//   skin_meta[n0.w].x = skin_base    (into skin_joints / skin_weights)
//   skin_meta[n0.w].y = palette_base (into skin_palette)
@group(0) @binding(6) var<storage, read> skin_joints: array<vec4<u32>>;
@group(0) @binding(7) var<storage, read> skin_weights: array<vec4<f32>>;
@group(0) @binding(8) var<storage, read> skin_palette: array<mat4x4<f32>>;
@group(0) @binding(9) var<storage, read> skin_meta: array<vec4<u32>>;
// The SURFACE EXTRAS store — the per-material properties that had no instance
// attribute left to ride (the stream is FULL at 16/16). Two vec4s per entry,
// indexed by `ext_index` (which arrives packed above the modulate bit in n1.w):
//   mat_ext[i*3 + 0] = roughness, metallic, normal strength, occlusion strength
//   mat_ext[i*3 + 1] = flag bits, retro jitter, reflectivity, transmission
//   mat_ext[i*3 + 2] = ior, thickness, 0, 0
// Entry 0 is the reserved NEUTRAL: an instance that sets none of this reads it
// and shades exactly as it did before these lanes existed.
@group(0) @binding(10) var<storage, read> mat_ext: array<vec4<f32>>;
// This surface's five maps. Slots 1..4 have NEUTRAL 1×1 defaults bound whenever
// the material names no map — a flat (0.5, 0.5, 1) normal and white for the
// three scalar maps — so there is exactly one code path and no "is a map bound"
// flag that could disagree with what is actually bound.
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;
@group(1) @binding(2) var normal_tex: texture_2d<f32>;
@group(1) @binding(3) var normal_samp: sampler;
@group(1) @binding(4) var rough_tex: texture_2d<f32>;
@group(1) @binding(5) var rough_samp: sampler;
@group(1) @binding(6) var metal_tex: texture_2d<f32>;
@group(1) @binding(7) var metal_samp: sampler;
@group(1) @binding(8) var ao_tex: texture_2d<f32>;
@group(1) @binding(9) var ao_samp: sampler;

// vec4s per entry. Mirrored as `EXT_LANES` in raster.rs; the two are the same
// number and the store is misread by exactly one material if they disagree.
const EXT_LANES: u32 = 3u;

// Flag bits in `mat_ext[i*3 + 1].x`. Mirrored in raster.rs (`EXT_*`) — the two
// lists are the same list and must be edited together.
const EXT_PHYSICAL: u32 = 1u;
const EXT_AFFINE_UV: u32 = 2u;
const EXT_VERTEX_LIT: u32 = 4u;
const EXT_DITHER_ALPHA: u32 = 8u;
// Set when the surface is exempt from the scene's fog. Stored INVERTED so the
// neutral entry's all-zero flags still mean "fogged", which is what every
// instance authored before this bit existed meant.
const EXT_NO_FOG: u32 = 16u;

struct Ext {
    rough: f32,
    metal: f32,
    nstr: f32,
    ostr: f32,
    flags: u32,
    jitter: f32,
    reflect: f32,
    transmit: f32,
    ior: f32,
    thick: f32,
};

// Read entry `idx`, clamped into the store. The store always holds at least the
// neutral entry, so the clamp always lands on something meaningful.
fn ext_at(idx: u32) -> Ext {
    let last = (arrayLength(&mat_ext) / EXT_LANES) - 1u;
    let i = min(idx, last) * EXT_LANES;
    let a = mat_ext[i];
    let b = mat_ext[i + 1u];
    let c = mat_ext[i + 2u];
    return Ext(a.x, a.y, a.z, a.w, u32(b.x + 0.5), b.y, b.z, b.w, c.x, c.y);
}

fn ext_has(e: Ext, bit: u32) -> bool {
    return (e.flags & bit) != 0u;
}

// The scene's fog over a shaded surface, unless this surface is exempt from it
// (`Material::fog = false`). Spelled once and called from all THREE shading
// returns below — unlit, vertex-lit and the full path — because a fog opt-out
// that only held on one of them is worse than none: it would work in the frame
// somebody tested and quietly come back when the material was lit differently.
//
// Aerial perspective is deliberately still applied: `apply_fog` composites a
// planet's atmosphere before the fog, and that is a separate effect with its
// own controls on the CelestialBody. This exempts the SCENE'S FOG.
fn surface_fog(e: Ext, color: vec3<f32>, pos: vec3<f32>, pix: vec2<u32>) -> vec3<f32> {
    if (ext_has(e, EXT_NO_FOG)) {
        return atmo_composite(color, pos, length(pos), false);
    }
    return apply_fog(color, pos, pix);
}
// The shared SDF field (struct + all functions in field.wgsl): the raymarch
// globals buffer and distance atlas, bound read-only here.
@group(2) @binding(0) var<uniform> G: Globals;
@group(2) @binding(1) var dist_tex: texture_3d<f32>;
@group(2) @binding(2) var vol_samp: sampler;
// The field's COLOR atlas (rgba8; alpha = the voxel's palette slot byte, EXACT via
// textureLoad) — what the splat below reads so slot boundaries blend the two real
// textures instead of sweeping through every palette index in between.
@group(2) @binding(3) var field_color_tex: texture_3d<f32>;
// Baked GI probes (Matter::LightProbes) — see `gi_bounce` in field.wgsl.
@group(2) @binding(4) var gi_tex: texture_3d<f32>;
// The opaque-mesh depth prepass, for CONTACT shadows (`contact_vis` in
// field.wgsl). The raymarch pass binds the same texture at its own group(0)
// binding 7, where it caps the march instead.
@group(2) @binding(5) var prime_tex: texture_2d<f32>;

// `PI` itself lives in raymarch.wgsl, which this module is NOT concatenated
// with — these are the raster pass's own.
const INV_PI: f32 = 0.31830988618;
const INV_TAU: f32 = 0.15915494309;
const TAU: f32 = 6.28318530718;
// Where a screen-space reflection starts giving way to the probe and the sky,
// and where it stops being traced at all. See `ssr_trace`.
const SSR_ROUGH_FADE: f32 = 0.35;
const SSR_ROUGH_MAX: f32 = 0.6;

// The captured sky (equirectangular, with a roughness mip chain) — see `env.rs`.
// A 1x1 map means "no environment this frame": offscreen previews and probes
// with no raymarch pass reflect nothing rather than reflecting a stale sky.
@group(2) @binding(6) var env_tex: texture_2d<f32>;
@group(2) @binding(7) var env_samp: sampler;

// The scene colour history: last frame's composited picture with a mip chain,
// in linear light and before any post — what a screen-space reflection reads.
// 1×1 means there is no history (an offscreen preview, a probe, the first frame
// after a load) and reflections fall back to the sky alone. See `ssr.rs`.
@group(2) @binding(8) var scene_tex: texture_2d<f32>;
@group(2) @binding(9) var scene_samp: sampler;

// Reflection probes: one equirectangular capture per probe, same projection and
// same roughness chain as the sky above, plus the box each was captured in.
// 1×1 means no probes, and every surface falls back to the sky. See `reflect.rs`.
@group(2) @binding(10) var probe_tex: texture_2d_array<f32>;
@group(2) @binding(11) var probe_samp: sampler;

// Karis' analytic stand-in for the split-sum environment BRDF, which is the
// alternative to shipping a lookup table and a pass to bake it. It answers
// "how much of the environment does a surface with this f0 and this roughness
// return at this angle", and it is what puts the grazing sheen on a rough
// surface — the thing that reads as a real reflection rather than a decal.
fn env_brdf(f0: vec3<f32>, rough: f32, ndv: f32) -> vec3<f32> {
    let c0 = vec4<f32>(-1.0, -0.0275, -0.572, 0.022);
    let c1 = vec4<f32>(1.0, 0.0425, 1.04, -0.04);
    let r = rough * c0 + c1;
    let a004 = min(r.x * r.x, exp2(-9.28 * ndv)) * r.x + r.y;
    let ab = vec2<f32>(-1.04, 1.04) * a004 + r.zw;
    return f0 * ab.x + vec3<f32>(ab.y);
}

// ---- how blurred a reflection should be -------------------------------------
//
// A GGX lobe's half-angle is very nearly its `alpha`, which is the SQUARE of the
// roughness slider, and that angle is what a blur has to match. Every chain
// below is box-filtered, so level m blurs by one texel × 2^m: the level whose
// blur matches the lobe is log2(lobe ÷ texel).
//
// **This used to be `sqrt(roughness) × levels` everywhere, which is backwards.**
// The stated intent was to spend more of the chain on the polished end, but sqrt
// LIFTS small values: roughness 0.1 came out at 0.32 and landed three levels up
// the chain — an eightfold blur on a surface the author had asked to be nearly a
// mirror. Only an exact 0 stayed sharp, and no slider sits exactly on 0. That
// one line is why a mirror was easy to frost and hard to polish.
fn lobe_alpha(rough: f32) -> f32 {
    let r = clamp(rough, 0.0, 1.0);
    return max(r * r, 1e-5);
}

// For a map that spans the whole sphere across its width — the sky, and each
// reflection probe — one texel is TAU/width radians wherever you read it.
fn equirect_mip(rough: f32, levels: f32, width: f32) -> f32 {
    let texel = TAU / max(width, 1.0);
    return clamp(log2(lobe_alpha(rough) / texel), 0.0, levels);
}

// For the screen-space chain, whose texels are not angles: take the lobe's WORLD
// radius where the ray landed and measure it in pixels by projecting it.
//
// This is the part a roughness curve cannot express on its own, because it does
// not know how far the ray went. The same polished floor should mirror a chair
// leg an inch away and blur the far wall, and the difference between those is
// entirely the ray's length.
fn screen_cone_mip(
    rough: f32,
    origin: vec3<f32>,
    at: vec3<f32>,
    axis: vec3<f32>,
    levels: f32,
    h: f32,
) -> f32 {
    let radius = lobe_alpha(rough) * length(at - origin);
    // Any direction across the ray will do — the footprint is a disc.
    var side = cross(axis, vec3<f32>(0.0, 1.0, 0.0));
    if (dot(side, side) < 1e-8) {
        side = cross(axis, vec3<f32>(1.0, 0.0, 0.0));
    }
    let off = normalize(side) * radius;
    let c0 = G.view_proj * vec4<f32>(at, 1.0);
    let c1 = G.view_proj * vec4<f32>(at + off, 1.0);
    if (c0.w <= 1e-4 || c1.w <= 1e-4) {
        return 0.0;
    }
    let px = length(ssr_uv(c1.xy / c1.w) - ssr_uv(c0.xy / c0.w)) * max(h, 1.0);
    return clamp(log2(max(px, 1.0)), 0.0, levels);
}

// What this surface reflects of the sky. `n` and `v` are camera-relative but the
// captured sky is in WORLD directions — which are the same thing here, because
// the view matrix carries no translation and no rotation into the instance data
// (ADR-0015): a camera-relative direction IS a world direction.
fn env_radiance(r: vec3<f32>, rough: f32) -> vec3<f32> {
    let dims = textureDimensions(env_tex, 0);
    if (dims.x <= 1u || dims.y <= 1u) {
        return vec3<f32>(0.0); // no sky captured this frame
    }
    // Direction -> equirectangular uv, the exact inverse of `fs_env`'s mapping.
    let u = atan2(r.z, r.x) * INV_TAU + 0.5;
    let t = acos(clamp(r.y, -1.0, 1.0)) * INV_PI;
    let levels = f32(textureNumLevels(env_tex) - 1u);
    let mip = equirect_mip(rough, levels, f32(dims.x));
    return textureSampleLevel(env_tex, env_samp, vec2<f32>(u, t), mip).rgb;
}

// A screen-space reflection: `color` is what the ray found and `hit` is how much
// to believe it, 0..1.
struct Ssr {
    color: vec3<f32>,
    hit: f32,
};

// Camera-relative point -> the uv it lands on. `G.view_proj` carries no
// translation (ADR-0015), so this is the same mapping the depth prepass wrote.
fn ssr_uv(ndc: vec2<f32>) -> vec2<f32> {
    return vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
}

// The camera-relative position the depth prepass recorded at `uv`, and whether
// anything was drawn there at all.
fn ssr_surface(uv: vec2<f32>, dims: vec2<u32>) -> vec4<f32> {
    let fd = vec2<f32>(dims);
    let texel = vec2<i32>(clamp(uv * fd, vec2<f32>(0.0), fd - vec2<f32>(1.0)));
    let d = textureLoad(prime_tex, texel, 0).x;
    if (d >= 1.0) {
        return vec4<f32>(0.0); // sky: nothing to hit here
    }
    let ndc = vec2<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0);
    let sp = G.inv_view_proj * vec4<f32>(ndc, d, 1.0);
    return vec4<f32>(sp.xyz / sp.w, 1.0);
}

// March the depth prepass along the reflected ray and, on a hit, read the colour
// the scene had there last frame.
//
// **Why the previous frame.** Shading is forward: when this fragment runs, most
// of the other pixels' colours do not exist yet. There is no current picture to
// sample, so the reflection reads the one that finished — see `ssr.rs` for why
// that beats the deferred rewrite. The cost is a frame of lag in the CONTENTS of
// a reflection; the geometry of it is this frame's, because the march is against
// this frame's depth.
//
// **The march is uniform in SCREEN space, not in world space.** Equal world
// steps crowd hundreds of samples into the few pixels near the surface and then
// leap whole objects in the distance, which is exactly backwards: a reflection
// is read at screen resolution. Stepping the uv line and recovering the world
// position per sample (perspective-correct, by interpolating 1/w) spends the
// budget where the pixels are.
fn ssr_trace(p: vec3<f32>, n: vec3<f32>, r: vec3<f32>, rough: f32, pix: vec2<u32>) -> Ssr {
    var out: Ssr;
    out.color = vec3<f32>(0.0);
    out.hit = 0.0;
    if (G.ssr.x < 0.5) {
        return out; // off, or no stored picture yet
    }
    let sdims = textureDimensions(scene_tex, 0);
    let ddims = textureDimensions(prime_tex, 0);
    // Either stand-in being 1×1 means the piece it stands for is absent. No flag
    // to keep in step: what is bound IS the answer.
    if (sdims.x <= 1u || sdims.y <= 1u || ddims.x <= 1u || ddims.y <= 1u) {
        return out;
    }

    // **Rough surfaces do not need the march.** Past about a quarter of the
    // roughness range the lobe is wide enough that the hit is blurred to the top
    // of the chain, where it is indistinguishable from what the probe or the sky
    // already says — so the march buys nothing and costs the most of anything in
    // the frame. Faded rather than cut, or the change of answer draws a line
    // across every surface whose roughness crosses the threshold.
    let sharp = 1.0 - smoothstep(SSR_ROUGH_FADE, SSR_ROUGH_MAX, rough);
    if (sharp <= 0.0) {
        return out;
    }

    let reach = max(G.ssr.y, 0.001);
    // Sharper surfaces get more of the budget. A mirror shows the step size as
    // banding and needs every sample; a satin one is about to blur the result
    // anyway, so the same count is spent on detail nobody can see.
    let steps = i32(clamp(G.ssr.z * mix(0.35, 1.0, sharp), 8.0, 64.0));
    let thickness = max(G.ssr.w, 0.001);
    // Start off the surface along the normal, or the first sample reads the very
    // texel this fragment is shading and every surface reflects itself. Scaled by
    // distance because a texel covers more world the further away it is.
    let bias = max(length(p) * 0.002, 0.005);
    let p0 = p + n * bias;
    let p1 = p0 + r * reach;

    let c0 = G.view_proj * vec4<f32>(p0, 1.0);
    let c1 = G.view_proj * vec4<f32>(p1, 1.0);
    // A ray that ends behind the eye has no screen line to walk. Clipping it to
    // the near plane would buy a sliver of extra reflection and a pile of
    // special cases; the environment map answers these instead.
    if (c0.w <= 1e-4 || c1.w <= 1e-4) {
        return out;
    }
    let q0 = 1.0 / c0.w;
    let q1 = 1.0 / c1.w;
    let uv0 = ssr_uv(c0.xy * q0);
    let uv1 = ssr_uv(c1.xy * q1);
    // Perspective-correct interpolation: position/w and 1/w are BOTH linear in
    // screen space, their ratio is the world point. Interpolating the position
    // alone is the classic wrong version — it is linear in world space, so the
    // samples bunch toward whichever end happens to be nearer the camera.
    let pq0 = p0 * q0;
    let pq1 = p1 * q1;

    // Dither the phase so the march's own step size stops being visible. Without
    // it a mirror shows concentric bands at the sample distances — a texture the
    // scene does not have, which reads as the reflection being broken rather
    // than as it being cheap.
    let jitter = bayer4(pix);
    var prev_f = 0.0;
    let inv = 1.0 / f32(steps);

    for (var i = 0; i < steps; i = i + 1) {
        let f = (f32(i) + jitter) * inv;
        let uv = mix(uv0, uv1, f);
        if (uv.x < 0.0 || uv.x > 1.0 || uv.y < 0.0 || uv.y > 1.0) {
            return out; // walked off the frame: no data, and the sky takes over
        }
        let q = mix(q0, q1, f);
        if (q <= 1e-9) {
            return out;
        }
        let sample_pos = mix(pq0, pq1, f) / q;
        let surf = ssr_surface(uv, ddims);
        if (surf.w < 0.5) {
            prev_f = f;
            continue; // sky in the depth buffer — nothing here to reflect
        }
        // Positive = the ray has gone BEHIND the surface the camera can see, so
        // it crossed it somewhere between the last sample and this one.
        let dz = length(sample_pos) - length(surf.xyz);
        if (dz > 0.0) {
            // …unless it went so far behind that it is out the other side. The
            // depth buffer records one surface and says nothing about how solid
            // it is; without this window a ray that passes a metre behind a
            // railing reports the railing, and every thin object smears its
            // colour across the reflection of whatever is really there.
            if (dz > thickness + reach * inv) {
                prev_f = f;
                continue;
            }
            // Bisect between the last sample in front and this one behind. Five
            // rounds turn a step-sized error into a thirty-second of one, which
            // is what stops a reflected edge looking serrated.
            var lo = prev_f;
            var hi = f;
            for (var b = 0; b < 5; b = b + 1) {
                let mid = (lo + hi) * 0.5;
                let muv = mix(uv0, uv1, mid);
                let mq = mix(q0, q1, mid);
                let mpos = mix(pq0, pq1, mid) / max(mq, 1e-9);
                let ms = ssr_surface(muv, ddims);
                if (ms.w > 0.5 && length(mpos) - length(ms.xyz) > 0.0) {
                    hi = mid;
                } else {
                    lo = mid;
                }
            }
            let hit_uv = mix(uv0, uv1, hi);
            let hq = mix(q0, q1, hi);
            let hit_pos = mix(pq0, pq1, hi) / max(hq, 1e-9);

            // Where was that point in the stored picture? The world is
            // camera-relative, so this matrix already carries how far the camera
            // moved since — see `SceneHistory::prev_view_proj`.
            let pc = G.ssr_prev_vp * vec4<f32>(hit_pos, 1.0);
            if (pc.w <= 1e-4) {
                return out; // behind the old camera: it had no picture of this
            }
            let puv = ssr_uv(pc.xy / pc.w);
            if (puv.x < 0.0 || puv.x > 1.0 || puv.y < 0.0 || puv.y > 1.0) {
                return out; // off the edge of the stored frame
            }

            // Confidence. Three ways a screen-space hit is a lie, faded rather
            // than cut so the sky takes over smoothly instead of leaving a seam:
            //
            //  1. The frame edge. There is no data past it, and a hard stop
            //     draws a bright line around the picture in every mirror.
            let e = min(min(puv.x, 1.0 - puv.x), min(puv.y, 1.0 - puv.y));
            var conf = smoothstep(0.0, 0.08, e);
            //  2. Rays pointing back at the camera. What they want to show is
            //     behind the viewer, and the screen has never held it.
            conf = conf * (1.0 - smoothstep(0.25, 0.75, dot(r, -normalize(p))));
            //  3. Distance. A long ray crosses more of the scene it cannot see
            //     around, and its hit is the more likely to be the wrong surface.
            conf = conf * (1.0 - smoothstep(0.7, 1.0, hi));
            //  4. Roughness. The crossover to the probe/sky, matching the early
            //     return above so there is no step where the march stops.
            conf = conf * sharp;

            let levels = f32(textureNumLevels(scene_tex) - 1u);
            let mip = screen_cone_mip(rough, p, hit_pos, r, levels, f32(sdims.y));
            let c = textureSampleLevel(scene_tex, scene_samp, puv, mip).rgb;

            // **Bound what one bounce may carry.** The stored picture already
            // contains last frame's reflections, so two mirrors facing each other
            // re-reflect each other's reflection every frame. With a polished
            // metal the loop barely loses anything per pass, and the pair does not
            // settle — it RAMPS, over about a second, into a white blob, which
            // reads as the renderer breaking rather than as a hall of mirrors.
            //
            // Clamping the brightest channel and scaling the others with it keeps
            // the colour and only takes the runaway: an ordinary highlight is far
            // below the cap and passes through untouched.
            //
            // Zero means NO CAP, so a globals block that never learned about this
            // field reflects exactly as it did before. Read the other way round —
            // zero as a cap of zero — an unset uniform would turn every
            // screen-space reflection black, which is the failure this engine
            // keeps meeting and the reason the harmless reading gets the 0.
            var lit = c;
            let cap = G.probe_meta.y;
            if (cap > 0.0) {
                let lum = max(max(c.r, c.g), c.b);
                lit = c * (cap / max(lum, cap));
            }
            out.color = lit;
            out.hit = clamp(conf, 0.0, 1.0);
            return out;
        }
        prev_f = f;
    }
    return out;
}

// What you can see THROUGH this surface: the scene behind it, bent.
//
// **Where the picture comes from.** `scene_tex` during the refraction pass holds
// THIS frame's scene with the glass itself not yet drawn — see `ssr.rs` and the
// pass split in `raster.rs`. That is the whole reason glass gets its own pass:
// the previous frame's picture already has the glass composited into it, so
// refracting that would sample the glass through the glass, and its tint would
// deepen every frame it stayed on screen.
//
// **The bend is a real ray, not a uv nudge.** `refract` gives the direction the
// view ray takes on entering the material; following it for `thick` world units
// and projecting THAT point is what makes the effect scale correctly — a
// windowpane barely shifts what is behind it, a solid ball throws it across, and
// the difference is the thickness rather than a number tuned per material.
//
// At total internal reflection `refract` returns zero, and so does this: those
// rays have nothing behind the surface to show, and the reflection term is
// already what they return.
fn refracted(p: vec3<f32>, n: vec3<f32>, v: vec3<f32>, rough: f32, ior: f32, thick: f32) -> vec3<f32> {
    let dims = textureDimensions(scene_tex, 0);
    if (dims.x <= 1u || dims.y <= 1u) {
        return vec3<f32>(0.0); // no capture of what is behind: nothing to show
    }
    let t = refract(-v, n, 1.0 / max(ior, 1.0));
    if (dot(t, t) < 1e-8) {
        return vec3<f32>(0.0);
    }
    // How far the bent ray travels before we look along it: through the glass,
    // and then ON to whatever is actually behind it.
    //
    // The second half is what makes this read as glass rather than as a slight
    // smudge. A bend only displaces what you see in proportion to how far the
    // ray travels afterwards, so marching the material's own thickness and
    // stopping — the usual approximation — gives a marble on a table and a ball
    // held against a distant wall exactly the same tiny shift, when the second
    // should be throwing the wall around. The depth prepass already knows that
    // distance, and glass is deliberately absent from it, so what it reports at
    // this pixel IS the scene behind.
    var travel = max(thick, 0.0);
    let straight_clip = G.view_proj * vec4<f32>(p, 1.0);
    if (straight_clip.w > 1e-4) {
        let here = ssr_uv(straight_clip.xy / straight_clip.w);
        let ddims = textureDimensions(prime_tex, 0);
        if (ddims.x > 1u && ddims.y > 1u
            && here.x >= 0.0 && here.x <= 1.0 && here.y >= 0.0 && here.y <= 1.0) {
            let behind = ssr_surface(here, ddims);
            if (behind.w > 0.5) {
                travel = travel + max(length(behind.xyz) - length(p), 0.0);
            }
        }
    }
    let exit = p + t * travel;
    let clip = G.view_proj * vec4<f32>(exit, 1.0);
    if (clip.w <= 1e-4) {
        return vec3<f32>(0.0);
    }
    var uv = ssr_uv(clip.xy / clip.w);
    // Off the edge of the frame there is no picture of what is behind. Clamping
    // back to the border smears the edge pixel across the whole rim of a glass
    // object; falling back to the UNBENT sample keeps it showing the scene it is
    // actually in, which is wrong by a few pixels instead of wrong by a streak.
    if (uv.x < 0.0 || uv.x > 1.0 || uv.y < 0.0 || uv.y > 1.0) {
        let straight = G.view_proj * vec4<f32>(p, 1.0);
        if (straight.w <= 1e-4) {
            return vec3<f32>(0.0);
        }
        uv = clamp(ssr_uv(straight.xy / straight.w), vec2<f32>(0.0), vec2<f32>(1.0));
    }
    // Roughness frosts it, by the same rule the sky and the screen-space
    // reflection use — so a frosted pane blurs what is behind it by as much as it
    // blurs what it reflects, and a pane held against a wall frosts it less than
    // the same pane held across the room.
    let levels = f32(textureNumLevels(scene_tex) - 1u);
    let mip = screen_cone_mip(
        rough, p, exit, t, levels, f32(textureDimensions(scene_tex, 0).y));
    return textureSampleLevel(scene_tex, scene_samp, uv, mip).rgb;
}

// What this surface reflects: the scene where a screen-space ray found it, and
// the sky everywhere else.
//
// `n` and `v` are camera-relative but the captured sky is in WORLD directions —
// which are the same thing here, because the view matrix carries no translation
// and no rotation into the instance data (ADR-0015): a camera-relative direction
// IS a world direction.
//
// The two are mixed BEFORE the BRDF, not added after it. A reflection is one
// lobe looking in one direction; whether the thing it lands on is a wall or the
// horizon does not change how much light the surface returns, only what colour
// it is. Adding them would make any surface with a partial screen-space hit
// brighter than a mirror, which is the artefact that reads as "reflections glow".
// How much probe `i` speaks for the point `p`.
//
// 1 anywhere inside its box and fading to 0 over `probe_half.w` OUTSIDE it. The
// fade is on the outside deliberately: a surface standing against a wall is the
// one that most needs the room's reflection, so a weight that tailed off as it
// approached the wall would drop the probe exactly where it matters. Two
// overlapping rooms cross-fade instead across the strip where one box has ended
// and the other has not.
fn probe_weight(i: u32, p: vec3<f32>) -> f32 {
    let d = abs(p - G.probe_pos[i].xyz) - G.probe_half[i].xyz;
    let outside = max(max(d.x, d.y), d.z); // negative inside the box
    let fade = max(G.probe_half[i].w, 1e-3);
    return clamp((fade - outside) / fade, 0.0, 1.0);
}

// What probe `i` shows along the reflected ray `r` from `p`.
//
// **The box intersection is the whole feature.** Sampled by direction alone, an
// environment map is a picture at infinity: move the camera and every reflection
// slides with it, so a reflected wall never sits on the wall. Meeting the ray
// with the probe's box first, and then aiming from the PROBE at that meeting
// point, puts the reflection where the geometry is. It is exact when the room
// really is a box and gracefully approximate when it is not, which is why the
// box is authored rather than derived.
fn probe_radiance(i: u32, p: vec3<f32>, r: vec3<f32>, rough: f32) -> vec3<f32> {
    let c = G.probe_pos[i].xyz;
    let h = max(G.probe_half[i].xyz, vec3<f32>(1e-3));
    // Slab test against the box, taking the FAR hit: the ray starts inside, so
    // the near intersections are behind it.
    let inv = 1.0 / select(r, vec3<f32>(1e-8), abs(r) < vec3<f32>(1e-8));
    let t1 = (c + h - p) * inv;
    let t2 = (c - h - p) * inv;
    let tf = min(min(max(t1.x, t2.x), max(t1.y, t2.y)), max(t1.z, t2.z));
    // A point outside the box can produce a negative or degenerate hit; fall
    // back to the plain direction there, which is the picture-at-infinity answer
    // and the right one once you are far enough away for the box not to matter.
    let dir = select(r, normalize(p + r * tf - c), tf > 1e-4);
    let u = atan2(dir.z, dir.x) * INV_TAU + 0.5;
    let t = acos(clamp(dir.y, -1.0, 1.0)) * INV_PI;
    let levels = f32(textureNumLevels(probe_tex) - 1u);
    let mip = equirect_mip(rough, levels, f32(textureDimensions(probe_tex, 0).x));
    return textureSampleLevel(probe_tex, probe_samp, vec2<f32>(u, t), i32(i), mip).rgb
        * max(G.probe_pos[i].w, 0.0);
}

// The environment this surface sits in: the probes whose rooms it stands in,
// and the sky for whatever is left over.
//
// The sky is the FALLBACK rather than an addition. Indoors it is the wrong
// answer entirely — a sealed room lit by daylight through its own ceiling — and
// outdoors there are no probes, so the weights sum to zero and this returns the
// sky exactly as it did before probes existed.
fn env_radiance_probed(p: vec3<f32>, r: vec3<f32>, rough: f32) -> vec3<f32> {
    let sky = env_radiance(r, rough);
    let dims = textureDimensions(probe_tex, 0);
    let count = min(u32(G.probe_meta.x), 4u);
    if (count == 0u || dims.x <= 1u || dims.y <= 1u) {
        return sky;
    }
    var acc = vec3<f32>(0.0);
    var wsum = 0.0;
    for (var i = 0u; i < count; i = i + 1u) {
        let w = probe_weight(i, p);
        if (w > 0.0) {
            acc = acc + probe_radiance(i, p, r, rough) * w;
            wsum = wsum + w;
        }
    }
    if (wsum <= 0.0) {
        return sky;
    }
    // Normalised, so one probe covering a room gives that room's picture at full
    // strength and two overlapping ones average rather than double. The
    // un-normalised total is what decides how much sky is left — which is how a
    // surface walking out of a doorway crosses back to the sky smoothly.
    return mix(sky, acc / wsum, min(wsum, 1.0));
}

fn env_specular(p: vec3<f32>, n: vec3<f32>, v: vec3<f32>, f0: vec3<f32>, rough: f32, pix: vec2<u32>) -> vec3<f32> {
    let r = reflect(-v, n);
    let sky = env_radiance_probed(p, r, rough);
    let s = ssr_trace(p, n, r, rough, pix);
    let radiance = mix(sky, s.color, s.hit);
    let ndv = max(dot(n, v), 1e-4);
    return radiance * env_brdf(f0, rough, ndv);
}

// Accumulated diffuse from the point lights at camera-relative position `pos_rel`
// (same space as point_pos) with surface normal `n`. Smooth falloff to 0 at range.
fn point_diffuse(pos_rel: vec3<f32>, n: vec3<f32>) -> vec3<f32> {
    var acc = vec3<f32>(0.0);
    let count = min(u32(g.point_count.x), 16u);
    for (var i = 0u; i < count; i = i + 1u) {
        let lp = g.point_pos[i];
        // See `area_terms` in field.wgsl — an emitter with no size gives back
        // exactly the point light this used to compute inline.
        let a = area_terms(g.point_shape[i], g.point_rot[i], g.point_cone[i], lp.xyz - pos_rel, n, n);
        let x = clamp(1.0 - a.dist / max(lp.w, 0.0001), 0.0, 1.0);
        acc = acc + g.point_color[i].rgb * (a.ndl * x * x * a.atten);
    }
    return acc;
}

// [`point_diffuse`] with local shadows — the FRAGMENT-stage version.
//
// Two functions rather than one with a flag, because the vertex stage calls the
// one above. `point_vis` reads the depth prepass and the field globals, which
// are bound for FRAGMENT only; reaching for them from `vs` does not fail at the
// shading site, it fails at PIPELINE CREATION, for every raster draw in the
// engine. (Per-vertex lighting has no per-pixel shadow to give anyway — the
// retro path skips the sun's shadow march for the same reason.)
fn point_diffuse_lit(pos_rel: vec3<f32>, n: vec3<f32>, pix: vec2<u32>) -> vec3<f32> {
    var acc = vec3<f32>(0.0);
    let count = min(u32(g.point_count.x), 16u);
    for (var i = 0u; i < count; i = i + 1u) {
        let lp = g.point_pos[i];
        let a = area_terms(g.point_shape[i], g.point_rot[i], g.point_cone[i], lp.xyz - pos_rel, n, n);
        let x = clamp(1.0 - a.dist / max(lp.w, 0.0001), 0.0, 1.0);
        if (x <= 0.0 || a.atten <= 0.0) { continue; }
        acc = acc + g.point_color[i].rgb * (a.ndl * x * x * a.atten) * point_vis(pos_rel, n, i, pix);
    }
    return acc;
}

// 0 while "show only the bounce" is on, 1 otherwise — the multiplier that
// switches every direct light off at once. See `key_light` in field.wgsl.
fn gi_only_gate() -> f32 {
    return select(1.0, 0.0, G.gi_meta.y > 0.5);
}

struct VsIn {
    // Indexes this vertex's slot in the mesh's `vpaint` block. Under an indexed draw
    // with base_vertex = 0 (what this pass issues) this is the index-buffer value —
    // i.e. the same index the paint block was built against at import.
    @builtin(vertex_index) vid: u32,
    @location(0) pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) m0: vec4<f32>,
    @location(4) m1: vec4<f32>,
    @location(5) m2: vec4<f32>,
    @location(6) m3: vec4<f32>,
    @location(7) n0: vec4<f32>,       // xyz = normal-matrix column 0; w = terrain color base
    @location(8) n1: vec4<f32>,
    @location(9) n2: vec4<f32>,
    @location(10) color: vec4<f32>,
    @location(11) emissive: vec4<f32>,  // rgb, a = strength
    @location(12) specular: vec4<f32>,  // rgb, a = strength
    @location(13) params: vec4<f32>,    // shininess, rim_strength, unlit, ambient_mul
    @location(14) rim: vec4<f32>,       // rgb; w = packed tiling flags (mode + rot·10·4)
    @location(15) tile: vec4<f32>,      // uv: count.xy, offset.xy | triplanar: scale, blend
};

struct VsOut {
    // `@invariant` guarantees the depth prepass and the color pass compute
    // byte-identical positions from the same inputs, so the color pass's
    // fragments always pass `LessEqual` against their own primed depth.
    @invariant @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) color: vec4<f32>,
    // The fragment's position relative to the camera (the model matrix is already
    // camera-relative, ADR-0015), so the camera sits at the origin — view dir is
    // just -normalize(view_pos). Used for specular + rim.
    @location(3) view_pos: vec3<f32>,
    @location(4) emissive: vec4<f32>,
    @location(5) specular: vec4<f32>,
    @location(6) params: vec4<f32>,
    @location(7) rim: vec4<f32>,
    @location(8) tile: vec4<f32>,
    // Object-local position + normal: what triplanar projects along, so the
    // texture STICKS to the object (camera-relative space would swim under the
    // floating origin, ADR-0015).
    @location(9) lpos: vec3<f32>,
    @location(10) lnorm: vec3<f32>,
    // This vertex's painted color, or white when the instance is unpainted. Unlike
    // `params`, this SHOULD interpolate — that gradient across the triangle is the
    // whole point of vertex painting.
    @location(11) vcolor: vec4<f32>,
    // Two per-instance constants that must NOT interpolate: x = the meshed-terrain
    // splat flag (0/1, from `n2.w`) — interpolating it would make the fs threshold
    // wrong at triangle edges — and y = this instance's SURFACE EXTRAS index,
    // where interpolating a ~16.7M integer could land one entry over and read
    // another material's roughness. They share one varying because the raster's
    // inter-stage budget is nearly as tight as its attribute budget.
    @location(12) @interpolate(flat) imeta: vec2<f32>,
    // The SAME UVs, interpolated WITHOUT the perspective divide — the PS1's
    // affine texture mapping, and the swimming it produces on big near-camera
    // polygons. Always emitted (a varying costs nothing to write); `surface_uv`
    // picks between this and `uv` per material. Two varyings rather than a
    // shader variant, because the choice is per-INSTANCE and instances batch.
    @location(13) @interpolate(linear) uv_affine: vec2<f32>,
    // Per-VERTEX lighting, interpolated across the face (the Gouraud look).
    // Computed in `vs` from the group(0) globals only — group(2)'s field is
    // fragment-visible, so a vertex-lit surface receives no SDF shadow and no
    // AO. That is not a shortcut around a limit: hardware that shaded per vertex
    // had neither, and a faceted highlight sliding across a face is the look.
    @location(14) vlit: vec3<f32>,
};

@vertex
fn vs(in: VsIn) -> VsOut {
    // `n0.w` is the terrain color base and `n2.w` the terrain-splat flag on this
    // path; `vs_skin` spends the same two lanes on skinning, which is safe
    // because terrain is never skinned.
    return build_vs(in, in.pos, in.normal, u32(in.n0.w), in.n2.w);
}

/// The skinned entry point (`floptle/0080`): deform the vertex by the bone
/// palette here instead of on the CPU, then run the identical shading tail.
///
/// The arithmetic is the same as `cpu_skin_part` line for line — weights
/// normalized by their own sum, a zero-weight vertex falling back to the part's
/// node matrix (which is what the importer's zero-weight pad means) — so the two
/// paths cannot drift. What changes is where it happens: the CPU path deformed
/// every vertex of every character every frame and re-uploaded the result to a
/// PRIVATE per-entity vertex buffer, because two characters sharing one `.glb`
/// would otherwise share one buffer and the last one baked would win for both.
/// Here every instance reads the same bind-pose buffer and supplies its own
/// palette, so that whole mechanism has nothing left to do.
@vertex
fn vs_skin(in: VsIn) -> VsOut {
    let entry = skin_meta[min(u32(in.n0.w), arrayLength(&skin_meta) - 1u)];
    let sbase = entry.x;
    let pbase = entry.y;
    let idx = min(sbase + in.vid, arrayLength(&skin_joints) - 1u);
    let j = skin_joints[idx];
    let w = skin_weights[min(sbase + in.vid, arrayLength(&skin_weights) - 1u)];
    let wsum = w.x + w.y + w.z + w.w;
    var m = skin_palette[min(pbase, arrayLength(&skin_palette) - 1u)];
    if (wsum > 1e-4) {
        let n = arrayLength(&skin_palette) - 1u;
        let inv = 1.0 / wsum;
        m = skin_palette[min(pbase + 1u + j.x, n)] * (w.x * inv)
          + skin_palette[min(pbase + 1u + j.y, n)] * (w.y * inv)
          + skin_palette[min(pbase + 1u + j.z, n)] * (w.z * inv)
          + skin_palette[min(pbase + 1u + j.w, n)] * (w.w * inv);
    }
    let p = (m * vec4<f32>(in.pos, 1.0)).xyz;
    let raw_n = mat3x3<f32>(m[0].xyz, m[1].xyz, m[2].xyz) * in.normal;
    let len = length(raw_n);
    // `normalize_or_zero`, matching the CPU path: a fully-collapsed bone would
    // otherwise produce a NaN normal and a black — or missing — triangle.
    let n = select(vec3<f32>(0.0), raw_n / len, len > 1e-6);
    // A skinned instance spends `n0.w` on its skin table index, so it can carry
    // no terrain color block — and terrain is never skinned, so the splat flag is
    // constant here too.
    return build_vs(in, p, n, 0u, 0.0);
}

/// Everything after the vertex has its final object-space position and normal:
/// clip position, the material varyings, triplanar space and the vertex-paint
/// unpack. Shared by `vs` and `vs_skin` so a skinned mesh cannot shade
/// differently from an unskinned one.
fn build_vs(in: VsIn, pos: vec3<f32>, normal: vec3<f32>, tbase_in: u32, tsplat_in: f32) -> VsOut {
    let model = mat4x4<f32>(in.m0, in.m1, in.m2, in.m3);
    let nmat = mat3x3<f32>(in.n0.xyz, in.n1.xyz, in.n2.xyz);
    var out: VsOut;
    let view_pos = model * vec4<f32>(pos, 1.0);
    var clip = g.view_proj * view_pos;

    // --- The surface extras, decoded ONCE, here. ---------------------------
    // `n1.w` packs `modulate_bit | (ext_index << 1)` on the same rule params.z
    // follows, and for the same reason: read off the attribute it is exact,
    // read off a varying it has been interpolated.
    let nw = u32(in.n1.w);
    let modul = (nw & 1u) != 0u;
    let ext_index = nw >> 1u;
    let ext = ext_at(ext_index);
    out.imeta = vec2<f32>(tsplat_in, f32(ext_index));

    // RETRO — vertex jitter. The PS1 had no fractional vertex coordinates: it
    // snapped to a screen grid, and geometry near the camera wobbled between
    // grid cells as it moved. `jitter` is that grid's steps across the viewport.
    //
    // Snapping in NDC and scaling back by w keeps the perspective divide honest.
    // A vertex at or behind the eye plane (w ≈ 0) is left alone — dividing there
    // sends it to infinity, which shows up as a triangle stretched across the
    // whole screen rather than as a subtle wobble.
    if (ext.jitter > 0.0 && abs(clip.w) > 1e-4) {
        let ndc = clip.xy / clip.w;
        clip = vec4<f32>(round(ndc * ext.jitter) / ext.jitter * clip.w, clip.z, clip.w);
    }
    out.clip = clip;
    out.uv = in.uv;
    out.uv_affine = in.uv;
    out.normal = normalize(nmat * normal);
    out.color = in.color;
    out.view_pos = view_pos.xyz;
    out.emissive = in.emissive;
    out.specular = in.specular;
    out.rim = in.rim;
    out.tile = in.tile;
    // Triplanar space is SCALE-AWARE object-local: multiply the mesh-local
    // position by the model's per-axis scale so texture density stays in
    // WORLD units. Without this, a unit cube stretched to a 48-unit wall
    // spreads ONE tile across the whole wall (Triplanar `scale` reads as
    // "world units per tile"). Terrain chunks bake at scale 1, so their
    // splat path (which shares lpos) is unchanged.
    let mscale = vec3<f32>(length(in.m0.xyz), length(in.m1.xyz), length(in.m2.xyz));
    out.lpos = pos * mscale;
    out.lnorm = normal;

    // --- Vertex paint: unpack params.z, and let the packing DIE HERE. -------------
    // params.z arrives packed as `unlit_bit | (paint_base << 1)`. Two reasons the
    // decode belongs in the vertex shader and nowhere else:
    //   1. fs tests `params.z > 0.5` as a THRESHOLD, so a packed value there would
    //      make every painted node render unlit. We re-emit a clean 0/1 below.
    //   2. `in.params` is read here straight off the INSTANCE ATTRIBUTE — exact.
    //      `VsOut.params` is perspective-interpolated, and interpolating a ~16.7M
    //      integer-as-float can land off-by-one and read another block's colors.
    //      Decoding pre-interpolation makes that impossible rather than unlikely.
    let pz = u32(in.params.z);
    let unlit = (pz & 1u) != 0u;
    let pbase = pz >> 1u;                       // 0 = this instance has no paint
    out.params = vec4<f32>(in.params.x, in.params.y, select(0.0, 1.0, unlit), in.params.w);

    // `select` evaluates BOTH arms, so the index must be in bounds even when unpainted
    // (pbase = 0, vid unbounded). Clamp rather than lean on driver robustness.
    let idx = min(pbase + in.vid, arrayLength(&vpaint) - 1u);
    let raw = unpack4x8unorm(vpaint[idx]);
    // MODULATE 2× (n1.w flag): brush paint is LIGHT, not just shadow. The multiply in `fs`
    // can only ever darken (white = ×1 = no effect), so "paint white" did nothing — the
    // exact complaint. Doubling the paint here makes mid-grey (0.5) the neutral point:
    // below grey darkens, above grey brightens up to 2×, so an artist paints baked light
    // and shadow in one stroke. Imported glTF COLOR_0 keeps the plain multiply (flag off),
    // because the glTF spec defines COLOR_0 as a linear ×1 multiply and doubling it would
    // silently over-brighten every imported vertex-coloured mesh. Alpha is never doubled —
    // it stays opacity.
    let prgb = select(raw.rgb, raw.rgb * 2.0, modul);
    var vc = select(vec4<f32>(1.0), vec4<f32>(prgb, raw.a), pbase != 0u);

    // Terrain chunk color rides the SAME varying from its own store (n0.w, no packing —
    // the lane is not shared with anything). An instance never has both bases, so the
    // order of these two only decides which wins in a case that cannot arise.
    let tbase = tbase_in;
    let tidx = min(tbase + in.vid, arrayLength(&tpaint) - 1u);
    out.vcolor = select(vc, unpack4x8unorm(tpaint[tidx]), tbase != 0u);

    // RETRO — per-vertex (Gouraud) lighting. Only the group(0) globals are
    // vertex-visible, so this is the key light + the placeable point lights and
    // nothing else: no shadow march, no SDF AO. See `VsOut.vlit`.
    out.vlit = vec3<f32>(0.0);
    if (ext_has(ext, EXT_VERTEX_LIT)) {
        let n = out.normal;
        let ndl = max(dot(n, g.light_dir.xyz), 0.0);
        out.vlit = g.light_color.rgb * ndl + point_diffuse(view_pos.xyz, n);
    }
    return out;
}

// ---- Surface maps -----------------------------------------------------------

// The UVs this material actually samples with: affine (no perspective divide) on
// a retro surface, perspective-correct everywhere else.
fn surface_uv(in: VsOut) -> vec2<f32> {
    let ext = ext_at(u32(in.imeta.y));
    return select(in.uv, in.uv_affine, ext_has(ext, EXT_AFFINE_UV));
}

// A tangent frame built from SCREEN-SPACE DERIVATIVES rather than from a stored
// tangent attribute.
//
// This is a deliberate choice, not a shortcut. The raster vertex stream is FULL
// at 16 attributes, so there is no room for a tangent — but more to the point,
// most of what this engine draws could never carry one: SDF terrain is extracted
// by surface nets every time it is sculpted, primitives and Model-tool meshes are
// generated, tilemaps are built per frame. A per-pixel frame works on all of them
// and on skinned characters too, because it reads the position AFTER skinning.
//
// The frame is re-orthogonalised against the interpolated normal, so the smooth
// shading normal still wins and only the tangent DIRECTION comes from the
// derivatives. `p` is camera-relative position; `n` the (already flipped)
// shading normal.
fn tangent_frame(p: vec3<f32>, n: vec3<f32>, uv: vec2<f32>) -> mat3x3<f32> {
    let dp1 = dpdx(p);
    let duv1 = dpdx(uv);
    // NEGATED, both of them, and that is the whole difference between a normal
    // map that reads as bumps and one that reads as dents.
    //
    // The standard cotangent-frame derivation assumes screen +y points UP (the
    // OpenGL convention it was written for). Here it points DOWN — the same fact
    // ssao.wgsl relies on when it says `ddy × ddx` faces the camera. Under a
    // downward y, `dpdy` of both position and UV come back with the opposite
    // sign, which negates T and B together: every tangent-space normal ends up
    // tilted the wrong way in BOTH axes. Nothing looks broken — the surface is
    // lit, the highlight moves — it is just inside out, and the `pbr_probe`
    // caught it as "the half tilted toward the light is the DARK one".
    //
    // Taking the derivative with respect to an upward y is one negation each and
    // leaves the rest of the formula exactly as published.
    let dp2 = -dpdy(p);
    let duv2 = -dpdy(uv);
    let dp2perp = cross(dp2, n);
    let dp1perp = cross(n, dp1);
    var t = dp2perp * duv1.x + dp1perp * duv2.x;
    var b = dp2perp * duv1.y + dp1perp * duv2.y;
    // A face with no UV variation (an untextured primitive, a degenerate island)
    // gives a zero tangent — normalizing that is a NaN and a black triangle. Fall
    // back to any frame perpendicular to n: with a flat normal map bound it
    // produces the geometric normal either way, so the fallback is invisible.
    let len2 = max(dot(t, t), dot(b, b));
    if (len2 < 1e-20) {
        let up = select(vec3<f32>(0.0, 1.0, 0.0), vec3<f32>(1.0, 0.0, 0.0), abs(n.y) > 0.9);
        t = normalize(cross(up, n));
        b = cross(n, t);
        return mat3x3<f32>(t, b, n);
    }
    let inv = inverseSqrt(len2);
    return mat3x3<f32>(t * inv, b * inv, n);
}

// The UVs the four SURFACE MAPS sample with: `surface_uv` put through the same
// UV-tiling transform the base colour takes, so a tiled brick wall's normal map
// tiles WITH its albedo instead of stretching once across the whole surface.
//
// Triplanar (mode 2) is the exception: the maps fall back to plain mesh UVs,
// because a triplanar normal map has to be projected and re-oriented per axis —
// its own piece of work, not a line here.
fn map_uv(in: VsOut) -> vec2<f32> {
    let uv0 = surface_uv(in);
    let flags = u32(in.rim.w + 0.5);
    if ((flags & 3u) == 1u) {
        let rot = f32(flags >> 2u) * 0.1 * 0.017453292519943295;
        let c = cos(rot);
        let sn = sin(rot);
        let m = mat2x2<f32>(vec2<f32>(c, sn), vec2<f32>(-sn, c));
        return m * ((uv0 - 0.5) * in.tile.xy) + 0.5 + in.tile.zw;
    }
    return uv0;
}

// The shading normal after the normal map. `strength` scales the tilt; NEGATIVE
// flips the green channel, which is the one-knob fix for a map authored in the
// other handedness (the difference that makes every dent read as a bump).
fn mapped_normal(in: VsOut, n: vec3<f32>, uv: vec2<f32>, strength: f32) -> vec3<f32> {
    let s = textureSample(normal_tex, normal_samp, uv).xyz * 2.0 - 1.0;
    // A flat map is exactly (0, 0, 1) and this whole function is then the
    // identity — which is why an unmapped material needs no branch anywhere.
    let tilt = vec2<f32>(s.x, s.y * sign(strength)) * abs(strength);
    let tn = normalize(vec3<f32>(tilt, max(s.z, 1e-3)));
    let tbn = tangent_frame(in.view_pos, n, uv);
    return normalize(tbn * tn);
}

// ---- Metal-rough (Cook-Torrance GGX) ---------------------------------------
//
// The `Shading::Physical` half of the two lighting models. The other half stays
// exactly the Blinn-Phong it always was: this is an alternative, not a
// replacement, because a stylised surface wants a highlight set by hand and a
// realistic one wants a highlight that falls out of a measured roughness.

fn d_ggx(ndh: f32, a: f32) -> f32 {
    let a2 = a * a;
    let d = ndh * ndh * (a2 - 1.0) + 1.0;
    return a2 / max(3.14159265 * d * d, 1e-7);
}

// Smith height-correlated visibility (the G term already divided by 4·NdV·NdL).
fn v_smith(ndv: f32, ndl: f32, a: f32) -> f32 {
    let a2 = a * a;
    let gv = ndl * sqrt(ndv * ndv * (1.0 - a2) + a2);
    let gl = ndv * sqrt(ndl * ndl * (1.0 - a2) + a2);
    return 0.5 / max(gv + gl, 1e-7);
}

fn f_schlick(f0: vec3<f32>, vdh: f32) -> vec3<f32> {
    return f0 + (vec3<f32>(1.0) - f0) * pow(clamp(1.0 - vdh, 0.0, 1.0), 5.0);
}

// One light's contribution: `.xyz` = the specular lobe, `.w` = the diffuse
// weight the caller multiplies by albedo. Split that way so the caller can keep
// applying AO and vertex paint to the diffuse half only.
fn ggx_light(n: vec3<f32>, v: vec3<f32>, l: vec3<f32>, f0: vec3<f32>, rough: f32) -> vec4<f32> {
    let ndl = max(dot(n, l), 0.0);
    if (ndl <= 0.0) {
        return vec4<f32>(0.0);
    }
    let h = normalize(l + v);
    let ndv = max(dot(n, v), 1e-4);
    let ndh = max(dot(n, h), 0.0);
    let vdh = max(dot(v, h), 0.0);
    // Perceptual roughness squared — the mapping that makes a 0..1 slider feel
    // linear. Floored so a mirror still has a highlight with an area, instead of
    // a single blindingly bright pixel that aliases into sparkle.
    let a = max(rough * rough, 1e-3);
    let f = f_schlick(f0, vdh);
    let spec = f * (d_ggx(ndh, a) * v_smith(ndv, ndl, a) * ndl);
    // Energy conservation: whatever the surface reflects specularly cannot also
    // come back diffusely. This is what stops "add a highlight" from also
    // meaning "make the surface brighter overall".
    let kd = (1.0 - max(max(f.r, f.g), f.b));
    return vec4<f32>(spec, kd * ndl * 0.3183098862);
}

/// The key light(s) under the metal-rough model — the GGX twin of field.wgsl's
/// `key_light`, and deliberately its mirror image: the same star loop, the same
/// `star_shadow` / `sun_shadow` calls in the same places, so a Physical surface
/// receives EXACTLY the shadows a Classic one does and the two models can only
/// differ in the BRDF. It lives here rather than in field.wgsl because the
/// raymarch pass includes field.wgsl too and has no `ggx_light`.
fn key_light_ggx(p: vec3<f32>, n: vec3<f32>, v: vec3<f32>, f0: vec3<f32>, rough: f32, pix: vec2<u32>) -> KeyLight {
    var out: KeyLight;
    out.diffuse = vec3<f32>(0.0);
    out.spec = vec3<f32>(0.0);
    if (G.gi_meta.y > 0.5) {
        return out; // "show only the bounce" — see key_light() in field.wgsl
    }
    // **A key light that emits nothing still cast a shadow.** The march below is
    // gated on the BRDF, which is geometry and is nonzero almost everywhere, so
    // an interior with the sun turned down to zero — every indoor scene — paid
    // for a 64-step shadow trace per pixel and multiplied the result by black.
    // Intensity is already folded into `light_color`, so this one test is the
    // whole saving.
    let ns = u32(G.star_meta.x);
    if (ns == 0u && dot(G.light_color.rgb, G.light_color.rgb) <= 1e-8) {
        return out;
    }
    if (ns == 0u) {
        let l = sun_dir_at(p);
        let r = ggx_light(n, v, l, f0, rough);
        var sh = vec3<f32>(1.0);
        if (r.w > 0.0 || dot(r.xyz, r.xyz) > 0.0) {
            sh = sun_shadow(p, n, pix);
        }
        out.diffuse = G.light_color.rgb * r.w * sh;
        out.spec = G.light_color.rgb * r.xyz * sh;
        return out;
    }
    for (var i = 0u; i < min(ns, 4u); i++) {
        let l = star_dir_at(i, p);
        let scol = star_col_at(i, p);
        let r = ggx_light(n, v, l, f0, rough);
        var sh = vec3<f32>(1.0);
        if (r.w > 0.0 || dot(r.xyz, r.xyz) > 0.0) {
            sh = star_shadow(i, p, n, pix);
        }
        out.diffuse += scol * r.w * sh;
        out.spec += scol * r.xyz * sh;
    }
    return out;
}

/// The placeable point lights under the metal-rough model — the GGX twin of
/// [`point_diffuse`]. Same lights, same smooth falloff to zero at range; the
/// difference is that the highlight now comes from the roughness rather than
/// from a hand-set exponent. Returned split so the caller multiplies only the
/// diffuse half by albedo.
fn point_ggx(pos_rel: vec3<f32>, n: vec3<f32>, v: vec3<f32>, f0: vec3<f32>, rough: f32, pix: vec2<u32>) -> KeyLight {
    var out: KeyLight;
    out.diffuse = vec3<f32>(0.0);
    out.spec = vec3<f32>(0.0);
    if (G.gi_meta.y > 0.5) {
        return out; // "show only the bounce" — see key_light() in field.wgsl
    }
    let count = min(u32(g.point_count.x), 16u);
    for (var i = 0u; i < count; i = i + 1u) {
        let lp = g.point_pos[i];
        let a = area_terms(g.point_shape[i], g.point_rot[i], g.point_cone[i], lp.xyz - pos_rel, n, v);
        let x = clamp(1.0 - a.dist / max(lp.w, 0.0001), 0.0, 1.0);
        // The cone rides the SAME factor the range falloff does, so it dims the
        // diffuse and the highlight together. A spot whose highlight stayed
        // outside its own cone would be the tell that this was bolted on.
        let atten = x * x * a.atten;
        if (atten <= 0.0) { continue; }
        // A wide emitter smears its own highlight: the lobe is widened by how
        // big the light looks from here, and re-normalised so growing a light
        // spreads its highlight rather than adding energy to it.
        let wide = clamp(rough + a.spread * 0.5, 0.0, 1.0);
        let norm = (rough * rough + 1e-4) / (wide * wide + 1e-4);
        let r = ggx_light(n, v, a.l, f0, wide);
        let col = g.point_color[i].rgb * atten;
        // The DIFFUSE takes the area response (`a.ndl`), not the representative
        // point's cosine — the representative point exists for the highlight and
        // is in the wrong place for anything else. What it must NOT drop is
        // `ggx_light`'s Fresnel energy split: whatever a surface reflects
        // specularly cannot also come back diffusely, and without it a metal
        // grows a Lambertian term it is not supposed to have.
        let fr = f_schlick(f0, max(dot(n, v), 1e-4));
        let kd = 1.0 - max(max(fr.r, fr.g), fr.b);
        // Local shadows. One factor over BOTH halves: a lamp behind a wall
        // stops lighting a surface and stops putting a highlight on it, and
        // shadowing only the diffuse leaves a floating specular dot in the
        // dark — the giveaway that a shadow is being faked.
        let sh = point_vis(pos_rel, n, i, pix);
        out.diffuse += col * (kd * a.ndl * 0.3183098862) * sh;
        out.spec += col * r.xyz * min(norm, 1.0) * sh;
    }
    return out;
}

// Triplanar-sample one terrain palette layer at object-space position `p`, blended by the
// object normal. Explicit-gradient sampling (`dpx`/`dpy` = dpdx/dpdy of `p`, computed by
// the caller BEFORE any branching) so this is legal inside data-dependent loops — the
// weight-blended splat below samples a variable number of slots per fragment. `slot` is
// the 0-based palette layer; per-slot nearest filtering comes from `terrain_bits.x`.
fn terrain_triplanar(slot: i32, p: vec3<f32>, n: vec3<f32>, dpx: vec3<f32>, dpy: vec3<f32>) -> vec3<f32> {
    let scale = g.terrain_mask.y;
    let an = abs(normalize(n)) + vec3<f32>(0.0001);
    let w = an / (an.x + an.y + an.z);
    let nearest = (g.terrain_bits.x & (1u << u32(slot))) != 0u;
    if (nearest) {
        let nx = textureSampleGrad(terrain_pal_nearest, terrain_pal_samp_nearest, p.zy * scale, slot, dpx.zy * scale, dpy.zy * scale).rgb;
        let ny = textureSampleGrad(terrain_pal_nearest, terrain_pal_samp_nearest, p.xz * scale, slot, dpx.xz * scale, dpy.xz * scale).rgb;
        let nz = textureSampleGrad(terrain_pal_nearest, terrain_pal_samp_nearest, p.xy * scale, slot, dpx.xy * scale, dpy.xy * scale).rgb;
        return nx * w.x + ny * w.y + nz * w.z;
    }
    let lx = textureSampleGrad(terrain_pal, terrain_pal_samp, p.zy * scale, slot, dpx.zy * scale, dpy.zy * scale).rgb;
    let ly = textureSampleGrad(terrain_pal, terrain_pal_samp, p.xz * scale, slot, dpx.xz * scale, dpy.xz * scale).rgb;
    let lz = textureSampleGrad(terrain_pal, terrain_pal_samp, p.xy * scale, slot, dpx.xy * scale, dpy.xy * scale).rgb;
    return lx * w.x + ly * w.y + lz * w.z;
}

// The terrain volume containing camera-relative `p` — like field.wgsl's
// `containing_volume`, but accepting kind 3 (meshed terrain, the raster's whole
// reason to ask) as well as kind 1. −1 = none (not resident / off the atlas).
fn splat_volume(p: vec3<f32>) -> i32 {
    var best = -1;
    var bd = 1e9;
    let vols = min(u32(G.params.w), 16u);
    for (var i = 0u; i < vols; i = i + 1u) {
        if (!vol_in_field(i)) { continue; }
        let q = abs(p - G.vol_center[i].xyz) - G.vol_half[i].xyz;
        if (max(q.x, max(q.y, q.z)) < 0.5) {
            let d = volume_d(i, p);
            if (d < bd) { bd = d; best = i32(i); }
        }
    }
    return best;
}

// The voxel's palette slot at integer atlas texel `c` (volume-local, clamped),
// read EXACTLY — textureLoad, no filtering, so a slot index can never come back
// fractional. 0 = untextured; 255 = the legacy "no slot" sentinel (reads as 0).
fn splat_slot_at(vi: u32, c: vec3<i32>) -> u32 {
    let dims = vec3<i32>(G.vol_dims[vi].xyz);
    let cc = clamp(c, vec3<i32>(0), dims - 1);
    let texel = vec3<i32>(G.vol_atlas[vi].xyz) + cc;
    let a = textureLoad(field_color_tex, texel, 0).a * 255.0;
    let slot = u32(round(a));
    return select(slot, 0u, slot > 254u);
}

// WEIGHT-BLENDED terrain splat: albedo (rgb) + glow weight (w).
//
// The old path crossfaded palette layers `floor(a)`↔`ceil(a)` of the INTERPOLATED
// vertex slot — correct only for adjacent slots; a slot-2↔slot-7 boundary swept
// through layers 3–6 ("two other textures transitioning between them"). Indices
// are identities, not quantities: never interpolate them. Instead, read the 8
// surrounding voxels' exact slots from the field's color atlas and blend each
// DISTINCT slot's triplanar sample by its trilinear weight — smooth transitions
// between ANY two slots, at any palette size. Fragments outside any resident
// volume (a skirt sliver, an evicted terrain) fall back to the old behavior.
fn terrain_splat(in: VsOut, dpx: vec3<f32>, dpy: vec3<f32>) -> vec4<f32> {
    let tint = in.vcolor.rgb;
    let av = in.vcolor.a * 255.0; // vertex slot: 1-based; 0 untextured, 255 legacy sentinel
    if (av < 0.5 || av > 254.5) {
        return vec4<f32>(tint, 0.0);
    }
    let vi = splat_volume(in.view_pos);
    if (vi < 0) {
        // Fallback: the old adjacent-layer crossfade on the vertex slot.
        let lo = floor(av);
        let f = av - lo;
        let c_lo = terrain_triplanar(i32(lo) - 1, in.lpos, in.lnorm, dpx, dpy) * tint * 1.6;
        let c_hi = terrain_triplanar(i32(ceil(av)) - 1, in.lpos, in.lnorm, dpx, dpy) * tint * 1.6;
        let g_lo = f32((g.terrain_bits.y >> u32(max(i32(lo) - 1, 0))) & 1u);
        let g_hi = f32((g.terrain_bits.y >> u32(max(i32(ceil(av)) - 1, 0))) & 1u);
        return vec4<f32>(mix(c_lo, c_hi, f), mix(g_lo, g_hi, f));
    }
    let uvi = u32(vi);
    // Continuous voxel coordinate (voxel centers at i+0.5 — the same mapping as
    // `atlas_uvw`), shifted so corner 0 is the voxel at/below the sample point.
    let dims = G.vol_dims[uvi].xyz;
    let frac = clamp((in.view_pos - G.vol_center[uvi].xyz) / (2.0 * G.vol_half[uvi].xyz) + 0.5,
                     vec3<f32>(0.0), vec3<f32>(1.0));
    let c = clamp(frac * dims - 0.5, vec3<f32>(0.0), dims - 1.0);
    let i0 = vec3<i32>(floor(c));
    let fw = c - floor(c);
    // Accumulate trilinear weight per DISTINCT slot (8 corners → usually 1–2 slots).
    var slots: array<u32, 8>;
    var wts: array<f32, 8>;
    var n_slots = 0;
    for (var corner = 0u; corner < 8u; corner = corner + 1u) {
        let o = vec3<i32>(i32(corner & 1u), i32((corner >> 1u) & 1u), i32((corner >> 2u) & 1u));
        let wv = mix(vec3<f32>(1.0) - fw, fw, vec3<f32>(o));
        let w = wv.x * wv.y * wv.z;
        if (w < 1e-4) { continue; }
        let s = splat_slot_at(uvi, i0 + o);
        var found = false;
        for (var k = 0; k < n_slots; k = k + 1) {
            if (slots[k] == s) { wts[k] = wts[k] + w; found = true; }
        }
        if (!found) { slots[n_slots] = s; wts[n_slots] = w; n_slots = n_slots + 1; }
    }
    var albedo = vec3<f32>(0.0);
    var glow = 0.0;
    var wsum = 0.0;
    for (var k = 0; k < n_slots; k = k + 1) {
        let w = wts[k];
        wsum = wsum + w;
        if (slots[k] == 0u) {
            albedo = albedo + tint * w; // untextured voxels blend toward the flat tint
        } else {
            let layer = i32(slots[k]) - 1;
            albedo = albedo + terrain_triplanar(layer, in.lpos, in.lnorm, dpx, dpy) * tint * 1.6 * w;
            glow = glow + f32((g.terrain_bits.y >> u32(layer)) & 1u) * w;
        }
    }
    if (wsum < 1e-4) {
        return vec4<f32>(tint, 0.0);
    }
    return vec4<f32>(albedo / wsum, glow / wsum);
}

// The base texture sampled through the material's tiling block (rim.w flags +
// the tile lanes). Mode 0 is EXACTLY the pre-tiling `textureSample` — sampled
// first, unconditionally, which also satisfies WGSL's uniform-control-flow rule
// for implicit derivatives; the tiled paths use explicit gradients because the
// mode comes from per-instance data (not provably uniform).
fn base_texel(in: VsOut) -> vec4<f32> {
    // Everything needing uniform control flow (the implicit-derivative sample
    // and the explicit derivatives) runs BEFORE any branching on instance data.
    let uv0 = surface_uv(in);
    let base = textureSample(tex, samp, uv0);
    let duvdx = dpdx(uv0);
    let duvdy = dpdy(uv0);
    let dlx = dpdx(in.lpos);
    let dly = dpdy(in.lpos);
    let flags = u32(in.rim.w + 0.5);
    let mode = flags & 3u;
    if (mode == 1u) {
        // Rotate around the UV center, repeat `count` times, scroll by offset.
        let rot = f32(flags >> 2u) * 0.1 * 0.017453292519943295;
        let c = cos(rot);
        let sn = sin(rot);
        let m = mat2x2<f32>(vec2<f32>(c, sn), vec2<f32>(-sn, c));
        let uv = m * ((uv0 - 0.5) * in.tile.xy) + 0.5 + in.tile.zw;
        return textureSampleGrad(tex, samp, uv, m * (duvdx * in.tile.xy), m * (duvdy * in.tile.xy));
    }
    if (mode == 2u) {
        // Triplanar: three object-axis projections blended by the local normal.
        let s = max(in.tile.x, 1e-4);
        let sharp = max(in.tile.y, 0.5);
        let p = in.lpos / s;
        let dx = dlx / s;
        let dy = dly / s;
        var w = pow(abs(normalize(in.lnorm)), vec3<f32>(sharp));
        w = w / (w.x + w.y + w.z);
        let cx = textureSampleGrad(tex, samp, p.zy, dx.zy, dy.zy);
        let cy = textureSampleGrad(tex, samp, p.xz, dx.xz, dy.xz);
        let cz = textureSampleGrad(tex, samp, p.xy, dx.xy, dy.xy);
        return cx * w.x + cy * w.y + cz * w.z;
    }
    return base;
}

// The shading normal, flipped when the surface is seen from BEHIND. Nothing culls, so
// single-face geometry (the Plane primitive, open meshes) rasterizes from both sides —
// this keeps its lighting right from either one.
//
// "From behind" is decided by the PRIMITIVE's winding (`@builtin(front_facing)`), NOT by
// the interpolated normal's own sign — a distinction with teeth. On any smooth closed
// mesh the interpolated normal rotates past 90° from the view direction slightly BEFORE
// the geometry actually ends, so a `dot(n, -view_pos) >= 0` test flips the normal across
// a band of genuinely front-facing pixels hugging every silhouette, and those pixels
// collapse to ambient — a black outline. On low-poly props it hides in a pixel or two;
// meshed terrain is nothing but smooth silhouette, and it drew a hard black rim around
// every hill (found by the P2 parity probe: `unlit` rendered clean, normals rendered
// clean, so only the flip was left). Winding has no such band: it is exact.
fn facing_normal(n: vec3<f32>, front: bool) -> vec3<f32> {
    return select(-n, n, front);
}

@fragment
fn fs(in: VsOut, @builtin(front_facing) front: bool) -> @location(0) vec4<f32> {
    // Everything that needs implicit derivatives happens up here, in uniform
    // control flow, BEFORE any discard or branch on per-instance data: the
    // surface maps, the tangent frame, and `base_texel`'s own sampling.
    let ext = ext_at(u32(in.imeta.y));
    let muv = map_uv(in);
    let geo_n = facing_normal(normalize(in.normal), front);
    // With no normal map bound this samples the flat 1×1 default and returns
    // `geo_n` unchanged, so an unmapped surface is bit-identical to before.
    // A vertex-lit surface skips it: there is no per-pixel lighting to perturb.
    let mapped_n = mapped_normal(in, geo_n, muv, ext.nstr);
    let n = select(mapped_n, geo_n, ext_has(ext, EXT_VERTEX_LIT));
    let rough_s = textureSample(rough_tex, rough_samp, muv).g;
    let metal_s = textureSample(metal_tex, metal_samp, muv).b;
    let ao_s = textureSample(ao_tex, ao_samp, muv).r;
    let v = normalize(-in.view_pos);
    let texel = base_texel(in);
    // MESHED TERRAIN (tsplat flag): the vertex color's alpha is a palette SLOT, not opacity,
    // and albedo comes from triplanar-splatting the palette. Terrain is always opaque, so it
    // bypasses both the alpha multiply and the cutout below (whose test would otherwise
    // discard it — a slot index reads as a near-zero alpha). Everything else takes the
    // normal vertex-paint multiply.
    let terrain = in.imeta.x > 0.5;
    // Vertex paint MULTIPLIES — it tints the textured surface rather than replacing
    // it, which is what lets painted color stand in for baked lighting/AO. "Replace"
    // needs no mode of its own: it's this multiply against a white texture.
    // Albedo + glow in ONE splat evaluation (both need the same voxel weights).
    // Gradients are taken HERE, in uniform control flow, so the guarded call below
    // only uses grad-sampling + textureLoad (legal in non-uniform flow) — and
    // non-terrain fragments skip the splat's voxel reads entirely.
    let tp_dpx = dpdx(in.lpos);
    let tp_dpy = dpdy(in.lpos);
    var tsplat_val = vec4<f32>(0.0);
    if (terrain) {
        tsplat_val = terrain_splat(in, tp_dpx, tp_dpy);
    }
    let albedo = select(texel.rgb * in.color.rgb * in.vcolor.rgb, tsplat_val.rgb * in.color.rgb, terrain);
    let emissive = in.emissive.rgb * in.emissive.a;
    // Opacity: the material's alpha (in.color.a) times the texture's own alpha,
    // times painted alpha. Terrain is forced opaque (its vcolor.a is a slot).
    // Terrain is opaque: its dissolve is the discard below, not a blend, so its
    // alpha out is a flat 1.0 rather than the instance's dissolve progress.
    let alpha = select(in.color.a * texel.a * in.vcolor.a, 1.0, terrain);

    // ALPHA CUTOUT for OPAQUE materials: a transparent-background texture (a PNG with an
    // alpha channel) shows through as actual holes, not black. Without this the opaque
    // pass — which does not blend — wrote the transparent texels straight to the target,
    // and a transparent PNG's see-through pixels are usually black RGB, so the "clear"
    // background rendered solid black. Discarding them is the retro-correct answer (PS1/N64
    // alpha test, crisp edges, no depth sorting). Genuinely TRANSLUCENT materials set
    // `color.a < 1` and route to the blended pass, which must NOT hard-cut — so this only
    // fires for opaque instances. The depth prepass already discards these (`fs_depth`),
    // so depth stays consistent. Terrain never cuts out.
    if (!terrain && in.color.a >= 0.999 && alpha < 0.5) {
        discard;
    }

    // Screen pixel index — drives the optional fog/shadow dither. Needed by the
    // unlit early-return's fog too, so it's computed before that branch.
    let pix = vec2<u32>(u32(in.clip.x), u32(in.clip.y));

    // A newly meshed terrain chunk DISSOLVES in rather than popping
    // (`floptle/0067`): the streamer ramps `color.a` 0 → 1 over its first
    // moments and the fraction of pixels that survive follows it.
    //
    // A dissolve and not a blend, because terrain is opaque and must stay in
    // the opaque pass: an alpha-blended chunk would need sorting against every
    // other chunk, and a half-transparent hillside shows the sky through the
    // hill behind it. Discarding is order-independent and free. The threshold
    // is `ign` rather than 4×4 Bayer for the same reason the fog uses it — a
    // regular grid across a whole hillside reads as a screen door, where the
    // gradient noise reads as the thing appearing.
    //
    // `fs_depth` runs the IDENTICAL test, so a fading chunk primes depth for
    // exactly the pixels it will shade. Both derive the threshold from
    // `in.clip` (invariant between the passes) and the same instance alpha.
    if (terrain && in.color.a < 0.999 && ign(pix) > in.color.a) {
        discard;
    }

    // RETRO — screen-door transparency. Partial opacity becomes an ordered 4×4
    // dither of fully-opaque pixels, so the surface stays in the OPAQUE pass and
    // needs no sorting at all. (`is_opaque` on the CPU side keeps it there; a
    // dithered surface in the blended pass would dither AND blend.)
    if (ext_has(ext, EXT_DITHER_ALPHA) && alpha < 0.999 && bayer4(pix) >= alpha) {
        discard;
    }
    // Once it dithers, what survives is fully opaque — anything less would blend
    // the surviving pixels on top of the screen door.
    let out_a = select(alpha, 1.0, ext_has(ext, EXT_DITHER_ALPHA));

    // Unlit (fullbright/flat) — pure albedo + emissive, the classic retro look.
    if (in.params.z > 0.5) {
        return vec4<f32>(surface_fog(ext, albedo + emissive, in.view_pos, pix), out_a);
    }

    // Field sun-shadows + true SDF AO, received from the fused field at group(2).
    // `in.view_pos` is camera-relative — the same space the field lives in
    // (ADR-0015) — so the mesh fragment marches it directly. Both gate to zero
    // work when their Lighting/PostProcess switches are off; only the DIRECTIONAL
    // terms are shadowed (ambient + point lights stay as fill), matching the
    // raymarch pass exactly. (`pix` was computed above the unlit branch.)
    var occ = 1.0;
    if (G.ao_params.x > 0.5) {
        occ = sdf_ao(in.view_pos, n);
    }

    // The AO map: baked contact shading. It multiplies AMBIENT (and, once there
    // is bounce, indirect) and NEVER the key light — occlusion darkens the light
    // that arrives from everywhere, not the light that arrives from one place.
    // Applying it to everything is the usual mistake, and it reads as a surface
    // covered in grey smudges.
    // The baked bounce, where a light probe volume covers this fragment; the
    // scene's flat ambient where none does.
    let ambient =
        gi_ambient(in.view_pos, n, g.ambient.rgb) * in.params.w * mix(1.0, ao_s, clamp(ext.ostr, 0.0, 1.0));

    // RETRO — per-vertex (Gouraud) lighting: use what `vs` interpolated and stop.
    // No shadow march, no SDF AO, no normal map; see `VsOut.vlit`.
    if (ext_has(ext, EXT_VERTEX_LIT)) {
        // `gi_only` is applied HERE and not inside `point_diffuse`, because the
        // per-vertex path calls that from the VERTEX stage — and the field group
        // (where `G` lives) is bound for FRAGMENT only. Reaching for `G` in `vs`
        // does not fail at the shading site; it fails at pipeline creation, for
        // every raster draw in the engine.
        let vl = albedo * (ambient + max(in.vlit, vec3<f32>(0.0)) * gi_only_gate());
        return vec4<f32>(surface_fog(ext, vl + emissive, in.view_pos, pix), out_a);
    }

    var lit: vec3<f32>;
    if (ext_has(ext, EXT_PHYSICAL)) {
        // --- Metal-rough. -------------------------------------------------
        // Roughness reads the GREEN channel and metallic the BLUE, so a glTF
        // packed occlusion/roughness/metallic image drops in unmodified — and
        // the AO map above reads RED from the same image for the same reason.
        let rough = clamp(ext.rough * rough_s, 0.045, 1.0);
        let metal = clamp(ext.metal * metal_s, 0.0, 1.0);
        // 4% normal-incidence reflectance is what every dielectric does; a metal
        // reflects its own colour and has no diffuse at all.
        let f0 = mix(vec3<f32>(0.04), albedo, metal);
        // Light that passes THROUGH a surface is not also scattering off it, so
        // the diffuse term fades out as transmission rises. Without this a glass
        // ball is a lit ball with a picture of the room added on top, which
        // reads as a glowing marble rather than as glass.
        let trans = clamp(ext.transmit, 0.0, 1.0);
        let diffuse_albedo = albedo * (1.0 - metal) * (1.0 - trans);
        let kl = key_light_ggx(in.view_pos, n, v, f0, rough, pix);
        lit = diffuse_albedo * (ambient + kl.diffuse) + kl.spec;
        let pl = point_ggx(in.view_pos, n, v, f0, rough, pix);
        lit += diffuse_albedo * pl.diffuse + pl.spec;
        // …and the sky. Without this a metal has a correct specular lobe and
        // nothing to put in it: at roughness 0 it was a sun dot on black, which
        // is exactly why a mirror could not be made. `ext.reflect` scales it,
        // and the AO map damps it like any other ambient term.
        //
        // **Tested before it is computed, not after.** `env_specular` contains
        // the screen-space march — up to sixty-nine depth samples — and
        // multiplying its result by a zero `reflect` afterwards still pays for
        // every one of them. A scene with two mirrors in it was marching every
        // wall, floor and ceiling pixel it had.
        let refl = ext.reflect * gi_only_gate();
        if (refl > 0.0) {
            lit += env_specular(in.view_pos, n, v, f0, rough, pix) * refl;
        }
        // …and what comes through from behind. Weighted by what the surface did
        // NOT reflect: a pane seen face-on is mostly window, the same pane seen
        // edge-on is mostly mirror, and that swap is Fresnel doing its job
        // rather than a second slider. Tinted by the material's own colour, so
        // green glass makes what is behind it green.
        if (trans > 0.0) {
            let fr = f_schlick(f0, max(dot(n, v), 1e-4));
            let kt = (1.0 - max(max(fr.r, fr.g), fr.b)) * trans;
            lit += refracted(in.view_pos, n, v, rough, ext.ior, ext.thick) * albedo * kt;
        }
    } else {
        // --- Blinn-Phong, exactly as it was. ------------------------------
        // Key light(s): the shared multi-star model — Σ color·NdotL·shadow per
        // star (or the one legacy directional/positional sun), plus its specular.
        let shininess = max(in.params.x, 1.0);
        let kl = key_light(in.view_pos, n, v, shininess, pix);
        lit = albedo * (ambient + kl.diffuse);
        // Placeable point lights (camera-relative; in.view_pos is in the same space).
        lit += albedo * point_diffuse_lit(in.view_pos, n, pix) * gi_only_gate();
        lit += in.specular.rgb * kl.spec * in.specular.a;
    }

    // Rim / fresnel — a cheap stylized edge glow. Available under BOTH models:
    // it is an art direction, not a physical term.
    let rim_f = pow(1.0 - max(dot(n, v), 0.0), 2.0) * in.params.y;
    lit += in.rim.rgb * rim_f;

    // Glowing terrain slots: their albedo bypasses lighting AND the AO multiply —
    // the cave-readability channel (per-voxel emissive without a new vertex stream).
    var glow = vec3<f32>(0.0);
    if (terrain) {
        glow = albedo * tsplat_val.w * 0.9;
    }

    return vec4<f32>(surface_fog(ext, lit * occ + emissive + glow, in.view_pos, pix), out_a);
}

// Silhouette mask: solid 1.0 wherever the mesh covers a pixel. Rendered into a
// single-channel target; a post-pass edge-detects this into a selection outline
// that hugs the true silhouette (works for any shape).
@fragment
fn fs_mask(in: VsOut) -> @location(0) vec4<f32> {
    return vec4<f32>(1.0, 0.0, 0.0, 1.0);
}

// Depth-only prepass: writes depth for texels that are CERTAINLY opaque and
// discards the rest — conservative, so cutout/blended texels never wrongly
// occlude what's behind them (they simply don't prime the depth buffer). The
// primed depth early-z-kills hidden fragments in the color pass (whose shading
// marches the shadow field — the expensive part) and caps the raymarch per pixel.
@fragment
fn fs_depth(in: VsOut) {
    // The texel FIRST, before the terrain test below returns early. Sampling
    // with implicit derivatives is only legal in uniform control flow, and a
    // return taken on per-instance data makes everything after it non-uniform.
    // Native drivers never minded; a browser's WGSL compiler refuses the whole
    // module ("'textureSample' must only be called from uniform control flow"),
    // and it was the first shader the web build was refused on. Terrain pays
    // one sample of the default texture for it, and nothing changes in `fs`'s
    // own ordering — see the same rule spelled out at the top of `fs`.
    // Same tiled sampling as the color pass, so the conservative alpha test
    // sees the texels that will actually shade — INCLUDING painted alpha, or the
    // prepass would prime depth for fragments the color pass then blends away.
    let texel_a = base_texel(in).a;
    // Terrain is always opaque and its vcolor.a is a SLOT, not opacity — prime depth for it
    // unconditionally (else a hill wouldn't cap the raymarch and blobs would show through it).
    // The one exception is a chunk still dissolving in (`floptle/0067`): priming depth for a
    // pixel the color pass then discards would punch a hole in whatever is behind it, so the
    // SAME test runs here. Identical inputs, identical result — see `fs`.
    if (in.imeta.x > 0.5) {
        if (in.color.a < 0.999 && ign(vec2<u32>(u32(in.clip.x), u32(in.clip.y))) > in.color.a) {
            discard;
        }
        return;
    }
    let a = texel_a * in.color.a * in.vcolor.a;
    if (a < 0.99) {
        discard;
    }
}
