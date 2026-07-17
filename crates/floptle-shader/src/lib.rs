//! # floptle-shader
//!
//! Floptle's signature feature (ADR-0007): a single shader **IR** that is the
//! source of truth, presented to the artist as either a node graph (in-editor,
//! later phase) or as readable text (`.flsl`, opened in VSCode for AI-assisted
//! editing). The IR transpiles to WGSL, validated by naga — the same naga wgpu
//! embeds. See `docs/shader-system-proposal.md` + `docs/subsystems/shaders.md`.
//!
//! Modules:
//! - [`ir`]        : the in-memory shader IR (expression DAG, types, checking).
//! - [`text`]      : `.flsl` text format ⇄ IR (round-trippable parse/print).
//! - [`transpile`] : checked IR → a WGSL chunk for the renderer's concat seam.
//! - [`stdlib`]    : the op registry + the WGSL support library.
//! - [`graph`]     : the node-graph view ⇄ IR mapping (nodes/ports/wires +
//!   graph-edit mutations; the editor's canvas renders it).
//! - [`examples`]  : the built-in worked-example shaders seeded into projects.

pub mod examples;
pub mod graph;
pub mod ir;
pub mod preview;
pub mod stdlib;
pub mod text;
pub mod transpile;

pub use ir::{Blend, IrError, ShaderIr, Stage, Ty, Uniform};
pub use text::{parse, print, ParseError};
pub use transpile::{
    transpile_fragment, transpile_sdf, transpile_sky, validate, validate_module, CompiledFragment,
    CompiledSdf, CompiledSky, CompiledUi, TilingPack, TranspileError, WgslDiag,
};

/// File extension for the textual shader format ("FLoptle Shading Language").
pub const SHADER_TEXT_EXT: &str = "flsl";

/// Parse + type-check + transpile a Fragment-stage `.flsl` source in one call —
/// what the editor's hot-reload path runs. The error string is already
/// human-readable with a 1-based `line:col` prefix where one is known.
pub fn compile_fragment(src: &str) -> Result<CompiledFragment, String> {
    let ir = text::parse(src).map_err(|e| {
        let (l, c) = text::line_col(src, e.span.start);
        format!("{l}:{c}: {}", e.message)
    })?;
    let ck = ir::check(&ir).map_err(|errs| {
        errs.iter()
            .map(|e| {
                let (l, c) = text::line_col(src, e.span.start);
                format!("{l}:{c}: {}", e.message)
            })
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    if ir.stage == Some(Stage::Sdf) {
        return Err("this is an sdf shader — assign it to a Field Shape, not a mesh material".into());
    }
    if ir.stage == Some(Stage::Sky) {
        return Err("this is a sky shader — assign it to the Skybox, not a mesh material".into());
    }
    if ir.stage == Some(Stage::Ui) {
        return Err("this is a ui shader — assign it to a UI element, not a mesh material".into());
    }
    transpile::transpile_fragment(&ir, &ck).map_err(|e| {
        let (l, c) = text::line_col(src, e.span.start);
        format!("{l}:{c}: {}", e.message)
    })
}

/// Parse + type-check + transpile a Ui-stage `.flsl`, for a game-UI element's
/// custom face. Human-readable `line:col`-prefixed errors; a non-ui shader is
/// rejected with a hint.
pub fn compile_ui(src: &str) -> Result<transpile::CompiledUi, String> {
    let ir = text::parse(src).map_err(|e| {
        let (l, c) = text::line_col(src, e.span.start);
        format!("{l}:{c}: {}", e.message)
    })?;
    let ck = ir::check(&ir).map_err(|errs| {
        errs.iter()
            .map(|e| {
                let (l, c) = text::line_col(src, e.span.start);
                format!("{l}:{c}: {}", e.message)
            })
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    if ir.stage != Some(Stage::Ui) {
        return Err("not a ui shader — add `stage ui` and `output color = …`".into());
    }
    transpile::transpile_ui(&ir, &ck).map_err(|e| {
        let (l, c) = text::line_col(src, e.span.start);
        format!("{l}:{c}: {}", e.message)
    })
}

/// Parse + type-check + transpile a Sky-stage `.flsl`, for the Skybox node. Human-readable
/// `line:col`-prefixed errors. A non-sky shader is rejected with a hint.
pub fn compile_sky(src: &str) -> Result<transpile::CompiledSky, String> {
    let ir = text::parse(src).map_err(|e| {
        let (l, c) = text::line_col(src, e.span.start);
        format!("{l}:{c}: {}", e.message)
    })?;
    let ck = ir::check(&ir).map_err(|errs| {
        errs.iter()
            .map(|e| {
                let (l, c) = text::line_col(src, e.span.start);
                format!("{l}:{c}: {}", e.message)
            })
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    if ir.stage != Some(Stage::Sky) {
        return Err("not a sky shader — add `stage sky` and `output color = …`".into());
    }
    transpile::transpile_sky(&ir, &ck).map_err(|e| {
        let (l, c) = text::line_col(src, e.span.start);
        format!("{l}:{c}: {}", e.message)
    })
}

/// Parse + type-check only, either stage — the IDE's live diagnostic (a
/// squiggle shouldn't care whether the file is destined for a mesh or a
/// Field Shape). Errors carry a 1-based `line:col` prefix.
pub fn check_source(src: &str) -> Result<(), String> {
    let parsed = text::parse(src).map_err(|e| {
        let (l, c) = text::line_col(src, e.span.start);
        format!("{l}:{c}: {}", e.message)
    })?;
    ir::check(&parsed).map_err(|errs| {
        errs.iter()
            .map(|e| {
                let (l, c) = text::line_col(src, e.span.start);
                format!("{l}:{c}: {}", e.message)
            })
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    Ok(())
}

/// Parse + type-check an Sdf-stage `.flsl` source, returning the pieces the
/// editor caches (per-slot transpilation happens later, when the scene's
/// Field Shape slots are assigned). Human-readable `line:col`-prefixed errors.
pub fn check_sdf(src: &str) -> Result<(ShaderIr, ir::Checked), String> {
    let parsed = text::parse(src).map_err(|e| {
        let (l, c) = text::line_col(src, e.span.start);
        format!("{l}:{c}: {}", e.message)
    })?;
    let ck = ir::check(&parsed).map_err(|errs| {
        errs.iter()
            .map(|e| {
                let (l, c) = text::line_col(src, e.span.start);
                format!("{l}:{c}: {}", e.message)
            })
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    if parsed.stage != Some(Stage::Sdf) {
        return Err("this is a fragment shader — assign it on a mesh Material, not a Field Shape".into());
    }
    Ok((parsed, ck))
}

/// The seed content for the editor's "New Shader…" — a small worked example
/// that touches uniforms, textures and the stdlib.
pub const NEW_SHADER_TEMPLATE: &str = r#"// A Floptle shader (.flsl) — one source of truth, editable as text or graph.
// Assign it on a Material (Inspector -> Material -> Shader). Exposed `uniform`s
// become Inspector knobs; `texture` slots take drag-and-dropped textures.
shader myShader {
  stage fragment
  uniform tint: color = #FFFFFF
  uniform glow: float = 0 range(0, 4)

  let base = baseTexture() * tint * instanceColor
  let lit = litSurface(base.rgb)

  output color = vec4(lit + base.rgb * glow, base.a)
}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{check, ExprKind};

    const PLASMA: &str = r#"
// A swirling, palette-cycled plasma over UV space.
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
//@layout { warped: (120, 80), n: (320, 80), hue: (520, 96) }
"#;

    /// A Ui-stage shader compiles, its chunk validates against the UI pass's
    /// seam contract, params pack in slot order, and round-trips through print.
    #[test]
    fn ui_stage_compiles_and_validates() {
        const GAUGE: &str = r#"
shader gauge {
  stage ui
  uniform fill: float = 0.5 range(0, 1)
  uniform hot: color = #FF7A2E

  let p = uv * 2 - 1
  let ring = smoothstep(0.95, 0.9, length(p)) * smoothstep(0.6, 0.65, length(p))
  let lit = step(1 - uv.y, fill)
  output color = vec4(hot.rgb * lit + vec3(0.15, 0.16, 0.2) * (1 - lit), ring) * instanceColor
}
"#;
        let compiled = compile_ui(GAUGE).expect("compiles");
        assert_eq!(compiled.uniforms.len(), 2);
        let prelude = format!("{}{}", transpile::UI_TEST_PRELUDE, transpile::UI_FIELD_SHIM);
        transpile::validate(&prelude, &compiled.chunk)
            .unwrap_or_else(|e| panic!("naga rejects: {} in:\n{}", e.message, compiled.chunk));
        let bytes = compiled.pack_params(&|name| (name == "fill").then_some([0.8, 0.0, 0.0, 0.0]));
        assert_eq!(bytes.len(), 32);
        assert_eq!(f32::from_le_bytes(bytes[0..4].try_into().unwrap()), 0.8);
        // Defaults fill unset slots (the color's red lane).
        assert!(f32::from_le_bytes(bytes[16..20].try_into().unwrap()) > 0.9);
        // A ui shader is rejected where a mesh material is expected, with a hint.
        assert!(compile_fragment(GAUGE).unwrap_err().contains("ui shader"));
        // And it round-trips through the printer keeping its stage.
        let ir = parse(GAUGE).expect("parses");
        let printed = print(&ir);
        assert_eq!(parse(&printed).unwrap().stage, Some(Stage::Ui));
    }

    #[test]
    fn parses_the_plasma_example() {
        let ir = parse(PLASMA).expect("parses");
        assert_eq!(ir.name, "plasma");
        assert_eq!(ir.stage, Some(Stage::Fragment));
        assert_eq!(ir.uniforms.len(), 2);
        assert_eq!(ir.uniforms[0].name, "speed");
        assert_eq!(ir.uniforms[0].range, Some((0.0, 2.0)));
        assert!(ir.uniforms[1].is_color);
        assert_eq!(ir.textures, vec!["ramp".to_string()]);
        assert_eq!(ir.lets.len(), 3);
        assert!(ir.outputs.contains_key("color"));
        assert_eq!(ir.layout.len(), 3);
        assert_eq!(ir.layout["warped"], (120.0, 80.0));
    }

    #[test]
    fn round_trips_through_print() {
        let ir = parse(PLASMA).expect("parses");
        let printed = print(&ir);
        let reparsed = parse(&printed)
            .unwrap_or_else(|e| panic!("reprint parses: {} in:\n{printed}", e.message));
        assert!(
            ir.same_shader(&reparsed),
            "round-trip changed the shader:\n{printed}"
        );
        assert_eq!(reparsed.layout, ir.layout, "layout survives the round trip");
        // And printing is a fixpoint: print(parse(print(x))) == print(x).
        assert_eq!(print(&reparsed), printed);
    }

    #[test]
    fn checks_and_transpiles_to_valid_wgsl() {
        let compiled = compile_fragment(PLASMA).expect("compiles");
        assert_eq!(compiled.textures, vec!["ramp".to_string()]);
        assert_eq!(compiled.uniforms.len(), 2);
        // 2 uniform slots + 2 tiling lanes for the one texture slot.
        assert_eq!(compiled.param_block_size(), 64);
        assert!(compiled.chunk.contains("fn flsl_surface"));
        assert!(compiled.chunk.contains("@fragment"));
        transpile::validate(transpile::TEST_PRELUDE, &compiled.chunk)
            .unwrap_or_else(|e| panic!("naga rejects: {} (chunk line {:?})\n{}", e.message, e.chunk_line, compiled.chunk));
    }

    #[test]
    fn the_new_shader_template_compiles() {
        let compiled = compile_fragment(NEW_SHADER_TEMPLATE).expect("template compiles");
        transpile::validate(transpile::TEST_PRELUDE, &compiled.chunk)
            .unwrap_or_else(|e| panic!("naga rejects the template: {}", e.message));
    }

    #[test]
    fn engine_hooks_and_generics_emit_valid_wgsl() {
        let src = r#"
shader hooks {
  stage fragment
  blend additive
  uniform amount: float = 1

  let p = worldPos + vec3(0, sin(time), 0)
  let glow = smoothstep(0, 1, 1 - fieldDistance(p))
  let sh = sunShadow(worldPos, normal)
  let ao = sdfAo(worldPos, normal)
  let fogged = applyFog(litSurface(vec3(0.5) * sh), worldPos)

  output color = vec4(fogged * ao + glow * amount, 0.5)
}
"#;
        let compiled = compile_fragment(src).expect("compiles");
        assert_eq!(compiled.blend, Blend::Additive);
        transpile::validate(transpile::TEST_PRELUDE, &compiled.chunk)
            .unwrap_or_else(|e| panic!("naga rejects: {} (chunk line {:?})\n{}", e.message, e.chunk_line, compiled.chunk));
    }

    #[test]
    fn type_errors_are_caught_with_spans() {
        // cross() wants vec3s; uv is a vec2.
        let src = r#"
shader bad {
  stage fragment
  let x = cross(uv, uv)
  output color = vec4(x, 1)
}
"#;
        let ir = parse(src).expect("parses");
        let errs = check(&ir).expect_err("type error");
        assert!(errs[0].message.contains("cross"), "{}", errs[0].message);
        assert!(errs[0].span.start > 0, "error carries a span");

        // Unknown names fail at parse (they can't resolve).
        assert!(parse("shader s { stage fragment\noutput color = vec4(nope, 0, 0, 1) }").is_err());
        // Missing output.
        let ir = parse("shader s { stage fragment\nlet a = 1 + 1 }").expect("parses");
        let errs = check(&ir).expect_err("missing output");
        assert!(errs.iter().any(|e| e.message.contains("output color")));
    }

    #[test]
    fn sdf_stage_parses_and_checks() {
        let src = r#"
shader wob {
  stage sdf
  uniform wobble: float = 0.2

  let p = twist(worldPos, wobble * sin(time))
  let d = smoothMin(sphere(p, radius: 1), box(p, vec3(0.8, 0.4, 0.8)), k: 0.3)

  output sdf = d
  output color = vec3(0.8, 0.4, 0.9)
}
"#;
        let ir = parse(src).expect("parses");
        assert_eq!(ir.stage, Some(Stage::Sdf));
        check(&ir).expect("checks");
        // Fragment-only inputs are rejected in sdf shaders.
        let bad = r#"
shader b {
  stage sdf
  output sdf = sphere(worldPos) + uv.x
}
"#;
        let ir = parse(bad).expect("parses");
        let errs = check(&ir).expect_err("uv is fragment-only");
        assert!(errs[0].message.contains("uv"));
    }

    #[test]
    fn defaults_named_args_and_scalar_splats() {
        // Omitted optional args take signature defaults; scalars splat into
        // vector slots (clamp needs same shapes in WGSL).
        let src = r#"
shader s {
  stage fragment
  let n = fbm(uv)
  let c = clamp(vec3(n, n, n) * 2 - 0.3, 0, 1)
  output color = vec4(c, 1)
}
"#;
        let compiled = compile_fragment(src).expect("compiles");
        assert!(compiled.chunk.contains("flsl_fbm2"), "type-directed overload");
        transpile::validate(transpile::TEST_PRELUDE, &compiled.chunk)
            .unwrap_or_else(|e| panic!("naga rejects: {}\n{}", e.message, compiled.chunk));
    }

    #[test]
    fn bad_wgsl_maps_back_to_chunk_lines() {
        // Force a naga failure by validating a chunk with a bogus symbol; the
        // diagnostic should carry a chunk-relative line.
        let diag = transpile::validate(transpile::TEST_PRELUDE, "fn boom() -> f32 {\n    return no_such_symbol;\n}\n")
            .expect_err("invalid");
        assert!(diag.chunk_line.is_some());
    }

    #[test]
    fn graph_facing_arena_is_stable() {
        // The parser resolves idents to typed references (a graph editor's
        // nodes), not raw strings.
        let ir = parse(PLASMA).expect("parses");
        let uses_uniform = ir
            .exprs
            .iter()
            .any(|e| matches!(e.kind, ExprKind::Uniform(0)));
        assert!(uses_uniform, "speed resolves to a Uniform reference");
    }
}
