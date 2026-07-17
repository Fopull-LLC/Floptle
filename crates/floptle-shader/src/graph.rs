//! The graph view of a shader (ADR-0007, proposal §10.2) — headless.
//!
//! Both authoring views project the SAME [`ShaderIr`]: the text view is
//! `text::print`/`text::parse`; this module is the node-graph view. It answers
//! two questions for the editor's canvas:
//!
//! 1. **What are the nodes and wires?** [`build_view`] explodes the IR's
//!    expression trees structurally — every call / operator / swizzle is its
//!    own node, literal arguments stay INLINE on their consumer's port row
//!    (editable in place), and inputs/uniforms/texture slots appear once as
//!    shared source nodes. Named `let`s keep their names; everything else is
//!    anonymous until the artist touches it.
//! 2. **How do edits map back to the IR?** The mutation functions below are
//!    plain tree surgery on the `ShaderIr`; the editor re-prints to `.flsl`
//!    after each one, so the file on disk stays the single source of truth
//!    and hot reload does the rest.
//!
//! Node positions ride the `//@layout` annotation (proposal §4.3): named lets
//! under their name, sources/sinks under namespaced keys (`in.uv`, `u.speed`,
//! `tex.ramp`, `out`). Anonymous nodes are auto-laid-out every frame and get
//! **promoted to a named let the moment they're dragged** — "touch it and it
//! becomes real".

use std::collections::BTreeMap;

use crate::ir::{
    BinOp, CallArg, Checked, Expr, ExprId, ExprKind, Input, ShaderIr, Span, Stage, Ty, Uniform,
};
use crate::stdlib::{self, OpSpec, SigTy};

/// A stable-enough identity for one view node. `Anon` keys are arena indices —
/// valid only for the view generation they came from (the editor rebuilds the
/// view after every mutation / reparse).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NodeKey {
    /// A named `let` binding.
    Let(String),
    /// An unnamed expression node (a nested call/operator in some tree).
    Anon(ExprId),
    /// A built-in input source (`uv`, `time`, …), shared by all its uses.
    Input(Input),
    /// A `uniform` declaration, shared by all its uses.
    Uniform(String),
    /// A `texture` slot declaration.
    Texture(String),
    /// The stage's output sink (one node; one port per output name).
    Out,
}

impl NodeKey {
    /// The `//@layout` key this node persists its position under (`None` for
    /// anonymous nodes — they auto-layout until promoted).
    pub fn layout_key(&self) -> Option<String> {
        match self {
            NodeKey::Let(n) => Some(n.clone()),
            NodeKey::Anon(_) => None,
            NodeKey::Input(i) => Some(format!("in.{}", i.name())),
            NodeKey::Uniform(n) => Some(format!("u.{n}")),
            NodeKey::Texture(n) => Some(format!("tex.{n}")),
            NodeKey::Out => Some("out".into()),
        }
    }
}

/// True when `key` is a namespaced layout key [`build_view`] understands —
/// `text::parse` uses this to keep non-let entries that name real things.
pub fn is_view_layout_key(ir: &ShaderIr, key: &str) -> bool {
    if key == "out" {
        return true;
    }
    if let Some(name) = key.strip_prefix("in.") {
        return Input::by_name(name).is_some();
    }
    if let Some(name) = key.strip_prefix("u.") {
        return ir.uniforms.iter().any(|u| u.name == name);
    }
    if let Some(name) = key.strip_prefix("tex.") {
        return ir.textures.iter().any(|t| t == name);
    }
    false
}

/// Where a value plugs in — one editable slot in the IR. Mutations address
/// ports through this, never through raw arena indices alone.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Site {
    /// The whole right-hand side of `lets[i]`.
    LetRoot(usize),
    /// The whole right-hand side of `output <which>` (an [`OutName`]).
    Output(OutName),
    /// Signature slot `slot` of the call at `call` (mapped to an authored
    /// argument, or appended as a named argument if currently omitted).
    Arg { call: ExprId, slot: usize },
    BinLhs(ExprId),
    BinRhs(ExprId),
    NegArg(ExprId),
    SwizArg(ExprId),
}

/// Output names are a tiny closed set — `Copy` keeps [`Site`] copyable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutName {
    Color,
    Sdf,
}

impl OutName {
    pub fn as_str(self) -> &'static str {
        match self {
            OutName::Color => "color",
            OutName::Sdf => "sdf",
        }
    }

    pub fn parse(s: &str) -> Option<OutName> {
        match s {
            "color" => Some(OutName::Color),
            "sdf" => Some(OutName::Sdf),
            _ => None,
        }
    }
}

/// A literal argument shown (and edited) inline on its consumer's port row.
#[derive(Clone, Debug, PartialEq)]
pub enum InlineVal {
    /// A number (covers `Neg(Num)` — the sign folds into the value).
    Num(f64),
    /// A `#RRGGBB[AA]` color literal.
    Color([f32; 4]),
    /// A `vecN(…)` constructor whose components are ALL numbers: `ctor` is the
    /// constructor expression, `vals` its per-lane values.
    Vec { ctor: ExprId, lanes: u8, vals: [f64; 4] },
    /// A string parameter (palette names) — the editor shows a combo.
    Str(String),
    /// An omitted optional argument, showing its signature default (greyed).
    Default(f64),
    /// A required slot that is currently empty in the authored call — the
    /// checker flags it; the port renders unfilled.
    Missing,
}

/// One input port of a view node.
#[derive(Clone, Debug)]
pub struct GPort {
    pub label: String,
    /// The connected value's type when the shader checks (port coloring).
    pub ty: Option<Ty>,
    /// The port takes a texture slot (wires only from Texture source nodes).
    pub is_texture: bool,
    pub wired: Option<NodeKey>,
    /// The inline editor when unwired.
    pub inline: Option<InlineVal>,
    pub site: Site,
}

/// What a view node IS — drives its title, ports and body widgets.
#[derive(Clone, Debug)]
pub enum NodeKind {
    /// A stdlib op call.
    Op(&'static OpSpec),
    /// A `vecN(…)` constructor (that isn't inline-able).
    VecCtor(&'static str),
    Binary(BinOp),
    Neg,
    /// The swizzle string is editable in the node body.
    Swizzle(String),
    /// A literal at a let/output root — a constant the artist named.
    Constant(InlineVal),
    Input(Input),
    Uniform(usize),
    Texture(usize),
    /// The output sink.
    Out,
}

/// One node of the graph view.
#[derive(Clone, Debug)]
pub struct GNode {
    pub key: NodeKey,
    pub kind: NodeKind,
    /// The `let` name when this node is a named binding.
    pub name: Option<String>,
    /// Output value type when known (`None` for the sink / unchecked trees).
    pub ty: Option<Ty>,
    pub inputs: Vec<GPort>,
    pub pos: (f32, f32),
    /// Position came from `//@layout` (drags persist); auto-laid-out otherwise.
    pub placed: bool,
}

impl GNode {
    /// The node's title line: the let name if it has one, else what it does.
    pub fn title(&self) -> String {
        if let Some(n) = &self.name {
            return n.clone();
        }
        self.op_label()
    }

    /// What the node does, for subtitles and palette entries.
    pub fn op_label(&self) -> String {
        match &self.kind {
            NodeKind::Op(op) => op.name.to_string(),
            NodeKind::VecCtor(n) => n.to_string(),
            NodeKind::Binary(op) => match op {
                BinOp::Add => "+".into(),
                BinOp::Sub => "−".into(),
                BinOp::Mul => "×".into(),
                BinOp::Div => "÷".into(),
            },
            NodeKind::Neg => "negate".into(),
            NodeKind::Swizzle(sw) => format!(".{sw}"),
            NodeKind::Constant(_) => "constant".into(),
            NodeKind::Input(i) => i.name().into(),
            NodeKind::Uniform(_) => "uniform".into(),
            NodeKind::Texture(_) => "texture".into(),
            NodeKind::Out => "output".into(),
        }
    }
}

// ---- view building ----------------------------------------------------------

/// Is this expression rendered INLINE on its consumer's port row (rather than
/// as its own node)? Literals, negated literals, strings, colors, and vecN
/// constructors whose components are all literals.
fn inline_val(ir: &ShaderIr, id: ExprId) -> Option<InlineVal> {
    match &ir.expr(id).kind {
        ExprKind::Num(n) => Some(InlineVal::Num(*n)),
        ExprKind::Neg(a) => match &ir.expr(*a).kind {
            ExprKind::Num(n) => Some(InlineVal::Num(-n)),
            _ => None,
        },
        ExprKind::ColorLit(c) => Some(InlineVal::Color(*c)),
        ExprKind::Str(s) => Some(InlineVal::Str(s.clone())),
        ExprKind::Call { op, args } => {
            let lanes: u8 = match op.as_str() {
                "vec2" => 2,
                "vec3" => 3,
                "vec4" => 4,
                _ => return None,
            };
            if args.len() != lanes as usize {
                return None;
            }
            let mut vals = [0.0f64; 4];
            for (i, a) in args.iter().enumerate() {
                match &ir.expr(a.value).kind {
                    ExprKind::Num(n) => vals[i] = *n,
                    ExprKind::Neg(inner) => match &ir.expr(*inner).kind {
                        ExprKind::Num(n) => vals[i] = -n,
                        _ => return None,
                    },
                    _ => return None,
                }
            }
            Some(InlineVal::Vec { ctor: id, lanes, vals })
        }
        _ => None,
    }
}

/// The node another expression wires FROM (references resolve to the shared
/// source / let nodes; everything else is an anonymous expression node).
fn source_key(ir: &ShaderIr, id: ExprId) -> NodeKey {
    match &ir.expr(id).kind {
        ExprKind::Input(i) => NodeKey::Input(*i),
        ExprKind::Uniform(u) => NodeKey::Uniform(ir.uniforms[*u].name.clone()),
        ExprKind::Texture(t) => NodeKey::Texture(ir.textures[*t].clone()),
        ExprKind::Let(l) => NodeKey::Let(ir.lets[*l].0.clone()),
        _ => NodeKey::Anon(id),
    }
}

/// Map a call's authored arguments onto its signature slots (positional then
/// named — the same filling rule as the checker, but lenient: unknown names
/// and overflow just don't land anywhere).
fn slot_args<'a>(op: &OpSpec, args: &'a [CallArg]) -> Vec<Option<&'a CallArg>> {
    let mut slots: Vec<Option<&CallArg>> = vec![None; op.inputs.len()];
    for a in args {
        match &a.name {
            None => {
                if let Some(s) = slots.iter_mut().find(|s| s.is_none()) {
                    *s = Some(a);
                }
            }
            Some(n) => {
                if let Some(i) = op.inputs.iter().position(|s| s.name == *n) {
                    slots[i] = Some(a);
                }
            }
        }
    }
    slots
}

/// Build the whole graph view from a (possibly not-type-clean) IR. `ck` adds
/// port/output types when the shader checks.
pub fn build_view(ir: &ShaderIr, ck: Option<&Checked>) -> Vec<GNode> {
    build_view_padded(ir, ck, &|_| 0.0)
}

/// [`build_view`] with per-node EXTRA height fed into the auto-layout — the
/// editor passes each node's preview-thumbnail strip so freshly laid-out
/// columns never stack nodes into each other.
pub fn build_view_padded(
    ir: &ShaderIr,
    ck: Option<&Checked>,
    extra_h: &dyn Fn(&GNode) -> f32,
) -> Vec<GNode> {
    let mut b = ViewBuilder { ir, ck, nodes: Vec::new(), seen: BTreeMap::new() };

    // The sink first (so it exists even with no outputs wired).
    let stage = ir.stage.unwrap_or(Stage::Fragment);
    let out_names: &[OutName] = match stage {
        Stage::Fragment | Stage::Sky => &[OutName::Color],
        Stage::Sdf => &[OutName::Sdf, OutName::Color],
    };
    let mut sink_ports = Vec::new();
    for name in out_names {
        let port = match ir.outputs.get(name.as_str()) {
            Some(&e) => {
                b.visit(e);
                GPort {
                    label: name.as_str().into(),
                    ty: b.ty_of(e),
                    is_texture: false,
                    wired: inline_val(ir, e).is_none().then(|| source_key(ir, e)),
                    inline: inline_val(ir, e),
                    site: Site::Output(*name),
                }
            }
            None => GPort {
                label: name.as_str().into(),
                ty: None,
                is_texture: false,
                wired: None,
                inline: Some(InlineVal::Missing),
                site: Site::Output(*name),
            },
        };
        sink_ports.push(port);
    }
    b.nodes.push(GNode {
        key: NodeKey::Out,
        kind: NodeKind::Out,
        name: None,
        ty: None,
        inputs: sink_ports,
        pos: (0.0, 0.0),
        placed: false,
    });

    // Every named let is a node even when nothing consumes it (yet).
    for i in 0..ir.lets.len() {
        b.visit_let(i);
    }

    // Every declared uniform/texture is a node too, even before anything
    // reads it — a freshly added knob has to APPEAR to be wireable at all.
    for u in &ir.uniforms {
        b.emit_source(NodeKey::Uniform(u.name.clone()));
    }
    for t in &ir.textures {
        b.emit_source(NodeKey::Texture(t.clone()));
    }

    let mut nodes = b.nodes;
    resolve_positions(ir, &mut nodes, extra_h);
    nodes
}

/// Re-run the auto-layout over EVERY node (ignoring current positions) and
/// store the result as `//@layout` entries — the "Arrange" button. Anonymous
/// nodes stay unpinned (they re-stack beside their consumers on each build).
pub fn arrange(ir: &mut ShaderIr, ck: Option<&Checked>, extra_h: &dyn Fn(&GNode) -> f32) {
    ir.layout.clear();
    let nodes = build_view_padded(ir, ck, extra_h);
    let r = |v: f32| {
        let v = v.round();
        if v == 0.0 { 0.0 } else { v } // normalize -0 (column x = -0·w)
    };
    for n in &nodes {
        if let Some(k) = n.key.layout_key() {
            ir.layout.insert(k, (r(n.pos.0), r(n.pos.1)));
        }
    }
}

struct ViewBuilder<'a> {
    ir: &'a ShaderIr,
    ck: Option<&'a Checked>,
    nodes: Vec<GNode>,
    /// Shared nodes already emitted (sources, lets), by key.
    seen: BTreeMap<NodeKey, ()>,
}

impl ViewBuilder<'_> {
    fn ty_of(&self, id: ExprId) -> Option<Ty> {
        self.ck.and_then(|c| c.types.get(id.0 as usize).copied().flatten())
    }

    /// Ensure the node for `id`'s VALUE exists (source node, let node, or an
    /// anonymous expression node) — recursing through arguments.
    fn visit(&mut self, id: ExprId) {
        if inline_val(self.ir, id).is_some() {
            return;
        }
        match &self.ir.expr(id).kind {
            ExprKind::Input(i) => self.emit_source(NodeKey::Input(*i)),
            ExprKind::Uniform(u) => {
                self.emit_source(NodeKey::Uniform(self.ir.uniforms[*u].name.clone()));
            }
            ExprKind::Texture(t) => {
                self.emit_source(NodeKey::Texture(self.ir.textures[*t].clone()));
            }
            ExprKind::Let(l) => self.visit_let(*l),
            _ => self.emit_expr(id, None),
        }
    }

    fn visit_let(&mut self, i: usize) {
        let key = NodeKey::Let(self.ir.lets[i].0.clone());
        if self.seen.contains_key(&key) {
            return;
        }
        let root = self.ir.lets[i].1;
        // A literal root renders as a named Constant node.
        if let Some(v) = inline_val(self.ir, root) {
            self.seen.insert(key, ());
            self.nodes.push(GNode {
                key: NodeKey::Let(self.ir.lets[i].0.clone()),
                kind: NodeKind::Constant(v.clone()),
                name: Some(self.ir.lets[i].0.clone()),
                ty: self.ty_of(root),
                inputs: vec![GPort {
                    label: "value".into(),
                    ty: self.ty_of(root),
                    is_texture: false,
                    wired: None,
                    inline: Some(v),
                    site: Site::LetRoot(i),
                }],
                pos: (0.0, 0.0),
                placed: false,
            });
            return;
        }
        // A let whose root is a bare reference (`let a = time`) renders as a
        // pass-through source-style node.
        match &self.ir.expr(root).kind {
            ExprKind::Input(_) | ExprKind::Uniform(_) | ExprKind::Texture(_) | ExprKind::Let(_) => {
                self.seen.insert(key, ());
                self.visit(root);
                self.nodes.push(GNode {
                    key: NodeKey::Let(self.ir.lets[i].0.clone()),
                    kind: NodeKind::Constant(InlineVal::Missing),
                    name: Some(self.ir.lets[i].0.clone()),
                    ty: self.ty_of(root),
                    inputs: vec![GPort {
                        label: "value".into(),
                        ty: self.ty_of(root),
                        is_texture: false,
                        wired: Some(source_key(self.ir, root)),
                        inline: None,
                        site: Site::LetRoot(i),
                    }],
                    pos: (0.0, 0.0),
                    placed: false,
                });
            }
            _ => self.emit_expr(root, Some(i)),
        }
    }

    fn emit_source(&mut self, key: NodeKey) {
        if self.seen.insert(key.clone(), ()).is_some() {
            return;
        }
        let (kind, ty) = match &key {
            NodeKey::Input(i) => (NodeKind::Input(*i), Some(i.ty())),
            NodeKey::Uniform(n) => {
                let idx = self.ir.uniforms.iter().position(|u| u.name == *n).unwrap_or(0);
                (NodeKind::Uniform(idx), self.ir.uniforms.get(idx).map(|u| u.ty))
            }
            NodeKey::Texture(n) => {
                let idx = self.ir.textures.iter().position(|t| t == n).unwrap_or(0);
                (NodeKind::Texture(idx), None)
            }
            _ => unreachable!("emit_source takes source keys"),
        };
        self.nodes.push(GNode {
            key,
            kind,
            name: None,
            ty,
            inputs: Vec::new(),
            pos: (0.0, 0.0),
            placed: false,
        });
    }

    /// Emit the node for a call/operator expression (named when it's the root
    /// of `lets[name_of]`), visiting its arguments first.
    fn emit_expr(&mut self, id: ExprId, name_of: Option<usize>) {
        let key = match name_of {
            Some(i) => NodeKey::Let(self.ir.lets[i].0.clone()),
            None => NodeKey::Anon(id),
        };
        if self.seen.insert(key.clone(), ()).is_some() {
            return;
        }
        let port = |b: &mut Self, label: String, value: ExprId, site: Site, is_texture: bool| {
            b.visit(value);
            GPort {
                label,
                ty: b.ty_of(value),
                is_texture,
                wired: inline_val(b.ir, value).is_none().then(|| source_key(b.ir, value)),
                inline: inline_val(b.ir, value),
                site,
            }
        };

        let (kind, inputs) = match &self.ir.expr(id).kind {
            ExprKind::Call { op, args } => {
                if let Some(spec) = stdlib::op(op) {
                    let slots = slot_args(spec, args);
                    let mut ports = Vec::new();
                    for (slot, (sig, arg)) in spec.inputs.iter().zip(&slots).enumerate() {
                        let site = Site::Arg { call: id, slot };
                        let is_tex = sig.ty == SigTy::Texture;
                        let p = match arg {
                            Some(a) => port(self, sig.name.into(), a.value, site, is_tex),
                            None => GPort {
                                label: sig.name.into(),
                                ty: None,
                                is_texture: is_tex,
                                wired: None,
                                inline: Some(match sig.default {
                                    Some(d) => InlineVal::Default(d),
                                    None => InlineVal::Missing,
                                }),
                                site,
                            },
                        };
                        ports.push(p);
                    }
                    (NodeKind::Op(spec), ports)
                } else if matches!(op.as_str(), "vec2" | "vec3" | "vec4") {
                    let lanes = op[3..].parse::<usize>().unwrap_or(2);
                    let labels: &[&str] = &["x", "y", "z", "w"];
                    let ports = args
                        .iter()
                        .enumerate()
                        .map(|(i, a)| {
                            let label = if args.len() == lanes {
                                labels[i.min(3)].to_string()
                            } else {
                                format!("{}", i + 1)
                            };
                            port(self, label, a.value, Site::Arg { call: id, slot: i }, false)
                        })
                        .collect();
                    (NodeKind::VecCtor(stdlib::constructor_spec(op).name), ports)
                } else {
                    // Unknown op (mid-edit): positional ports, no spec.
                    let ports = args
                        .iter()
                        .enumerate()
                        .map(|(i, a)| {
                            port(self, format!("{}", i + 1), a.value, Site::Arg { call: id, slot: i }, false)
                        })
                        .collect();
                    (NodeKind::VecCtor("call"), ports)
                }
            }
            ExprKind::Binary(op, a, b) => {
                let ports = vec![
                    port(self, "a".into(), *a, Site::BinLhs(id), false),
                    port(self, "b".into(), *b, Site::BinRhs(id), false),
                ];
                (NodeKind::Binary(*op), ports)
            }
            ExprKind::Neg(a) => {
                (NodeKind::Neg, vec![port(self, "x".into(), *a, Site::NegArg(id), false)])
            }
            ExprKind::Swizzle(a, sw) => (
                NodeKind::Swizzle(sw.clone()),
                vec![port(self, "v".into(), *a, Site::SwizArg(id), false)],
            ),
            // References/literals never reach here (visit dispatches them).
            _ => unreachable!("emit_expr takes structural expressions"),
        };
        self.nodes.push(GNode {
            key,
            kind,
            name: name_of.map(|i| self.ir.lets[i].0.clone()),
            ty: self.ty_of(id),
            inputs,
            pos: (0.0, 0.0),
            placed: false,
        });
    }
}

/// Node width/height the canvas ALSO uses — positions are computed against
/// these, so they live beside the layout code.
pub const NODE_W: f32 = 168.0;
pub const NODE_ROW_H: f32 = 22.0;
pub const NODE_HEADER_H: f32 = 26.0;

/// A node's deterministic body height (the canvas draws exactly this).
pub fn node_height(n: &GNode) -> f32 {
    let extra = match &n.kind {
        // Uniform nodes carry meta editors (name/type/default/range rows).
        NodeKind::Uniform(_) => 4.0 * NODE_ROW_H,
        NodeKind::Swizzle(_) => NODE_ROW_H,
        // Constants: a type-switch row; textures: a name row.
        NodeKind::Constant(_) | NodeKind::Texture(_) => NODE_ROW_H,
        _ => 0.0,
    };
    NODE_HEADER_H + n.inputs.len() as f32 * NODE_ROW_H + extra + 8.0
}

/// A REPARSE-STABLE identity string per node, for the editor's session
/// position cache. `NodeKey::Anon` carries an arena index that shifts on
/// every reprint→reparse; but an anonymous node hangs off exactly one
/// consumer port, so "consumer's stable key / port index" names it stably
/// for as long as the structure around it is unchanged. Named nodes and
/// sources are stable by name.
pub fn stable_keys(nodes: &[GNode]) -> BTreeMap<NodeKey, String> {
    // consumer[src] = (consumer node index, port index) — first wire wins.
    let index_of: BTreeMap<NodeKey, usize> =
        nodes.iter().enumerate().map(|(i, n)| (n.key.clone(), i)).collect();
    let mut consumer: Vec<Option<(usize, usize)>> = vec![None; nodes.len()];
    for (ci, n) in nodes.iter().enumerate() {
        for (pi, p) in n.inputs.iter().enumerate() {
            if let Some(src) = &p.wired
                && let Some(&si) = index_of.get(src)
                && consumer[si].is_none()
            {
                consumer[si] = Some((ci, pi));
            }
        }
    }
    fn key_of(
        i: usize,
        nodes: &[GNode],
        consumer: &[Option<(usize, usize)>],
        memo: &mut Vec<Option<String>>,
        depth: usize,
    ) -> String {
        if let Some(k) = &memo[i] {
            return k.clone();
        }
        let k = match &nodes[i].key {
            NodeKey::Let(n) => format!("let.{n}"),
            NodeKey::Input(inp) => format!("in.{}", inp.name()),
            NodeKey::Uniform(n) => format!("u.{n}"),
            NodeKey::Texture(n) => format!("tex.{n}"),
            NodeKey::Out => "out".into(),
            NodeKey::Anon(_) => match (depth < 64).then_some(consumer[i]).flatten() {
                Some((ci, pi)) => {
                    format!("{}/{pi}", key_of(ci, nodes, consumer, memo, depth + 1))
                }
                None => format!("orphan.{i}"),
            },
        };
        memo[i] = Some(k.clone());
        k
    }
    let mut memo: Vec<Option<String>> = vec![None; nodes.len()];
    (0..nodes.len())
        .map(|i| (nodes[i].key.clone(), key_of(i, nodes, &consumer, &mut memo, 0)))
        .collect()
}

/// Fill node positions: `//@layout` entries win; everything unplaced is
/// auto-laid-out by dependency depth (sources left, sink right), stacked
/// downward per column in emission order. Deterministic. `extra_h` adds the
/// host's per-node display height (preview thumbnails) to the stacking.
fn resolve_positions(ir: &ShaderIr, nodes: &mut [GNode], extra_h: &dyn Fn(&GNode) -> f32) {
    // Wire map: node -> the nodes it feeds (for depth).
    let index_of: BTreeMap<NodeKey, usize> =
        nodes.iter().enumerate().map(|(i, n)| (n.key.clone(), i)).collect();
    let mut consumers: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    for (i, n) in nodes.iter().enumerate() {
        for p in &n.inputs {
            if let Some(src) = &p.wired
                && let Some(&s) = index_of.get(src)
            {
                consumers[s].push(i);
            }
        }
    }
    // depth = longest path to the sink (sink 0). Iterate to fixpoint (the
    // graph is small and acyclic; lets can't self-reference).
    let mut depth = vec![0usize; nodes.len()];
    for _ in 0..nodes.len() {
        let mut changed = false;
        for i in 0..nodes.len() {
            let d = consumers[i].iter().map(|&c| depth[c] + 1).max().unwrap_or(0);
            if d > depth[i] && d <= nodes.len() {
                depth[i] = d;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Placed nodes take their stored spot.
    for n in nodes.iter_mut() {
        if let Some(key) = n.key.layout_key()
            && let Some(&(x, y)) = ir.layout.get(&key)
        {
            n.pos = (x, y);
            n.placed = true;
        }
    }
    // Unplaced: columns right-to-left from the sink at x = 0.
    let max_depth = depth.iter().copied().max().unwrap_or(0);
    let mut col_y: Vec<f32> = vec![0.0; max_depth + 1];
    for i in 0..nodes.len() {
        if nodes[i].placed {
            continue;
        }
        let d = depth[i];
        let x = -(d as f32) * (NODE_W + 60.0);
        let y = col_y[d];
        col_y[d] = y + node_height(&nodes[i]) + extra_h(&nodes[i]) + 24.0;
        nodes[i].pos = (x, y);
    }
}

// ---- mutations ---------------------------------------------------------------
//
// Every mutation is plain IR surgery; the caller re-prints + re-parses after.
// Replaced subtrees stay in the arena unreachable — the printer only walks
// reachable trees, and the next parse rebuilds a clean arena.

/// A short, human-readable reason a graph edit was refused.
pub type EditError = String;

/// A fresh binding name: `base1`, `base2`, … avoiding every taken name, op
/// name and the reserved `vec` prefix (mirrors the parser's rules).
pub fn fresh_name(ir: &ShaderIr, base: &str) -> String {
    let base: String = base.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let base = if base.is_empty() || !base.chars().next().unwrap().is_ascii_alphabetic() {
        "node".to_string()
    } else {
        base
    };
    let base = base.trim_end_matches(|c: char| c.is_ascii_digit()).to_string();
    let base = if base.is_empty() || base.starts_with("vec") { "node".to_string() } else { base };
    for n in 1..10_000 {
        let cand = format!("{base}{n}");
        if name_is_free(ir, &cand) {
            return cand;
        }
    }
    format!("{base}{}", ir.lets.len())
}

/// The parser's freshness rule, callable before a rename lands.
pub fn name_is_free(ir: &ShaderIr, name: &str) -> bool {
    let valid = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    valid
        && Input::by_name(name).is_none()
        && !ir.uniforms.iter().any(|u| u.name == name)
        && !ir.textures.iter().any(|t| t == name)
        && !ir.lets.iter().any(|(n, _)| n == name)
        && stdlib::op(name).is_none()
        && !name.starts_with("vec")
}

fn push(ir: &mut ShaderIr, kind: ExprKind) -> ExprId {
    ir.push(kind, Span::default())
}

/// Read the expression currently at `site` (None when an optional arg is
/// omitted or the output doesn't exist yet).
pub fn site_value(ir: &ShaderIr, site: Site) -> Option<ExprId> {
    match site {
        Site::LetRoot(i) => ir.lets.get(i).map(|(_, e)| *e),
        Site::Output(name) => ir.outputs.get(name.as_str()).copied(),
        Site::Arg { call, slot } => match &ir.expr(call).kind {
            ExprKind::Call { op, args } => {
                if let Some(spec) = stdlib::op(op) {
                    slot_args(spec, args).get(slot).copied().flatten().map(|a| a.value)
                } else {
                    args.get(slot).map(|a| a.value)
                }
            }
            _ => None,
        },
        Site::BinLhs(e) => match ir.expr(e).kind {
            ExprKind::Binary(_, a, _) => Some(a),
            _ => None,
        },
        Site::BinRhs(e) => match ir.expr(e).kind {
            ExprKind::Binary(_, _, b) => Some(b),
            _ => None,
        },
        Site::NegArg(e) => match ir.expr(e).kind {
            ExprKind::Neg(a) => Some(a),
            _ => None,
        },
        Site::SwizArg(e) => match ir.expr(e).kind {
            ExprKind::Swizzle(a, _) => Some(a),
            _ => None,
        },
    }
}

/// Write `value` into `site` (appending a named argument when an optional
/// slot was omitted).
fn set_site(ir: &mut ShaderIr, site: Site, value: ExprId) -> Result<(), EditError> {
    match site {
        Site::LetRoot(i) => {
            ir.lets.get_mut(i).ok_or("stale let")?.1 = value;
        }
        Site::Output(name) => {
            ir.outputs.insert(name.as_str().to_string(), value);
        }
        Site::Arg { call, slot } => {
            let ExprKind::Call { op, args } = &ir.expr(call).kind else {
                return Err("stale call".into());
            };
            let (op, mut new_args) = (op.clone(), args.clone());
            if let Some(spec) = stdlib::op(&op) {
                let idx = {
                    let slots = slot_args(spec, &new_args);
                    slots.get(slot).copied().flatten().and_then(|a| {
                        new_args.iter().position(|x| std::ptr::eq(x, a))
                    })
                };
                match idx {
                    Some(i) => new_args[i].value = value,
                    None => {
                        let name = spec
                            .inputs
                            .get(slot)
                            .map(|s| s.name.to_string())
                            .ok_or("stale arg slot")?;
                        new_args.push(CallArg { name: Some(name), value });
                    }
                }
            } else {
                new_args.get_mut(slot).ok_or("stale arg slot")?.value = value;
            }
            let span = ir.expr(call).span;
            ir.exprs[call.0 as usize] = Expr { kind: ExprKind::Call { op, args: new_args }, span };
        }
        Site::BinLhs(e) => match &mut ir.exprs[e.0 as usize].kind {
            ExprKind::Binary(_, a, _) => *a = value,
            _ => return Err("stale operator".into()),
        },
        Site::BinRhs(e) => match &mut ir.exprs[e.0 as usize].kind {
            ExprKind::Binary(_, _, b) => *b = value,
            _ => return Err("stale operator".into()),
        },
        Site::NegArg(e) => match &mut ir.exprs[e.0 as usize].kind {
            ExprKind::Neg(a) => *a = value,
            _ => return Err("stale negate".into()),
        },
        Site::SwizArg(e) => match &mut ir.exprs[e.0 as usize].kind {
            ExprKind::Swizzle(a, _) => *a = value,
            _ => return Err("stale swizzle".into()),
        },
    }
    Ok(())
}

/// Drop an optional call argument back to its signature default. Returns true
/// when the site was an omittable arg (caller falls back to a literal else).
fn remove_optional_arg(ir: &mut ShaderIr, site: Site) -> bool {
    let Site::Arg { call, slot } = site else { return false };
    let ExprKind::Call { op, args } = &ir.expr(call).kind else { return false };
    let (op, args) = (op.clone(), args.clone());
    let Some(spec) = stdlib::op(&op) else { return false };
    if spec.inputs.get(slot).is_none_or(|s| s.default.is_none()) {
        return false;
    }
    let idx = {
        let slots = slot_args(spec, &args);
        slots.get(slot).copied().flatten().and_then(|a| args.iter().position(|x| std::ptr::eq(x, a)))
    };
    let Some(idx) = idx else { return true }; // already omitted
    let mut new_args = args;
    let removed = new_args.remove(idx);
    // Positionals AFTER the removed one would shift slots — re-name them all
    // so every remaining arg keeps its meaning.
    if removed.name.is_none() {
        let slots_now = spec.inputs;
        let mut pos = 0usize;
        for a in new_args.iter_mut() {
            if a.name.is_none() {
                // This positional previously filled slot `pos` counting the
                // removed one — pin it by name to be unambiguous.
                let slot_i = if pos >= idx { pos + 1 } else { pos };
                if let Some(s) = slots_now.get(slot_i) {
                    a.name = Some(s.name.to_string());
                }
                pos += 1;
            }
        }
    }
    let span = ir.expr(call).span;
    ir.exprs[call.0 as usize] = Expr { kind: ExprKind::Call { op, args: new_args }, span };
    true
}

/// The literal a disconnected/deleted value falls back to at `site`.
fn fallback_literal(ir: &mut ShaderIr, site: Site) -> ExprId {
    // A texture slot arg can't be a number — point at slot 0 (the checker
    // complains if none exist, which is the honest state).
    if let Site::Arg { call, slot } = site
        && let ExprKind::Call { op, .. } = &ir.expr(call).kind
        && stdlib::op(op).and_then(|s| s.inputs.get(slot)).map(|s| s.ty) == Some(SigTy::Texture)
    {
        return push(ir, ExprKind::Texture(0));
    }
    push(ir, ExprKind::Num(0.0))
}

/// Wire `src`'s value into `site`. Anonymous sources gaining a second consumer
/// are promoted to named lets first; let ordering re-sorts afterwards (a wire
/// that would create a cycle is refused).
pub fn connect(ir: &mut ShaderIr, src: &NodeKey, site: Site) -> Result<(), EditError> {
    // Self-wire guard: wiring a let to its own tree is the cycle case caught
    // by sort_lets below; wiring a node to itself is caught here.
    let value = match src {
        NodeKey::Input(i) => push(ir, ExprKind::Input(*i)),
        NodeKey::Uniform(n) => {
            let u = ir.uniforms.iter().position(|u| u.name == *n).ok_or("stale uniform")?;
            push(ir, ExprKind::Uniform(u))
        }
        NodeKey::Texture(n) => {
            let t = ir.textures.iter().position(|t| t == n).ok_or("stale texture")?;
            push(ir, ExprKind::Texture(t))
        }
        NodeKey::Let(n) => {
            let l = ir.lets.iter().position(|(x, _)| x == n).ok_or("stale let")?;
            push(ir, ExprKind::Let(l))
        }
        NodeKey::Anon(e) => {
            // The expression currently lives at exactly one site. Moving it?
            // No — wiring it somewhere ELSE means sharing: promote to a let.
            let l = promote_to_let(ir, *e)?;
            push(ir, ExprKind::Let(l))
        }
        NodeKey::Out => return Err("the output node has no value to wire".into()),
    };
    set_site(ir, site, value)?;
    sort_lets(ir).map_err(|_| "that wire would create a loop".to_string())
}

/// Unwire `site`: optional args fall back to their defaults, the sdf stage's
/// optional `color` output is removed, everything else becomes a literal.
pub fn disconnect(ir: &mut ShaderIr, site: Site) -> Result<(), EditError> {
    if remove_optional_arg(ir, site) {
        return Ok(());
    }
    if let Site::Output(name) = site {
        // `output sdf` / fragment `color` are required (the checker pins the
        // missing-ness); the sdf stage's `color` is genuinely optional.
        if ir.stage == Some(Stage::Sdf) && name == OutName::Color {
            ir.outputs.remove("color");
            return Ok(());
        }
    }
    let lit = fallback_literal(ir, site);
    set_site(ir, site, lit)
}

/// Set an inline literal at `site` (the port row editors).
pub fn set_inline(ir: &mut ShaderIr, site: Site, v: &InlineVal) -> Result<(), EditError> {
    let value = match v {
        InlineVal::Num(n) => {
            if *n < 0.0 {
                let inner = push(ir, ExprKind::Num(-n));
                push(ir, ExprKind::Neg(inner))
            } else {
                push(ir, ExprKind::Num(*n))
            }
        }
        InlineVal::Color(c) => push(ir, ExprKind::ColorLit(*c)),
        InlineVal::Str(s) => push(ir, ExprKind::Str(s.clone())),
        InlineVal::Vec { lanes, vals, .. } => {
            let ctor = match lanes {
                2 => "vec2",
                3 => "vec3",
                _ => "vec4",
            };
            let mut args = Vec::new();
            for v in vals.iter().take(*lanes as usize) {
                let comp = if *v < 0.0 {
                    let inner = push(ir, ExprKind::Num(-v));
                    push(ir, ExprKind::Neg(inner))
                } else {
                    push(ir, ExprKind::Num(*v))
                };
                args.push(CallArg { name: None, value: comp });
            }
            push(ir, ExprKind::Call { op: ctor.into(), args })
        }
        InlineVal::Default(_) | InlineVal::Missing => return disconnect(ir, site),
    };
    set_site(ir, site, value)
}

/// One in-place component edit of an inline `vecN(…)` (drag one lane without
/// rebuilding the constructor — keeps sibling lanes' text untouched).
pub fn set_vec_component(ir: &mut ShaderIr, ctor: ExprId, lane: usize, v: f64) -> Result<(), EditError> {
    let ExprKind::Call { args, .. } = &ir.expr(ctor).kind else {
        return Err("stale vector".into());
    };
    let arg = args.get(lane).ok_or("stale lane")?.value;
    let comp = if v < 0.0 {
        let inner = push(ir, ExprKind::Num(-v));
        push(ir, ExprKind::Neg(inner))
    } else {
        push(ir, ExprKind::Num(v))
    };
    // Rewrite the arg in place.
    let ExprKind::Call { args, .. } = &mut ir.exprs[ctor.0 as usize].kind else { unreachable!() };
    for a in args.iter_mut() {
        if a.value == arg {
            a.value = comp;
            break;
        }
    }
    Ok(())
}

/// Find the single site referencing `id` (its consumer) by walking every
/// reachable tree.
fn find_site_of(ir: &ShaderIr, id: ExprId) -> Option<Site> {
    fn walk(ir: &ShaderIr, at: ExprId, target: ExprId) -> Option<Site> {
        match &ir.expr(at).kind {
            ExprKind::Call { .. } => {
                let ExprKind::Call { op, args } = &ir.expr(at).kind else { unreachable!() };
                for (i, a) in args.iter().enumerate() {
                    if a.value == target {
                        // Map authored index -> signature slot when known.
                        let slot = stdlib::op(op)
                            .map(|spec| {
                                let slots = slot_args(spec, args);
                                slots
                                    .iter()
                                    .position(|s| s.is_some_and(|x| std::ptr::eq(x, a)))
                                    .unwrap_or(i)
                            })
                            .unwrap_or(i);
                        return Some(Site::Arg { call: at, slot });
                    }
                    if let Some(s) = walk(ir, a.value, target) {
                        return Some(s);
                    }
                }
                None
            }
            ExprKind::Binary(_, a, b) => {
                if *a == target {
                    return Some(Site::BinLhs(at));
                }
                if *b == target {
                    return Some(Site::BinRhs(at));
                }
                walk(ir, *a, target).or_else(|| walk(ir, *b, target))
            }
            ExprKind::Neg(a) => {
                if *a == target {
                    return Some(Site::NegArg(at));
                }
                walk(ir, *a, target)
            }
            ExprKind::Swizzle(a, _) => {
                if *a == target {
                    return Some(Site::SwizArg(at));
                }
                walk(ir, *a, target)
            }
            _ => None,
        }
    }
    for (i, (_, root)) in ir.lets.iter().enumerate() {
        if *root == id {
            return Some(Site::LetRoot(i));
        }
        if let Some(s) = walk(ir, *root, id) {
            return Some(s);
        }
    }
    for (name, root) in &ir.outputs {
        let out = OutName::parse(name)?;
        if *root == id {
            return Some(Site::Output(out));
        }
        if let Some(s) = walk(ir, *root, id) {
            return Some(s);
        }
    }
    None
}

/// Name an anonymous expression: it becomes `let {fresh} = expr`, its old
/// spot references the new binding. Returns the new let index.
pub fn promote_to_let(ir: &mut ShaderIr, id: ExprId) -> Result<usize, EditError> {
    let site = find_site_of(ir, id).ok_or("that node is no longer in the shader")?;
    let base = match &ir.expr(id).kind {
        ExprKind::Call { op, .. } => op.clone(),
        ExprKind::Binary(..) => "mixdown".into(),
        ExprKind::Neg(_) => "neg".into(),
        ExprKind::Swizzle(..) => "part".into(),
        _ => "node".into(),
    };
    let name = fresh_name(ir, &base);
    ir.lets.push((name, id));
    let idx = ir.lets.len() - 1;
    let r = push(ir, ExprKind::Let(idx));
    set_site(ir, site, r)?;
    sort_lets(ir).map_err(|_| "loop".to_string())?;
    // sort_lets may have moved it — find it again by the expr it binds.
    Ok(ir.lets.iter().position(|(_, e)| *e == id).unwrap_or(idx))
}

/// Stable-topologically reorder `lets` so each references only earlier ones
/// (the parser/checker contract). Errs on a reference cycle.
pub fn sort_lets(ir: &mut ShaderIr) -> Result<(), EditError> {
    let n = ir.lets.len();
    // deps[i] = let indices let i's tree references.
    let mut deps: Vec<Vec<usize>> = Vec::with_capacity(n);
    fn collect(ir: &ShaderIr, at: ExprId, out: &mut Vec<usize>) {
        match &ir.expr(at).kind {
            ExprKind::Let(l) => out.push(*l),
            ExprKind::Call { args, .. } => {
                for a in args {
                    collect(ir, a.value, out);
                }
            }
            ExprKind::Binary(_, a, b) => {
                collect(ir, *a, out);
                collect(ir, *b, out);
            }
            ExprKind::Neg(a) | ExprKind::Swizzle(a, _) => collect(ir, *a, out),
            _ => {}
        }
    }
    for (_, root) in &ir.lets {
        let mut d = Vec::new();
        collect(ir, *root, &mut d);
        deps.push(d);
    }
    // Kahn, preferring the current order (stable for already-valid IRs).
    let mut emitted = vec![false; n];
    let mut order: Vec<usize> = Vec::with_capacity(n);
    while order.len() < n {
        let next = (0..n)
            .find(|&i| !emitted[i] && deps[i].iter().all(|&d| emitted[d]))
            .ok_or("cycle")?;
        emitted[next] = true;
        order.push(next);
    }
    if order.iter().enumerate().all(|(a, b)| a == *b) {
        return Ok(()); // already sorted
    }
    // Permute lets + remap every Let reference arena-wide.
    let mut new_index = vec![0usize; n];
    for (new_i, &old_i) in order.iter().enumerate() {
        new_index[old_i] = new_i;
    }
    let old = std::mem::take(&mut ir.lets);
    let mut slots: Vec<Option<(String, ExprId)>> = old.into_iter().map(Some).collect();
    ir.lets = order.iter().map(|&i| slots[i].take().unwrap()).collect();
    for e in ir.exprs.iter_mut() {
        if let ExprKind::Let(l) = &mut e.kind {
            *l = new_index[*l];
        }
    }
    Ok(())
}

/// Every site that references let `i` (for delete's unwiring).
fn sites_referencing_let(ir: &ShaderIr, i: usize) -> Vec<Site> {
    let mut hits = Vec::new();
    let mut ref_ids = Vec::new();
    for (id, e) in ir.exprs.iter().enumerate() {
        if matches!(e.kind, ExprKind::Let(l) if l == i) {
            ref_ids.push(ExprId(id as u32));
        }
    }
    for id in ref_ids {
        if let Some(s) = find_site_of(ir, id) {
            hits.push(s);
        }
    }
    hits
}

/// Delete a node. Named lets unwire their consumers (optional args fall back
/// to defaults, everything else to a literal); uniforms inline their default
/// into every use and drop the declaration; texture slots must be unused.
pub fn delete_node(ir: &mut ShaderIr, key: &NodeKey) -> Result<(), EditError> {
    match key {
        NodeKey::Out => Err("the output node is the shader — it can't be deleted".into()),
        NodeKey::Anon(id) => {
            let site = find_site_of(ir, *id).ok_or("that node is no longer in the shader")?;
            disconnect(ir, site)
        }
        NodeKey::Let(name) => {
            let i = ir.lets.iter().position(|(n, _)| n == name).ok_or("stale let")?;
            for site in sites_referencing_let(ir, i) {
                disconnect(ir, site)?;
            }
            ir.lets.remove(i);
            ir.layout.remove(name);
            for e in ir.exprs.iter_mut() {
                if let ExprKind::Let(l) = &mut e.kind
                    && *l > i
                {
                    *l -= 1;
                }
            }
            Ok(())
        }
        NodeKey::Uniform(name) => {
            let u = ir.uniforms.iter().position(|x| x.name == *name).ok_or("stale uniform")?;
            let uni = ir.uniforms[u].clone();
            // Inline the default everywhere it's read.
            loop {
                let hit = ir
                    .exprs
                    .iter()
                    .position(|e| matches!(e.kind, ExprKind::Uniform(x) if x == u))
                    .map(|i| ExprId(i as u32));
                let Some(id) = hit else { break };
                let Some(site) = find_site_of(ir, id) else {
                    // Unreachable leftover from an earlier edit — neutralize it
                    // so the loop terminates.
                    ir.exprs[id.0 as usize].kind = ExprKind::Num(0.0);
                    continue;
                };
                let v = if uni.is_color {
                    InlineVal::Color(uni.default)
                } else {
                    match uni.ty {
                        Ty::Float => InlineVal::Num(uni.default[0] as f64),
                        t => InlineVal::Vec {
                            ctor: ExprId(0),
                            lanes: t.lanes(),
                            vals: [
                                uni.default[0] as f64,
                                uni.default[1] as f64,
                                uni.default[2] as f64,
                                uni.default[3] as f64,
                            ],
                        },
                    }
                };
                set_inline(ir, site, &v)?;
            }
            ir.uniforms.remove(u);
            ir.layout.remove(&format!("u.{name}"));
            for e in ir.exprs.iter_mut() {
                if let ExprKind::Uniform(x) = &mut e.kind
                    && *x > u
                {
                    *x -= 1;
                }
            }
            Ok(())
        }
        NodeKey::Texture(name) => {
            let t = ir.textures.iter().position(|x| x == name).ok_or("stale texture")?;
            let used = ir.exprs.iter().enumerate().any(|(i, e)| {
                matches!(e.kind, ExprKind::Texture(x) if x == t)
                    && find_site_of(ir, ExprId(i as u32)).is_some()
            });
            if used {
                return Err("that texture slot is in use — disconnect its sample() first".into());
            }
            ir.textures.remove(t);
            ir.layout.remove(&format!("tex.{name}"));
            for e in ir.exprs.iter_mut() {
                if let ExprKind::Texture(x) = &mut e.kind
                    && *x > t
                {
                    *x -= 1;
                }
            }
            Ok(())
        }
        NodeKey::Input(i) => {
            // Input sources exist only through their uses; delete = unwire all.
            loop {
                let hit = ir
                    .exprs
                    .iter()
                    .position(|e| matches!(&e.kind, ExprKind::Input(x) if x == i))
                    .map(|idx| ExprId(idx as u32));
                let Some(id) = hit else { break };
                match find_site_of(ir, id) {
                    Some(site) => disconnect(ir, site)?,
                    None => ir.exprs[id.0 as usize].kind = ExprKind::Num(0.0),
                }
            }
            ir.layout.remove(&format!("in.{}", i.name()));
            Ok(())
        }
    }
}

/// Rename a let (graph title edit). Keeps its layout entry.
pub fn rename_let(ir: &mut ShaderIr, old: &str, new: &str) -> Result<(), EditError> {
    let new = new.trim();
    if new == old {
        return Ok(());
    }
    if !name_is_free(ir, new) {
        return Err(format!("`{new}` is taken or not a valid name"));
    }
    let i = ir.lets.iter().position(|(n, _)| n == old).ok_or("stale let")?;
    ir.lets[i].0 = new.to_string();
    if let Some(pos) = ir.layout.remove(old) {
        ir.layout.insert(new.to_string(), pos);
    }
    Ok(())
}

/// Rename a uniform, keeping its layout entry (the Inspector's param map keys
/// by name — a rename intentionally resets material overrides to defaults).
pub fn rename_uniform(ir: &mut ShaderIr, old: &str, new: &str) -> Result<(), EditError> {
    let new = new.trim();
    if new == old {
        return Ok(());
    }
    if !name_is_free(ir, new) {
        return Err(format!("`{new}` is taken or not a valid name"));
    }
    let u = ir.uniforms.iter().position(|x| x.name == old).ok_or("stale uniform")?;
    ir.uniforms[u].name = new.to_string();
    if let Some(pos) = ir.layout.remove(&format!("u.{old}")) {
        ir.layout.insert(format!("u.{new}"), pos);
    }
    Ok(())
}

/// Rename a texture slot, keeping its layout entry.
pub fn rename_texture(ir: &mut ShaderIr, old: &str, new: &str) -> Result<(), EditError> {
    let new = new.trim();
    if new == old {
        return Ok(());
    }
    if !name_is_free(ir, new) {
        return Err(format!("`{new}` is taken or not a valid name"));
    }
    let t = ir.textures.iter().position(|x| x == old).ok_or("stale texture")?;
    ir.textures[t] = new.to_string();
    if let Some(pos) = ir.layout.remove(&format!("tex.{old}")) {
        ir.layout.insert(format!("tex.{new}"), pos);
    }
    Ok(())
}

/// Persist a node position (dragging an anonymous node names it first —
/// returns the promoted key so the editor can keep its selection).
pub fn set_position(ir: &mut ShaderIr, key: &NodeKey, pos: (f32, f32)) -> Result<NodeKey, EditError> {
    let key = match key {
        NodeKey::Anon(id) => {
            let i = promote_to_let(ir, *id)?;
            NodeKey::Let(ir.lets[i].0.clone())
        }
        k => k.clone(),
    };
    if let Some(lk) = key.layout_key() {
        ir.layout.insert(lk, (pos.0.round(), pos.1.round()));
    }
    Ok(key)
}

/// Duplicate a set of nodes: each named/anonymous value node becomes a fresh
/// `let` copying its whole expression (anonymous ones are named first).
/// References BETWEEN duplicated nodes point at the copies; references to
/// everything else (sources, unselected lets) are shared, like Blender.
/// Sources and the sink don't duplicate. Returns the new nodes' keys.
pub fn duplicate_nodes(ir: &mut ShaderIr, keys: &[NodeKey]) -> Result<Vec<NodeKey>, EditError> {
    // Everything duplicable becomes a let index first.
    let mut lets: Vec<usize> = Vec::new();
    for key in keys {
        match key {
            NodeKey::Let(n) => {
                if let Some(i) = ir.lets.iter().position(|(x, _)| x == n) {
                    lets.push(i);
                }
            }
            NodeKey::Anon(e) => lets.push(promote_to_let(ir, *e)?),
            _ => {}
        }
    }
    lets.sort_unstable();
    lets.dedup();
    if lets.is_empty() {
        return Err("nothing duplicable selected (sources and the output stay single)".into());
    }
    fn deep_copy(
        ir: &mut ShaderIr,
        at: ExprId,
        remap: &BTreeMap<usize, usize>,
    ) -> ExprId {
        let kind = match ir.expr(at).kind.clone() {
            ExprKind::Let(l) => ExprKind::Let(remap.get(&l).copied().unwrap_or(l)),
            ExprKind::Call { op, args } => {
                let args = args
                    .into_iter()
                    .map(|a| CallArg { name: a.name, value: deep_copy(ir, a.value, remap) })
                    .collect();
                ExprKind::Call { op, args }
            }
            ExprKind::Binary(op, a, b) => {
                let (a, b) = (deep_copy(ir, a, remap), deep_copy(ir, b, remap));
                ExprKind::Binary(op, a, b)
            }
            ExprKind::Neg(a) => ExprKind::Neg(deep_copy(ir, a, remap)),
            ExprKind::Swizzle(a, sw) => ExprKind::Swizzle(deep_copy(ir, a, remap), sw),
            leaf => leaf,
        };
        ir.push(kind, Span::default())
    }
    // Ascending order so intra-set references hit already-made copies.
    let mut remap: BTreeMap<usize, usize> = BTreeMap::new();
    let mut out = Vec::new();
    for &i in &lets {
        let (name, root) = ir.lets[i].clone();
        let copy = deep_copy(ir, root, &remap);
        let fresh = fresh_name(ir, &name);
        ir.lets.push((fresh.clone(), copy));
        remap.insert(i, ir.lets.len() - 1);
        if let Some(&(x, y)) = ir.layout.get(&name) {
            ir.layout.insert(fresh.clone(), (x + 32.0, y + 32.0));
        }
        out.push(NodeKey::Let(fresh));
    }
    sort_lets(ir).map_err(|_| "loop".to_string())?;
    Ok(out)
}

/// Add a stdlib-op node as a fresh named let, with beginner-friendly required
/// args (spatial args wire to `uv`/`worldPos`, palettes get a name, texture
/// args take slot 0). Returns the new node's key.
pub fn add_op_node(ir: &mut ShaderIr, op: &OpSpec, pos: (f32, f32)) -> Result<NodeKey, EditError> {
    let stage = ir.stage.unwrap_or(Stage::Fragment);
    let mut args = Vec::new();
    for sig in op.inputs {
        if sig.default.is_some() {
            continue; // optionals ride their signature defaults
        }
        let value = match sig.ty {
            SigTy::Texture => {
                if ir.textures.is_empty() {
                    let name = fresh_name(ir, "tex");
                    ir.textures.push(name);
                }
                push(ir, ExprKind::Texture(0))
            }
            SigTy::Str => push(ir, ExprKind::Str("sunset".into())),
            SigTy::GenericVec => spatial_default(ir, stage),
            SigTy::Exact(Ty::Vec2) => push(ir, ExprKind::Input(Input::Uv)),
            SigTy::Exact(Ty::Vec3) => match sig.name {
                "p" => spatial_default(ir, stage),
                "n" | "normal" => {
                    if stage == Stage::Fragment {
                        push(ir, ExprKind::Input(Input::Normal))
                    } else {
                        vec3_lit(ir, [0.0, 1.0, 0.0])
                    }
                }
                "size" => vec3_lit(ir, [0.5, 0.5, 0.5]),
                "cell" => vec3_lit(ir, [1.0, 1.0, 1.0]),
                _ => vec3_lit(ir, [0.0, 0.0, 0.0]),
            },
            SigTy::Exact(Ty::Vec4) => push(ir, ExprKind::ColorLit([1.0, 1.0, 1.0, 1.0])),
            _ => push(ir, ExprKind::Num(0.0)),
        };
        args.push(CallArg { name: Some(sig.name.to_string()), value });
    }
    let call = push(ir, ExprKind::Call { op: op.name.to_string(), args });
    let name = fresh_name(ir, op.name);
    ir.lets.push((name.clone(), call));
    ir.layout.insert(name.clone(), (pos.0.round(), pos.1.round()));
    Ok(NodeKey::Let(name))
}

fn spatial_default(ir: &mut ShaderIr, stage: Stage) -> ExprId {
    match stage {
        Stage::Fragment => push(ir, ExprKind::Input(Input::Uv)),
        Stage::Sdf => push(ir, ExprKind::Input(Input::WorldPos)),
        Stage::Sky => push(ir, ExprKind::Input(Input::SkyDir)),
    }
}

fn vec3_lit(ir: &mut ShaderIr, v: [f64; 3]) -> ExprId {
    let args = v
        .iter()
        .map(|n| CallArg { name: None, value: push(ir, ExprKind::Num(*n)) })
        .collect();
    push(ir, ExprKind::Call { op: "vec3".into(), args })
}

/// Add a `+ − × ÷` operator node.
pub fn add_binary_node(ir: &mut ShaderIr, op: BinOp, pos: (f32, f32)) -> Result<NodeKey, EditError> {
    let a = push(ir, ExprKind::Num(0.0));
    let b = push(ir, ExprKind::Num(0.0));
    let e = push(ir, ExprKind::Binary(op, a, b));
    let base = match op {
        BinOp::Add => "sum",
        BinOp::Sub => "diff",
        BinOp::Mul => "scaled",
        BinOp::Div => "ratio",
    };
    let name = fresh_name(ir, base);
    ir.lets.push((name.clone(), e));
    ir.layout.insert(name.clone(), (pos.0.round(), pos.1.round()));
    Ok(NodeKey::Let(name))
}

/// Add a swizzle/split node (`.x` of whatever wires in).
pub fn add_swizzle_node(ir: &mut ShaderIr, pos: (f32, f32)) -> Result<NodeKey, EditError> {
    let stage = ir.stage.unwrap_or(Stage::Fragment);
    let src = spatial_default(ir, stage);
    let e = push(ir, ExprKind::Swizzle(src, "x".into()));
    let name = fresh_name(ir, "part");
    ir.lets.push((name.clone(), e));
    ir.layout.insert(name.clone(), (pos.0.round(), pos.1.round()));
    Ok(NodeKey::Let(name))
}

/// Add a `vecN(…)` combine node.
pub fn add_vec_node(ir: &mut ShaderIr, lanes: u8, pos: (f32, f32)) -> Result<NodeKey, EditError> {
    let ctor = match lanes {
        2 => "vec2",
        3 => "vec3",
        _ => "vec4",
    };
    let args = (0..lanes)
        .map(|_| CallArg { name: None, value: push(ir, ExprKind::Num(0.0)) })
        .collect();
    let e = push(ir, ExprKind::Call { op: ctor.into(), args });
    let name = fresh_name(ir, "combined");
    ir.lets.push((name.clone(), e));
    ir.layout.insert(name.clone(), (pos.0.round(), pos.1.round()));
    Ok(NodeKey::Let(name))
}

/// Add a named constant node.
pub fn add_constant_node(ir: &mut ShaderIr, pos: (f32, f32)) -> Result<NodeKey, EditError> {
    let e = push(ir, ExprKind::Num(1.0));
    let name = fresh_name(ir, "value");
    ir.lets.push((name.clone(), e));
    ir.layout.insert(name.clone(), (pos.0.round(), pos.1.round()));
    Ok(NodeKey::Let(name))
}

/// Add an input source node (its node exists once something references it, so
/// a fresh unwired source is a pass-through let: `let time1 = time`).
pub fn add_input_node(ir: &mut ShaderIr, input: Input, pos: (f32, f32)) -> Result<NodeKey, EditError> {
    let e = push(ir, ExprKind::Input(input));
    let name = fresh_name(ir, input.name());
    ir.lets.push((name.clone(), e));
    ir.layout.insert(name.clone(), (pos.0.round(), pos.1.round()));
    Ok(NodeKey::Let(name))
}

/// Add a `uniform` declaration (an Inspector knob) + place its source node.
pub fn add_uniform_node(ir: &mut ShaderIr, is_color: bool, pos: (f32, f32)) -> Result<NodeKey, EditError> {
    let name = fresh_name(ir, if is_color { "tint" } else { "amount" });
    ir.uniforms.push(Uniform {
        name: name.clone(),
        ty: if is_color { Ty::Vec4 } else { Ty::Float },
        default: if is_color { [1.0, 1.0, 1.0, 1.0] } else { [0.5, 0.0, 0.0, 0.0] },
        is_color,
        range: if is_color { None } else { Some((0.0, 1.0)) },
    });
    ir.layout.insert(format!("u.{name}"), (pos.0.round(), pos.1.round()));
    Ok(NodeKey::Uniform(name))
}

/// Add a `texture` slot declaration + place its source node.
pub fn add_texture_node(ir: &mut ShaderIr, pos: (f32, f32)) -> Result<NodeKey, EditError> {
    let name = fresh_name(ir, "tex");
    ir.textures.push(name.clone());
    ir.layout.insert(format!("tex.{name}"), (pos.0.round(), pos.1.round()));
    Ok(NodeKey::Texture(name))
}

/// Change a swizzle node's component string (validated liberally here; the
/// checker reports lane errors with a red pin).
pub fn set_swizzle(ir: &mut ShaderIr, key: &NodeKey, sw: &str) -> Result<(), EditError> {
    let sw = sw.trim();
    if sw.is_empty()
        || sw.len() > 4
        || !sw.chars().all(|c| matches!(c, 'x' | 'y' | 'z' | 'w' | 'r' | 'g' | 'b' | 'a'))
    {
        return Err("a swizzle is 1–4 of x y z w (or r g b a)".into());
    }
    let id = match key {
        NodeKey::Let(n) => ir.lets.iter().find(|(x, _)| x == n).map(|(_, e)| *e),
        NodeKey::Anon(e) => Some(*e),
        _ => None,
    }
    .ok_or("stale node")?;
    match &mut ir.exprs[id.0 as usize].kind {
        ExprKind::Swizzle(_, s) => {
            *s = sw.to_string();
            Ok(())
        }
        _ => Err("stale swizzle".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::{parse, print};

    const PLASMA: &str = r#"
shader plasma {
  stage fragment
  uniform speed: float = 0.1 range(0, 2)
  uniform tint: color = #E6E6F2
  texture ramp

  let warped = domainWarp(uv, scale: 3.0, time: time)
  let n = fbm(warped, octaves: 5)
  let hue = hueShift(palette(n, "sunset"), time * speed)

  output color = vec4(posterize(hue, steps: 6), 1.0) * tint
}
//@layout { warped: (120, 80), n: (320, 80), hue: (520, 96), in.uv: (-60, 40), out: (900, 90) }
"#;

    fn reload(ir: &ShaderIr) -> ShaderIr {
        parse(&print(ir)).expect("mutated shader still parses")
    }

    #[test]
    fn view_explodes_structurally() {
        let ir = parse(PLASMA).unwrap();
        let ck = crate::ir::check(&ir).unwrap();
        let view = build_view(&ir, Some(&ck));
        // Named lets, sources, sink, and the anonymous palette/mul/posterize/vec4.
        let named: Vec<_> = view.iter().filter_map(|n| n.name.clone()).collect();
        assert!(named.contains(&"warped".to_string()));
        assert!(view.iter().any(|n| matches!(n.key, NodeKey::Input(Input::Time))));
        assert!(view.iter().any(|n| matches!(n.key, NodeKey::Out)));
        let anon = view.iter().filter(|n| matches!(n.key, NodeKey::Anon(_))).count();
        assert!(anon >= 3, "palette, the multiply and vec4 are anonymous nodes (got {anon})");
        // Inline literals stay off the node list: `octaves: 5` is not a node.
        let fbm = view.iter().find(|n| n.name.as_deref() == Some("n")).unwrap();
        let oct = fbm.inputs.iter().find(|p| p.label == "octaves").unwrap();
        assert_eq!(oct.inline, Some(InlineVal::Num(5.0)));
        assert!(oct.wired.is_none());
        // Layout keys resolved: warped placed, namespaced keys survive parse.
        let warped = view.iter().find(|n| n.name.as_deref() == Some("warped")).unwrap();
        assert!(warped.placed);
        assert_eq!(warped.pos, (120.0, 80.0));
        let uv = view.iter().find(|n| matches!(n.key, NodeKey::Input(Input::Uv))).unwrap();
        assert!(uv.placed, "in.uv layout key survives");
    }

    #[test]
    fn connect_disconnect_and_inline_round_trip() {
        let mut ir = parse(PLASMA).unwrap();
        // Wire `speed` into fbm's octaves (an omitted-optional appends).
        let (fbm_id, _) = find_call(&ir, "fbm");
        connect(&mut ir, &NodeKey::Uniform("speed".into()), Site::Arg { call: fbm_id, slot: 1 })
            .unwrap();
        let ir = reload(&ir);
        crate::ir::check(&ir).expect("still checks");
        let (fbm_id, _) = find_call(&ir, "fbm");
        let v = site_value(&ir, Site::Arg { call: fbm_id, slot: 1 }).unwrap();
        assert!(matches!(ir.expr(v).kind, ExprKind::Uniform(0)));

        // Disconnect it again — optional falls back to omitted.
        let mut ir = ir;
        disconnect(&mut ir, Site::Arg { call: fbm_id, slot: 1 }).unwrap();
        let ir = reload(&ir);
        let (fbm_id, _) = find_call(&ir, "fbm");
        assert!(site_value(&ir, Site::Arg { call: fbm_id, slot: 1 }).is_none());

        // Inline edit: fbm's gain (slot 3).
        let mut ir = ir;
        set_inline(&mut ir, Site::Arg { call: fbm_id, slot: 3 }, &InlineVal::Num(0.75)).unwrap();
        let ir = reload(&ir);
        crate::ir::check(&ir).expect("still checks");
        let (fbm_id, _) = find_call(&ir, "fbm");
        let v = site_value(&ir, Site::Arg { call: fbm_id, slot: 3 }).unwrap();
        assert!(matches!(ir.expr(v).kind, ExprKind::Num(x) if (x - 0.75).abs() < 1e-9));
    }

    #[test]
    fn cycles_are_refused() {
        let mut ir = parse(PLASMA).unwrap();
        // hue -> warped's `p` would loop (warped feeds n feeds hue).
        let (warp_id, _) = find_call(&ir, "domainWarp");
        let err = connect(&mut ir, &NodeKey::Let("hue".into()), Site::Arg { call: warp_id, slot: 0 })
            .expect_err("cycle");
        assert!(err.contains("loop"), "{err}");
    }

    #[test]
    fn promote_names_an_anonymous_node_and_survives() {
        let mut ir = parse(PLASMA).unwrap();
        let (pal_id, _) = find_call(&ir, "palette");
        let i = promote_to_let(&mut ir, pal_id).unwrap();
        assert!(ir.lets[i].0.starts_with("palette"));
        let printed = print(&ir);
        assert!(printed.contains("let palette1 = palette("), "{printed}");
        let ir = reload(&ir);
        crate::ir::check(&ir).expect("promotion keeps the shader well-typed");
    }

    #[test]
    fn delete_unwires_consumers() {
        let mut ir = parse(PLASMA).unwrap();
        delete_node(&mut ir, &NodeKey::Let("n".into())).unwrap();
        let ir = reload(&ir);
        assert!(!ir.lets.iter().any(|(n, _)| n == "n"));
        // hue's palette arg fell back to a literal; shader still parses and
        // (with a float where t was) still checks.
        crate::ir::check(&ir).expect("checks after delete");

        // Deleting a uniform inlines its default.
        let mut ir = ir;
        delete_node(&mut ir, &NodeKey::Uniform("speed".into())).unwrap();
        let ir = reload(&ir);
        assert_eq!(ir.uniforms.len(), 1);
        crate::ir::check(&ir).expect("checks after uniform delete");
    }

    #[test]
    fn add_nodes_are_wired_and_named_kindly() {
        let mut ir = parse("shader s {\n  stage fragment\n  output color = vec4(1, 1, 1, 1)\n}\n")
            .unwrap();
        let key = add_op_node(&mut ir, crate::stdlib::op("fbm").unwrap(), (10.0, 20.0)).unwrap();
        assert_eq!(key, NodeKey::Let("fbm1".into()));
        let key2 = add_op_node(&mut ir, crate::stdlib::op("sample").unwrap(), (0.0, 0.0)).unwrap();
        assert_eq!(key2, NodeKey::Let("sample1".into()));
        assert_eq!(ir.textures.len(), 1, "sample auto-declared a slot");
        add_binary_node(&mut ir, BinOp::Mul, (0.0, 0.0)).unwrap();
        add_uniform_node(&mut ir, true, (0.0, 0.0)).unwrap();
        add_texture_node(&mut ir, (0.0, 0.0)).unwrap();
        add_input_node(&mut ir, Input::Time, (0.0, 0.0)).unwrap();
        let ir = reload(&ir);
        crate::ir::check(&ir).expect("everything the palette adds type-checks");
        // fbm's p defaulted to uv (fragment stage).
        let (fbm_id, _) = find_call(&ir, "fbm");
        let p = site_value(&ir, Site::Arg { call: fbm_id, slot: 0 }).unwrap();
        assert!(matches!(ir.expr(p).kind, ExprKind::Input(Input::Uv)));
    }

    #[test]
    fn fresh_knobs_and_slots_appear_before_anything_reads_them() {
        // The palette's "knob"/"texture slot" entries only DECLARE — the view
        // must still show them or adding one looks like nothing happened.
        let mut ir = parse("shader s {\n  stage fragment\n  output color = vec4(1, 1, 1, 1)\n}\n")
            .unwrap();
        let k = add_uniform_node(&mut ir, false, (5.0, 6.0)).unwrap();
        let t = add_texture_node(&mut ir, (7.0, 8.0)).unwrap();
        let ir = reload(&ir);
        let view = build_view(&ir, None);
        let uni = view.iter().find(|n| n.key == k).expect("knob node visible");
        assert_eq!(uni.pos, (5.0, 6.0), "layout entry places it");
        assert!(view.iter().any(|n| n.key == t), "texture slot node visible");
    }

    #[test]
    fn sdf_stage_defaults_and_output_ports() {
        let mut ir =
            parse("shader s {\n  stage sdf\n  output sdf = sphere(worldPos)\n}\n").unwrap();
        let key = add_op_node(&mut ir, crate::stdlib::op("box").unwrap(), (0.0, 0.0)).unwrap();
        // box's p → worldPos, size → vec3(0.5).
        let ir2 = reload(&ir);
        crate::ir::check(&ir2).expect("sdf palette node checks");
        // Wire the box into the sdf output, then give the optional color one.
        let mut ir = ir2;
        connect(&mut ir, &key, Site::Output(OutName::Sdf)).unwrap();
        set_inline(
            &mut ir,
            Site::Output(OutName::Color),
            &InlineVal::Vec { ctor: ExprId(0), lanes: 3, vals: [0.8, 0.2, 0.2, 0.0] },
        )
        .unwrap();
        let ir = reload(&ir);
        crate::ir::check(&ir).expect("checks");
        // And the optional color output disconnects away entirely.
        let mut ir = ir;
        disconnect(&mut ir, Site::Output(OutName::Color)).unwrap();
        assert!(!ir.outputs.contains_key("color"));
        crate::ir::check(&reload(&ir)).expect("checks");
    }

    #[test]
    fn arrange_spaces_columns_by_display_height() {
        let mut ir = parse(PLASMA).unwrap();
        // Mirror the editor: most nodes carry a ~154px preview strip.
        let extra =
            |n: &GNode| if matches!(n.kind, NodeKind::Uniform(_)) { 0.0 } else { 154.0 };
        arrange(&mut ir, None, &extra);
        let view = build_view_padded(&ir, None, &extra);
        for a in &view {
            for b in &view {
                if a.key == b.key || (a.pos.0 - b.pos.0).abs() > 1.0 || a.pos.1 >= b.pos.1 {
                    continue;
                }
                assert!(
                    b.pos.1 + 0.5 >= a.pos.1 + node_height(a) + extra(a),
                    "{:?} overlaps {:?}",
                    a.key,
                    b.key
                );
            }
        }
        // Every placeable node got pinned, and the layout survives a reprint.
        let back = reload(&ir);
        assert_eq!(back.layout, ir.layout);
    }

    #[test]
    fn drag_promotes_and_layout_prints() {
        let mut ir = parse(PLASMA).unwrap();
        let (pal_id, _) = find_call(&ir, "palette");
        let key = set_position(&mut ir, &NodeKey::Anon(pal_id), (200.0, 300.0)).unwrap();
        let NodeKey::Let(name) = &key else { panic!("promoted") };
        assert_eq!(ir.layout.get(name), Some(&(200.0, 300.0)));
        let printed = print(&ir);
        assert!(printed.contains(&format!("{name}: (200, 300)")), "{printed}");
        let back = parse(&printed).unwrap();
        assert_eq!(back.layout.get(name), Some(&(200.0, 300.0)));
    }

    #[test]
    fn renames_keep_everything_consistent() {
        let mut ir = parse(PLASMA).unwrap();
        rename_let(&mut ir, "warped", "melted").unwrap();
        rename_uniform(&mut ir, "speed", "pace").unwrap();
        rename_texture(&mut ir, "ramp", "gradient").unwrap();
        assert!(rename_let(&mut ir, "n", "melted").is_err(), "collision refused");
        assert!(rename_let(&mut ir, "hue", "fbm").is_err(), "op name refused");
        let ir = reload(&ir);
        crate::ir::check(&ir).expect("checks after renames");
        assert_eq!(ir.layout.get("melted"), Some(&(120.0, 80.0)), "layout followed the rename");
    }

    #[test]
    fn stable_keys_survive_reprint() {
        // Anon arena indices shift across print/parse; stable keys must not —
        // they anchor the editor's session position cache.
        let ir = parse(PLASMA).unwrap();
        let ck = crate::ir::check(&ir).unwrap();
        let view = build_view(&ir, Some(&ck));
        let keys_a: std::collections::BTreeSet<String> =
            stable_keys(&view).into_values().collect();
        let ir2 = reload(&ir);
        let ck2 = crate::ir::check(&ir2).unwrap();
        let view2 = build_view(&ir2, Some(&ck2));
        let keys_b: std::collections::BTreeSet<String> =
            stable_keys(&view2).into_values().collect();
        assert_eq!(keys_a, keys_b);
        // And every node got a distinct identity.
        assert_eq!(keys_a.len(), view.len());
    }

    #[test]
    fn duplicate_copies_trees_and_remaps_internal_refs() {
        let mut ir = parse(PLASMA).unwrap();
        // Duplicate warped + n together: n's copy must read warped's copy.
        let new = duplicate_nodes(
            &mut ir,
            &[NodeKey::Let("warped".into()), NodeKey::Let("n".into())],
        )
        .unwrap();
        assert_eq!(new.len(), 2);
        let ir = reload(&ir);
        crate::ir::check(&ir).expect("checks after duplicate");
        let n1 = ir.lets.iter().position(|(n, _)| n == "n1").expect("n1 exists");
        let w1 = ir.lets.iter().position(|(n, _)| n == "warped1").expect("warped1 exists");
        // n1's fbm reads warped1, not warped.
        let root = ir.lets[n1].1;
        let ExprKind::Call { args, .. } = &ir.expr(root).kind else { panic!("fbm call") };
        assert!(matches!(ir.expr(args[0].value).kind, ExprKind::Let(l) if l == w1));
        // The originals are untouched and layout offsets applied.
        assert!(ir.lets.iter().any(|(n, _)| n == "warped"));
        assert_eq!(ir.layout.get("warped1"), Some(&(152.0, 112.0)));
        // Duplicating only a source is refused with a friendly message.
        let mut ir = ir;
        assert!(duplicate_nodes(&mut ir, &[NodeKey::Input(Input::Time)]).is_err());
    }

    /// First call expression named `op` (tests address args through it).
    fn find_call(ir: &ShaderIr, op: &str) -> (ExprId, usize) {
        for (i, e) in ir.exprs.iter().enumerate() {
            if let ExprKind::Call { op: o, args } = &e.kind
                && o == op
                && find_site_of(ir, ExprId(i as u32)).is_some()
            {
                return (ExprId(i as u32), args.len());
            }
        }
        panic!("no reachable `{op}` call");
    }
}
