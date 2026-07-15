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
};

@group(0) @binding(0) var<uniform> g: RasterGlobals;
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;
// The shared SDF field (struct + all functions in field.wgsl): the raymarch
// globals buffer and distance atlas, bound read-only here.
@group(2) @binding(0) var<uniform> G: Globals;
@group(2) @binding(1) var dist_tex: texture_3d<f32>;
@group(2) @binding(2) var vol_samp: sampler;

// Accumulated diffuse from the point lights at camera-relative position `pos_rel`
// (same space as point_pos) with surface normal `n`. Smooth falloff to 0 at range.
fn point_diffuse(pos_rel: vec3<f32>, n: vec3<f32>) -> vec3<f32> {
    var acc = vec3<f32>(0.0);
    let count = min(u32(g.point_count.x), 16u);
    for (var i = 0u; i < count; i = i + 1u) {
        let lp = g.point_pos[i];
        let to = lp.xyz - pos_rel;
        let dist = length(to);
        let range = max(lp.w, 0.0001);
        let ndl = max(dot(n, to / max(dist, 1e-4)), 0.0);
        let x = clamp(1.0 - dist / range, 0.0, 1.0);
        acc = acc + g.point_color[i].rgb * (ndl * x * x);
    }
    return acc;
}

struct VsIn {
    @location(0) pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) m0: vec4<f32>,
    @location(4) m1: vec4<f32>,
    @location(5) m2: vec4<f32>,
    @location(6) m3: vec4<f32>,
    @location(7) n0: vec4<f32>,
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
};

@vertex
fn vs(in: VsIn) -> VsOut {
    let model = mat4x4<f32>(in.m0, in.m1, in.m2, in.m3);
    let nmat = mat3x3<f32>(in.n0.xyz, in.n1.xyz, in.n2.xyz);
    var out: VsOut;
    let view_pos = model * vec4<f32>(in.pos, 1.0);
    out.clip = g.view_proj * view_pos;
    out.uv = in.uv;
    out.normal = normalize(nmat * in.normal);
    out.color = in.color;
    out.view_pos = view_pos.xyz;
    out.emissive = in.emissive;
    out.specular = in.specular;
    out.params = in.params;
    out.rim = in.rim;
    out.tile = in.tile;
    out.lpos = in.pos;
    out.lnorm = in.normal;
    return out;
}

// The base texture sampled through the material's tiling block (rim.w flags +
// the tile lanes). Mode 0 is EXACTLY the pre-tiling `textureSample` — sampled
// first, unconditionally, which also satisfies WGSL's uniform-control-flow rule
// for implicit derivatives; the tiled paths use explicit gradients because the
// mode comes from per-instance data (not provably uniform).
fn base_texel(in: VsOut) -> vec4<f32> {
    // Everything needing uniform control flow (the implicit-derivative sample
    // and the explicit derivatives) runs BEFORE any branching on instance data.
    let base = textureSample(tex, samp, in.uv);
    let duvdx = dpdx(in.uv);
    let duvdy = dpdy(in.uv);
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
        let uv = m * ((in.uv - 0.5) * in.tile.xy) + 0.5 + in.tile.zw;
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

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    let n = normalize(in.normal);
    let l = normalize(g.light_dir.xyz);
    let v = normalize(-in.view_pos);
    let ndl = max(dot(n, l), 0.0);
    let texel = base_texel(in);
    let albedo = texel.rgb * in.color.rgb;
    let emissive = in.emissive.rgb * in.emissive.a;
    // Opacity: the material's alpha (in.color.a) times the texture's own alpha.
    let alpha = in.color.a * texel.a;

    // Screen pixel index — drives the optional fog/shadow dither. Needed by the
    // unlit early-return's fog too, so it's computed before that branch.
    let pix = vec2<u32>(u32(in.clip.x), u32(in.clip.y));

    // Unlit (fullbright/flat) — pure albedo + emissive, the classic retro look.
    if (in.params.z > 0.5) {
        return vec4<f32>(apply_fog(albedo + emissive, in.view_pos, pix), alpha);
    }

    // Field sun-shadows + true SDF AO, received from the fused field at group(2).
    // `in.view_pos` is camera-relative — the same space the field lives in
    // (ADR-0015) — so the mesh fragment marches it directly. Both gate to zero
    // work when their Lighting/PostProcess switches are off; only the DIRECTIONAL
    // terms are shadowed (ambient + point lights stay as fill), matching the
    // raymarch pass exactly. (`pix` was computed above the unlit branch.)
    var sh = vec3<f32>(1.0);
    if (ndl > 0.0) {
        sh = sun_shadow(in.view_pos, n, pix);
    }
    var occ = 1.0;
    if (G.ao_params.x > 0.5) {
        occ = sdf_ao(in.view_pos, n);
    }

    let ambient = g.ambient.rgb * in.params.w;
    var lit = albedo * (ambient + g.light_color.rgb * ndl * sh);
    // Placeable point lights (camera-relative; in.view_pos is in the same space).
    lit += albedo * point_diffuse(in.view_pos, n);

    // Blinn-Phong specular, gated to the lit hemisphere.
    let h = normalize(l + v);
    let shininess = max(in.params.x, 1.0);
    let spec = pow(max(dot(n, h), 0.0), shininess) * in.specular.a * select(0.0, 1.0, ndl > 0.0);
    lit += in.specular.rgb * spec * g.light_color.rgb * sh;

    // Rim / fresnel — a cheap stylized edge glow.
    let rim_f = pow(1.0 - max(dot(n, v), 0.0), 2.0) * in.params.y;
    lit += in.rim.rgb * rim_f;

    return vec4<f32>(apply_fog(lit * occ + emissive, in.view_pos, pix), alpha);
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
    // Same tiled sampling as the color pass, so the conservative alpha test
    // sees the texels that will actually shade.
    let a = base_texel(in).a * in.color.a;
    if (a < 0.99) {
        discard;
    }
}
