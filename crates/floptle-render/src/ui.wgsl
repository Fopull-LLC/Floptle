// The game-UI pass (docs/ui-system-proposal.md §10): one instanced pipeline
// draws EVERYTHING — solid rounded-rect shapes (SDF in the fragment), images
// (any registered texture), and text glyphs (alpha from the atlas). Instances
// arrive in painter's order; batches switch only the bound texture.

struct Globals {
    // x, y = viewport px; z = mode (0 = screen-space, 1 = world canvas).
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
    @location(2) color: vec4<f32>,
    @location(3) border_color: vec4<f32>,
    @location(4) params: vec4<f32>,          // radius px, border px, kind, unused
    @location(5) uv_rect: vec4<f32>,         // u0, v0, u1, v1
}

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) border_color: vec4<f32>,
    @location(2) params: vec4<f32>,
    @location(3) uv: vec2<f32>,
    @location(4) local: vec2<f32>,           // px within the rect
    @location(5) half_size: vec2<f32>,       // rect half extents px
}

@vertex
fn vs_main(in: VsIn) -> VsOut {
    var out: VsOut;
    let p = in.rect.xy + in.corner * in.rect.zw;
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

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let kind = in.params.z;
    let tint = vec4<f32>(srgb_to_linear(in.color.rgb), in.color.a);
    if kind > 0.5 {
        // Glyph: atlas red channel is coverage.
        let a = textureSample(tex, samp, in.uv).r;
        return vec4<f32>(tint.rgb, tint.a * a);
    }
    // Shape/image: rounded-rect mask (1px anti-aliased edge) + optional border.
    let r = min(in.params.x, min(in.half_size.x, in.half_size.y));
    let d = sd_round_rect(in.local, in.half_size, r);
    let mask = clamp(0.5 - d, 0.0, 1.0);
    var col = tint * textureSample(tex, samp, in.uv);
    let bw = in.params.y;
    if bw > 0.0 {
        // Inside within `bw` of the edge → border color.
        let t = clamp(0.5 - (d + bw), 0.0, 1.0);
        let bc = vec4<f32>(srgb_to_linear(in.border_color.rgb), in.border_color.a);
        col = mix(bc, col, t);
    }
    return vec4<f32>(col.rgb, col.a * mask);
}
