//! Per-node LIVE previews for the ◈ Shaders graph — every node renders a
//! little thumbnail of its value, Unity-style, so a beginner can watch the
//! look build up stage by stage.
//!
//! One shader edit = ONE generated WGSL module: every preview tile lives in a
//! grid atlas and a single fullscreen pass computes the shader's `let`s once
//! per pixel, then selects the hovered tile's value. Every tile value is
//! hoisted UNCONDITIONALLY before the `switch` so texture sampling stays in
//! uniform control flow (naga's uniformity analysis rejects `textureSample`
//! inside a position-dependent branch).
//!
//! Fragment previews evaluate on a soft dome (tile uv, a fake sphere normal,
//! a view-space slab position), lit by the same `flsl_lit` helper the real
//! pass uses, with neutral stand-ins for the pass symbols (no shadows, no
//! fog, a ground plane for `fieldDistance`). Sdf previews draw the classic
//! 2D distance cross-section (blue inside / amber outside, iso bands, a
//! white zero line) through the z = 0 plane.
//!
//! Literal numbers ride a uniform lane array (`DynNums`) instead of being
//! baked in, so dragging any inline value repaints the preview WITHOUT a
//! pipeline rebuild — the host re-uploads lanes each frame.

use crate::graph::{GNode, NodeKey, NodeKind};
use crate::ir::{self, Checked, ExprId, Input, ShaderIr, Stage, Ty};
use crate::stdlib;
use crate::transpile::{EmitCtx, TranspileError, Writer, MAX_TEXTURE_SLOTS, MAX_UNIFORMS};

/// The atlas holds at most this many tiles (an 8×8 grid) — graphs beyond it
/// preview their first 64 nodes.
pub const PREVIEW_MAX_TILES: usize = 64;

/// One thing a preview tile can show.
#[derive(Clone, Debug, PartialEq)]
pub enum PreviewTarget {
    /// A named let's value (`lets[i]`).
    Let(usize),
    /// An anonymous expression's value.
    Expr(ExprId),
    /// A knob's current value.
    Uniform(usize),
    /// A built-in input, visualized raw (uv gradient, time pulse…).
    Input(Input),
    /// A texture slot, sampled at tile uv.
    Texture(usize),
    /// The shader's final output (color, or the sdf cross-section).
    Output,
}

/// A compiled preview module + everything the host needs to drive it.
#[derive(Clone, Debug)]
pub struct CompiledPreview {
    /// A COMPLETE standalone WGSL module: `vs_pv` + `fs_pv` entry points.
    pub wgsl: String,
    pub stage: Stage,
    /// Uniforms in param-slot order (fragment: `P.u{i}`; sdf: the
    /// `G.shape_uniforms` array) — upload defaults every frame.
    pub uniforms: Vec<ir::Uniform>,
    /// Texture slot names in binding order (group(2), bindings 1+2i / 2+2i).
    pub textures: Vec<String>,
    /// Live-scalar slots: `(expr, lanes)` in lane order — read the expr's
    /// current literal each frame and upload into `PV.nums`.
    pub dyn_slots: Vec<(ExprId, u8)>,
    /// Tile count and grid shape (row-major, tile i at (i % cols, i / cols)).
    pub tiles: usize,
    pub cols: u32,
    pub rows: u32,
}

/// The tiles a graph view wants, in view order: one per node that draws a
/// thumbnail. Uniform/Constant nodes skip (their value is already an editor
/// widget on the node); everything else previews.
pub fn preview_targets(ir: &ShaderIr, view: &[GNode]) -> Vec<(NodeKey, PreviewTarget)> {
    let mut out = Vec::new();
    for n in view {
        if matches!(n.kind, NodeKind::Uniform(_) | NodeKind::Constant(_)) {
            continue;
        }
        let t = match &n.key {
            NodeKey::Out => PreviewTarget::Output,
            NodeKey::Anon(id) => PreviewTarget::Expr(*id),
            NodeKey::Let(name) => match ir.lets.iter().position(|(x, _)| x == name) {
                Some(i) => PreviewTarget::Let(i),
                None => continue,
            },
            NodeKey::Input(i) => PreviewTarget::Input(*i),
            NodeKey::Uniform(_) => continue,
            NodeKey::Texture(name) => match ir.textures.iter().position(|t| t == name) {
                Some(i) => PreviewTarget::Texture(i),
                None => continue,
            },
        };
        out.push((n.key.clone(), t));
        if out.len() == PREVIEW_MAX_TILES {
            break;
        }
    }
    out
}

/// Build the preview module for a checked shader. `targets` come from
/// [`preview_targets`] (at most [`PREVIEW_MAX_TILES`]).
pub fn transpile_preview(
    ir: &ShaderIr,
    ck: &Checked,
    targets: &[PreviewTarget],
) -> Result<CompiledPreview, TranspileError> {
    let stage = ir.stage.unwrap_or(Stage::Fragment);
    if targets.len() > PREVIEW_MAX_TILES {
        return Err(TranspileError { message: "too many preview tiles".into(), span: Default::default() });
    }
    if ir.textures.len() > MAX_TEXTURE_SLOTS || ir.uniforms.len() > MAX_UNIFORMS {
        return Err(TranspileError { message: "over slot caps".into(), span: Default::default() });
    }
    let count = targets.len().max(1);
    let cols = (count as f32).sqrt().ceil() as u32;
    let rows = count.div_ceil(cols as usize) as u32;

    let ctx = match stage {
        Stage::Fragment => EmitCtx::Fragment,
        Stage::Sdf => EmitCtx::Sdf { slot: 0 },
    };
    let mut w = Writer::new(ir, ck, ctx);
    w.dyn_nums = Some(Default::default());

    match stage {
        Stage::Fragment => w.raw(FRAG_PRELUDE),
        Stage::Sdf => w.raw(SDF_PRELUDE),
    }
    w.raw(stdlib::SUPPORT_WGSL);

    if stage == Stage::Fragment {
        // The same param block as the real material path, at group(2).
        w.line("struct FlslParams {".into(), None);
        if ir.uniforms.is_empty() && ir.textures.is_empty() {
            w.line("    _pad: vec4<f32>,".into(), None);
        }
        for (i, u) in ir.uniforms.iter().enumerate() {
            w.line(format!("    u{i}: vec4<f32>, // {}", u.name), None);
        }
        for (i, name) in ir.textures.iter().enumerate() {
            w.line(format!("    t{i}a: vec4<f32>, // {name}"), None);
            w.line(format!("    t{i}b: vec4<f32>,"), None);
        }
        w.line("};".into(), None);
        w.line("@group(2) @binding(0) var<uniform> P: FlslParams;".into(), None);
        for (i, name) in ir.textures.iter().enumerate() {
            w.line(
                format!("@group(2) @binding({}) var flsl_tex{i}: texture_2d<f32>; // {name}", 1 + 2 * i),
                None,
            );
            w.line(format!("@group(2) @binding({}) var flsl_samp{i}: sampler;", 2 + 2 * i), None);
        }
        w.raw(crate::transpile::FRAGMENT_LIT_WGSL);
        w.line("fn flsl_pv(in: VsOut, tile: u32) -> vec4<f32> {".into(), None);
    } else {
        w.line("fn flsl_pv(q: vec3<f32>, tile: u32) -> vec4<f32> {".into(), None);
    }

    // Every let once, in order (targets reference them by name).
    for (i, (name, root)) in ir.lets.iter().enumerate() {
        let expr = w.emit(*root)?;
        let ty = ck.ty(*root).wgsl();
        w.line(format!("    let l{i}_{name}: {ty} = {expr};"), None);
    }

    // Hoist every tile's visualized value BEFORE the switch (uniform control
    // flow for texture samples); the switch just selects.
    let mut vis: Vec<String> = Vec::new();
    for t in targets {
        vis.push(target_vis(&w, ir, ck, stage, t)?);
    }
    for (k, v) in vis.iter().enumerate() {
        w.line(format!("    let pv{k}: vec4<f32> = {v};"), None);
    }
    w.line("    var v = vec4<f32>(0.06, 0.06, 0.07, 1.0);".into(), None);
    w.line("    switch tile {".into(), None);
    for k in 0..vis.len() {
        w.line(format!("        case {k}u: {{ v = pv{k}; }}"), None);
    }
    w.line("        default: {}".into(), None);
    w.line("    }".into(), None);
    w.line("    return v;".into(), None);
    w.line("}".into(), None);

    w.raw(PV_VERTEX);
    match stage {
        Stage::Fragment => w.raw(FRAG_MAIN),
        Stage::Sdf => w.raw(SDF_MAIN),
    }

    let dyn_slots = w.dyn_nums.take().map(|d| d.into_inner().slots).unwrap_or_default();
    Ok(CompiledPreview {
        wgsl: std::mem::take(&mut w.out),
        stage,
        uniforms: ir.uniforms.clone(),
        textures: ir.textures.clone(),
        dyn_slots,
        tiles: targets.len(),
        cols,
        rows,
    })
}

/// One target's value as a display-ready `vec4<f32>` WGSL expression.
fn target_vis(
    w: &Writer,
    ir: &ShaderIr,
    ck: &Checked,
    stage: Stage,
    t: &PreviewTarget,
) -> Result<String, TranspileError> {
    let wrap = |e: String, ty: Ty| match (stage, ty) {
        (Stage::Sdf, Ty::Float) => format!("pv_dist({e})"),
        (_, Ty::Float) => format!("vec4<f32>(vec3<f32>({e}), 1.0)"),
        (_, Ty::Vec2) => format!("vec4<f32>({e}, 0.0, 1.0)"),
        (_, Ty::Vec3) => format!("vec4<f32>({e}, 1.0)"),
        (_, Ty::Vec4) => format!("({e})"),
    };
    Ok(match t {
        PreviewTarget::Let(i) => {
            let (name, root) = ir
                .lets
                .get(*i)
                .ok_or_else(|| TranspileError { message: "stale let".into(), span: Default::default() })?;
            wrap(format!("l{i}_{name}"), ck.ty(*root))
        }
        PreviewTarget::Expr(id) => wrap(w.emit(*id)?, ck.ty(*id)),
        PreviewTarget::Uniform(u) => {
            let uni = ir.uniforms.get(*u).ok_or_else(|| TranspileError {
                message: "stale uniform".into(),
                span: Default::default(),
            })?;
            let access = match uni.ty {
                Ty::Float => ".x",
                Ty::Vec2 => ".xy",
                Ty::Vec3 => ".xyz",
                Ty::Vec4 => "",
            };
            let e = match stage {
                Stage::Fragment => format!("P.u{u}{access}"),
                Stage::Sdf => format!("G.shape_uniforms[{u}u]{access}"),
            };
            wrap(e, uni.ty)
        }
        // Mirrors the emitter's Input arm (which needs a real expr id).
        PreviewTarget::Input(i) => {
            let e = match (stage, i) {
                (Stage::Fragment, Input::Uv) => "in.uv",
                (Stage::Fragment, Input::Normal) => "normalize(in.normal)",
                (Stage::Fragment, Input::WorldPos) => "in.view_pos",
                (Stage::Fragment, Input::ViewDir) => "normalize(-in.view_pos)",
                (_, Input::Time) => "fract(G.params.x * 0.25)",
                (Stage::Fragment, Input::InstanceColor) => "in.color",
                (Stage::Sdf, Input::WorldPos) => "q",
                (Stage::Sdf, _) => {
                    return Err(TranspileError {
                        message: format!("`{}` is not available in sdf shaders", i.name()),
                        span: Default::default(),
                    });
                }
            };
            let ty = if *i == Input::Time { Ty::Float } else { i.ty() };
            wrap(e.to_string(), ty)
        }
        PreviewTarget::Texture(t) => {
            format!("textureSample(flsl_tex{t}, flsl_samp{t}, in.uv)")
        }
        PreviewTarget::Output => match stage {
            Stage::Fragment => match ir.outputs.get("color") {
                Some(&out) => match ck.ty(out) {
                    Ty::Vec4 => format!("({})", w.emit(out)?),
                    _ => format!("vec4<f32>({}, 1.0)", w.emit(out)?),
                },
                None => "vec4<f32>(0.06, 0.06, 0.07, 1.0)".into(),
            },
            Stage::Sdf => match ir.outputs.get("sdf") {
                Some(&out) => format!("pv_dist({})", w.emit(out)?),
                None => "vec4<f32>(0.06, 0.06, 0.07, 1.0)".into(),
            },
        },
    })
}

/// Stand-ins for every raster-pass symbol a fragment chunk references — the
/// preview's own bind groups. MUST declare the same names/shapes as
/// [`crate::transpile::TEST_PRELUDE`] (the seam contract).
const FRAG_PRELUDE: &str = r#"
struct RasterGlobals {
    view_proj: mat4x4<f32>,
    light_dir: vec4<f32>,
    light_color: vec4<f32>,
    ambient: vec4<f32>,
    point_count: vec4<f32>,
    point_pos: array<vec4<f32>, 16>,
    point_color: array<vec4<f32>, 16>,
};
@group(0) @binding(0) var<uniform> g: RasterGlobals;
struct Globals {
    params: vec4<f32>,
    ao_params: vec4<f32>,
};
@group(0) @binding(1) var<uniform> G: Globals;
struct PvGlobals {
    grid: vec4<f32>,
    nums: array<vec4<f32>, 64>,
};
@group(0) @binding(2) var<uniform> PV: PvGlobals;
fn pvn(i: u32) -> f32 { return PV.nums[i / 4u][i % 4u]; }
struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) color: vec4<f32>,
    @location(3) view_pos: vec3<f32>,
    @location(4) emissive: vec4<f32>,
    @location(5) specular: vec4<f32>,
    @location(6) params: vec4<f32>,
    @location(7) rim: vec4<f32>,
    @location(8) tile: vec4<f32>,
    @location(9) lpos: vec3<f32>,
    @location(10) lnorm: vec3<f32>,
};
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;
fn point_diffuse(pos_rel: vec3<f32>, n: vec3<f32>) -> vec3<f32> { return vec3<f32>(0.0); }
fn sun_shadow(p: vec3<f32>, n: vec3<f32>, pix: vec2<u32>) -> vec3<f32> { return vec3<f32>(1.0); }
fn sdf_ao(p: vec3<f32>, n: vec3<f32>) -> f32 { return 1.0; }
fn apply_fog(color: vec3<f32>, pos: vec3<f32>, pix: vec2<u32>) -> vec3<f32> { return color; }
// A "ground" below the tile's lower third, so fieldDistance-driven looks
// (shoreline foam, contact glows) show a gradient instead of a constant.
fn map_d(p: vec3<f32>) -> f32 { return p.y + 0.55; }
fn base_texel(in: VsOut) -> vec4<f32> { return textureSample(tex, samp, in.uv); }
fn facing_normal(n: vec3<f32>, view_pos: vec3<f32>) -> vec3<f32> { return select(-n, n, dot(n, -view_pos) >= 0.0); }
"#;

/// Stand-ins for the field/raymarch symbols an sdf chunk references, plus the
/// 2D cross-section colorizer.
const SDF_PRELUDE: &str = r#"
struct Globals {
    params: vec4<f32>,
    ao_params: vec4<f32>,
    shape_uniforms: array<vec4<f32>, 64>,
};
@group(0) @binding(0) var<uniform> G: Globals;
struct PvGlobals {
    grid: vec4<f32>,
    nums: array<vec4<f32>, 64>,
};
@group(0) @binding(1) var<uniform> PV: PvGlobals;
fn pvn(i: u32) -> f32 { return PV.nums[i / 4u][i % 4u]; }
fn map_d(p: vec3<f32>) -> f32 { return p.y + 0.55; }
fn sdf_ao(p: vec3<f32>, n: vec3<f32>) -> f32 { return 1.0; }
// The classic 2D sdf debug view: blue inside / amber outside, distance
// bands, a bright zero line.
fn pv_dist(d: f32) -> vec4<f32> {
    var col = vec3<f32>(1.0) - sign(d) * vec3<f32>(0.1, 0.4, 0.7);
    col *= 1.0 - exp(-4.0 * abs(d));
    col *= 0.8 + 0.2 * cos(64.0 * d);
    col = mix(col, vec3<f32>(1.0), 1.0 - smoothstep(0.0, 0.02, abs(d)));
    return vec4<f32>(col, 1.0);
}
"#;

/// The shared fullscreen-triangle vertex stage.
const PV_VERTEX: &str = r#"
struct PvVsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) suv: vec2<f32>,
};
@vertex
fn vs_pv(@builtin(vertex_index) vi: u32) -> PvVsOut {
    var p = array<vec2<f32>, 3>(vec2<f32>(-1.0, -3.0), vec2<f32>(3.0, 1.0), vec2<f32>(-1.0, 1.0));
    var out: PvVsOut;
    let xy = p[vi];
    out.clip = vec4<f32>(xy, 0.0, 1.0);
    out.suv = vec2<f32>(xy.x * 0.5 + 0.5, 0.5 - xy.y * 0.5);
    return out;
}
"#;

/// Fragment-stage tile shader: synthesize plausible varyings per tile (tile
/// uv, a soft dome normal, a view-space slab), evaluate, composite over a
/// checkerboard by alpha.
const FRAG_MAIN: &str = r#"
@fragment
fn fs_pv(pin: PvVsOut) -> @location(0) vec4<f32> {
    let cols = max(PV.grid.x, 1.0);
    let rows = max(PV.grid.y, 1.0);
    let count = u32(PV.grid.z);
    let cell = vec2<f32>(floor(min(pin.suv.x * cols, cols - 1.0)), floor(min(pin.suv.y * rows, rows - 1.0)));
    let tile = u32(cell.y) * u32(cols) + u32(cell.x);
    let tuv = clamp(pin.suv * vec2<f32>(cols, rows) - cell, vec2<f32>(0.0), vec2<f32>(1.0));
    let q = vec2<f32>(tuv.x * 2.0 - 1.0, 1.0 - tuv.y * 2.0);
    let r2 = min(dot(q, q), 1.0);
    var vin: VsOut;
    vin.clip = pin.clip;
    vin.uv = tuv;
    vin.normal = normalize(vec3<f32>(q.x * 0.8, q.y * 0.8, sqrt(max(1.0 - r2 * 0.64, 0.05))));
    vin.color = vec4<f32>(1.0);
    vin.view_pos = vec3<f32>(q * 1.2, -2.5);
    vin.emissive = vec4<f32>(0.0);
    vin.specular = vec4<f32>(1.0, 1.0, 1.0, 0.35);
    vin.params = vec4<f32>(24.0, 0.15, 0.0, 1.0);
    vin.rim = vec4<f32>(1.0, 1.0, 1.0, 0.0);
    vin.tile = vec4<f32>(0.0);
    vin.lpos = vin.view_pos;
    vin.lnorm = vin.normal;
    let v = flsl_pv(vin, tile);
    let ch = f32((u32(tuv.x * 8.0) + u32(tuv.y * 8.0)) % 2u);
    let bg = mix(vec3<f32>(0.13, 0.13, 0.15), vec3<f32>(0.18, 0.18, 0.20), ch);
    let a = clamp(v.a, 0.0, 1.0);
    var rgb = mix(bg, clamp(v.rgb, vec3<f32>(0.0), vec3<f32>(1.0)), a);
    if (tile >= count) { rgb = bg * 0.6; }
    return vec4<f32>(rgb, 1.0);
}
"#;

/// Sdf-stage tile shader: each tile is a z = 0 cross-section, ±1.6 units.
const SDF_MAIN: &str = r#"
@fragment
fn fs_pv(pin: PvVsOut) -> @location(0) vec4<f32> {
    let cols = max(PV.grid.x, 1.0);
    let rows = max(PV.grid.y, 1.0);
    let count = u32(PV.grid.z);
    let cell = vec2<f32>(floor(min(pin.suv.x * cols, cols - 1.0)), floor(min(pin.suv.y * rows, rows - 1.0)));
    let tile = u32(cell.y) * u32(cols) + u32(cell.x);
    let tuv = clamp(pin.suv * vec2<f32>(cols, rows) - cell, vec2<f32>(0.0), vec2<f32>(1.0));
    let q2 = vec2<f32>(tuv.x * 2.0 - 1.0, 1.0 - tuv.y * 2.0);
    let v = flsl_pv(vec3<f32>(q2 * 1.6, 0.0), tile);
    let ch = f32((u32(tuv.x * 8.0) + u32(tuv.y * 8.0)) % 2u);
    let bg = mix(vec3<f32>(0.13, 0.13, 0.15), vec3<f32>(0.18, 0.18, 0.20), ch);
    let a = clamp(v.a, 0.0, 1.0);
    var rgb = mix(bg, clamp(v.rgb, vec3<f32>(0.0), vec3<f32>(1.0)), a);
    if (tile >= count) { rgb = bg * 0.6; }
    return vec4<f32>(rgb, 1.0);
}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::build_view;
    use crate::text::parse;
    use crate::transpile::validate_module;

    fn preview_of(src: &str) -> CompiledPreview {
        let ir = parse(src).expect("parses");
        let ck = ir::check(&ir).expect("checks");
        let view = build_view(&ir, Some(&ck));
        let targets: Vec<PreviewTarget> =
            preview_targets(&ir, &view).into_iter().map(|(_, t)| t).collect();
        transpile_preview(&ir, &ck, &targets).expect("preview transpiles")
    }

    #[test]
    fn every_example_builds_a_valid_preview_module() {
        for (name, src) in crate::examples::EXAMPLES {
            let pv = preview_of(src);
            assert!(pv.tiles > 0, "{name} has tiles");
            if let Err(d) = validate_module(&pv.wgsl) {
                panic!("{name}: preview module rejected: {} \n---\n{}", d.message, pv.wgsl);
            }
        }
    }

    #[test]
    fn literals_ride_live_lanes() {
        let pv = preview_of(
            "shader s {\n  stage fragment\n  let n = fbm(uv, octaves: 4, gain: 0.55)\n  output color = vec4(n, n, n, 1)\n}\n",
        );
        assert!(pv.wgsl.contains("pvn("), "literals read lanes:\n{}", pv.wgsl);
        assert!(!pv.dyn_slots.is_empty());
        assert!(validate_module(&pv.wgsl).is_ok());
    }

    #[test]
    fn sdf_previews_cross_section() {
        let pv = preview_of(
            "shader s {\n  stage sdf\n  uniform bulge: float = 0.2\n  let d = sphere(worldPos, radius: 1)\n  output sdf = d - bulge\n}\n",
        );
        assert_eq!(pv.stage, Stage::Sdf);
        if let Err(d) = validate_module(&pv.wgsl) {
            panic!("sdf preview rejected: {}\n{}", d.message, pv.wgsl);
        }
        assert!(pv.wgsl.contains("pv_dist("));
    }
}
