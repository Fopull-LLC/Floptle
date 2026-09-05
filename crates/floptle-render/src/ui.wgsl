// The game-UI pass (docs/ui-styles.md §A): one instanced pipeline
// draws EVERYTHING — rounded-rect shapes with per-corner radii, per-side
// borders and two-stop gradients; images; text glyphs; and the three flavours
// of soft edge (drop shadow, glow, inset shadow). Instances arrive in painter's
// order; batches switch only the bound texture and the blend mode.
//
// The rule this file follows: the COMMON visual grammar lives here, in instance
// data, so it batches. Genuinely procedural faces — a navball, a guard meter —
// stay `stage ui` .flsl shaders. A gradient should never have cost anyone a
// WGSL file, and after this it doesn't.

struct Globals {
    // x, y = viewport px; z = mode (0 = screen-space, 1 = world canvas);
    // w = time in seconds.
    viewport: vec4<f32>,
    // World-canvas basis (mode 1): origin (top-left), right + down are the
    // plane axes scaled to world-units-per-design-unit.
    plane_origin: vec4<f32>,
    plane_right: vec4<f32>,
    plane_down: vec4<f32>,
    view_proj: mat4x4<f32>,
}
@group(0) @binding(0) var<uniform> globals: Globals;

@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;

struct VsIn {
    @location(0) corner: vec2<f32>,          // unit quad 0..1
    @location(1) rect: vec4<f32>,            // x, y, w, h (physical px)
    @location(2) color: vec4<f32>,           // fill, or the gradient's near stop
    @location(3) border_color: vec4<f32>,
    @location(4) params: vec4<f32>,          // feather px, kind, clip radius px, -
    @location(5) uv_rect: vec4<f32>,         // u0, v0, u1, v1
    @location(6) clip: vec4<f32>,            // mask rect x, y, w, h px (w <= 0 = none)
    @location(7) radius: vec4<f32>,          // TL, TR, BR, BL px
    @location(8) border: vec4<f32>,          // L, T, R, B px
    @location(9) grad_to: vec4<f32>,         // gradient far stop
    @location(10) grad_cfg: vec4<f32>,       // kind, angle rad, mid, extent
    @location(11) xform: vec4<f32>,          // rotation rad, scale x, scale y, -
    @location(12) fx: vec4<f32>,             // grain amount, grain px, pivot x, pivot y
    @location(13) inset: vec4<f32>,          // inset offset x, y, spread px, -
}

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) border_color: vec4<f32>,
    @location(2) params: vec4<f32>,
    @location(3) uv: vec2<f32>,
    @location(4) local: vec2<f32>,           // px within the rect, pre-transform
    @location(5) half_size: vec2<f32>,       // rect half extents px
    @location(6) px: vec2<f32>,              // post-transform px position
    @location(7) @interpolate(flat) clip: vec4<f32>,
    @location(8) @interpolate(flat) radius: vec4<f32>,
    @location(9) @interpolate(flat) border: vec4<f32>,
    @location(10) @interpolate(flat) grad_to: vec4<f32>,
    @location(11) @interpolate(flat) grad_cfg: vec4<f32>,
    @location(12) @interpolate(flat) fx: vec4<f32>,
    @location(13) @interpolate(flat) inset: vec4<f32>,
}

@vertex
fn vs_main(in: VsIn) -> VsOut {
    var out: VsOut;

    // The visual transform (docs §A: rotation/scale are layout-NEUTRAL). We
    // move the quad's corners, but hand the fragment stage the UNTRANSFORMED
    // local position — so the rounded-rect SDF is evaluated in the element's
    // own upright frame and the shape rotates as a rigid body instead of
    // shearing its corner radii.
    let pivot = in.fx.zw;
    let rot = in.xform.x;
    let scale = vec2<f32>(in.xform.y, in.xform.z);
    var offset = (in.corner - pivot) * in.rect.zw * scale;
    if rot != 0.0 {
        let c = cos(rot);
        let s = sin(rot);
        offset = vec2<f32>(offset.x * c - offset.y * s, offset.x * s + offset.y * c);
    }
    let p = in.rect.xy + pivot * in.rect.zw + offset;

    if globals.viewport.z > 0.5 {
        // World canvas (Scene-view authoring): design units on the layer plane.
        let world = globals.plane_origin.xyz
            + globals.plane_right.xyz * p.x
            + globals.plane_down.xyz * p.y;
        out.pos = globals.view_proj * vec4<f32>(world, 1.0);
    } else {
        // Screen space: px → NDC (y down in px, up in NDC).
        let ndc = vec2<f32>(
            p.x / globals.viewport.x * 2.0 - 1.0,
            1.0 - p.y / globals.viewport.y * 2.0,
        );
        out.pos = vec4<f32>(ndc, 0.0, 1.0);
    }
    out.color = in.color;
    out.border_color = in.border_color;
    out.params = in.params;
    out.uv = mix(in.uv_rect.xy, in.uv_rect.zw, in.corner);
    out.local = (in.corner - vec2<f32>(0.5)) * in.rect.zw;
    out.half_size = in.rect.zw * 0.5;
    out.px = p;
    out.clip = in.clip;
    out.radius = in.radius;
    out.border = in.border;
    out.grad_to = in.grad_to;
    out.grad_cfg = in.grad_cfg;
    out.fx = in.fx;
    out.inset = in.inset;
    return out;
}

// Editor/authored colors are sRGB values; the render target is sRGB-encoded,
// so anything we write must be LINEAR or it comes out washed-out bright (a
// 0.12 dark panel rendering light grey — the "transparency looks broken" bug).
// Textures are already linearized by their sRGB views; only vertex colors need
// converting.
fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

// Signed distance to a rounded rect centered at origin.
fn sd_round_rect(p: vec2<f32>, half: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - half + vec2<f32>(r);
    return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
}

// Which corner's radius applies at `p`. Per-corner radii are what make a header
// with square bottom corners possible — the case that was flatly unbuildable
// while radius was one scalar.
fn corner_radius(p: vec2<f32>, half: vec2<f32>, r: vec4<f32>) -> f32 {
    // r is [TL, TR, BR, BL] clockwise from the top-left; +x right, +y down.
    // `select(f, t, cond)` picks `t` when cond holds.
    let top = select(r.y, r.x, p.x < 0.0);       // left → TL, right → TR
    let bottom = select(r.z, r.w, p.x < 0.0);    // left → BL, right → BR
    let picked = select(bottom, top, p.y < 0.0);
    return min(picked, min(half.x, half.y));
}

// Two-stop gradient parameter at `p` (local px), 0 at the near stop.
fn gradient_t(p: vec2<f32>, half: vec2<f32>, cfg: vec4<f32>) -> f32 {
    let kind = cfg.x;
    // Normalize to -1..1 so the ramp is aspect-independent: a 45° sweep looks
    // like 45° on a wide bar as well as a square.
    let n = p / max(half, vec2<f32>(1e-4));
    var t = 0.0;
    if kind < 1.5 {
        // Linear: project onto the sweep direction.
        let dir = vec2<f32>(cos(cfg.y), sin(cfg.y));
        t = dot(n, dir) * 0.5 + 0.5;
    } else if kind < 2.5 {
        // Radial from the rect's centre; `extent` is a fraction of the
        // half-diagonal.
        t = length(n) / max(cfg.w, 1e-4);
    } else {
        // Angular: a conic sweep starting at `angle`.
        let a = atan2(n.y, n.x) - cfg.y;
        t = fract(a / 6.28318530718 + 1.0);
    }
    t = clamp(t, 0.0, 1.0);
    // `mid` biases where the two colours meet — most of what a third stop
    // would have bought, for none of the instance lanes.
    let mid = clamp(cfg.z, 0.001, 0.999);
    if t < mid {
        return 0.5 * t / mid;
    }
    return 0.5 + 0.5 * (t - mid) / (1.0 - mid);
}

// Cheap hash noise. Grain is the single most effective anti-"flat slab" tool
// available, and a couple of percent of it is the difference between a surface
// and a swatch.
//
// Integer bit-mixing, not the usual `fract(sin(dot(...)) * 43758.5)`: the sine
// trick has visible diagonal banding at small cell sizes (it looked like
// corduroy rather than grain), and its precision varies by driver. This is
// stable everywhere and costs the same.
fn hash21(p: vec2<f32>) -> f32 {
    // Parenthesised on purpose: WGSL forbids mixing `*` and `^` without them,
    // and a browser's compiler refuses the whole module over it. naga lets it
    // through, which is why this shipped for as long as it did.
    var n = (u32(i32(p.x)) * 1597334673u) ^ (u32(i32(p.y)) * 3812015801u);
    n = (n ^ (n >> 15u)) * 2246822519u;
    n = (n ^ (n >> 13u)) * 3266489917u;
    n = n ^ (n >> 16u);
    return f32(n) / 4294967295.0;
}

// Coverage of the per-side border at `p`. When all four widths agree we use the
// SDF, which follows the corner curve exactly; mixed widths fall back to
// straight edge distances, which is what a rule or an accent bar wants anyway.
fn border_coverage(p: vec2<f32>, half: vec2<f32>, r: f32, d: f32, bw: vec4<f32>) -> f32 {
    let uniform_w = bw.x;
    if bw.y == uniform_w && bw.z == uniform_w && bw.w == uniform_w {
        if uniform_w <= 0.0 {
            return 0.0;
        }
        // Inside, within `w` of the edge.
        return 1.0 - clamp(0.5 - (d + uniform_w), 0.0, 1.0);
    }
    // Distance from each edge, inside-positive.
    let dl = p.x + half.x;
    let dt = p.y + half.y;
    let dr = half.x - p.x;
    let db = half.y - p.y;
    var cov = 0.0;
    if bw.x > 0.0 { cov = max(cov, 1.0 - smoothstep(bw.x - 0.5, bw.x + 0.5, dl)); }
    if bw.y > 0.0 { cov = max(cov, 1.0 - smoothstep(bw.y - 0.5, bw.y + 0.5, dt)); }
    if bw.z > 0.0 { cov = max(cov, 1.0 - smoothstep(bw.z - 0.5, bw.z + 0.5, dr)); }
    if bw.w > 0.0 { cov = max(cov, 1.0 - smoothstep(bw.w - 0.5, bw.w + 0.5, db)); }
    return cov;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // UI mask: pixels outside the clip's rounded rect vanish (1px AA edge).
    var cmask = 1.0;
    if in.clip.z > 0.0 {
        let chalf = in.clip.zw * 0.5;
        let ccenter = in.clip.xy + chalf;
        let cr = min(in.params.z, min(chalf.x, chalf.y));
        let cd = sd_round_rect(in.px - ccenter, chalf, cr);
        cmask = clamp(0.5 - cd, 0.0, 1.0);
    }
    let kind = in.params.y;
    // The texture, sampled ONCE up here before any branch on `kind`. Every
    // element kind returns from its own branch on per-instance data, which is
    // non-uniform control flow, and a browser's WGSL compiler refuses an
    // implicit-derivative `textureSample` inside one — it took the whole UI
    // module with it. The shadow kinds pay one sample of the 1x1 white default.
    let texel = textureSample(tex, samp, in.uv);
    let tint = vec4<f32>(srgb_to_linear(in.color.rgb), in.color.a);
    let r = corner_radius(in.local, in.half_size, in.radius);

    if kind > 2.5 {
        // INSET shadow: a recessed well. The lit shape is displaced by
        // `offset` and shrunk by `spread`; we draw what falls OUTSIDE it, then
        // clip that to the element's own rounded rect. Reads as a hole in the
        // surface, which is what a pressed button and a progress track want.
        let ip = in.local - in.inset.xy;
        let inner_half = max(in.half_size - vec2<f32>(in.inset.z), vec2<f32>(0.0));
        let id = sd_round_rect(ip, inner_half, min(r, min(inner_half.x, inner_half.y)));
        let feather = max(in.params.x, 0.5);
        // FULL strength at the inner shape's edge, fading inward over
        // `feather`. Ramping symmetrically about the edge instead would cap the
        // shadow at half opacity everywhere inside the element, which reads as
        // a smudge rather than a recess.
        let ring = smoothstep(-feather, 0.0, id);
        let outer = sd_round_rect(in.local, in.half_size, r);
        let inside = clamp(0.5 - outer, 0.0, 1.0);
        return vec4<f32>(tint.rgb, tint.a * ring * inside * cmask);
    }
    if kind > 1.5 {
        // Drop shadow / glow: a rounded rect with a soft `feather`-wide edge.
        // The quad was grown by `inset.z` so the feather has somewhere to
        // spread; inset the SDF by the same amount to recover the real shape.
        let pad = in.inset.z;
        let sd = sd_round_rect(in.local, max(in.half_size - vec2<f32>(pad), vec2<f32>(0.0)), r);
        let feather = max(in.params.x, 0.5);
        let mask = 1.0 - smoothstep(-feather, feather, sd);
        return vec4<f32>(tint.rgb, tint.a * mask * cmask);
    }
    if kind > 0.5 {
        // Glyph: atlas red channel is coverage.
        let a = texel.r;
        return vec4<f32>(tint.rgb, tint.a * a * cmask);
    }

    // Shape/image: rounded-rect mask (1px anti-aliased edge), optional
    // gradient, optional grain, optional per-side border.
    let d = sd_round_rect(in.local, in.half_size, r);
    let mask = clamp(0.5 - d, 0.0, 1.0);

    var fill = tint;
    if in.grad_cfg.x > 0.5 {
        let far = vec4<f32>(srgb_to_linear(in.grad_to.rgb), in.grad_to.a);
        fill = mix(fill, far, gradient_t(in.local, in.half_size, in.grad_cfg));
    }
    var col = fill * texel;

    if in.fx.x > 0.0 {
        // Quantize to a cell so `scale` above 1 gives chunky noise rather than
        // per-pixel static that shimmers when the window resizes.
        let cell = max(in.fx.y, 1.0);
        let n = hash21(floor(in.px / cell)) - 0.5;
        col = vec4<f32>(clamp(col.rgb + vec3<f32>(n * in.fx.x), vec3<f32>(0.0), vec3<f32>(1.0)), col.a);
    }

    let bcov = border_coverage(in.local, in.half_size, r, d, in.border);
    if bcov > 0.0 {
        let bc = vec4<f32>(srgb_to_linear(in.border_color.rgb), in.border_color.a);
        col = mix(col, bc, bcov);
    }
    return vec4<f32>(col.rgb, col.a * mask * cmask);
}
