//! The stdlib: every op a shader can call — its signature (what the parser,
//! type checker, autocomplete and the graph palette read) and how it emits
//! WGSL (either a builtin spelling or a support function from
//! [`SUPPORT_WGSL`]). Growth rule (ADR-0007): if a node doesn't help make
//! something nobody's seen, it waits.

use crate::ir::{Stage, Ty};

/// A signature slot type. `Exact`/`Generic` are value edges; `Texture`/`Str`
/// are compile-time params (a declared slot name, a literal string).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SigTy {
    Exact(Ty),
    /// Any type; all `Generic` slots of one call unify (scalars mix in free).
    Generic,
    /// `vec2` or `vec3` (spatial ops that work in UV space and world space).
    GenericVec,
    Texture,
    Str,
}

/// One named input slot of an op.
#[derive(Clone, Copy, Debug)]
pub struct SigInput {
    pub name: &'static str,
    pub ty: SigTy,
    /// `Some` = optional, filled with this scalar when omitted.
    pub default: Option<f64>,
}

/// How a call spells itself in WGSL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Emit {
    /// A WGSL builtin or plain function: `name(args…)`.
    Fn(&'static str),
    /// A support function suffixed by the generic's lane count (`flsl_fbm2`,
    /// `flsl_fbm3`) — the type-directed overload trick.
    FnByLanes(&'static str),
    /// Handled specially by the emitter (sample, palette, constructors, …).
    Special,
}

/// One stdlib op.
#[derive(Debug)]
pub struct OpSpec {
    pub name: &'static str,
    pub inputs: &'static [SigInput],
    pub output: SigTy,
    pub stages: &'static [Stage],
    pub emit: Emit,
    /// One-liner for autocomplete / the docs page / graph palette tooltips.
    pub doc: &'static str,
    /// Palette category (math / noise / color / texture / sdf / engine).
    pub category: &'static str,
}

const fn req(name: &'static str, ty: SigTy) -> SigInput {
    SigInput { name, ty, default: None }
}

const fn opt(name: &'static str, ty: SigTy, default: f64) -> SigInput {
    SigInput { name, ty, default: Some(default) }
}

const BOTH: &[Stage] = &[Stage::Fragment, Stage::Sdf];
const FRAG: &[Stage] = &[Stage::Fragment];
// Pure math/noise/color: valid in ANY stage, including a Sky shader.
const ANY: &[Stage] = &[Stage::Fragment, Stage::Sdf, Stage::Sky];
// Fragment + Sky (surface-or-sky color helpers; no field position needed).
const FSKY: &[Stage] = &[Stage::Fragment, Stage::Sky];

const F: SigTy = SigTy::Exact(Ty::Float);
const V2: SigTy = SigTy::Exact(Ty::Vec2);
const V3: SigTy = SigTy::Exact(Ty::Vec3);
const V4: SigTy = SigTy::Exact(Ty::Vec4);
const G: SigTy = SigTy::Generic;
const GV: SigTy = SigTy::GenericVec;

/// The registry. Order = palette/docs order within categories.
pub static OPS: &[OpSpec] = &[
    // ---- math (WGSL builtins, generic) ------------------------------------
    OpSpec { name: "abs", inputs: &[req("x", G)], output: G, stages: ANY, emit: Emit::Fn("abs"), doc: "Absolute value.", category: "math" },
    OpSpec { name: "sign", inputs: &[req("x", G)], output: G, stages: ANY, emit: Emit::Fn("sign"), doc: "-1, 0 or 1 per component.", category: "math" },
    OpSpec { name: "floor", inputs: &[req("x", G)], output: G, stages: ANY, emit: Emit::Fn("floor"), doc: "Round down.", category: "math" },
    OpSpec { name: "ceil", inputs: &[req("x", G)], output: G, stages: ANY, emit: Emit::Fn("ceil"), doc: "Round up.", category: "math" },
    OpSpec { name: "round", inputs: &[req("x", G)], output: G, stages: ANY, emit: Emit::Fn("round"), doc: "Round to nearest.", category: "math" },
    OpSpec { name: "fract", inputs: &[req("x", G)], output: G, stages: ANY, emit: Emit::Fn("fract"), doc: "Fractional part (x - floor(x)).", category: "math" },
    OpSpec { name: "sqrt", inputs: &[req("x", G)], output: G, stages: ANY, emit: Emit::Fn("sqrt"), doc: "Square root.", category: "math" },
    OpSpec { name: "sin", inputs: &[req("x", G)], output: G, stages: ANY, emit: Emit::Fn("sin"), doc: "Sine (radians).", category: "math" },
    OpSpec { name: "cos", inputs: &[req("x", G)], output: G, stages: ANY, emit: Emit::Fn("cos"), doc: "Cosine (radians).", category: "math" },
    OpSpec { name: "tan", inputs: &[req("x", G)], output: G, stages: ANY, emit: Emit::Fn("tan"), doc: "Tangent (radians).", category: "math" },
    OpSpec { name: "exp", inputs: &[req("x", G)], output: G, stages: ANY, emit: Emit::Fn("exp"), doc: "e^x.", category: "math" },
    OpSpec { name: "log", inputs: &[req("x", G)], output: G, stages: ANY, emit: Emit::Fn("log"), doc: "Natural log.", category: "math" },
    OpSpec { name: "pow", inputs: &[req("x", G), req("y", G)], output: G, stages: ANY, emit: Emit::Fn("pow"), doc: "x^y.", category: "math" },
    OpSpec { name: "min", inputs: &[req("a", G), req("b", G)], output: G, stages: ANY, emit: Emit::Fn("min"), doc: "Per-component minimum.", category: "math" },
    OpSpec { name: "max", inputs: &[req("a", G), req("b", G)], output: G, stages: ANY, emit: Emit::Fn("max"), doc: "Per-component maximum.", category: "math" },
    OpSpec { name: "clamp", inputs: &[req("x", G), req("lo", G), req("hi", G)], output: G, stages: ANY, emit: Emit::Fn("clamp"), doc: "Constrain x to [lo, hi].", category: "math" },
    OpSpec { name: "saturate", inputs: &[req("x", G)], output: G, stages: ANY, emit: Emit::Fn("saturate"), doc: "Clamp to [0, 1].", category: "math" },
    OpSpec { name: "mix", inputs: &[req("a", G), req("b", G), req("t", G)], output: G, stages: ANY, emit: Emit::Fn("mix"), doc: "Linear blend: a + (b-a)·t.", category: "math" },
    OpSpec { name: "step", inputs: &[req("edge", G), req("x", G)], output: G, stages: ANY, emit: Emit::Fn("step"), doc: "0 below edge, 1 at/above.", category: "math" },
    OpSpec { name: "smoothstep", inputs: &[req("lo", G), req("hi", G), req("x", G)], output: G, stages: ANY, emit: Emit::Fn("smoothstep"), doc: "Smooth 0→1 ramp between lo and hi.", category: "math" },
    OpSpec { name: "dot", inputs: &[req("a", G), req("b", G)], output: F, stages: ANY, emit: Emit::Fn("dot"), doc: "Dot product.", category: "math" },
    OpSpec { name: "cross", inputs: &[req("a", V3), req("b", V3)], output: V3, stages: ANY, emit: Emit::Fn("cross"), doc: "Cross product.", category: "math" },
    OpSpec { name: "normalize", inputs: &[req("v", G)], output: G, stages: ANY, emit: Emit::Fn("normalize"), doc: "Unit-length vector.", category: "math" },
    OpSpec { name: "length", inputs: &[req("v", G)], output: F, stages: ANY, emit: Emit::Fn("length"), doc: "Vector length.", category: "math" },
    OpSpec { name: "distance", inputs: &[req("a", G), req("b", G)], output: F, stages: ANY, emit: Emit::Fn("distance"), doc: "Distance between points.", category: "math" },
    OpSpec { name: "reflect", inputs: &[req("i", G), req("n", G)], output: G, stages: ANY, emit: Emit::Fn("reflect"), doc: "Reflect i around normal n.", category: "math" },

    // ---- noise -------------------------------------------------------------
    OpSpec { name: "valueNoise", inputs: &[req("p", GV)], output: F, stages: ANY, emit: Emit::FnByLanes("flsl_vnoise"), doc: "Smooth value noise in [0, 1].", category: "noise" },
    OpSpec { name: "noise", inputs: &[req("p", GV)], output: F, stages: ANY, emit: Emit::FnByLanes("flsl_gnoise"), doc: "Gradient noise in [-1, 1] (simplex-style look).", category: "noise" },
    OpSpec { name: "worley", inputs: &[req("p", GV)], output: F, stages: ANY, emit: Emit::FnByLanes("flsl_worley"), doc: "Cellular noise: distance to the nearest feature point.", category: "noise" },
    OpSpec { name: "fbm", inputs: &[req("p", GV), opt("octaves", F, 4.0), opt("lacunarity", F, 2.0), opt("gain", F, 0.5)], output: F, stages: ANY, emit: Emit::FnByLanes("flsl_fbm"), doc: "Fractal noise: layered octaves of gradient noise, in ≈[-1, 1].", category: "noise" },
    OpSpec { name: "domainWarp", inputs: &[req("p", GV), opt("scale", F, 1.0), opt("time", F, 0.0), opt("strength", F, 1.0)], output: GV, stages: ANY, emit: Emit::FnByLanes("flsl_warp"), doc: "Melt space: offset p by drifting noise (feed the result to other nodes).", category: "noise" },

    // ---- color -------------------------------------------------------------
    OpSpec { name: "luminance", inputs: &[req("c", V3)], output: F, stages: FSKY, emit: Emit::Fn("flsl_luma"), doc: "Perceptual brightness of a color.", category: "color" },
    OpSpec { name: "toHsv", inputs: &[req("c", V3)], output: V3, stages: FSKY, emit: Emit::Fn("flsl_to_hsv"), doc: "RGB → hue/saturation/value.", category: "color" },
    OpSpec { name: "fromHsv", inputs: &[req("c", V3)], output: V3, stages: FSKY, emit: Emit::Fn("flsl_from_hsv"), doc: "Hue/saturation/value → RGB.", category: "color" },
    OpSpec { name: "hueShift", inputs: &[req("c", V3), req("shift", F)], output: V3, stages: FSKY, emit: Emit::Fn("flsl_hue_shift"), doc: "Rotate a color's hue (1.0 = a full cycle).", category: "color" },
    OpSpec { name: "posterize", inputs: &[req("c", V3), opt("steps", F, 6.0)], output: V3, stages: FSKY, emit: Emit::Fn("flsl_posterize"), doc: "Quantize to N bands — the retro look.", category: "color" },
    OpSpec { name: "gamma", inputs: &[req("c", V3), req("g", F)], output: V3, stages: FSKY, emit: Emit::Fn("flsl_gamma"), doc: "Gamma curve (g > 1 darkens mids).", category: "color" },
    OpSpec { name: "contrast", inputs: &[req("c", V3), req("amount", F)], output: V3, stages: FSKY, emit: Emit::Fn("flsl_contrast"), doc: "Push colors away from mid grey (1 = unchanged).", category: "color" },
    OpSpec { name: "palette", inputs: &[req("t", F), req("name", SigTy::Str)], output: V3, stages: FSKY, emit: Emit::Special, doc: "A looping color ramp: \"sunset\", \"bruise\", \"neon\", \"ocean\", \"ember\", \"mono\".", category: "color" },

    // ---- texture -----------------------------------------------------------
    OpSpec { name: "sample", inputs: &[req("tex", SigTy::Texture), req("uv", V2)], output: V4, stages: FRAG, emit: Emit::Special, doc: "Sample a declared texture slot (honors the slot's tiling block from the material).", category: "texture" },
    OpSpec { name: "sampleTriplanar", inputs: &[req("tex", SigTy::Texture), req("p", V3), req("n", V3)], output: V4, stages: FRAG, emit: Emit::Special, doc: "Project a slot's texture from three axes, blended by the normal — clean tiling with no UVs (scale/blend come from the slot's tiling block).", category: "texture" },
    OpSpec { name: "baseTexture", inputs: &[opt("uv", V2, f64::NAN)], output: V4, stages: FRAG, emit: Emit::Special, doc: "The node's own base-color texture, through the material's tiling (pass a uv to sample it raw somewhere else).", category: "texture" },

    // ---- sdf ---------------------------------------------------------------
    OpSpec { name: "sphere", inputs: &[req("p", V3), opt("radius", F, 1.0)], output: F, stages: BOTH, emit: Emit::Fn("flsl_sd_sphere"), doc: "Distance to a sphere at the origin.", category: "sdf" },
    OpSpec { name: "box", inputs: &[req("p", V3), req("size", V3), opt("rounding", F, 0.0)], output: F, stages: BOTH, emit: Emit::Fn("flsl_sd_box"), doc: "Distance to a box (half-extents `size`), optionally rounded.", category: "sdf" },
    OpSpec { name: "torus", inputs: &[req("p", V3), opt("major", F, 1.0), opt("minor", F, 0.25)], output: F, stages: BOTH, emit: Emit::Fn("flsl_sd_torus"), doc: "Distance to a torus in the XZ plane.", category: "sdf" },
    OpSpec { name: "plane", inputs: &[req("p", V3), req("normal", V3), opt("offset", F, 0.0)], output: F, stages: BOTH, emit: Emit::Fn("flsl_sd_plane"), doc: "Distance to an infinite plane.", category: "sdf" },
    OpSpec { name: "opUnion", inputs: &[req("a", F), req("b", F)], output: F, stages: BOTH, emit: Emit::Fn("min"), doc: "Merge two shapes (hard edge).", category: "sdf" },
    OpSpec { name: "opSubtract", inputs: &[req("a", F), req("b", F)], output: F, stages: BOTH, emit: Emit::Fn("flsl_op_sub"), doc: "Carve b out of a.", category: "sdf" },
    OpSpec { name: "opIntersect", inputs: &[req("a", F), req("b", F)], output: F, stages: BOTH, emit: Emit::Fn("max"), doc: "Keep only the overlap of two shapes.", category: "sdf" },
    OpSpec { name: "smoothMin", inputs: &[req("a", F), req("b", F), opt("k", F, 0.3)], output: F, stages: BOTH, emit: Emit::Fn("flsl_smin"), doc: "Merge two shapes with a soft organic blend of width k.", category: "sdf" },
    OpSpec { name: "repeat", inputs: &[req("p", V3), req("cell", V3)], output: V3, stages: BOTH, emit: Emit::Fn("flsl_op_repeat"), doc: "Tile space: repeats whatever shape reads the result every `cell` units.", category: "sdf" },
    OpSpec { name: "twist", inputs: &[req("p", V3), req("amount", F)], output: V3, stages: BOTH, emit: Emit::Fn("flsl_op_twist"), doc: "Twist space around the Y axis (radians per unit height).", category: "sdf" },

    // ---- engine hooks (the concat-seam dividend) ----------------------------
    OpSpec { name: "litSurface", inputs: &[req("albedo", V3)], output: V3, stages: FRAG, emit: Emit::Special, doc: "The engine's full lighting on your albedo: sun + shadows + AO + point lights + specular/rim from the node's Material.", category: "engine" },
    OpSpec { name: "sunShadow", inputs: &[req("p", V3), req("n", V3)], output: V3, stages: FRAG, emit: Emit::Special, doc: "The scene's marched sun-shadow factor at a point (1 = fully lit).", category: "engine" },
    OpSpec { name: "sdfAo", inputs: &[req("p", V3), req("n", V3)], output: F, stages: FRAG, emit: Emit::Fn("flsl_ao"), doc: "True SDF ambient occlusion at a point (1 = open sky).", category: "engine" },
    OpSpec { name: "applyFog", inputs: &[req("c", V3), req("p", V3)], output: V3, stages: FRAG, emit: Emit::Special, doc: "The scene's distance fog applied to a color.", category: "engine" },
    OpSpec { name: "fieldDistance", inputs: &[req("p", V3)], output: F, stages: FRAG, emit: Emit::Fn("map_d"), doc: "Distance from a point to the scene's SDF field (terrain + blobs) — glow near walls, darken in crevices…", category: "engine" },
];

/// Look up an op by its `.flsl` name.
pub fn op(name: &str) -> Option<&'static OpSpec> {
    OPS.iter().find(|o| o.name == name)
}

/// The pseudo-specs behind `vec2/3/4(…)` constructors (shape-checked
/// structurally in ir.rs; these exist so `ResolvedCall.op` is uniform).
pub fn constructor_spec(name: &str) -> &'static OpSpec {
    static V2C: OpSpec = OpSpec { name: "vec2", inputs: &[], output: V2, stages: ANY, emit: Emit::Special, doc: "Build a vec2 from components.", category: "math" };
    static V3C: OpSpec = OpSpec { name: "vec3", inputs: &[], output: V3, stages: ANY, emit: Emit::Special, doc: "Build a vec3 from components.", category: "math" };
    static V4C: OpSpec = OpSpec { name: "vec4", inputs: &[], output: V4, stages: ANY, emit: Emit::Special, doc: "Build a vec4 from components.", category: "math" };
    match name {
        "vec2" => &V2C,
        "vec3" => &V3C,
        _ => &V4C,
    }
}

/// Named palette coefficient sets for `palette(t, "name")` — iq's cosine
/// palette `a + b·cos(2π(c·t + d))`, coefficients baked at emit time.
pub fn palette_coeffs(name: &str) -> Option<[[f32; 3]; 4]> {
    Some(match name {
        "sunset" => [[0.50, 0.36, 0.35], [0.50, 0.40, 0.35], [1.00, 1.00, 1.00], [0.00, 0.15, 0.30]],
        "bruise" => [[0.45, 0.30, 0.50], [0.45, 0.35, 0.40], [1.00, 1.00, 1.00], [0.30, 0.60, 0.75]],
        "neon" => [[0.50, 0.50, 0.50], [0.50, 0.50, 0.50], [1.00, 1.00, 1.00], [0.00, 0.33, 0.67]],
        "ocean" => [[0.20, 0.45, 0.55], [0.30, 0.35, 0.40], [1.00, 1.00, 1.00], [0.55, 0.65, 0.75]],
        "ember" => [[0.55, 0.25, 0.10], [0.45, 0.30, 0.15], [1.00, 1.00, 1.00], [0.00, 0.10, 0.20]],
        "mono" => [[0.50, 0.50, 0.50], [0.50, 0.50, 0.50], [1.00, 1.00, 1.00], [0.00, 0.00, 0.00]],
        _ => return None,
    })
}

pub const PALETTE_NAMES: &[&str] = &["sunset", "bruise", "neon", "ocean", "ember", "mono"];

/// The WGSL support library appended (once) to any generated module that uses
/// a `flsl_*` function. Self-contained pure math — everything engine-side
/// (`sun_shadow`, `sdf_ao`, `map_d`, `apply_fog`, `G`, `in`) comes from the
/// pass module the generated code is concatenated onto.
pub const SUPPORT_WGSL: &str = r#"
// ---- floptle-shader stdlib support (generated modules only) ----------------

// Integer-lattice hashes (PCG-style bit mixing). The old float hashes
// (`fract(p * 456.21)`…) silently quantized once the product outgrew f32
// fract precision — beyond ~50 lattice cells the noise degraded into
// straight-edged constant sheets (sky shaders hit this instantly: a cloud
// plane projected toward the horizon runs through hundreds of cells). The
// lattice ids these receive are integers (± constant offsets), so hashing
// their BITS is exact at any distance from the origin.
fn flsl_hash2(p: vec2<f32>) -> f32 {
    let xi = bitcast<u32>(i32(round(p.x * 8.0)));
    let yi = bitcast<u32>(i32(round(p.y * 8.0)));
    var h = xi * 0x85EBCA6Bu ^ yi * 0xC2B2AE35u;
    h = (h ^ (h >> 16u)) * 0x045D9F3Bu;
    h = h ^ (h >> 16u);
    return f32(h >> 8u) * (1.0 / 16777216.0);
}

fn flsl_hash3(p: vec3<f32>) -> f32 {
    let xi = bitcast<u32>(i32(round(p.x * 8.0)));
    let yi = bitcast<u32>(i32(round(p.y * 8.0)));
    let zi = bitcast<u32>(i32(round(p.z * 8.0)));
    var h = xi * 0x85EBCA6Bu ^ yi * 0xC2B2AE35u ^ zi * 0x27D4EB2Fu;
    h = (h ^ (h >> 16u)) * 0x045D9F3Bu;
    h = h ^ (h >> 16u);
    return f32(h >> 8u) * (1.0 / 16777216.0);
}

fn flsl_hash22(p: vec2<f32>) -> vec2<f32> {
    return vec2<f32>(flsl_hash2(p), flsl_hash2(p + 17.17));
}

fn flsl_hash33(p: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(flsl_hash3(p), flsl_hash3(p + 17.17), flsl_hash3(p + 31.31));
}

fn flsl_vnoise2(p: vec2<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * (3.0 - 2.0 * f);
    let a = flsl_hash2(i);
    let b = flsl_hash2(i + vec2<f32>(1.0, 0.0));
    let c = flsl_hash2(i + vec2<f32>(0.0, 1.0));
    let d = flsl_hash2(i + vec2<f32>(1.0, 1.0));
    return mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
}

fn flsl_vnoise3(p: vec3<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * (3.0 - 2.0 * f);
    let n000 = flsl_hash3(i);
    let n100 = flsl_hash3(i + vec3<f32>(1.0, 0.0, 0.0));
    let n010 = flsl_hash3(i + vec3<f32>(0.0, 1.0, 0.0));
    let n110 = flsl_hash3(i + vec3<f32>(1.0, 1.0, 0.0));
    let n001 = flsl_hash3(i + vec3<f32>(0.0, 0.0, 1.0));
    let n101 = flsl_hash3(i + vec3<f32>(1.0, 0.0, 1.0));
    let n011 = flsl_hash3(i + vec3<f32>(0.0, 1.0, 1.0));
    let n111 = flsl_hash3(i + vec3<f32>(1.0, 1.0, 1.0));
    let nx00 = mix(n000, n100, u.x);
    let nx10 = mix(n010, n110, u.x);
    let nx01 = mix(n001, n101, u.x);
    let nx11 = mix(n011, n111, u.x);
    return mix(mix(nx00, nx10, u.y), mix(nx01, nx11, u.y), u.z);
}

// Gradient-style noise in [-1, 1]: value noise resampled around zero with a
// second rotated tap to break up the grid (cheap, looks organic).
fn flsl_gnoise2(p: vec2<f32>) -> f32 {
    let a = flsl_vnoise2(p);
    let b = flsl_vnoise2(mat2x2<f32>(vec2<f32>(0.8, 0.6), vec2<f32>(-0.6, 0.8)) * p * 1.7 + 11.5);
    return (a + b) - 1.0;
}

fn flsl_gnoise3(p: vec3<f32>) -> f32 {
    let a = flsl_vnoise3(p);
    let b = flsl_vnoise3(p.yzx * 1.7 + 11.5);
    return (a + b) - 1.0;
}

fn flsl_worley2(p: vec2<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    var d = 8.0;
    for (var y = -1; y <= 1; y++) {
        for (var x = -1; x <= 1; x++) {
            let g = vec2<f32>(f32(x), f32(y));
            let o = flsl_hash22(i + g);
            d = min(d, length(g + o - f));
        }
    }
    return d;
}

fn flsl_worley3(p: vec3<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    var d = 8.0;
    for (var z = -1; z <= 1; z++) {
        for (var y = -1; y <= 1; y++) {
            for (var x = -1; x <= 1; x++) {
                let g = vec3<f32>(f32(x), f32(y), f32(z));
                let o = flsl_hash33(i + g);
                d = min(d, length(g + o - f));
            }
        }
    }
    return d;
}

fn flsl_fbm2(p: vec2<f32>, octaves: f32, lacunarity: f32, gain: f32) -> f32 {
    var q = p;
    var amp = 0.5;
    var acc = 0.0;
    let n = clamp(i32(round(octaves)), 1, 8);
    for (var i = 0; i < n; i++) {
        acc += amp * flsl_gnoise2(q);
        q *= lacunarity;
        amp *= gain;
    }
    return acc;
}

fn flsl_fbm3(p: vec3<f32>, octaves: f32, lacunarity: f32, gain: f32) -> f32 {
    var q = p;
    var amp = 0.5;
    var acc = 0.0;
    let n = clamp(i32(round(octaves)), 1, 8);
    for (var i = 0; i < n; i++) {
        acc += amp * flsl_gnoise3(q);
        q *= lacunarity;
        amp *= gain;
    }
    return acc;
}

fn flsl_warp2(p: vec2<f32>, scale: f32, time: f32, strength: f32) -> vec2<f32> {
    let q = p * scale;
    let dx = flsl_fbm2(q + vec2<f32>(0.0, time * 0.35), 4.0, 2.0, 0.5);
    let dy = flsl_fbm2(q + vec2<f32>(5.2, 1.3 - time * 0.28), 4.0, 2.0, 0.5);
    return p + vec2<f32>(dx, dy) * (strength / max(scale, 1e-4));
}

fn flsl_warp3(p: vec3<f32>, scale: f32, time: f32, strength: f32) -> vec3<f32> {
    let q = p * scale;
    let dx = flsl_fbm3(q + vec3<f32>(0.0, time * 0.35, 0.0), 4.0, 2.0, 0.5);
    let dy = flsl_fbm3(q + vec3<f32>(5.2, 1.3, -time * 0.28), 4.0, 2.0, 0.5);
    let dz = flsl_fbm3(q + vec3<f32>(-3.1, 2.7, time * 0.22), 4.0, 2.0, 0.5);
    return p + vec3<f32>(dx, dy, dz) * (strength / max(scale, 1e-4));
}

fn flsl_luma(c: vec3<f32>) -> f32 {
    return dot(c, vec3<f32>(0.2126, 0.7152, 0.0722));
}

fn flsl_to_hsv(c: vec3<f32>) -> vec3<f32> {
    let mx = max(c.r, max(c.g, c.b));
    let mn = min(c.r, min(c.g, c.b));
    let d = mx - mn;
    var h = 0.0;
    if (d > 1e-5) {
        if (mx == c.r) {
            h = ((c.g - c.b) / d) / 6.0;
        } else if (mx == c.g) {
            h = (2.0 + (c.b - c.r) / d) / 6.0;
        } else {
            h = (4.0 + (c.r - c.g) / d) / 6.0;
        }
        h = fract(h + 1.0);
    }
    let s = select(0.0, d / mx, mx > 1e-5);
    return vec3<f32>(h, s, mx);
}

fn flsl_from_hsv(c: vec3<f32>) -> vec3<f32> {
    let k = fract(vec3<f32>(0.0, 2.0 / 3.0, 1.0 / 3.0) + c.x) * 6.0;
    let rgb = clamp(abs(k - 3.0) - 1.0, vec3<f32>(0.0), vec3<f32>(1.0));
    return c.z * mix(vec3<f32>(1.0), rgb, c.y);
}

fn flsl_hue_shift(c: vec3<f32>, shift: f32) -> vec3<f32> {
    var hsv = flsl_to_hsv(c);
    hsv.x = fract(hsv.x + shift);
    return flsl_from_hsv(hsv);
}

fn flsl_posterize(c: vec3<f32>, steps: f32) -> vec3<f32> {
    let n = max(steps, 1.0);
    return floor(c * n) / n;
}

fn flsl_gamma(c: vec3<f32>, g: f32) -> vec3<f32> {
    return pow(max(c, vec3<f32>(0.0)), vec3<f32>(g));
}

fn flsl_contrast(c: vec3<f32>, amount: f32) -> vec3<f32> {
    return (c - 0.5) * amount + 0.5;
}

fn flsl_palette(t: f32, a: vec3<f32>, b: vec3<f32>, c: vec3<f32>, d: vec3<f32>) -> vec3<f32> {
    return a + b * cos(6.2831853 * (c * t + d));
}

fn flsl_sd_sphere(p: vec3<f32>, r: f32) -> f32 {
    return length(p) - r;
}

fn flsl_sd_box(p: vec3<f32>, size: vec3<f32>, rounding: f32) -> f32 {
    let q = abs(p) - size + rounding;
    return length(max(q, vec3<f32>(0.0))) + min(max(q.x, max(q.y, q.z)), 0.0) - rounding;
}

fn flsl_sd_torus(p: vec3<f32>, major: f32, minor: f32) -> f32 {
    let q = vec2<f32>(length(p.xz) - major, p.y);
    return length(q) - minor;
}

fn flsl_sd_plane(p: vec3<f32>, n: vec3<f32>, offset: f32) -> f32 {
    return dot(p, normalize(n)) + offset;
}

fn flsl_op_sub(a: f32, b: f32) -> f32 {
    return max(a, -b);
}

fn flsl_smin(a: f32, b: f32, k: f32) -> f32 {
    let kk = max(k, 1e-4);
    let h = clamp(0.5 + 0.5 * (b - a) / kk, 0.0, 1.0);
    return mix(b, a, h) - kk * h * (1.0 - h);
}

// Tile space every `cell` units per axis; a zero (or negative) cell leaves
// that axis un-repeated instead of collapsing it.
fn flsl_op_repeat(p: vec3<f32>, cell: vec3<f32>) -> vec3<f32> {
    let reps = step(vec3<f32>(1e-3), cell);
    let c = max(cell, vec3<f32>(1e-3));
    return mix(p, p - c * round(p / c), reps);
}

fn flsl_op_twist(p: vec3<f32>, amount: f32) -> vec3<f32> {
    let a = amount * p.y;
    let c = cos(a);
    let s = sin(a);
    return vec3<f32>(c * p.x - s * p.z, p.y, s * p.x + c * p.z);
}

// AO gated exactly like the built-in surface path: free when the PostProcess
// node has SDF AO off.
fn flsl_ao(p: vec3<f32>, n: vec3<f32>) -> f32 {
    if (G.ao_params.x > 0.5) {
        return sdf_ao(p, n);
    }
    return 1.0;
}

// A texture slot sampled through its material tiling block.
// ta = (count.xy, offset.xy); tb = (mode, rotation_rad, triplanar_scale, blend).
// The branch condition loads from a UNIFORM buffer, so control flow stays
// uniform and plain textureSample is legal inside.
fn flsl_tiled_sample(t: texture_2d<f32>, s: sampler, uv: vec2<f32>, ta: vec4<f32>, tb: vec4<f32>) -> vec4<f32> {
    let mode = u32(tb.x + 0.5);
    if (mode == 1u) {
        let c = cos(tb.y);
        let sn = sin(tb.y);
        let m = mat2x2<f32>(vec2<f32>(c, sn), vec2<f32>(-sn, c));
        return textureSample(t, s, m * ((uv - 0.5) * ta.xy) + 0.5 + ta.zw);
    }
    return textureSample(t, s, uv);
}

// Triplanar projection of a slot's texture: three axis samples blended by the
// normal. scale = tile size in the units of `p`, blend = axis-edge sharpness.
fn flsl_triplanar(t: texture_2d<f32>, s: sampler, p: vec3<f32>, n: vec3<f32>, scale: f32, blend: f32) -> vec4<f32> {
    let sc = max(scale, 1e-4);
    let q = p / sc;
    var w = pow(abs(normalize(n)), vec3<f32>(max(blend, 0.5)));
    w = w / (w.x + w.y + w.z);
    return textureSample(t, s, q.zy) * w.x
        + textureSample(t, s, q.xz) * w.y
        + textureSample(t, s, q.xy) * w.z;
}
"#;
