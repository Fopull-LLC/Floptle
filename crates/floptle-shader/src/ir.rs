//! The in-memory shader IR — the single source of truth (ADR-0007).
//!
//! A shader is a DAG: an arena of expressions ([`Expr`], indexed by [`ExprId`])
//! plus ordered named bindings (`lets`) and stage-defined output sinks. The
//! `.flsl` text format (text.rs) and the node graph (the editor, later) are both
//! projections of this one structure. Type checking is edge-level and happens
//! here; the WGSL emitter (transpile.rs) consumes the checked result.

use std::collections::BTreeMap;

use crate::stdlib::{self, OpSpec};

/// What kind of shader this is — which output contract it fills.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// A surface look: `output color` becomes the mesh's fragment color
    /// (spliced into the raster pass as `flsl_surface`).
    Fragment,
    /// A distance field: `output sdf` (+ optional `output color`) joins the
    /// scene's fused field (spliced into `map_d`/`map` — see the proposal §7).
    Sdf,
    /// A procedural sky: `output color` becomes the environment color along a
    /// ray direction (spliced into the raymarch's `sky_color`). The only input
    /// that makes sense is the ray direction (`skyDir`) + time.
    Sky,
    /// A game-UI element's face: `output color` (vec4 — alpha shapes the
    /// element) drawn by the UI pass over the finished frame. Inputs are the
    /// element's `uv` (0..1 across its rect), its tint (`instanceColor`) and
    /// `time` — procedural instruments (navballs, gauges) live here.
    Ui,
}

/// How a Fragment-stage surface composites over the scene.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Blend {
    /// Solid: draws in the opaque phase, writes depth.
    #[default]
    Opaque,
    /// Classic alpha blend: draws in the transparent phase, no depth write.
    Alpha,
    /// Additive glow: draws in the transparent phase, no depth write.
    Additive,
}

/// A value type flowing along an edge. `color` in `.flsl` is a `Vec4` with a
/// UI hint (see [`Uniform::is_color`]); the SDF distance itself is a `Float`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ty {
    Float,
    Vec2,
    Vec3,
    Vec4,
}

impl Ty {
    /// Component count (1, 2, 3 or 4).
    pub fn lanes(self) -> u8 {
        match self {
            Ty::Float => 1,
            Ty::Vec2 => 2,
            Ty::Vec3 => 3,
            Ty::Vec4 => 4,
        }
    }

    pub fn of_lanes(n: u8) -> Option<Ty> {
        match n {
            1 => Some(Ty::Float),
            2 => Some(Ty::Vec2),
            3 => Some(Ty::Vec3),
            4 => Some(Ty::Vec4),
            _ => None,
        }
    }

    /// The WGSL spelling of this type.
    pub fn wgsl(self) -> &'static str {
        match self {
            Ty::Float => "f32",
            Ty::Vec2 => "vec2<f32>",
            Ty::Vec3 => "vec3<f32>",
            Ty::Vec4 => "vec4<f32>",
        }
    }

    /// The `.flsl` spelling of this type.
    pub fn flsl(self) -> &'static str {
        match self {
            Ty::Float => "float",
            Ty::Vec2 => "vec2",
            Ty::Vec3 => "vec3",
            Ty::Vec4 => "vec4",
        }
    }
}

/// A shader-exposed knob: becomes a field in the material's param block
/// (group(3) UBO) and a widget row in the Inspector.
#[derive(Clone, Debug, PartialEq)]
pub struct Uniform {
    pub name: String,
    pub ty: Ty,
    /// Default value (unused lanes zero).
    pub default: [f32; 4],
    /// Declared as `color` — the Inspector shows a color picker and the
    /// printer writes the default as `#RRGGBB[AA]`. Always `ty == Vec4`.
    pub is_color: bool,
    /// Optional `range(lo, hi)` annotation — Inspector slider bounds.
    pub range: Option<(f32, f32)>,
}

/// The built-in values a shader can read (stage-dependent availability).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Input {
    /// Mesh UV (`vec2`). Fragment only.
    Uv,
    /// Surface normal, normalized (`vec3`). Fragment only.
    Normal,
    /// CAMERA-RELATIVE position (`vec3`) — the engine's floating-origin space
    /// (ADR-0015): the camera sits at the origin. Both stages.
    WorldPos,
    /// Unit vector from the surface toward the camera (`vec3`). Fragment only.
    ViewDir,
    /// Scene time in seconds (`float`). Both stages.
    Time,
    /// The node's own Material tint + alpha (`vec4`) — so one shader composes
    /// with per-node coloring. Fragment only.
    InstanceColor,
    /// The world-space RAY DIRECTION (`vec3`, normalized) for a Sky shader — the
    /// direction the camera ray travels toward the horizon/zenith. Sky only.
    SkyDir,
}

impl Input {
    pub fn name(self) -> &'static str {
        match self {
            Input::Uv => "uv",
            Input::Normal => "normal",
            Input::WorldPos => "worldPos",
            Input::ViewDir => "viewDir",
            Input::Time => "time",
            Input::InstanceColor => "instanceColor",
            Input::SkyDir => "skyDir",
        }
    }

    pub fn ty(self) -> Ty {
        match self {
            Input::Uv => Ty::Vec2,
            Input::Normal | Input::WorldPos | Input::ViewDir | Input::SkyDir => Ty::Vec3,
            Input::Time => Ty::Float,
            Input::InstanceColor => Ty::Vec4,
        }
    }

    pub fn all() -> &'static [Input] {
        &[
            Input::Uv,
            Input::Normal,
            Input::WorldPos,
            Input::ViewDir,
            Input::Time,
            Input::InstanceColor,
            Input::SkyDir,
        ]
    }

    pub fn by_name(name: &str) -> Option<Input> {
        Input::all().iter().copied().find(|i| i.name() == name)
    }

    /// Which inputs exist per stage: SDF shaders run at arbitrary field points
    /// (no surface yet), so only position + time make sense there.
    pub fn in_stage(self, stage: Stage) -> bool {
        match stage {
            Stage::Fragment => !matches!(self, Input::SkyDir),
            Stage::Sdf => matches!(self, Input::WorldPos | Input::Time),
            Stage::Sky => matches!(self, Input::SkyDir | Input::Time),
            Stage::Ui => matches!(self, Input::Uv | Input::InstanceColor | Input::Time),
        }
    }
}

/// A byte span into the `.flsl` source, for error reporting. Zero for
/// graph-authored nodes that never came from text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
}

impl BinOp {
    pub fn symbol(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
        }
    }
}

/// Index of an expression in [`ShaderIr::exprs`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExprId(pub u32);

/// A call argument as authored: positional or named. Resolution against the
/// op's signature happens in [`check`].
#[derive(Clone, Debug, PartialEq)]
pub struct CallArg {
    pub name: Option<String>,
    pub value: ExprId,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExprKind {
    /// A number literal.
    Num(f64),
    /// A `#RRGGBB` / `#RRGGBBAA` color literal (sRGB floats).
    ColorLit([f32; 4]),
    /// A string literal — only valid as a constant op param (palette names).
    Str(String),
    /// A built-in input value.
    Input(Input),
    /// A shader uniform, by index into [`ShaderIr::uniforms`].
    Uniform(usize),
    /// A texture slot, by index into [`ShaderIr::textures`] — only valid as an
    /// argument to texture ops (`sample`).
    Texture(usize),
    /// A named `let` binding, by index into [`ShaderIr::lets`].
    Let(usize),
    /// A stdlib op call.
    Call { op: String, args: Vec<CallArg> },
    Binary(BinOp, ExprId, ExprId),
    /// Unary minus.
    Neg(ExprId),
    /// Component selection / rearrangement (`v.xyz`, `c.rgb`, `p.x`).
    Swizzle(ExprId, String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

/// One shader — the single source of truth both views project.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ShaderIr {
    pub name: String,
    pub stage: Option<Stage>,
    pub blend: Blend,
    pub uniforms: Vec<Uniform>,
    /// Texture slot names, in declaration order (= group(3) binding order).
    pub textures: Vec<String>,
    /// The expression arena. `lets` and `outputs` index into it.
    pub exprs: Vec<Expr>,
    /// Ordered named bindings — each is a graph node the artist named.
    pub lets: Vec<(String, ExprId)>,
    /// Stage-defined sinks: Fragment wants `color`; Sdf wants `sdf` and
    /// optionally `color`.
    pub outputs: BTreeMap<String, ExprId>,
    /// Cosmetic graph-editor node positions (`//@layout`), keyed by let name.
    /// Never semantic: [`ShaderIr::same_shader`] ignores it.
    pub layout: BTreeMap<String, (f32, f32)>,
}

impl ShaderIr {
    pub fn expr(&self, id: ExprId) -> &Expr {
        &self.exprs[id.0 as usize]
    }

    pub fn push(&mut self, kind: ExprKind, span: Span) -> ExprId {
        self.exprs.push(Expr { kind, span });
        ExprId(self.exprs.len() as u32 - 1)
    }

    /// Structural equality ignoring the cosmetic layout block, source spans and
    /// arena ordering — the round-trip contract (`parse(print(ir))` must be
    /// `same_shader`). Expressions compare recursively; `Let` references
    /// compare by binding NAME so two arenas laid out differently still match.
    pub fn same_shader(&self, other: &ShaderIr) -> bool {
        if self.name != other.name
            || self.stage != other.stage
            || self.blend != other.blend
            || self.uniforms != other.uniforms
            || self.textures != other.textures
            || self.lets.len() != other.lets.len()
            || self.outputs.len() != other.outputs.len()
        {
            return false;
        }
        for ((na, ea), (nb, eb)) in self.lets.iter().zip(&other.lets) {
            if na != nb || !same_expr(self, *ea, other, *eb) {
                return false;
            }
        }
        for (name, ea) in &self.outputs {
            match other.outputs.get(name) {
                Some(eb) if same_expr(self, *ea, other, *eb) => {}
                _ => return false,
            }
        }
        true
    }
}

/// Recursive structural expression equality across two arenas (see
/// [`ShaderIr::same_shader`]). `Uniform`/`Texture` indices compare directly
/// (both shaders passed the header equality check) and `Let` references by
/// index too (let ORDER is part of the header comparison).
fn same_expr(a: &ShaderIr, ea: ExprId, b: &ShaderIr, eb: ExprId) -> bool {
    match (&a.expr(ea).kind, &b.expr(eb).kind) {
        (ExprKind::Num(x), ExprKind::Num(y)) => x == y,
        (ExprKind::ColorLit(x), ExprKind::ColorLit(y)) => x == y,
        (ExprKind::Str(x), ExprKind::Str(y)) => x == y,
        (ExprKind::Input(x), ExprKind::Input(y)) => x == y,
        (ExprKind::Uniform(x), ExprKind::Uniform(y)) => x == y,
        (ExprKind::Texture(x), ExprKind::Texture(y)) => x == y,
        (ExprKind::Let(x), ExprKind::Let(y)) => x == y,
        (ExprKind::Neg(x), ExprKind::Neg(y)) => same_expr(a, *x, b, *y),
        (ExprKind::Swizzle(x, sx), ExprKind::Swizzle(y, sy)) => {
            sx == sy && same_expr(a, *x, b, *y)
        }
        (ExprKind::Binary(ox, xa, xb), ExprKind::Binary(oy, ya, yb)) => {
            ox == oy && same_expr(a, *xa, b, *ya) && same_expr(a, *xb, b, *yb)
        }
        (ExprKind::Call { op: ox, args: xs }, ExprKind::Call { op: oy, args: ys }) => {
            ox == oy
                && xs.len() == ys.len()
                && xs
                    .iter()
                    .zip(ys)
                    .all(|(x, y)| x.name == y.name && same_expr(a, x.value, b, y.value))
        }
        _ => false,
    }
}

/// A type/shape error found by [`check`], anchored to a source span.
#[derive(Clone, Debug)]
pub struct IrError {
    pub message: String,
    pub span: Span,
}

impl IrError {
    fn new(message: impl Into<String>, span: Span) -> Self {
        Self { message: message.into(), span }
    }
}

/// A call argument after resolution against the op's signature, in signature
/// slot order. What the emitter consumes.
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum ResolvedArg {
    Expr(ExprId),
    /// An omitted optional input, filled from the signature default.
    Default(f64),
    /// A compile-time string param (palette names).
    Str(String),
    /// A texture slot index.
    Texture(usize),
}

/// One resolved call: the op plus its slot-ordered args and the concrete type
/// its generic (if any) resolved to.
#[derive(Clone, Debug)]
pub struct ResolvedCall {
    pub op: &'static OpSpec,
    pub args: Vec<ResolvedArg>,
    /// The concrete type the op's `Generic` inputs unified to (if the op is
    /// generic) — drives type-directed WGSL emission (fbm2 vs fbm3, …).
    pub generic: Option<Ty>,
}

/// The result of type-checking: per-expression types + per-call resolutions.
#[derive(Clone, Debug, Default)]
pub struct Checked {
    /// Type of every expression, parallel to [`ShaderIr::exprs`].
    pub types: Vec<Option<Ty>>,
    /// Resolved signature info for every `Call` expression.
    pub calls: BTreeMap<ExprId, ResolvedCall>,
}

impl Checked {
    pub fn ty(&self, id: ExprId) -> Ty {
        self.types[id.0 as usize].expect("checked expr has a type")
    }
}

/// Type-check the whole shader: every let, every output, every edge. Returns
/// the per-node types + resolved calls, or every error found (not just the
/// first — the IDE squiggles want them all).
pub fn check(ir: &ShaderIr) -> Result<Checked, Vec<IrError>> {
    let Some(stage) = ir.stage else {
        return Err(vec![IrError::new("shader is missing a `stage` declaration", Span::default())]);
    };
    let mut ck = Checked { types: vec![None; ir.exprs.len()], calls: BTreeMap::new() };
    let mut errors = Vec::new();

    // Lets are ordered and may only reference EARLIER lets (the parser
    // guarantees it by construction; graph edits must too), so one pass in
    // order types everything.
    let mut let_ty: Vec<Option<Ty>> = vec![None; ir.lets.len()];
    for (i, (_, root)) in ir.lets.iter().enumerate() {
        let t = infer(ir, stage, *root, &let_ty, &mut ck, &mut errors);
        let_ty[i] = t;
    }

    // Stage output contract.
    match stage {
        Stage::Fragment => {
            expect_output(ir, "color", &[Ty::Vec3, Ty::Vec4], &let_ty, &mut ck, &mut errors);
            for name in ir.outputs.keys() {
                if name != "color" {
                    errors.push(IrError::new(
                        format!("fragment shaders output `color` only (got `{name}`)"),
                        ir.expr(ir.outputs[name]).span,
                    ));
                }
            }
        }
        Stage::Sdf => {
            expect_output(ir, "sdf", &[Ty::Float], &let_ty, &mut ck, &mut errors);
            if ir.outputs.contains_key("color") {
                expect_output(ir, "color", &[Ty::Vec3], &let_ty, &mut ck, &mut errors);
            }
            for name in ir.outputs.keys() {
                if name != "sdf" && name != "color" {
                    errors.push(IrError::new(
                        format!("sdf shaders output `sdf` (and optionally `color`), got `{name}`"),
                        ir.expr(ir.outputs[name]).span,
                    ));
                }
            }
            if ir.blend != Blend::Opaque {
                errors.push(IrError::new("`blend` only applies to fragment shaders", Span::default()));
            }
        }
        Stage::Sky => {
            expect_output(ir, "color", &[Ty::Vec3, Ty::Vec4], &let_ty, &mut ck, &mut errors);
            for name in ir.outputs.keys() {
                if name != "color" {
                    errors.push(IrError::new(
                        format!("sky shaders output `color` only (got `{name}`)"),
                        ir.expr(ir.outputs[name]).span,
                    ));
                }
            }
            if ir.blend != Blend::Opaque {
                errors.push(IrError::new("`blend` only applies to fragment shaders", Span::default()));
            }
        }
        Stage::Ui => {
            expect_output(ir, "color", &[Ty::Vec3, Ty::Vec4], &let_ty, &mut ck, &mut errors);
            for name in ir.outputs.keys() {
                if name != "color" {
                    errors.push(IrError::new(
                        format!("ui shaders output `color` only (got `{name}`)"),
                        ir.expr(ir.outputs[name]).span,
                    ));
                }
            }
            if ir.blend != Blend::Opaque {
                errors.push(IrError::new("`blend` only applies to fragment shaders", Span::default()));
            }
        }
    }

    if errors.is_empty() { Ok(ck) } else { Err(errors) }
}

fn expect_output(
    ir: &ShaderIr,
    name: &str,
    allowed: &[Ty],
    let_ty: &[Option<Ty>],
    ck: &mut Checked,
    errors: &mut Vec<IrError>,
) {
    let Some(stage) = ir.stage else { return };
    match ir.outputs.get(name) {
        None => errors.push(IrError::new(format!("missing `output {name} = …`"), Span::default())),
        Some(&id) => {
            if let Some(t) = infer(ir, stage, id, let_ty, ck, errors)
                && !allowed.contains(&t)
            {
                let want =
                    allowed.iter().map(|t| t.flsl()).collect::<Vec<_>>().join(" or ");
                errors.push(IrError::new(
                    format!("`output {name}` must be {want}, got {}", t.flsl()),
                    ir.expr(id).span,
                ));
            }
        }
    }
}

/// Infer (and memoize) the type of one expression. Pushes errors and returns
/// `None` when the subtree is unwell — parents then skip their own check.
fn infer(
    ir: &ShaderIr,
    stage: Stage,
    id: ExprId,
    let_ty: &[Option<Ty>],
    ck: &mut Checked,
    errors: &mut Vec<IrError>,
) -> Option<Ty> {
    if let Some(t) = ck.types[id.0 as usize] {
        return Some(t);
    }
    let e = ir.expr(id);
    let t = match &e.kind {
        ExprKind::Num(_) => Some(Ty::Float),
        ExprKind::ColorLit(_) => Some(Ty::Vec4),
        ExprKind::Str(_) => {
            errors.push(IrError::new(
                "a string is only valid as a named op parameter (e.g. palette(n, \"sunset\"))",
                e.span,
            ));
            None
        }
        ExprKind::Input(i) => {
            if !i.in_stage(stage) {
                errors.push(IrError::new(
                    format!("`{}` is not available in {} shaders", i.name(), stage_name(stage)),
                    e.span,
                ));
                None
            } else {
                Some(i.ty())
            }
        }
        ExprKind::Uniform(u) => Some(ir.uniforms[*u].ty),
        ExprKind::Texture(_) => {
            errors.push(IrError::new(
                "a texture slot is only valid as the first argument of sample()",
                e.span,
            ));
            None
        }
        ExprKind::Let(l) => {
            let t = let_ty[*l];
            if t.is_none() {
                // The binding itself already errored; stay quiet here.
            }
            t
        }
        ExprKind::Binary(op, a, b) => {
            let ta = infer(ir, stage, *a, let_ty, ck, errors);
            let tb = infer(ir, stage, *b, let_ty, ck, errors);
            match (ta, tb) {
                (Some(ta), Some(tb)) => {
                    if ta == tb {
                        Some(ta)
                    } else if ta == Ty::Float {
                        Some(tb) // scalar op vector: splat-widen
                    } else if tb == Ty::Float {
                        Some(ta)
                    } else {
                        errors.push(IrError::new(
                            format!(
                                "can't `{}` a {} and a {}",
                                op.symbol(),
                                ta.flsl(),
                                tb.flsl()
                            ),
                            e.span,
                        ));
                        None
                    }
                }
                _ => None,
            }
        }
        ExprKind::Neg(a) => infer(ir, stage, *a, let_ty, ck, errors),
        ExprKind::Swizzle(a, sw) => {
            let ta = infer(ir, stage, *a, let_ty, ck, errors)?;
            check_swizzle(sw, ta, e.span, errors)
        }
        ExprKind::Call { op, args } => {
            resolve_call(ir, stage, id, op, args, let_ty, ck, errors)
        }
    };
    ck.types[id.0 as usize] = t;
    t
}

fn stage_name(stage: Stage) -> &'static str {
    match stage {
        Stage::Fragment => "fragment",
        Stage::Sdf => "sdf",
        Stage::Sky => "sky",
        Stage::Ui => "ui",
    }
}

/// Validate a swizzle string against the source type; result type = its length.
fn check_swizzle(sw: &str, src: Ty, span: Span, errors: &mut Vec<IrError>) -> Option<Ty> {
    let lanes = src.lanes();
    if sw.is_empty() || sw.len() > 4 {
        errors.push(IrError::new(format!("bad swizzle `.{sw}`"), span));
        return None;
    }
    for c in sw.chars() {
        let lane = match c {
            'x' | 'r' => 0,
            'y' | 'g' => 1,
            'z' | 'b' => 2,
            'w' | 'a' => 3,
            _ => {
                errors.push(IrError::new(format!("bad swizzle component `{c}`"), span));
                return None;
            }
        };
        if lane >= lanes {
            errors.push(IrError::new(
                format!("swizzle `.{sw}` reads component `{c}` of a {}", src.flsl()),
                span,
            ));
            return None;
        }
    }
    Ty::of_lanes(sw.len() as u8)
}

/// Resolve a call's authored args against its signature (positional then
/// named), fill defaults, and unify generic input types.
#[allow(clippy::too_many_arguments)]
fn resolve_call(
    ir: &ShaderIr,
    stage: Stage,
    id: ExprId,
    op_name: &str,
    args: &[CallArg],
    let_ty: &[Option<Ty>],
    ck: &mut Checked,
    errors: &mut Vec<IrError>,
) -> Option<Ty> {
    let span = ir.expr(id).span;

    // vecN constructors are shape-polymorphic (vec3(1), vec3(uv, 0), …) and
    // handled structurally instead of via a signature.
    if let Some(target) = match op_name {
        "vec2" => Some(Ty::Vec2),
        "vec3" => Some(Ty::Vec3),
        "vec4" => Some(Ty::Vec4),
        _ => None,
    } {
        let mut lanes = 0u8;
        let mut resolved = Vec::new();
        for a in args {
            if a.name.is_some() {
                errors.push(IrError::new("vector constructors take positional arguments", span));
                return None;
            }
            let t = infer(ir, stage, a.value, let_ty, ck, errors)?;
            lanes += t.lanes();
            resolved.push(ResolvedArg::Expr(a.value));
        }
        // Single-scalar splat (vec3(0.5)) or exact component count.
        let ok = (args.len() == 1 && lanes == 1) || lanes == target.lanes();
        if !ok {
            errors.push(IrError::new(
                format!("{op_name}(…) needs {} components, got {lanes}", target.lanes()),
                span,
            ));
            return None;
        }
        ck.calls.insert(
            id,
            ResolvedCall { op: stdlib::constructor_spec(op_name), args: resolved, generic: None },
        );
        return Some(target);
    }

    let Some(op) = stdlib::op(op_name) else {
        errors.push(IrError::new(format!("unknown op `{op_name}`"), span));
        return None;
    };
    if !op.stages.contains(&stage) {
        errors.push(IrError::new(
            format!("`{op_name}` is not available in {} shaders", stage_name(stage)),
            span,
        ));
        return None;
    }

    // Fill signature slots: positionals in order, then named.
    let mut slots: Vec<Option<&CallArg>> = vec![None; op.inputs.len()];
    let mut positional = true;
    for a in args {
        match &a.name {
            None => {
                if !positional {
                    errors.push(IrError::new(
                        "positional arguments can't follow named ones",
                        ir.expr(a.value).span,
                    ));
                    return None;
                }
                match slots.iter_mut().find(|s| s.is_none()) {
                    Some(slot) => *slot = Some(a),
                    None => {
                        errors.push(IrError::new(
                            format!("too many arguments for `{op_name}`"),
                            ir.expr(a.value).span,
                        ));
                        return None;
                    }
                }
            }
            Some(name) => {
                positional = false;
                match op.inputs.iter().position(|i| i.name == name) {
                    Some(idx) => {
                        if slots[idx].is_some() {
                            errors.push(IrError::new(
                                format!("`{name}` given twice"),
                                ir.expr(a.value).span,
                            ));
                            return None;
                        }
                        slots[idx] = Some(a);
                    }
                    None => {
                        errors.push(IrError::new(
                            format!("`{op_name}` has no parameter `{name}`"),
                            ir.expr(a.value).span,
                        ));
                        return None;
                    }
                }
            }
        }
    }

    // Type each slot against the signature, unifying generics.
    let mut generic: Option<Ty> = None;
    let mut resolved = Vec::with_capacity(op.inputs.len());
    for (sig, slot) in op.inputs.iter().zip(&slots) {
        match slot {
            None => match sig.default {
                Some(d) => resolved.push(ResolvedArg::Default(d)),
                None => {
                    errors.push(IrError::new(
                        format!("`{op_name}` is missing `{}`", sig.name),
                        span,
                    ));
                    return None;
                }
            },
            Some(a) => {
                // Texture and string params are compile-time, not edges.
                match (&sig.ty, &ir.expr(a.value).kind) {
                    (stdlib::SigTy::Texture, ExprKind::Texture(slot_idx)) => {
                        resolved.push(ResolvedArg::Texture(*slot_idx));
                        continue;
                    }
                    (stdlib::SigTy::Texture, _) => {
                        errors.push(IrError::new(
                            format!("`{}` must be a declared texture slot", sig.name),
                            ir.expr(a.value).span,
                        ));
                        return None;
                    }
                    (stdlib::SigTy::Str, ExprKind::Str(s)) => {
                        resolved.push(ResolvedArg::Str(s.clone()));
                        continue;
                    }
                    (stdlib::SigTy::Str, _) => {
                        errors.push(IrError::new(
                            format!("`{}` must be a string literal", sig.name),
                            ir.expr(a.value).span,
                        ));
                        return None;
                    }
                    _ => {}
                }
                let t = infer(ir, stage, a.value, let_ty, ck, errors)?;
                let ok = match sig.ty {
                    stdlib::SigTy::Exact(want) => {
                        // A scalar splats into any vector slot.
                        t == want || (t == Ty::Float && want != Ty::Float)
                    }
                    stdlib::SigTy::Generic => match generic {
                        None => {
                            generic = Some(t);
                            true
                        }
                        // Scalars mix freely with the resolved generic, and a
                        // vector arg widens a scalar-so-far generic (so
                        // `smoothstep(0, 1, someVec3)` resolves to vec3).
                        Some(g) => {
                            if t == g || t == Ty::Float {
                                true
                            } else if g == Ty::Float {
                                generic = Some(t);
                                true
                            } else {
                                false
                            }
                        }
                    },
                    stdlib::SigTy::GenericVec => {
                        let ok =
                            matches!(t, Ty::Vec2 | Ty::Vec3) && generic.is_none_or(|g| g == t);
                        if ok {
                            generic = Some(t);
                        }
                        ok
                    }
                    stdlib::SigTy::Texture | stdlib::SigTy::Str => unreachable!(),
                };
                if !ok {
                    errors.push(IrError::new(
                        format!(
                            "`{}` of `{op_name}` can't take a {}",
                            sig.name,
                            t.flsl()
                        ),
                        ir.expr(a.value).span,
                    ));
                    return None;
                }
                resolved.push(ResolvedArg::Expr(a.value));
            }
        }
    }

    let out = match op.output {
        stdlib::SigTy::Exact(t) => t,
        stdlib::SigTy::Generic | stdlib::SigTy::GenericVec => match generic {
            Some(g) => g,
            None => {
                errors.push(IrError::new(
                    format!("can't infer the type of `{op_name}` here"),
                    span,
                ));
                return None;
            }
        },
        stdlib::SigTy::Texture | stdlib::SigTy::Str => unreachable!("ops never output textures"),
    };
    ck.calls.insert(id, ResolvedCall { op, args: resolved, generic });
    Some(out)
}
