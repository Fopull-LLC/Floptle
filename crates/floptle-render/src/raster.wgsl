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
    terrain_mask: vec4<f32>,           // x = per-slot NEAREST bitmask, y = triplanar scale
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
@group(0) @binding(3) var terrain_pal: texture_2d_array<f32>;
@group(0) @binding(4) var terrain_pal_samp: sampler;
@group(0) @binding(5) var terrain_pal_samp_nearest: sampler;
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
    // Meshed-terrain splat flag (0/1), from the instance's `n2.w`. Flat: it's per-instance
    // constant, and interpolation would make the fs threshold wrong at triangle edges.
    @location(12) @interpolate(flat) tsplat: f32,
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
    out.rim = in.rim;
    out.tile = in.tile;
    out.lpos = in.pos;
    out.lnorm = in.normal;

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
    let modul = in.n1.w > 0.5;
    let prgb = select(raw.rgb, raw.rgb * 2.0, modul);
    var vc = select(vec4<f32>(1.0), vec4<f32>(prgb, raw.a), pbase != 0u);

    // Terrain chunk color rides the SAME varying from its own store (n0.w, no packing —
    // the lane is not shared with anything). An instance never has both bases, so the
    // order of these two only decides which wins in a case that cannot arise.
    let tbase = u32(in.n0.w);
    let tidx = min(tbase + in.vid, arrayLength(&tpaint) - 1u);
    out.vcolor = select(vc, unpack4x8unorm(tpaint[tidx]), tbase != 0u);
    out.tsplat = in.n2.w;
    return out;
}

// Triplanar-sample one terrain palette layer at object-space position `p`, blended by the
// object normal. A byte-for-byte mirror of the raymarch's `triplanar` (scale, per-slot
// nearest mask) so meshed terrain and any still-raymarched terrain match. `slot` is the
// 0-based palette layer.
fn terrain_triplanar(slot: i32, p: vec3<f32>, n: vec3<f32>) -> vec3<f32> {
    let scale = g.terrain_mask.y;
    let an = abs(normalize(n)) + vec3<f32>(0.0001);
    let w = an / (an.x + an.y + an.z);
    let nearest = (u32(g.terrain_mask.x) & (1u << u32(slot))) != 0u;
    let lx = textureSample(terrain_pal, terrain_pal_samp, p.zy * scale, slot).rgb;
    let ly = textureSample(terrain_pal, terrain_pal_samp, p.xz * scale, slot).rgb;
    let lz = textureSample(terrain_pal, terrain_pal_samp, p.xy * scale, slot).rgb;
    let nx = textureSample(terrain_pal, terrain_pal_samp_nearest, p.zy * scale, slot).rgb;
    let ny = textureSample(terrain_pal, terrain_pal_samp_nearest, p.xz * scale, slot).rgb;
    let nz = textureSample(terrain_pal, terrain_pal_samp_nearest, p.xy * scale, slot).rgb;
    return select(lx, nx, nearest) * w.x + select(ly, ny, nearest) * w.y + select(lz, nz, nearest) * w.z;
}

// The meshed-terrain albedo: the palette slot (vcolor.a, 1-based, fractional at slot
// boundaries for a smooth crossfade) triplanar-sampled × the per-vertex tint × 1.6, or the
// flat tint where untextured (slot 0). Mirrors the raymarch's `terrain_albedo`.
fn terrain_splat_albedo(in: VsOut) -> vec3<f32> {
    let tint = in.vcolor.rgb;
    let a = in.vcolor.a * 255.0; // 1-based slot; 0 = untextured
    if (a < 0.5) {
        return tint;
    }
    let lo = floor(a);
    let f = a - lo;
    let c_lo = terrain_triplanar(i32(lo) - 1, in.lpos, in.lnorm) * tint * 1.6;
    let c_hi = terrain_triplanar(i32(ceil(a)) - 1, in.lpos, in.lnorm) * tint * 1.6;
    return mix(c_lo, c_hi, f);
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
    let n = facing_normal(normalize(in.normal), front);
    let l = normalize(g.light_dir.xyz);
    let v = normalize(-in.view_pos);
    let ndl = max(dot(n, l), 0.0);
    let texel = base_texel(in);
    // MESHED TERRAIN (tsplat flag): the vertex color's alpha is a palette SLOT, not opacity,
    // and albedo comes from triplanar-splatting the palette. Terrain is always opaque, so it
    // bypasses both the alpha multiply and the cutout below (whose test would otherwise
    // discard it — a slot index reads as a near-zero alpha). Everything else takes the
    // normal vertex-paint multiply.
    let terrain = in.tsplat > 0.5;
    // Vertex paint MULTIPLIES — it tints the textured surface rather than replacing
    // it, which is what lets painted color stand in for baked lighting/AO. "Replace"
    // needs no mode of its own: it's this multiply against a white texture.
    let albedo = select(texel.rgb * in.color.rgb * in.vcolor.rgb, terrain_splat_albedo(in) * in.color.rgb, terrain);
    let emissive = in.emissive.rgb * in.emissive.a;
    // Opacity: the material's alpha (in.color.a) times the texture's own alpha,
    // times painted alpha. Terrain is forced opaque (its vcolor.a is a slot).
    let alpha = select(in.color.a * texel.a * in.vcolor.a, in.color.a, terrain);

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
    // Terrain is always opaque and its vcolor.a is a SLOT, not opacity — prime depth for it
    // unconditionally (else a hill wouldn't cap the raymarch and blobs would show through it).
    if (in.tsplat > 0.5) {
        return;
    }
    // Same tiled sampling as the color pass, so the conservative alpha test
    // sees the texels that will actually shade — INCLUDING painted alpha, or the
    // prepass would prime depth for fragments the color pass then blends away.
    let a = base_texel(in).a * in.color.a * in.vcolor.a;
    if (a < 0.99) {
        discard;
    }
}
