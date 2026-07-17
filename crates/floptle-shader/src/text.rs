//! The `.flsl` text format — parse and print, round-trippable (ADR-0007).
//!
//! Text is the on-disk canon: `parse(print(ir))` is structurally the same
//! shader ([`ShaderIr::same_shader`]). The printer is deterministic so diffs
//! stay small; hand-written text that parses is valid even when the printer
//! would format it differently (formatting normalizes on the next print).
//!
//! ```flsl
//! shader plasma {
//!   stage fragment
//!   uniform speed: float = 0.1 range(0, 2)
//!   uniform tint: color = #E6E6F2
//!   texture ramp
//!
//!   let warped = domainWarp(uv, scale: 3.0, time: time)
//!   let n = fbm(warped, octaves: 5)
//!
//!   output color = posterize(hueShift(palette(n, "sunset"), time * speed), steps: 6) * tint
//! }
//! ```
//!
//! Graph node positions ride a trailing `//@layout { name: (x, y), … }`
//! annotation — cosmetic only, ignored by semantic equality.

use std::collections::BTreeMap;

use crate::ir::{
    BinOp, Blend, CallArg, ExprId, ExprKind, Input, ShaderIr, Span, Stage, Ty, Uniform,
};
use crate::stdlib;

/// A parse failure, anchored to a byte span of the source.
#[derive(Clone, Debug)]
pub struct ParseError {
    pub message: String,
    pub span: Span,
}

impl ParseError {
    fn new(message: impl Into<String>, span: Span) -> Self {
        Self { message: message.into(), span }
    }
}

/// 1-based line + column of a byte offset (for error display).
pub fn line_col(src: &str, offset: u32) -> (u32, u32) {
    let mut line = 1;
    let mut col = 1;
    for (i, c) in src.char_indices() {
        if i as u32 >= offset {
            break;
        }
        if c == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

// ---- lexer ---------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Ident(String),
    Num(f64),
    Str(String),
    /// `#RRGGBB` / `#RRGGBBAA`.
    Color([f32; 4]),
    Punct(char),
}

#[derive(Clone, Debug)]
struct Token {
    tok: Tok,
    span: Span,
}

fn lex(src: &str) -> Result<Vec<Token>, ParseError> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i] as char;
        let start = i as u32;
        match c {
            ' ' | '\t' | '\r' | '\n' => i += 1,
            '/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            '{' | '}' | '(' | ')' | ',' | ':' | '=' | '.' | '+' | '-' | '*' | '/' => {
                out.push(Token { tok: Tok::Punct(c), span: Span { start, end: start + 1 } });
                i += 1;
            }
            '"' => {
                i += 1;
                let s0 = i;
                while i < bytes.len() && bytes[i] != b'"' && bytes[i] != b'\n' {
                    i += 1;
                }
                if bytes.get(i) != Some(&b'"') {
                    return Err(ParseError::new(
                        "unterminated string",
                        Span { start, end: i as u32 },
                    ));
                }
                out.push(Token {
                    tok: Tok::Str(src[s0..i].to_string()),
                    span: Span { start, end: i as u32 + 1 },
                });
                i += 1;
            }
            '#' => {
                i += 1;
                let s0 = i;
                while i < bytes.len() && (bytes[i] as char).is_ascii_hexdigit() {
                    i += 1;
                }
                let hex = &src[s0..i];
                let span = Span { start, end: i as u32 };
                let rgba = parse_hex_color(hex)
                    .ok_or_else(|| ParseError::new("bad color (use #RRGGBB or #RRGGBBAA)", span))?;
                out.push(Token { tok: Tok::Color(rgba), span });
            }
            c if c.is_ascii_digit() => {
                let s0 = i;
                while i < bytes.len()
                    && ((bytes[i] as char).is_ascii_digit() || bytes[i] == b'.')
                {
                    // A digit followed by `.` then a non-digit is a number then a
                    // member access (rare on literals; swizzling a number is an
                    // error later anyway) — keep the simple greedy scan.
                    i += 1;
                }
                let span = Span { start, end: i as u32 };
                let n: f64 = src[s0..i]
                    .parse()
                    .map_err(|_| ParseError::new("bad number", span))?;
                out.push(Token { tok: Tok::Num(n), span });
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let s0 = i;
                while i < bytes.len()
                    && ((bytes[i] as char).is_ascii_alphanumeric() || bytes[i] == b'_')
                {
                    i += 1;
                }
                out.push(Token {
                    tok: Tok::Ident(src[s0..i].to_string()),
                    span: Span { start, end: i as u32 },
                });
            }
            _ => {
                return Err(ParseError::new(
                    format!("unexpected character `{c}`"),
                    Span { start, end: start + 1 },
                ));
            }
        }
    }
    Ok(out)
}

fn parse_hex_color(hex: &str) -> Option<[f32; 4]> {
    let byte = |s: &str| u8::from_str_radix(s, 16).ok().map(|b| b as f32 / 255.0);
    match hex.len() {
        6 => Some([byte(&hex[0..2])?, byte(&hex[2..4])?, byte(&hex[4..6])?, 1.0]),
        8 => Some([byte(&hex[0..2])?, byte(&hex[2..4])?, byte(&hex[4..6])?, byte(&hex[6..8])?]),
        _ => None,
    }
}

// ---- parser ---------------------------------------------------------------

struct Parser<'a> {
    toks: &'a [Token],
    pos: usize,
    ir: ShaderIr,
}

/// Parse `.flsl` source into the IR. Returns the first error found (the
/// checker reports multi-error type diagnostics separately).
pub fn parse(src: &str) -> Result<ShaderIr, ParseError> {
    let layout = parse_layout(src);
    let toks = lex(src)?;
    let mut p = Parser { toks: &toks, pos: 0, ir: ShaderIr::default() };
    p.shader()?;
    if p.pos < p.toks.len() {
        return Err(ParseError::new("unexpected trailing content", p.toks[p.pos].span));
    }
    // Keep only layout entries that name real things: lets by name, plus the
    // graph view's namespaced source/sink keys (`in.uv`, `u.speed`, `tex.ramp`,
    // `out`) — see `graph::is_view_layout_key`.
    p.ir.layout = layout
        .into_iter()
        .filter(|(name, _)| {
            p.ir.lets.iter().any(|(n, _)| n == name)
                || crate::graph::is_view_layout_key(&p.ir, name)
        })
        .collect();
    Ok(p.ir)
}

/// The trailing `//@layout { name: (x, y), … }` annotation — scanned from the
/// raw source (the lexer drops comments). Lenient: a malformed layout line is
/// simply ignored (it's cosmetic).
fn parse_layout(src: &str) -> BTreeMap<String, (f32, f32)> {
    let mut out = BTreeMap::new();
    for line in src.lines() {
        let Some(rest) = line.trim().strip_prefix("//@layout") else { continue };
        let body = rest.trim().trim_start_matches('{').trim_end_matches('}');
        for entry in body.split(',').collect::<Vec<_>>().chunks(2) {
            // Entries are `name: (x, y)` — the `,` inside the tuple splits them,
            // so stitch pairs back together.
            let joined = entry.join(",");
            let Some((name, pos)) = joined.split_once(':') else { continue };
            let pos = pos.trim().trim_start_matches('(').trim_end_matches(')');
            let Some((x, y)) = pos.split_once(',') else { continue };
            if let (Ok(x), Ok(y)) = (x.trim().parse::<f32>(), y.trim().parse::<f32>()) {
                out.insert(name.trim().to_string(), (x, y));
            }
        }
    }
    out
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|t| &t.tok)
    }

    fn span(&self) -> Span {
        self.toks
            .get(self.pos)
            .or_else(|| self.toks.last())
            .map(|t| t.span)
            .unwrap_or_default()
    }

    fn bump(&mut self) -> Option<&Token> {
        let t = self.toks.get(self.pos);
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn expect_punct(&mut self, c: char) -> Result<(), ParseError> {
        match self.peek() {
            Some(Tok::Punct(p)) if *p == c => {
                self.pos += 1;
                Ok(())
            }
            _ => Err(ParseError::new(format!("expected `{c}`"), self.span())),
        }
    }

    fn eat_punct(&mut self, c: char) -> bool {
        if matches!(self.peek(), Some(Tok::Punct(p)) if *p == c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_ident(&mut self, what: &str) -> Result<(String, Span), ParseError> {
        let span = self.span();
        match self.bump().map(|t| t.tok.clone()) {
            Some(Tok::Ident(s)) => Ok((s, span)),
            _ => Err(ParseError::new(format!("expected {what}"), span)),
        }
    }

    fn expect_keyword(&mut self, kw: &str) -> Result<(), ParseError> {
        let span = self.span();
        match self.peek() {
            Some(Tok::Ident(s)) if s == kw => {
                self.pos += 1;
                Ok(())
            }
            _ => Err(ParseError::new(format!("expected `{kw}`"), span)),
        }
    }

    fn shader(&mut self) -> Result<(), ParseError> {
        self.expect_keyword("shader")?;
        let (name, span) = self.expect_ident("a shader name")?;
        if !name.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
            return Err(ParseError::new("shader names start with a letter", span));
        }
        self.ir.name = name;
        self.expect_punct('{')?;
        while !self.eat_punct('}') {
            let span = self.span();
            let Some(Tok::Ident(kw)) = self.peek().cloned() else {
                return Err(ParseError::new(
                    "expected `stage`, `blend`, `uniform`, `texture`, `let` or `output`",
                    span,
                ));
            };
            match kw.as_str() {
                "stage" => self.stage_decl()?,
                "blend" => self.blend_decl()?,
                "uniform" => self.uniform_decl()?,
                "texture" => self.texture_decl()?,
                "let" => self.let_decl()?,
                "output" => self.output_decl()?,
                other => {
                    return Err(ParseError::new(
                        format!("unexpected `{other}` (expected `stage`, `blend`, `uniform`, `texture`, `let` or `output`)"),
                        span,
                    ));
                }
            }
        }
        Ok(())
    }

    fn stage_decl(&mut self) -> Result<(), ParseError> {
        self.pos += 1; // `stage`
        let (name, span) = self.expect_ident("`fragment` or `sdf`")?;
        if self.ir.stage.is_some() {
            return Err(ParseError::new("`stage` declared twice", span));
        }
        self.ir.stage = Some(match name.as_str() {
            "fragment" => Stage::Fragment,
            "sdf" => Stage::Sdf,
            "sky" => Stage::Sky,
            "ui" => Stage::Ui,
            other => {
                return Err(ParseError::new(
                    format!("unknown stage `{other}` (fragment | sdf | sky | ui)"),
                    span,
                ));
            }
        });
        Ok(())
    }

    fn blend_decl(&mut self) -> Result<(), ParseError> {
        self.pos += 1; // `blend`
        let (name, span) = self.expect_ident("`opaque`, `alpha` or `additive`")?;
        self.ir.blend = match name.as_str() {
            "opaque" => Blend::Opaque,
            "alpha" => Blend::Alpha,
            "additive" => Blend::Additive,
            other => {
                return Err(ParseError::new(
                    format!("unknown blend `{other}` (opaque | alpha | additive)"),
                    span,
                ));
            }
        };
        Ok(())
    }

    fn uniform_decl(&mut self) -> Result<(), ParseError> {
        self.pos += 1; // `uniform`
        let (name, nspan) = self.expect_ident("a uniform name")?;
        self.check_fresh_name(&name, nspan)?;
        self.expect_punct(':')?;
        let (tyname, tspan) = self.expect_ident("a type (float | vec2 | vec3 | vec4 | color)")?;
        let (ty, is_color) = match tyname.as_str() {
            "float" => (Ty::Float, false),
            "vec2" => (Ty::Vec2, false),
            "vec3" => (Ty::Vec3, false),
            "vec4" => (Ty::Vec4, false),
            "color" => (Ty::Vec4, true),
            other => {
                return Err(ParseError::new(
                    format!("unknown type `{other}` (float | vec2 | vec3 | vec4 | color)"),
                    tspan,
                ));
            }
        };
        let mut default = [0.0f32; 4];
        if is_color {
            default = [1.0, 1.0, 1.0, 1.0];
        }
        if self.eat_punct('=') {
            default = self.const_value(ty, is_color)?;
        }
        let mut range = None;
        if matches!(self.peek(), Some(Tok::Ident(s)) if s == "range") {
            self.pos += 1;
            self.expect_punct('(')?;
            let lo = self.const_number()?;
            self.expect_punct(',')?;
            let hi = self.const_number()?;
            self.expect_punct(')')?;
            range = Some((lo as f32, hi as f32));
        }
        self.ir.uniforms.push(Uniform { name, ty, default, is_color, range });
        Ok(())
    }

    /// A constant initializer: a number, `-number`, `#hex`, or `vecN(n, …)`.
    fn const_value(&mut self, ty: Ty, is_color: bool) -> Result<[f32; 4], ParseError> {
        let span = self.span();
        match self.peek().cloned() {
            Some(Tok::Color(c)) => {
                self.pos += 1;
                if !is_color && ty != Ty::Vec4 {
                    return Err(ParseError::new("a #color initializes color/vec4 uniforms", span));
                }
                Ok(c)
            }
            Some(Tok::Num(_)) | Some(Tok::Punct('-')) => {
                let n = self.const_number()? as f32;
                match ty {
                    Ty::Float => Ok([n, 0.0, 0.0, 0.0]),
                    // A scalar splats across vector uniforms.
                    Ty::Vec2 => Ok([n, n, 0.0, 0.0]),
                    Ty::Vec3 => Ok([n, n, n, 0.0]),
                    Ty::Vec4 => Ok([n, n, n, n]),
                }
            }
            Some(Tok::Ident(v)) if v.starts_with("vec") => {
                self.pos += 1;
                self.expect_punct('(')?;
                let mut vals = Vec::new();
                loop {
                    vals.push(self.const_number()? as f32);
                    if !self.eat_punct(',') {
                        break;
                    }
                }
                self.expect_punct(')')?;
                if vals.len() != ty.lanes() as usize {
                    return Err(ParseError::new(
                        format!("expected {} components", ty.lanes()),
                        span,
                    ));
                }
                let mut out = [0.0f32; 4];
                out[..vals.len()].copy_from_slice(&vals);
                if is_color && vals.len() == 3 {
                    out[3] = 1.0;
                }
                Ok(out)
            }
            _ => Err(ParseError::new("expected a default value", span)),
        }
    }

    fn const_number(&mut self) -> Result<f64, ParseError> {
        let neg = self.eat_punct('-');
        let span = self.span();
        match self.bump().map(|t| t.tok.clone()) {
            Some(Tok::Num(n)) => Ok(if neg { -n } else { n }),
            _ => Err(ParseError::new("expected a number", span)),
        }
    }

    fn texture_decl(&mut self) -> Result<(), ParseError> {
        self.pos += 1; // `texture`
        let (name, span) = self.expect_ident("a texture slot name")?;
        self.check_fresh_name(&name, span)?;
        self.ir.textures.push(name);
        Ok(())
    }

    fn let_decl(&mut self) -> Result<(), ParseError> {
        self.pos += 1; // `let`
        let (name, span) = self.expect_ident("a binding name")?;
        self.check_fresh_name(&name, span)?;
        self.expect_punct('=')?;
        let e = self.expr(0)?;
        self.ir.lets.push((name, e));
        Ok(())
    }

    fn output_decl(&mut self) -> Result<(), ParseError> {
        self.pos += 1; // `output`
        let (name, span) = self.expect_ident("an output name (color | sdf)")?;
        if self.ir.outputs.contains_key(&name) {
            return Err(ParseError::new(format!("output `{name}` declared twice"), span));
        }
        self.expect_punct('=')?;
        let e = self.expr(0)?;
        self.ir.outputs.insert(name, e);
        Ok(())
    }

    fn check_fresh_name(&self, name: &str, span: Span) -> Result<(), ParseError> {
        let taken = Input::by_name(name).is_some()
            || self.ir.uniforms.iter().any(|u| u.name == name)
            || self.ir.textures.iter().any(|t| t == name)
            || self.ir.lets.iter().any(|(n, _)| n == name);
        if taken {
            return Err(ParseError::new(format!("`{name}` is already defined"), span));
        }
        if stdlib::op(name).is_some() || name.starts_with("vec") {
            return Err(ParseError::new(format!("`{name}` is a stdlib op name"), span));
        }
        Ok(())
    }

    // Pratt expression parser: + - (prec 1) < * / (prec 2) < unary - < postfix.
    fn expr(&mut self, min_prec: u8) -> Result<ExprId, ParseError> {
        let mut lhs = self.unary()?;
        loop {
            let (op, prec) = match self.peek() {
                Some(Tok::Punct('+')) => (BinOp::Add, 1),
                Some(Tok::Punct('-')) => (BinOp::Sub, 1),
                Some(Tok::Punct('*')) => (BinOp::Mul, 2),
                Some(Tok::Punct('/')) => (BinOp::Div, 2),
                _ => break,
            };
            if prec < min_prec {
                break;
            }
            let op_span = self.span();
            self.pos += 1;
            let rhs = self.expr(prec + 1)?;
            let span = Span { start: self.ir.expr(lhs).span.start, end: op_span.end.max(self.ir.expr(rhs).span.end) };
            lhs = self.ir.push(ExprKind::Binary(op, lhs, rhs), span);
        }
        Ok(lhs)
    }

    fn unary(&mut self) -> Result<ExprId, ParseError> {
        if matches!(self.peek(), Some(Tok::Punct('-'))) {
            let span = self.span();
            self.pos += 1;
            let e = self.unary()?;
            let span = Span { start: span.start, end: self.ir.expr(e).span.end };
            return Ok(self.ir.push(ExprKind::Neg(e), span));
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Result<ExprId, ParseError> {
        let mut e = self.atom()?;
        while self.eat_punct('.') {
            let (sw, span) = self.expect_ident("a swizzle (e.g. .xyz)")?;
            let full = Span { start: self.ir.expr(e).span.start, end: span.end };
            e = self.ir.push(ExprKind::Swizzle(e, sw), full);
        }
        Ok(e)
    }

    fn atom(&mut self) -> Result<ExprId, ParseError> {
        let span = self.span();
        match self.bump().map(|t| t.tok.clone()) {
            Some(Tok::Num(n)) => Ok(self.ir.push(ExprKind::Num(n), span)),
            Some(Tok::Color(c)) => Ok(self.ir.push(ExprKind::ColorLit(c), span)),
            Some(Tok::Str(s)) => Ok(self.ir.push(ExprKind::Str(s), span)),
            Some(Tok::Punct('(')) => {
                let e = self.expr(0)?;
                self.expect_punct(')')?;
                Ok(e)
            }
            Some(Tok::Ident(name)) => {
                if matches!(self.peek(), Some(Tok::Punct('('))) {
                    self.pos += 1;
                    let mut args = Vec::new();
                    if !self.eat_punct(')') {
                        loop {
                            // `name: value` (named) or plain expression.
                            let arg_name = if let (Some(Tok::Ident(n)), Some(Tok::Punct(':'))) =
                                (self.peek(), self.toks.get(self.pos + 1).map(|t| &t.tok))
                            {
                                let n = n.clone();
                                self.pos += 2;
                                Some(n)
                            } else {
                                None
                            };
                            let value = self.expr(0)?;
                            args.push(CallArg { name: arg_name, value });
                            if !self.eat_punct(',') {
                                break;
                            }
                        }
                        self.expect_punct(')')?;
                    }
                    let end = self.toks.get(self.pos - 1).map(|t| t.span.end).unwrap_or(span.end);
                    return Ok(self
                        .ir
                        .push(ExprKind::Call { op: name, args }, Span { start: span.start, end }));
                }
                // A bare name: input, uniform, texture slot, or earlier let.
                if let Some(i) = Input::by_name(&name) {
                    return Ok(self.ir.push(ExprKind::Input(i), span));
                }
                if let Some(u) = self.ir.uniforms.iter().position(|u| u.name == name) {
                    return Ok(self.ir.push(ExprKind::Uniform(u), span));
                }
                if let Some(t) = self.ir.textures.iter().position(|t| *t == name) {
                    return Ok(self.ir.push(ExprKind::Texture(t), span));
                }
                if let Some(l) = self.ir.lets.iter().position(|(n, _)| *n == name) {
                    return Ok(self.ir.push(ExprKind::Let(l), span));
                }
                Err(ParseError::new(format!("unknown name `{name}`"), span))
            }
            _ => Err(ParseError::new("expected an expression", span)),
        }
    }
}

// ---- printer ---------------------------------------------------------------

/// Print the IR as canonical `.flsl` text (the graph's "Open in VSCode"
/// projection). Deterministic; `parse(print(ir))` is `same_shader(ir)`.
pub fn print(ir: &ShaderIr) -> String {
    let mut s = String::new();
    s.push_str(&format!("shader {} {{\n", ir.name));
    match ir.stage {
        Some(Stage::Fragment) | None => s.push_str("  stage fragment\n"),
        Some(Stage::Sdf) => s.push_str("  stage sdf\n"),
        Some(Stage::Sky) => s.push_str("  stage sky\n"),
        Some(Stage::Ui) => s.push_str("  stage ui\n"),
    }
    match ir.blend {
        Blend::Opaque => {}
        Blend::Alpha => s.push_str("  blend alpha\n"),
        Blend::Additive => s.push_str("  blend additive\n"),
    }
    for u in &ir.uniforms {
        s.push_str(&format!("  uniform {}: {}", u.name, if u.is_color { "color" } else { u.ty.flsl() }));
        s.push_str(&format!(" = {}", print_const(u)));
        if let Some((lo, hi)) = u.range {
            s.push_str(&format!(" range({}, {})", lo, hi));
        }
        s.push('\n');
    }
    for t in &ir.textures {
        s.push_str(&format!("  texture {t}\n"));
    }
    if !ir.lets.is_empty() {
        s.push('\n');
    }
    for (name, e) in &ir.lets {
        s.push_str(&format!("  let {name} = {}\n", print_expr(ir, *e, 0)));
    }
    if !ir.outputs.is_empty() {
        s.push('\n');
    }
    for (name, e) in &ir.outputs {
        s.push_str(&format!("  output {name} = {}\n", print_expr(ir, *e, 0)));
    }
    s.push_str("}\n");
    if !ir.layout.is_empty() {
        let entries: Vec<String> =
            ir.layout.iter().map(|(n, (x, y))| format!("{n}: ({x}, {y})")).collect();
        s.push_str(&format!("//@layout {{ {} }}\n", entries.join(", ")));
    }
    s
}

fn print_const(u: &Uniform) -> String {
    let d = u.default;
    if u.is_color {
        let b = |v: f32| ((v.clamp(0.0, 1.0) * 255.0).round()) as u8;
        return if (d[3] - 1.0).abs() < 1e-6 {
            format!("#{:02X}{:02X}{:02X}", b(d[0]), b(d[1]), b(d[2]))
        } else {
            format!("#{:02X}{:02X}{:02X}{:02X}", b(d[0]), b(d[1]), b(d[2]), b(d[3]))
        };
    }
    match u.ty {
        Ty::Float => format!("{}", d[0]),
        Ty::Vec2 => format!("vec2({}, {})", d[0], d[1]),
        Ty::Vec3 => format!("vec3({}, {}, {})", d[0], d[1], d[2]),
        Ty::Vec4 => format!("vec4({}, {}, {}, {})", d[0], d[1], d[2], d[3]),
    }
}

/// Print one expression. `parent_prec`: parenthesize when this node binds
/// looser than the context (0 = statement root, no parens ever needed).
fn print_expr(ir: &ShaderIr, id: ExprId, parent_prec: u8) -> String {
    let e = ir.expr(id);
    match &e.kind {
        ExprKind::Num(n) => format!("{n}"),
        ExprKind::ColorLit(c) => {
            let b = |v: f32| ((v.clamp(0.0, 1.0) * 255.0).round()) as u8;
            if (c[3] - 1.0).abs() < 1e-6 {
                format!("#{:02X}{:02X}{:02X}", b(c[0]), b(c[1]), b(c[2]))
            } else {
                format!("#{:02X}{:02X}{:02X}{:02X}", b(c[0]), b(c[1]), b(c[2]), b(c[3]))
            }
        }
        ExprKind::Str(s) => format!("\"{s}\""),
        ExprKind::Input(i) => i.name().to_string(),
        ExprKind::Uniform(u) => ir.uniforms[*u].name.clone(),
        ExprKind::Texture(t) => ir.textures[*t].clone(),
        ExprKind::Let(l) => ir.lets[*l].0.clone(),
        ExprKind::Call { op, args } => {
            let parts: Vec<String> = args
                .iter()
                .map(|a| match &a.name {
                    Some(n) => format!("{n}: {}", print_expr(ir, a.value, 0)),
                    None => print_expr(ir, a.value, 0),
                })
                .collect();
            format!("{op}({})", parts.join(", "))
        }
        ExprKind::Binary(op, a, b) => {
            let prec = match op {
                BinOp::Add | BinOp::Sub => 1,
                BinOp::Mul | BinOp::Div => 2,
            };
            // The right operand of - and / needs parens at EQUAL precedence
            // (a - (b - c) != a - b - c), so it's printed one level tighter.
            let s = format!(
                "{} {} {}",
                print_expr(ir, *a, prec),
                op.symbol(),
                print_expr(ir, *b, prec + 1)
            );
            if prec < parent_prec { format!("({s})") } else { s }
        }
        ExprKind::Neg(a) => {
            let s = format!("-{}", print_expr(ir, *a, 3));
            if parent_prec > 3 { format!("({s})") } else { s }
        }
        ExprKind::Swizzle(a, sw) => format!("{}.{sw}", print_expr(ir, *a, 4)),
    }
}
