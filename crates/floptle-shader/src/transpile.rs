//! IR → WGSL (ADR-0007). A checked shader becomes a **chunk** of WGSL that the
//! renderer concatenates onto its pass module (`raster.wgsl + field.wgsl` for
//! Fragment-stage shaders) — the same composition trick the engine already
//! uses for the shared field module. The chunk references pass symbols
//! (`VsOut`, `g`, `G`, `tex`, `samp`, `sun_shadow`, `sdf_ao`, `apply_fog`,
//! `map_d`, `point_diffuse`); [`TEST_PRELUDE`] documents that exact contract
//! and stands in for the real pass sources in headless validation/tests.
//!
//! naga (the same version wgpu 29 embeds) validates the assembled module
//! before any pipeline is built; errors map back to `.flsl` lines through the
//! chunk's line map.

use crate::ir::{self, Blend, Checked, ExprId, ExprKind, Input, ResolvedArg, ShaderIr, Span, Stage, Ty};
use crate::stdlib::{self, Emit, SigTy};

/// A transpile failure (op semantics the type checker can't see, caps, …).
#[derive(Clone, Debug)]
pub struct TranspileError {
    pub message: String,
    pub span: Span,
}

impl TranspileError {
    fn new(message: impl Into<String>, span: Span) -> Self {
        Self { message: message.into(), span }
    }
}

/// Hard caps (proposal §6.2): keeps the generated bind group comfortably under
/// every backend's per-stage limits.
pub const MAX_TEXTURE_SLOTS: usize = 8;
pub const MAX_UNIFORMS: usize = 16;

/// A fragment-stage shader compiled to a WGSL chunk, ready for the renderer.
#[derive(Clone, Debug)]
pub struct CompiledFragment {
    pub name: String,
    pub blend: Blend,
    /// The generated WGSL: param block (group(3)), texture slot bindings,
    /// `flsl_surface`, and the `fs_flsl` entry point. Concatenate as
    /// `raster.wgsl + field.wgsl + SUPPORT + chunk`.
    pub chunk: String,
    /// The shader's exposed uniforms, in param-block slot order (one vec4
    /// slot each — see [`param_block_size`]).
    pub uniforms: Vec<ir::Uniform>,
    /// Texture slot names, in group(3) binding order (binding 1+2i / 2+2i).
    pub textures: Vec<String>,
    /// chunk line (0-based) → `.flsl` source span, for naga error mapping.
    pub line_map: Vec<(u32, Span)>,
}

/// A texture slot's tiling packed into its two param-block lanes (mirrors the
/// generated `t{i}a`/`t{i}b` fields): `a` = (count.xy, offset.xy),
/// `b` = (mode 0|1|2, rotation_radians, triplanar_scale, blend).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TilingPack {
    pub a: [f32; 4],
    pub b: [f32; 4],
}

impl CompiledFragment {
    /// Size in bytes of the group(3) params UBO: one vec4 slot per uniform +
    /// two per texture slot's tiling (never zero — an empty block still binds
    /// a 16-byte buffer).
    pub fn param_block_size(&self) -> u64 {
        ((self.uniforms.len() + 2 * self.textures.len()).max(1) * 16) as u64
    }

    /// Pack this shader's uniform values (material overrides where given,
    /// declared defaults otherwise) + per-slot tiling into param-block bytes.
    pub fn pack_params(
        &self,
        overrides: &dyn Fn(&str) -> Option<[f32; 4]>,
        tiling: &dyn Fn(&str) -> Option<TilingPack>,
    ) -> Vec<u8> {
        let mut out = vec![0u8; self.param_block_size() as usize];
        let mut write = |slot: usize, v: [f32; 4]| {
            for (l, val) in v.iter().enumerate() {
                out[slot * 16 + l * 4..slot * 16 + l * 4 + 4]
                    .copy_from_slice(&val.to_le_bytes());
            }
        };
        for (i, u) in self.uniforms.iter().enumerate() {
            write(i, overrides(&u.name).unwrap_or(u.default));
        }
        for (i, name) in self.textures.iter().enumerate() {
            let t = tiling(name).unwrap_or_default();
            write(self.uniforms.len() + 2 * i, t.a);
            write(self.uniforms.len() + 2 * i + 1, t.b);
        }
        out
    }

    /// Map a 0-based line inside the chunk to a `.flsl` span (naga errors).
    pub fn flsl_span_of_chunk_line(&self, line: u32) -> Option<Span> {
        self.line_map
            .iter()
            .rev()
            .find(|(l, _)| *l <= line)
            .map(|(_, s)| *s)
    }
}

/// Transpile a checked Fragment-stage shader into its WGSL chunk.
pub fn transpile_fragment(ir: &ShaderIr, ck: &Checked) -> Result<CompiledFragment, TranspileError> {
    if ir.stage != Some(Stage::Fragment) {
        return Err(TranspileError::new("not a fragment shader", Span::default()));
    }
    if ir.textures.len() > MAX_TEXTURE_SLOTS {
        return Err(TranspileError::new(
            format!("a shader can declare at most {MAX_TEXTURE_SLOTS} texture slots"),
            Span::default(),
        ));
    }
    if ir.uniforms.len() > MAX_UNIFORMS {
        return Err(TranspileError::new(
            format!("a shader can expose at most {MAX_UNIFORMS} uniforms"),
            Span::default(),
        ));
    }

    let mut w = Writer::new(ir, ck, EmitCtx::Fragment);

    w.line(format!("// generated from shader `{}` — edit the .flsl, not this", ir.name), None);
    w.line("struct FlslParams {".into(), None);
    if ir.uniforms.is_empty() && ir.textures.is_empty() {
        w.line("    _pad: vec4<f32>,".into(), None);
    }
    for (i, u) in ir.uniforms.iter().enumerate() {
        w.line(format!("    u{i}: vec4<f32>, // {}", u.name), None);
    }
    for (i, name) in ir.textures.iter().enumerate() {
        w.line(format!("    t{i}a: vec4<f32>, // {name} tiling: count.xy, offset.xy"), None);
        w.line(format!("    t{i}b: vec4<f32>, // {name} tiling: mode, rot, scale, blend"), None);
    }
    w.line("};".into(), None);
    w.line("@group(3) @binding(0) var<uniform> P: FlslParams;".into(), None);
    for (i, name) in ir.textures.iter().enumerate() {
        w.line(format!("@group(3) @binding({}) var flsl_tex{i}: texture_2d<f32>; // {name}", 1 + 2 * i), None);
        w.line(format!("@group(3) @binding({}) var flsl_samp{i}: sampler;", 2 + 2 * i), None);
    }
    w.line(String::new(), None);

    // The engine-lighting helper is per-chunk (it reads VsOut + the raster
    // globals `g`, which only exist in the raster module).
    w.raw(FRAGMENT_LIT_WGSL);

    w.line("fn flsl_surface(in: VsOut) -> vec4<f32> {".into(), None);
    for (i, (name, root)) in ir.lets.iter().enumerate() {
        let expr = w.emit(*root)?;
        let ty = ck.ty(*root).wgsl();
        let span = ir.expr(*root).span;
        w.line(format!("    let l{i}_{name}: {ty} = {expr};"), Some(span));
    }
    let out = ir.outputs["color"];
    let expr = w.emit(out)?;
    let span = ir.expr(out).span;
    match ck.ty(out) {
        Ty::Vec4 => w.line(format!("    return {expr};"), Some(span)),
        _ => w.line(format!("    return vec4<f32>({expr}, 1.0);"), Some(span)),
    }
    w.line("}".into(), None);
    w.line(String::new(), None);

    // The entry point: scene fog + the node's own alpha compose OUTSIDE the
    // authored surface, so every custom material stays scene-coherent.
    w.line("@fragment".into(), None);
    w.line("fn fs_flsl(in: VsOut) -> @location(0) vec4<f32> {".into(), None);
    w.line("    let c = flsl_surface(in);".into(), None);
    w.line("    let pix = vec2<u32>(u32(in.clip.x), u32(in.clip.y));".into(), None);
    w.line("    return vec4<f32>(apply_fog(c.rgb, in.view_pos, pix), c.a * in.color.a);".into(), None);
    w.line("}".into(), None);

    Ok(CompiledFragment {
        name: ir.name.clone(),
        blend: ir.blend,
        chunk: w.out,
        uniforms: ir.uniforms.clone(),
        textures: ir.textures.clone(),
        line_map: w.line_map,
    })
}

/// The engine-lighting helper included in every fragment chunk: the built-in
/// surface path (raster.wgsl `fs`) refactored over an authored albedo — sun +
/// marched shadows + AO + point lights + the node Material's specular/rim.
/// MUST stay in sync with `fs` in raster.wgsl.
pub(crate) const FRAGMENT_LIT_WGSL: &str = r#"fn flsl_lit(in: VsOut, albedo: vec3<f32>) -> vec3<f32> {
    let n = facing_normal(normalize(in.normal), in.view_pos);
    let l = normalize(g.light_dir.xyz);
    let v = normalize(-in.view_pos);
    let ndl = max(dot(n, l), 0.0);
    let pix = vec2<u32>(u32(in.clip.x), u32(in.clip.y));
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
    lit += albedo * point_diffuse(in.view_pos, n);
    let h = normalize(l + v);
    let shininess = max(in.params.x, 1.0);
    let spec = pow(max(dot(n, h), 0.0), shininess) * in.specular.a * select(0.0, 1.0, ndl > 0.0);
    lit += in.specular.rgb * spec * g.light_color.rgb * sh;
    let rim_f = pow(1.0 - max(dot(n, v), 0.0), 2.0) * in.params.y;
    lit += in.rim.rgb * rim_f;
    return lit * occ;
}
"#;

/// What the emitter's inputs/uniforms resolve to — the stage's data source.
#[derive(Clone, Copy)]
pub(crate) enum EmitCtx {
    /// Surface shaders: `in: VsOut` varyings + the group(3) param block.
    Fragment,
    /// Field shapes: `q` = shape-local position, uniforms ride the globals'
    /// `shape_uniforms` array at this scene slot.
    Sdf { slot: usize },
}

/// The preview transpiler's live-scalar registry: every literal number (and
/// color literal) in the shader gets a lane in a uniform array instead of
/// being baked into the WGSL, so dragging an inline value updates the preview
/// WITHOUT a pipeline rebuild. Lane order is emission order (deterministic).
#[derive(Default)]
pub(crate) struct DynNums {
    /// (expr, lanes) in allocation order; a slot's base lane is the sum of
    /// the preceding slots' lanes.
    pub(crate) slots: Vec<(ExprId, u8)>,
    by_expr: std::collections::BTreeMap<u32, usize>,
    lanes: usize,
}

/// 64 vec4s — the `PV.nums` array in the preview prelude. Literals past the
/// cap fall back to baked constants (still correct, just not live-draggable).
pub(crate) const MAX_DYN_LANES: usize = 256;

impl DynNums {
    fn alloc(&mut self, id: ExprId, lanes: u8) -> Option<usize> {
        if let Some(&b) = self.by_expr.get(&id.0) {
            return Some(b);
        }
        if self.lanes + lanes as usize > MAX_DYN_LANES {
            return None;
        }
        let base = self.lanes;
        self.lanes += lanes as usize;
        self.by_expr.insert(id.0, base);
        self.slots.push((id, lanes));
        Some(base)
    }
}

/// Incremental chunk writer that records the line map.
pub(crate) struct Writer<'a> {
    pub(crate) ir: &'a ShaderIr,
    pub(crate) ck: &'a Checked,
    ctx: EmitCtx,
    pub(crate) out: String,
    lines: u32,
    line_map: Vec<(u32, Span)>,
    /// When set, literal numbers/colors emit as `pvn(lane)` reads (preview).
    pub(crate) dyn_nums: Option<std::cell::RefCell<DynNums>>,
}

impl<'a> Writer<'a> {
    pub(crate) fn new(ir: &'a ShaderIr, ck: &'a Checked, ctx: EmitCtx) -> Self {
        Self { ir, ck, ctx, out: String::new(), lines: 0, line_map: Vec::new(), dyn_nums: None }
    }

    pub(crate) fn line(&mut self, s: String, span: Option<Span>) {
        if let Some(span) = span {
            self.line_map.push((self.lines, span));
        }
        self.out.push_str(&s);
        self.out.push('\n');
        self.lines += 1;
    }

    pub(crate) fn raw(&mut self, s: &str) {
        self.out.push_str(s);
        self.lines += s.matches('\n').count() as u32;
    }

    /// Allocate live-scalar lanes for `id` when in preview mode.
    fn dyn_lane(&self, id: ExprId, lanes: u8) -> Option<usize> {
        self.dyn_nums.as_ref().and_then(|d| d.borrow_mut().alloc(id, lanes))
    }

    /// Emit one expression as a WGSL expression string.
    pub(crate) fn emit(&self, id: ExprId) -> Result<String, TranspileError> {
        let e = self.ir.expr(id);
        Ok(match &e.kind {
            ExprKind::Num(n) => match self.dyn_lane(id, 1) {
                Some(b) => format!("pvn({b}u)"),
                None => wgsl_num(*n),
            },
            ExprKind::ColorLit(c) => match self.dyn_lane(id, 4) {
                Some(b) => format!(
                    "vec4<f32>(pvn({b}u), pvn({}u), pvn({}u), pvn({}u))",
                    b + 1,
                    b + 2,
                    b + 3
                ),
                None => format!(
                    "vec4<f32>({}, {}, {}, {})",
                    wgsl_num(c[0] as f64),
                    wgsl_num(c[1] as f64),
                    wgsl_num(c[2] as f64),
                    wgsl_num(c[3] as f64)
                ),
            },
            ExprKind::Str(_) | ExprKind::Texture(_) => {
                // Only reachable as resolved call params, which never emit here.
                return Err(TranspileError::new("internal: bare texture/string", e.span));
            }
            ExprKind::Input(i) => match (self.ctx, i) {
                (EmitCtx::Fragment, Input::Uv) => "in.uv".into(),
                (EmitCtx::Fragment, Input::Normal) => {
                    "facing_normal(normalize(in.normal), in.view_pos)".into()
                }
                (EmitCtx::Fragment, Input::WorldPos) => "in.view_pos".into(),
                (EmitCtx::Fragment, Input::ViewDir) => "normalize(-in.view_pos)".into(),
                (_, Input::Time) => "G.params.x".into(),
                (EmitCtx::Fragment, Input::InstanceColor) => "in.color".into(),
                // Sdf shaders author in shape-LOCAL space (the node's transform
                // is applied by shape_local; distances scale back after).
                (EmitCtx::Sdf { .. }, Input::WorldPos) => "q".into(),
                (EmitCtx::Sdf { .. }, _) => {
                    return Err(TranspileError::new(
                        format!("`{}` is not available in sdf shaders", i.name()),
                        e.span,
                    ));
                }
            },
            ExprKind::Uniform(u) => {
                let access = match self.ir.uniforms[*u].ty {
                    Ty::Float => ".x",
                    Ty::Vec2 => ".xy",
                    Ty::Vec3 => ".xyz",
                    Ty::Vec4 => "",
                };
                match self.ctx {
                    EmitCtx::Fragment => format!("P.u{u}{access}"),
                    EmitCtx::Sdf { slot } => {
                        format!("G.shape_uniforms[{}u]{access}", slot * 16 + u)
                    }
                }
            }
            ExprKind::Let(l) => format!("l{l}_{}", self.ir.lets[*l].0),
            ExprKind::Binary(op, a, b) => {
                format!("({} {} {})", self.emit(*a)?, op.symbol(), self.emit(*b)?)
            }
            ExprKind::Neg(a) => format!("(-{})", self.emit(*a)?),
            ExprKind::Swizzle(a, sw) => format!("{}.{sw}", self.emit(*a)?),
            ExprKind::Call { .. } => self.emit_call(id)?,
        })
    }

    fn emit_call(&self, id: ExprId) -> Result<String, TranspileError> {
        let e = self.ir.expr(id);
        let call = &self.ck.calls[&id];
        let op = call.op;

        // Constructors: emit components verbatim (WGSL has the same overloads).
        if matches!(op.name, "vec2" | "vec3" | "vec4") {
            let parts = self.emit_plain_args(&call.args)?;
            return Ok(format!("{}<f32>({})", op.name, parts.join(", ")));
        }

        match op.emit {
            Emit::Fn(name) => {
                let parts = self.emit_sig_args(call, id)?;
                Ok(format!("{name}({})", parts.join(", ")))
            }
            Emit::FnByLanes(base) => {
                let lanes = call.generic.map(|t| t.lanes()).unwrap_or(2);
                let parts = self.emit_sig_args(call, id)?;
                Ok(format!("{base}{lanes}({})", parts.join(", ")))
            }
            Emit::Special => match op.name {
                "sample" => {
                    let ResolvedArg::Texture(slot) = call.args[0] else {
                        return Err(TranspileError::new("internal: sample slot", e.span));
                    };
                    let uv = self.emit_resolved(&call.args[1], Some(Ty::Vec2))?;
                    Ok(format!(
                        "flsl_tiled_sample(flsl_tex{slot}, flsl_samp{slot}, {uv}, P.t{slot}a, P.t{slot}b)"
                    ))
                }
                "sampleTriplanar" => {
                    let ResolvedArg::Texture(slot) = call.args[0] else {
                        return Err(TranspileError::new("internal: sample slot", e.span));
                    };
                    let p = self.emit_resolved(&call.args[1], Some(Ty::Vec3))?;
                    let n = self.emit_resolved(&call.args[2], Some(Ty::Vec3))?;
                    Ok(format!(
                        "flsl_triplanar(flsl_tex{slot}, flsl_samp{slot}, {p}, {n}, P.t{slot}b.z, P.t{slot}b.w)"
                    ))
                }
                "baseTexture" => {
                    match &call.args[0] {
                        // Omitted → the node's own texture THROUGH its material
                        // tiling (the raster pass's helper — instance-driven).
                        ResolvedArg::Default(_) => Ok("base_texel(in)".to_string()),
                        a => {
                            let uv = self.emit_resolved(a, Some(Ty::Vec2))?;
                            Ok(format!("textureSample(tex, samp, {uv})"))
                        }
                    }
                }
                "litSurface" => {
                    let albedo = self.emit_resolved(&call.args[0], Some(Ty::Vec3))?;
                    Ok(format!("flsl_lit(in, {albedo})"))
                }
                "sunShadow" => {
                    let p = self.emit_resolved(&call.args[0], Some(Ty::Vec3))?;
                    let n = self.emit_resolved(&call.args[1], Some(Ty::Vec3))?;
                    Ok(format!(
                        "sun_shadow({p}, {n}, vec2<u32>(u32(in.clip.x), u32(in.clip.y)))"
                    ))
                }
                "applyFog" => {
                    let c = self.emit_resolved(&call.args[0], Some(Ty::Vec3))?;
                    let p = self.emit_resolved(&call.args[1], Some(Ty::Vec3))?;
                    Ok(format!(
                        "apply_fog({c}, {p}, vec2<u32>(u32(in.clip.x), u32(in.clip.y)))"
                    ))
                }
                "palette" => {
                    let t = self.emit_resolved(&call.args[0], Some(Ty::Float))?;
                    let ResolvedArg::Str(name) = &call.args[1] else {
                        return Err(TranspileError::new("internal: palette name", e.span));
                    };
                    let co = stdlib::palette_coeffs(name).ok_or_else(|| {
                        TranspileError::new(
                            format!(
                                "unknown palette \"{name}\" (try {})",
                                stdlib::PALETTE_NAMES.join(", ")
                            ),
                            e.span,
                        )
                    })?;
                    let v = |c: [f32; 3]| {
                        format!(
                            "vec3<f32>({}, {}, {})",
                            wgsl_num(c[0] as f64),
                            wgsl_num(c[1] as f64),
                            wgsl_num(c[2] as f64)
                        )
                    };
                    Ok(format!(
                        "flsl_palette({t}, {}, {}, {}, {})",
                        v(co[0]),
                        v(co[1]),
                        v(co[2]),
                        v(co[3])
                    ))
                }
                other => Err(TranspileError::new(format!("internal: special op `{other}`"), e.span)),
            },
        }
    }

    /// Emit constructor args (no signature — shapes checked structurally).
    fn emit_plain_args(&self, args: &[ResolvedArg]) -> Result<Vec<String>, TranspileError> {
        args.iter().map(|a| self.emit_resolved(a, None)).collect()
    }

    /// Emit an op's slot-ordered args, splatting scalars into vector slots
    /// where WGSL's builtins demand matching shapes (clamp/pow/min/…).
    fn emit_sig_args(
        &self,
        call: &ir::ResolvedCall,
        _id: ExprId,
    ) -> Result<Vec<String>, TranspileError> {
        let mut out = Vec::with_capacity(call.args.len());
        for (sig, arg) in call.op.inputs.iter().zip(&call.args) {
            let want = match sig.ty {
                SigTy::Exact(t) => Some(t),
                SigTy::Generic => call.generic,
                SigTy::GenericVec => call.generic,
                SigTy::Texture | SigTy::Str => None,
            };
            // Generic scalar-flexible slots: `mix`'s t and fbm's octaves stay
            // scalar in WGSL, so only splat where the actual overload needs it.
            let want = if scalar_ok(call.op.name, sig.name) { None } else { want };
            out.push(self.emit_resolved(arg, want)?);
        }
        Ok(out)
    }

    /// Emit a resolved arg, widening a scalar to `want` when needed.
    fn emit_resolved(&self, arg: &ResolvedArg, want: Option<Ty>) -> Result<String, TranspileError> {
        match arg {
            ResolvedArg::Default(d) => Ok(wgsl_num(*d)),
            ResolvedArg::Str(_) | ResolvedArg::Texture(_) => {
                Err(TranspileError::new("internal: const param as value", Span::default()))
            }
            ResolvedArg::Expr(id) => {
                let s = self.emit(*id)?;
                let actual = self.ck.ty(*id);
                match want {
                    Some(w) if w != Ty::Float && actual == Ty::Float => {
                        Ok(format!("{}({s})", w.wgsl()))
                    }
                    _ => Ok(s),
                }
            }
        }
    }
}

/// WGSL builtin slots where a scalar is a legal overload alongside vectors —
/// don't splat these (everything else same-shapes its args).
fn scalar_ok(op: &str, slot: &str) -> bool {
    matches!(
        (op, slot),
        ("mix", "t")
            | ("fbm", "octaves")
            | ("fbm", "lacunarity")
            | ("fbm", "gain")
            | ("domainWarp", "scale")
            | ("domainWarp", "time")
            | ("domainWarp", "strength")
            | ("smoothMin", "k")
            | ("sphere", "radius")
            | ("box", "rounding")
            | ("torus", "major")
            | ("torus", "minor")
            | ("plane", "offset")
            | ("twist", "amount")
            | ("posterize", "steps")
            | ("hueShift", "shift")
            | ("gamma", "g")
            | ("contrast", "amount")
    )
}

/// A number as a WGSL f32 literal (`5` → `5.0`).
fn wgsl_num(n: f64) -> String {
    let s = format!("{n:?}");
    if s.contains('.') || s.contains('e') || s.contains("inf") || s.contains("NaN") {
        s
    } else {
        format!("{s}.0")
    }
}

/// An Sdf-stage shader compiled for one scene slot (0..4): a distance function
/// for the shared field module and a color function for the raymarch surface
/// pass. The node's transform/scale/bounding radius and the shader's uniform
/// values ride the raymarch globals (`shape_pos/rot/aux/uniforms` arrays), so
/// param edits are uniform writes — the code only changes when the shader or
/// its slot does.
#[derive(Clone, Debug)]
pub struct CompiledSdf {
    pub name: String,
    pub uniforms: Vec<ir::Uniform>,
    /// `fn flsl_shape{slot}_d(p: vec3<f32>) -> f32` — world-space distance,
    /// bounding-sphere early-out included. For field.wgsl's custom block.
    pub dist_fn: String,
    /// `fn flsl_shape{slot}_col(p: vec3<f32>) -> vec3<f32>` — the surface
    /// albedo (the shader's `output color`, or a neutral default). For
    /// raymarch.wgsl's custom block.
    pub col_fn: String,
}

/// Transpile a checked Sdf-stage shader for scene slot `slot`.
pub fn transpile_sdf(ir: &ShaderIr, ck: &Checked, slot: usize) -> Result<CompiledSdf, TranspileError> {
    if ir.stage != Some(Stage::Sdf) {
        return Err(TranspileError::new("not an sdf shader", Span::default()));
    }
    if !ir.textures.is_empty() {
        return Err(TranspileError::new(
            "sdf shaders can't declare texture slots (distance is pure math)",
            Span::default(),
        ));
    }
    if ir.uniforms.len() > MAX_UNIFORMS {
        return Err(TranspileError::new(
            format!("a shader can expose at most {MAX_UNIFORMS} uniforms"),
            Span::default(),
        ));
    }

    let mut w = Writer::new(ir, ck, EmitCtx::Sdf { slot });
    let lets = |w: &mut Writer| -> Result<(), TranspileError> {
        for (i, (name, root)) in ir.lets.iter().enumerate() {
            let expr = w.emit(*root)?;
            let ty = ck.ty(*root).wgsl();
            w.line(format!("    let l{i}_{name}: {ty} = {expr};"), Some(ir.expr(*root).span));
        }
        Ok(())
    };

    w.line(format!("fn flsl_shape{slot}_d(p: vec3<f32>) -> f32 {{"), None);
    // The bounding sphere both skips distant evaluation AND is a valid
    // conservative distance for the march (a lower bound of the true field).
    w.line(format!(
        "    let bound = length(p - G.shape_pos[{slot}].xyz) - G.shape_aux[{slot}].x;"
    ), None);
    w.line("    if (bound > 0.5) { return bound; }".into(), None);
    w.line(format!("    let q = shape_local({slot}u, p);"), None);
    lets(&mut w)?;
    let sdf = ir.outputs["sdf"];
    let expr = w.emit(sdf)?;
    // Distances were authored in shape-local units — scale back to world.
    w.line(
        format!("    return ({expr}) * max(G.shape_pos[{slot}].w, 1e-6);"),
        Some(ir.expr(sdf).span),
    );
    w.line("}".into(), None);
    let dist_fn = std::mem::take(&mut w.out);

    w.line(format!("fn flsl_shape{slot}_col(p: vec3<f32>) -> vec3<f32> {{"), None);
    w.line(format!("    let q = shape_local({slot}u, p);"), None);
    lets(&mut w)?;
    match ir.outputs.get("color") {
        Some(&col) => {
            let expr = w.emit(col)?;
            w.line(format!("    return {expr};"), Some(ir.expr(col).span));
        }
        None => w.line("    return vec3<f32>(0.85, 0.85, 0.85);".into(), None),
    }
    w.line("}".into(), None);
    let col_fn = w.out;

    Ok(CompiledSdf { name: ir.name.clone(), uniforms: ir.uniforms.clone(), dist_fn, col_fn })
}

/// A naga diagnostic mapped back toward the `.flsl` source.
#[derive(Clone, Debug)]
pub struct WgslDiag {
    pub message: String,
    /// 0-based line within the CHUNK (None = the error is in the prelude or
    /// support — an engine bug, not the artist's).
    pub chunk_line: Option<u32>,
}

/// Assemble `prelude + SUPPORT + chunk` and validate with naga (parse +
/// full validation) — the same naga wgpu embeds, so passing here means the
/// pipeline build will accept it. The renderer calls this with its real pass
/// sources as `prelude`; tests use [`TEST_PRELUDE`].
pub fn validate(prelude: &str, chunk: &str) -> Result<(), WgslDiag> {
    let support = stdlib::SUPPORT_WGSL;
    let src = format!("{prelude}\n{support}\n{chunk}");
    let chunk_first_line =
        (prelude.matches('\n').count() + 1 + support.matches('\n').count() + 1 + 1) as u32;
    let to_chunk = |line1: u32| line1.checked_sub(chunk_first_line);

    let module = match naga::front::wgsl::parse_str(&src) {
        Ok(m) => m,
        Err(e) => {
            let line = e.location(&src).map(|l| l.line_number);
            return Err(WgslDiag {
                message: e.message().to_string(),
                chunk_line: line.and_then(to_chunk),
            });
        }
    };
    let mut validator = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    );
    match validator.validate(&module) {
        Ok(_) => Ok(()),
        Err(e) => {
            let line = e.spans().next().map(|(span, _)| span.location(&src).line_number);
            Err(WgslDiag {
                message: format!("{e}"),
                chunk_line: line.and_then(to_chunk),
            })
        }
    }
}

/// Validate an ALREADY-ASSEMBLED WGSL module (the renderer's spliced pass
/// sources) with naga — parse + full validation, 1-based error line included.
pub fn validate_module(src: &str) -> Result<(), WgslDiag> {
    let module = match naga::front::wgsl::parse_str(src) {
        Ok(m) => m,
        Err(e) => {
            return Err(WgslDiag {
                message: e.message().to_string(),
                chunk_line: e.location(src).map(|l| l.line_number),
            });
        }
    };
    let mut validator = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    );
    match validator.validate(&module) {
        Ok(_) => Ok(()),
        Err(e) => Err(WgslDiag {
            message: format!("{e}"),
            chunk_line: e.spans().next().map(|(span, _)| span.location(src).line_number),
        }),
    }
}

/// A minimal stand-in for the raster pass module, declaring exactly the
/// symbols a generated fragment chunk may reference. This IS the seam
/// contract: if raster.wgsl / field.wgsl rename or reshape any of these, this
/// prelude (and the emitter) must follow. Used by headless validation/tests;
/// the renderer validates against its real sources instead.
pub const TEST_PRELUDE: &str = r#"
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
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;
struct Globals {
    params: vec4<f32>,
    ao_params: vec4<f32>,
};
@group(2) @binding(0) var<uniform> G: Globals;
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
fn point_diffuse(pos_rel: vec3<f32>, n: vec3<f32>) -> vec3<f32> { return vec3<f32>(0.0); }
fn sun_shadow(p: vec3<f32>, n: vec3<f32>, pix: vec2<u32>) -> vec3<f32> { return vec3<f32>(1.0); }
fn sdf_ao(p: vec3<f32>, n: vec3<f32>) -> f32 { return 1.0; }
fn apply_fog(color: vec3<f32>, pos: vec3<f32>, pix: vec2<u32>) -> vec3<f32> { return color; }
fn map_d(p: vec3<f32>) -> f32 { return 1e9; }
fn base_texel(in: VsOut) -> vec4<f32> { return textureSample(tex, samp, in.uv); }
fn facing_normal(n: vec3<f32>, view_pos: vec3<f32>) -> vec3<f32> { return select(-n, n, dot(n, -view_pos) >= 0.0); }
"#;
