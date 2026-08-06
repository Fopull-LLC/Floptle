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
    // normal, flat until normal maps land: (0.5, 0.5) decodes to +Z.
    g.surface = vec4<f32>(clamp(in.info.x, 0.0, 63.0) / 63.0, 0.5, 0.5, 1.0);
    return g;
}

// ---- accumulation -----------------------------------------------------------

struct Lights {
    // x = how many lights, y = 1 when the pass should run at all.
    count: vec4<f32>,
    // rgb = the flat ambient every 2D surface gets. Without it an unlit scene
    // with no lights in it would come out black, which reads as the feature
    // having broken the game rather than as "there are no lights".
    ambient: vec4<f32>,
    // Clip → camera-relative world, to put a G-buffer pixel back in the scene.
    inv_view_proj: mat4x4<f32>,
    // xyz = camera-relative position, w = range.
    pos: array<vec4<f32>, 16>,
    // rgb = colour × intensity.
    color: array<vec4<f32>, 16>,
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
    for (var i = 0u; i < n; i = i + 1u) {
        // A light that does not reach this pixel's sorting layer contributes
        // nothing — this is how a torch passes over a background without
        // lighting it, which is the single most-asked-for thing in 2D lighting.
        // Split across the mask's four words: a rank of 40 is bit 8 of word 1,
        // and shifting by 40 would be an out-of-range shift, not a big number.
        if ((L.mask[i][rank >> 5u] & (1u << (rank & 31u))) == 0u) {
            continue;
        }
        let lp = L.pos[i];
        let d = distance(lp.xyz, world);
        // Smooth to exactly zero at the range, so a light has a real edge rather
        // than an inverse-square tail that never quite ends and quietly costs
        // every pixel on screen.
        let x = clamp(1.0 - d / max(lp.w, 1e-4), 0.0, 1.0);
        acc = acc + L.color[i].rgb * (x * x);
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
