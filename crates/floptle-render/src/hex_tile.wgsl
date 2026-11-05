// Hex tiling (card 0626): the helpers every textured pass shares. Spliced
// after `field.wgsl` into the raster and raymarch modules, and after
// `light2d.wgsl` into the 2D lighting module, so a surface's tile is broken
// up the same way in every picture that samples it. Bindings-free on purpose:
// anything here must be includable anywhere.
//
// A texture that repeats every couple of metres shows its period as a grid
// from 30 m up. Hex tiling (Mikkelsen 2022, "Practical Real-Time Hex-Tiling")
// lays a grid of hexagons about a tile across over the UVs; each hexagon reads
// the tile at its own random offset and rotation, and the three hexagons
// around a point blend, weighted so the blend keeps the texture's contrast
// instead of washing it grey. Three taps where there was one, and only for
// what asks for it. Gradients are rotated with the UVs, so filtering and mip
// selection are those of the plain tile.

// The hexagons around `uv`: three blend weights and, for each hexagon, the
// tile's UVs there and the rotation the gradients take with them.
struct HexTaps {
    w: vec3<f32>,
    uv1: vec2<f32>,
    uv2: vec2<f32>,
    uv3: vec2<f32>,
    r1: mat2x2<f32>,
    r2: mat2x2<f32>,
    r3: mat2x2<f32>,
};

// A hexagon's random (rotation, offset x, offset y), each 0..1, from its
// integer grid vertex. Integer hashing, so a cell a million tiles from the
// origin is as random as one at it.
fn hex_rand(v: vec2<f32>) -> vec3<f32> {
    var h = vec2<u32>(vec2<i32>(v));
    h = h * 1664525u + 1013904223u;
    h.x += h.y * 1664525u;
    h.y += h.x * 1664525u;
    h = h ^ (h >> vec2<u32>(16u));
    h.x += h.y * 1664525u;
    h.y += h.x * 1664525u;
    h = h ^ (h >> vec2<u32>(16u));
    let z = h.x * 747796405u + h.y * 2891336453u;
    return vec3<f32>(vec3<u32>(h, z ^ (z >> 16u)) >> vec3<u32>(8u)) * (1.0 / 16777216.0);
}

// One hexagon's tap: its centre in UV space, a rotation of up to ±π × `amount`
// about it, and a whole-tile random offset.
fn hex_tap(uv: vec2<f32>, v: vec2<f32>, amount: f32) -> vec4<f32> {
    let rnd = hex_rand(v);
    // The grid vertex back in UV space (the inverse of the skew below, then
    // of the grid's scale).
    let centre = vec2<f32>(v.x + 0.5 * v.y, 0.8660254 * v.y) * 0.57735027;
    let a = (rnd.x * 2.0 - 1.0) * 3.14159265 * amount;
    let r = vec2<f32>(cos(a), sin(a));
    let d = uv - centre;
    let t = vec2<f32>(r.x * d.x - r.y * d.y, r.y * d.x + r.x * d.y) + centre + rnd.yz;
    return vec4<f32>(t, r);
}

fn hex_taps(uv: vec2<f32>, amount: f32) -> HexTaps {
    // A triangle grid whose vertices sit about 0.58 tile apart, so the
    // hexagon around each is about a tile across: big enough that a texture's
    // features survive whole, small enough that no period is left.
    let k = uv * 1.7320508;
    let skewed = vec2<f32>(k.x - 0.57735027 * k.y, 1.15470054 * k.y);
    let base = floor(skewed);
    let f = fract(skewed);
    let fz = 1.0 - f.x - f.y;
    let s = step(0.0, -fz);
    let s2 = 2.0 * s - 1.0;
    var o: HexTaps;
    o.w = vec3<f32>(-fz * s2, s - f.y * s2, s - f.x * s2);
    let t1 = hex_tap(uv, base + vec2<f32>(s, s), amount);
    let t2 = hex_tap(uv, base + vec2<f32>(s, 1.0 - s), amount);
    let t3 = hex_tap(uv, base + vec2<f32>(1.0 - s, s), amount);
    o.uv1 = t1.xy;
    o.uv2 = t2.xy;
    o.uv3 = t3.xy;
    o.r1 = mat2x2<f32>(vec2<f32>(t1.z, t1.w), vec2<f32>(-t1.w, t1.z));
    o.r2 = mat2x2<f32>(vec2<f32>(t2.z, t2.w), vec2<f32>(-t2.w, t2.z));
    o.r3 = mat2x2<f32>(vec2<f32>(t3.z, t3.w), vec2<f32>(-t3.w, t3.z));
    return o;
}

// The three taps blended. A plain barycentric blend averages three
// unrelated patches into a grey smear; raising the weights to a power keeps
// most of each point to one hexagon, and leaning them toward the brighter tap
// (Mikkelsen's contrast term) hides the seams in the texture's own detail.
fn hex_blend(c1: vec4<f32>, c2: vec4<f32>, c3: vec4<f32>, w: vec3<f32>) -> vec4<f32> {
    let ww = hex_weights(c1, c2, c3, w);
    return c1 * ww.x + c2 * ww.y + c3 * ww.z;
}

// The three blend weights `hex_blend` uses, for passes that blend several
// maps (albedo, normal, roughness...) with ONE set: weighed on the albedo
// taps, so the seams of every map fall in the same place.
fn hex_weights(c1: vec4<f32>, c2: vec4<f32>, c3: vec4<f32>, w: vec3<f32>) -> vec3<f32> {
    let lum = vec3<f32>(0.299, 0.587, 0.114);
    let dw = mix(vec3<f32>(1.0), vec3<f32>(dot(c1.rgb, lum), dot(c2.rgb, lum), dot(c3.rgb, lum)), 0.6);
    let ww = dw * pow(max(w, vec3<f32>(0.0)), vec3<f32>(7.0));
    return ww / max(ww.x + ww.y + ww.z, 1e-6);
}

// The packed tiling flags an instance carries (`InstanceRaw::rim.w` in the
// raster pass): bits 0-1 the mode, 2-13 the UV rotation in tenths of a
// degree, 14-19 the anti-tiling amount in 63rds.
fn tile_flags_rotation(flags: u32) -> mat2x2<f32> {
    let rot = f32((flags >> 2u) & 4095u) * 0.1 * 0.017453292519943295;
    let c = cos(rot);
    let sn = sin(rot);
    return mat2x2<f32>(vec2<f32>(c, sn), vec2<f32>(-sn, c));
}

fn tile_flags_antitile(flags: u32) -> f32 {
    return f32((flags >> 14u) & 63u) / 63.0;
}
