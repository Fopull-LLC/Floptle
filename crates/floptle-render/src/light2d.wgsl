// 2D lighting — the deferred pass.
//
// Two stages:
//
//   1. `vs_fill` / `fs_fill` write a G-buffer for the flat surfaces only:
//      albedo + coverage, and a second target carrying the surface's SORTING
//      RANK (and, later, its normal). The rank rides the geometry rather than a
//      uniform because a light reaches a set of layers, and the accumulation
//      below has to know which layer each pixel belongs to.
//
//   2. `vs_full` / `fs_darken` + `fs_brighten` read it back and apply every 2D
//      light that reaches that pixel's layer — as a **difference** against what
//      the raster pass already drew, never as a redraw.
//
// Stage 2 re-emits the G-buffer's depth as `frag_depth`, so 3D geometry standing
// in front of a flat surface still occludes it. Without that the composite would
// paint lit tiles over anything drawn between them and the camera.
//
// ## Why a difference, and not `over` (`floptle/0121`)
//
// The composite used to write `albedo * light` over the frame at the surface's
// own alpha. For an opaque surface that is exact. For a translucent one it is
// the same sprite blended **twice**: the raster pass had already put
// `C·a + B·(1-a)` there, and compositing `C·light` over that again lands at an
// effective alpha of `1 - (1-a)²`. A sprite authored at 0.5 reached the screen
// at 0.75 and one at 0.72 at 0.92 — silently, in every 2D project, with no light
// placed and nothing switched on, and invisible in every place an author could
// look: the source said 0.72, the Inspector said 0.72, the screen said 0.92.
//
// So this pass never contributes colour of its own. It adds
//
//     delta = C·a·(light - 1)
//
// which is exactly the difference between the frame that exists and the frame
// that should. `a = 1` gives `C·light`, unchanged from before. `light = 1` gives
// zero — the pass is an identity wherever no light reaches, whatever the
// material was. And a translucent surface keeps the alpha its author typed.
//
// The delta is signed and a fixed-point target clamps a negative source to zero
// before blending, so it goes out as two non-negative halves: `fs_darken`
// through a `ReverseSubtract` pipeline and `fs_brighten` through an additive
// one. Per channel, because a warm light darkens blue while it brightens red.
//
// This is also why `C` and `a` in the G-buffer must be exactly what the raster
// pass drew — the gather forces every 2D-lit surface onto the raster pass's
// UNLIT path so that they are (`render_frame.rs`). A surface shaded by the 3D
// sun and then corrected by this delta would be corrected by the wrong amount.

struct Fill {
    view_proj: mat4x4<f32>,
};

@group(0) @binding(0) var<uniform> f: Fill;
@group(1) @binding(0) var albedo_tex: texture_2d<f32>;
@group(1) @binding(1) var albedo_samp: sampler;

struct FillIn {
    @location(0) pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) m0: vec4<f32>,
    @location(4) m1: vec4<f32>,
    @location(5) m2: vec4<f32>,
    @location(6) m3: vec4<f32>,
    @location(7) tint: vec4<f32>,   // rgb tint, a = opacity
    @location(8) info: vec4<f32>,   // x = sorting rank, y/z/w spare
};

struct FillOut {
    // `@invariant`, and it is not optional. The composite re-emits this pass's
    // depth and depth-tests it against the depth the RASTER pass wrote for the
    // very same triangles. Without invariance the two pipelines may contract or
    // reassociate the same multiply differently, land a few ULPs apart, and
    // then `LessEqual` flips between pass and fail as the camera moves — which
    // reads as the tilemap blinking in and out. The raster pass marks its own
    // clip position invariant for exactly this reason (`raster.wgsl`), and this
    // pass has to make the same promise or the promise is worthless.
    @invariant @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) tint: vec4<f32>,
    @location(2) info: vec4<f32>,
};

@vertex
fn vs_fill(in: FillIn) -> FillOut {
    let model = mat4x4<f32>(in.m0, in.m1, in.m2, in.m3);
    // The model matrix is already camera-relative (ADR-0015), exactly as the
    // raster pass builds it — the two are fed from ONE gather, so a flat surface
    // lands on the same pixels in both or neither.
    let world = model * vec4<f32>(in.pos, 1.0);
    var out: FillOut;
    out.clip = f.view_proj * world;
    out.uv = in.uv;
    out.tint = in.tint;
    out.info = in.info;
    return out;
}

struct GBuffer {
    @location(0) albedo: vec4<f32>,
    @location(1) surface: vec4<f32>,
};

@fragment
fn fs_fill(in: FillOut) -> GBuffer {
    let tex = textureSample(albedo_tex, albedo_samp, in.uv);
    let c = tex * in.tint;
    // A tilemap is mostly holes. Discarding rather than writing a transparent
    // pixel keeps the empty squares out of the G-buffer's depth as well as its
    // colour, so a light shining through a gap is not stopped by the gap.
    if (c.a < 0.004) {
        discard;
    }
    var g: GBuffer;
    g.albedo = vec4<f32>(c.rgb, c.a);
    // r = rank, quantized to the 64 ranks a sorting layer can have (the layer
    // step is 1/64 — see `floptle_core::SORT_LAYER_STEP`). gb = the surface
    // normal, flat until normal maps land: (0.5, 0.5) decodes to +Z. a = does
    // this surface block light, which is what the shadow march reads.
    //
    // It matters that this is written by the geometry and cleared to zero: the
    // holes in a tilemap never reach here at all (the discard above), so "no
    // surface" and "a surface that does not cast" are the same answer and
    // neither stops a light.
    g.surface = vec4<f32>(clamp(in.info.x, 0.0, 63.0) / 63.0, 0.5, 0.5, in.info.y);
    return g;
}

// ---- accumulation -----------------------------------------------------------

struct Lights {
    // x = how many lights, y = 1 when the pass should run at all, z = how many
    // steps the shadow march may take (0 = nothing in this frame casts, so the
    // march never runs and the pass costs what it did before shadows existed).
    count: vec4<f32>,
    // rgb = the flat ambient every 2D surface gets. Without it an unlit scene
    // with no lights in it would come out black, which reads as the feature
    // having broken the game rather than as "there are no lights".
    ambient: vec4<f32>,
    // Clip → camera-relative world, to put a G-buffer pixel back in the scene.
    inv_view_proj: mat4x4<f32>,
    // …and back, to find where a light IS on screen for the shadow march.
    view_proj: mat4x4<f32>,
    // xy = the viewport in pixels. Not the G-buffer's size — that only ever
    // grows, and one renderer serves several viewports in a frame.
    viewport: vec4<f32>,
    // xyz = camera-relative position, w = range.
    pos: array<vec4<f32>, 16>,
    // rgb = colour × intensity.
    color: array<vec4<f32>, 16>,
    // x = inner radius, y = exponent, z = 1 when casters stop this light.
    falloff: array<vec4<f32>, 16>,
    // A bitmask over sorting-layer RANK: bit r of word r/32 set = this light
    // reaches rank r. All four words are used, because a uniform array's stride
    // is 16 bytes whatever we put in it and one word would cover only 32 of the
    // 64 ranks a sorting layer can have.
    mask: array<vec4<u32>, 16>,
};

@group(0) @binding(0) var<uniform> L: Lights;
@group(1) @binding(0) var g_albedo: texture_2d<f32>;
@group(1) @binding(1) var g_surface: texture_2d<f32>;
@group(1) @binding(2) var g_depth: texture_depth_2d;

struct FullOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

/// One triangle covering the screen — the same trick every full-screen pass in
/// this renderer uses, so there is no quad seam down the middle.
@vertex
fn vs_full(@builtin(vertex_index) vid: u32) -> FullOut {
    var out: FullOut;
    let x = f32((vid << 1u) & 2u) * 2.0 - 1.0;
    let y = f32(vid & 2u) * 2.0 - 1.0;
    out.clip = vec4<f32>(x, -y, 0.0, 1.0);
    out.uv = vec2<f32>((x + 1.0) * 0.5, (y + 1.0) * 0.5);
    return out;
}

struct LitOut {
    @location(0) color: vec4<f32>,
    // Re-emitting the flat surface's own depth is what lets 3D geometry in front
    // of it win the depth test. A composite that wrote no depth would paint over
    // anything between the tiles and the camera.
    @builtin(frag_depth) depth: f32,
};

/// The signed correction this pixel needs, and the depth to emit with it.
///
/// `C·a·(light - 1)`: what the frame should hold, minus what the raster pass put
/// there. Zero wherever no light reaches — see the header.
struct Delta {
    rgb: vec3<f32>,
    depth: f32,
};

/// Does light `i` reach a surface on sorting rank `r`?
///
/// Split across the mask's four words: a rank of 40 is bit 8 of word 1, and
/// shifting by 40 would be an out-of-range shift rather than a big number.
fn reaches(i: u32, r: u32) -> bool {
    return (L.mask[i][r >> 5u] & (1u << (r & 31u))) != 0u;
}

/// How bright light `i` is at distance `d` — the authorable ramp (`floptle/0126`).
///
/// Full brightness out to the inner radius, then falling to exactly zero at the
/// range. A real edge and not an inverse-square tail that never quite ends and
/// quietly costs every pixel on screen.
///
/// The `exp == 2` branch is not an optimization, it is the compatibility
/// promise: `pow(x, 2.0)` and `x * x` are allowed to land a ULP apart, and every
/// light written before this was authorable arrives here with an exponent of 2.
fn falloff_at(i: u32, d: f32) -> f32 {
    let range = max(L.pos[i].w, 1e-4);
    let inner = clamp(L.falloff[i].x, 0.0, range * 0.999);
    let x = clamp((range - d) / (range - inner), 0.0, 1.0);
    let e = max(L.falloff[i].y, 0.01);
    if (e == 2.0) {
        return x * x;
    }
    return pow(x, e);
}

/// Is light `i` stopped by something between it and this pixel (`floptle/0125`)?
///
/// The G-buffer already holds every flat surface in the frame, and its `a`
/// channel says which of them cast — so occlusion is a walk along the segment
/// from this pixel to the light's own pixel, sampling that channel. **Nothing is
/// built per light**, which is the property this had to have: every light in a
/// game moves, and a design that re-baked geometry when one did would be
/// unusable however fast it was standing still.
///
/// Three things it has to get right:
///
/// * **The layer mask applies to the occluder too.** A light that skips a
///   background must not be *blocked* by it — that would be the worst of both,
///   an unlit surface throwing a shadow.
/// * **A caster does not shadow itself.** The march leaves the run of solid
///   pixels it starts inside before it begins testing, so the face of a wall
///   turned towards a light is lit and only what is behind it goes dark.
/// * **It never marches past the light.** The segment ends there, so a caster
///   on the far side of a lamp does not shade the near side.
fn occluded(i: u32, origin_px: vec2<f32>, self_casts: bool) -> bool {
    let steps = L.count.z;
    if (steps < 1.0 || L.falloff[i].z < 0.5) {
        return false;
    }
    // Where the light is on screen. `w <= 0` puts it behind the eye, which for
    // a flat scene means there is no sensible segment to walk.
    let lc = L.view_proj * vec4<f32>(L.pos[i].xyz, 1.0);
    if (lc.w <= 0.0) {
        return false;
    }
    let ndc = lc.xy / lc.w;
    let light_px = vec2<f32>((ndc.x * 0.5 + 0.5) * L.viewport.x, (0.5 - ndc.y * 0.5) * L.viewport.y);
    let seg = light_px - origin_px;
    let n = i32(steps);
    var left_self = !self_casts;
    for (var s = 1; s <= n; s = s + 1) {
        let p = vec2<i32>(origin_px + seg * (f32(s) / f32(n + 1)));
        let g = textureLoad(g_surface, p, 0);
        // `a` is 1 only where a surface that casts was written. Empty space —
        // the holes a tilemap is mostly made of — never blocks anything,
        // because `fs_fill` discards there rather than writing a clear pixel.
        let solid = g.a > 0.5 && reaches(i, u32(round(g.r * 63.0)));
        if (!left_self) {
            // Still inside our own body. Everything up to leaving it is ours.
            left_self = !solid;
            continue;
        }
        if (solid) {
            return true;
        }
    }
    return false;
}

fn shade(in: FullOut) -> Delta {
    let px = vec2<i32>(in.clip.xy);
    let alb = textureLoad(g_albedo, px, 0);
    var out: Delta;
    if (alb.a < 0.004) {
        // No flat surface here. Depth 1.0 puts the fragment behind everything,
        // so the depth test rejects it and the scene underneath is untouched.
        out.rgb = vec3<f32>(0.0);
        out.depth = 1.0;
        return out;
    }
    let surf = textureLoad(g_surface, px, 0);
    let depth = textureLoad(g_depth, px, 0);
    let rank = u32(round(surf.r * 63.0));

    // Put the pixel back in the world so distance to a light means something.
    let ndc = vec4<f32>(in.uv.x * 2.0 - 1.0, 1.0 - in.uv.y * 2.0, depth, 1.0);
    let p = L.inv_view_proj * ndc;
    let world = p.xyz / p.w;

    var acc = L.ambient.rgb;
    let n = min(u32(L.count.x), 16u);
    let self_casts = surf.a > 0.5;
    for (var i = 0u; i < n; i = i + 1u) {
        // A light that does not reach this pixel's sorting layer contributes
        // nothing — this is how a torch passes over a background without
        // lighting it, which is the single most-asked-for thing in 2D lighting.
        if (!reaches(i, rank)) {
            continue;
        }
        let d = distance(L.pos[i].xyz, world);
        let x = falloff_at(i, d);
        // Out of range already: no contribution, and — the part that pays for
        // shadows — nothing to march. Only pixels actually inside a light's
        // radius ever walk the G-buffer, so the cost follows the lit area
        // rather than the screen.
        if (x <= 0.0) {
            continue;
        }
        if (occluded(i, in.clip.xy, self_casts)) {
            continue;
        }
        acc = acc + L.color[i].rgb * x;
    }
    // Premultiplied by the surface's own alpha, because that is exactly the
    // share of this pixel the raster pass gave it. The other `1 - a` is the
    // background, and this pass must not touch it.
    out.rgb = alb.rgb * alb.a * (acc - vec3<f32>(1.0));
    out.depth = depth;
    return out;
}

/// The negative half, through a `ReverseSubtract` pipeline (`dst - src`).
///
/// Never larger than what is there: `dst >= C·a` and the factor is at most 1, so
/// the subtraction cannot underflow into a clamp.
@fragment
fn fs_darken(in: FullOut) -> LitOut {
    let d = shade(in);
    var out: LitOut;
    out.color = vec4<f32>(max(-d.rgb, vec3<f32>(0.0)), 0.0);
    out.depth = d.depth;
    return out;
}

/// …and the positive half, through an additive one.
@fragment
fn fs_brighten(in: FullOut) -> LitOut {
    let d = shade(in);
    var out: LitOut;
    out.color = vec4<f32>(max(d.rgb, vec3<f32>(0.0)), 0.0);
    out.depth = d.depth;
    return out;
}
