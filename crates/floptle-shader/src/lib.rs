//! # floptle-shader
//!
//! Floptle's signature feature: a single shader **IR** that is the
//! source of truth, presented to the artist as either a node graph (in-editor,
//! later phase) or as readable text (`.flsl`, opened in VSCode for AI-assisted
//! editing). The IR transpiles to WGSL, validated by naga — the same naga wgpu
//! embeds. See `docs/subsystems/shaders.md` + `docs/subsystems/shaders.md`.
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
    transpile_fragment, transpile_post, transpile_sdf, transpile_sky, validate, validate_module,
    CompiledFragment, CompiledPost, CompiledSdf, CompiledSky, CompiledUi, TilingPack,
    TranspileError, WgslDiag,
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
    if ir.stage == Some(Stage::Post) {
        return Err(
            "this is a post shader — add it to the PostProcess node's screen shaders, not a mesh material"
                .into(),
        );
    }
    transpile::transpile_fragment(&ir, &ck).map_err(|e| {
        let (l, c) = text::line_col(src, e.span.start);
        format!("{l}:{c}: {}", e.message)
    })
}

/// Parse + type-check + transpile a UI-stage `.flsl`, for a game-UI element's
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

/// Parse + type-check + transpile a Post-stage `.flsl`, for a full-screen pass
/// on the PostProcess node. Human-readable `line:col`-prefixed errors; a shader
/// of any other stage is rejected with a hint.
pub fn compile_post(src: &str) -> Result<transpile::CompiledPost, String> {
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
    if ir.stage != Some(Stage::Post) {
        return Err("not a post shader — add `stage post` and `output color = …`".into());
    }
    transpile::transpile_post(&ir, &ck).map_err(|e| {
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

/// Parse + type-check an SDF-stage `.flsl` source, returning the pieces the
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

    /// A UI-stage shader compiles, its chunk validates against the UI pass's
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

    /// the inverse trig, so a shader can read an angle and not
    /// only produce one. Without `atan2` nothing could work in polar coordinates
    /// — no radial wipe, no cooldown dial, no swirl, no skyline laid out around
    /// a horizon — and the reported workaround was a diamond angle whose spacing
    /// is not proportional to the real one.
    #[test]
    fn a_shader_can_read_an_angle() {
        const DIAL: &str = r#"
shader dial {
  stage ui
  uniform fill: float = 0.25 range(0, 1)

  let p = uv * 2 - 1
  // The whole point: a coordinate that runs once around, evenly.
  let az = atan2(p.y, p.x) / 6.2831853 + 0.5
  // …and wrapping it at something other than 1, which `fract` cannot do.
  let spokes = mod(az * 12, 1)
  let tilt = acos(clamp(p.y, -1, 1)) + asin(clamp(p.x, -1, 1)) + atan(p.x)
  let falloff = exp2(-length(p) * 4) + log2(length(p) + 1)
  output color = vec4(vec3(step(az, fill) * spokes * falloff + tilt * 0), 1.0)
}
"#;
        let compiled = compile_ui(DIAL).expect("a polar shader compiles");
        let prelude = format!("{}{}", transpile::UI_TEST_PRELUDE, transpile::UI_FIELD_SHIM);
        transpile::validate(&prelude, &compiled.chunk)
            .unwrap_or_else(|e| panic!("naga rejects: {} in:\n{}", e.message, compiled.chunk));
        assert!(compiled.chunk.contains("atan2("), "atan2 did not reach the WGSL");
        // `mod` is floored, not WGSL's `%` — `%` truncates toward zero and so
        // returns a negative remainder for negative x, which is half of every
        // angle atan2 hands back.
        assert!(compiled.chunk.contains("flsl_mod1("), "mod must use the floored support fn");
        assert!(
            stdlib::SUPPORT_WGSL.contains("fn flsl_mod1"),
            "the support fn the emitter names must exist"
        );
        // …and `mod` is a call, not a keyword the lexer swallows.
        assert!(parse(DIAL).is_ok(), "the parser must accept `mod` as a name");
    }

    /// Every arity of `mod` the generic signature admits has a support function
    /// behind it. `FnByLanes` picks the name from the resolved type, so a
    /// missing one is a WGSL compile error in somebody's shader rather than
    /// anything Rust would catch.
    #[test]
    fn mod_has_a_support_fn_at_every_width() {
        for lanes in 1..=4 {
            assert!(
                stdlib::SUPPORT_WGSL.contains(&format!("fn flsl_mod{lanes}(")),
                "no flsl_mod{lanes}"
            );
        }
    }

    /// A UI-stage shader can sample the element'S own image via `baseTexture`
    /// (bound at group(1) like a material texture) — both with an explicit uv and
    /// with the default rect UV — and the result validates against the UI seam.
    #[test]
    fn ui_shader_samples_element_image() {
        const DUO: &str = r#"
shader duo {
  stage ui
  uniform dark: color = #101020
  uniform light: color = #FFE0A0
  let src = baseTexture(uv)
  let l = (src.r + src.g + src.b) * 0.3333
  output color = vec4(mix(dark.rgb, light.rgb, l), src.a) * instanceColor
}
"#;
        let compiled = compile_ui(DUO).expect("compiles");
        let prelude = format!("{}{}", transpile::UI_TEST_PRELUDE, transpile::UI_FIELD_SHIM);
        transpile::validate(&prelude, &compiled.chunk)
            .unwrap_or_else(|e| panic!("naga rejects: {} in:\n{}", e.message, compiled.chunk));
        // `baseTexture()` with no arg uses the element's 0..1 rect UV.
        let dflt = compile_ui("shader d { stage ui\n  output color = baseTexture() }")
            .expect("default baseTexture compiles");
        assert!(dflt.chunk.contains("flsl_uv(in)"), "default UV path: {}", dflt.chunk);
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

    /// A texture slot can carry its own image, so a shader shows its real art
    /// in the graph's previews before any material exists. The path rides the
    /// slot through print/parse and out to the compiled binding order.
    #[test]
    fn a_texture_slot_carries_its_default_image() {
        const SRC: &str = "shader t {\n  stage fragment\n  texture ramp = \"art/ramp.png\"\n  \
                           texture mask\n  output color = sample(ramp, uv) * sample(mask, uv)\n}\n";
        let ir = parse(SRC).expect("parses");
        assert_eq!(ir.textures, vec!["ramp".to_string(), "mask".to_string()]);
        assert_eq!(ir.texture_defaults.get("ramp").map(String::as_str), Some("art/ramp.png"));
        assert!(!ir.texture_defaults.contains_key("mask"), "a bare slot has no default");

        let printed = print(&ir);
        assert!(printed.contains("texture ramp = \"art/ramp.png\""), "printed:\n{printed}");
        let reparsed = parse(&printed).expect("reprint parses");
        assert!(ir.same_shader(&reparsed), "the default survives the round trip:\n{printed}");
        assert_eq!(print(&reparsed), printed);

        // …and reaches the host that binds it, aligned with the slot order.
        let compiled = compile_fragment(SRC).expect("compiles");
        assert_eq!(compiled.texture_defaults, vec![Some("art/ramp.png".to_string()), None]);

        // A slot renamed in the graph keeps its image; deleting it drops it.
        let mut ir = ir;
        graph::rename_texture(&mut ir, "ramp", "gradient").expect("renames");
        assert_eq!(ir.texture_defaults.get("gradient").map(String::as_str), Some("art/ramp.png"));
        graph::set_texture_default(&mut ir, "gradient", None).expect("clears");
        assert!(ir.texture_defaults.is_empty());
    }

    /// **A slot can say it holds numbers.** `data` (or `linear`) after a
    /// slot, with or without a default image, marks it: it survives a print
    /// and a parse, a rename in the graph and a copy into another shader, and
    /// reaches the host that binds it, aligned with the slot order, in a
    /// material shader and a screen shader alike. `data` stays an ordinary
    /// name everywhere else.
    #[test]
    fn a_texture_slot_can_be_declared_data() {
        const SRC: &str = "shader t {\n  stage fragment\n  texture flow = \"art/flow.png\" data\n  \
                           texture tint\n  texture mask linear\n  let data = sample(flow, uv)\n  \
                           output color = data.rgb * sample(tint, uv).rgb * sample(mask, uv).r\n}\n";
        let ir = parse(SRC).expect("parses");
        assert_eq!(ir.texture_data.iter().map(String::as_str).collect::<Vec<_>>(), ["flow", "mask"]);
        assert_eq!(ir.texture_defaults.get("flow").map(String::as_str), Some("art/flow.png"));

        let printed = print(&ir);
        assert!(printed.contains("texture flow = \"art/flow.png\" data\n"), "printed:\n{printed}");
        assert!(printed.contains("texture mask data\n") && printed.contains("texture tint\n"), "printed:\n{printed}");
        let reparsed = parse(&printed).expect("reprint parses");
        assert!(ir.same_shader(&reparsed), "the flag survives the round trip:\n{printed}");
        let mut plain = reparsed.clone();
        plain.texture_data.clear();
        assert!(!ir.same_shader(&plain), "a shader that lost its data flag is the same shader");

        assert_eq!(compile_fragment(SRC).expect("compiles").texture_data, [true, false, true]);
        let post = compile_post("shader p {\n  stage post\n  texture lut data\n  output color = sample(lut, uv).rgb\n}\n")
            .expect("compiles");
        assert_eq!(post.texture_data, [true]);

        let mut ir = ir;
        graph::rename_texture(&mut ir, "flow", "current").expect("renames");
        assert!(ir.texture_data.contains("current") && !ir.texture_data.contains("flow"));
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

    /// `litPbr` and the screen derivatives transpile to valid WGSL, the
    /// scorch example the shader docs give included.
    #[test]
    fn lit_pbr_and_the_derivatives_compile() {
        let src = "shader scorch {\n  stage fragment\n  let scorch = smoothstep(0.4, 0.6, noise(worldPos * 2))\n  \
                   let edge = smoothstep(0, fwidth(uv.x), uv.x - 0.5) + dpdx(uv.y) + dpdy(uv.x)\n  \
                   let lit = litPbr(baseTexture().rgb * mix(1, 0.2, scorch)) + litPbr(vec3(edge), normal)\n  \
                   output color = vec4(lit, 1)\n}\n";
        let compiled = compile_fragment(src).expect("compiles");
        assert!(compiled.chunk.contains("surface_pbr(in, front,"), "{}", compiled.chunk);
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

    /// The new stdlib batch — `lerp`, `inverseLerp`, `remap`, `rotate2D` and
    /// `refract` — each exercised at a width naga can catch a lane mismatch
    /// at (scalar for the FnByLanes ones, vec3/vec2 for the rest), so a typo
    /// in a support function's signature fails here instead of at the first
    /// artist who reaches for the node.
    #[test]
    fn the_lerp_batch_emits_valid_wgsl() {
        let src = r#"
shader lerp_batch {
  stage fragment
  uniform t: float = 0.5

  let a = lerp(0, 10, t)
  let b = inverseLerp(0, 10, a)
  let c = remap(b, 0, 1, -1, 1)
  let spun = rotate2D(uv, 3.14159)
  let bent = refract(normalize(worldPos), normal, 0.75)

  output color = vec4(vec3(c) + spun.x + bent, 1)
}
"#;
        let compiled = compile_fragment(src).expect("compiles");
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

#[cfg(test)]
mod offset_tests {
    /// `output offset` survives print → parse, so a graph edit (which
    /// re-prints the file) cannot drop it.
    #[test]
    fn an_offset_survives_a_print_and_a_parse() {
        let src = "shader s {\n  stage fragment\n  output color = vec3(1, 0, 0)\n  output offset = normal * 0.1\n}\n";
        let ir = crate::parse(src).expect("parses");
        let again = crate::parse(&crate::print(&ir)).expect("re-parses");
        assert!(again.outputs.contains_key("offset"), "printed:\n{}", crate::print(&ir));
        assert!(crate::compile_fragment(src).expect("compiles").displaced);
        assert!(!crate::compile_fragment("shader s {\n  stage fragment\n  output color = vec3(1, 0, 0)\n}\n")
            .expect("compiles")
            .displaced);
    }

    /// The offset runs per vertex: what only a lit pixel has is refused, with
    /// a reason, including when it arrives through a `let`.
    #[test]
    fn an_offset_that_reads_a_pixel_only_value_is_refused() {
        for (body, says) in [
            ("output offset = viewDir * 0.1", "viewDir"),
            ("let v = lightDir\n  output offset = v * 0.1", "lightDir"),
            ("texture t\n  output offset = sampleTriplanar(t, objectPos, normal).rgb", "sampleTriplanar"),
        ] {
            let src = format!("shader s {{\n  stage fragment\n  output color = vec3(1)\n  {body}\n}}\n");
            let err = crate::compile_fragment(&src).expect_err(body);
            assert!(err.contains(says) && err.contains("offset"), "{body}: {err}");
        }
        // A `let` the colour reads may use anything; only the offset's own
        // inputs are restricted.
        let ok = "shader s {\n  stage fragment\n  let shade = dot(normal, viewDir)\n  output color = vec3(shade)\n  output offset = normal * 0.1\n}\n";
        crate::compile_fragment(ok).expect("a per-pixel let the offset does not read is fine");
    }

    /// **A height map can move a surface.** `sample` in an offset reads the
    /// slot's largest level, since a vertex has no derivatives to choose a
    /// smaller one with, so the vertex entry point carries an explicit-level
    /// read and the pixel one keeps its ordinary sample.
    #[test]
    fn an_offset_samples_a_height_map_at_its_largest_level() {
        let src = "shader s {\n  stage fragment\n  texture height\n  output color = sample(height, uv)\n  \
                   output offset = normal * sample(height, uv).r * 0.5\n}\n";
        let c = crate::compile_fragment(src).expect("an offset may sample a texture");
        assert!(c.displaced);
        let at = c.chunk.find("fn flsl_offset(").expect("the offset function");
        let off = &c.chunk[at..at + c.chunk[at..].find("\n}").expect("its end")];
        assert!(off.contains("flsl_tiled_sample_level(flsl_tex0"), "the offset does not read the slot:\n{off}");
        assert!(c.chunk.contains("flsl_tiled_sample(flsl_tex0"), "the pixel stage lost its ordinary sample");
    }

    /// **A water shader reads the water's own waves.** `waterSwell` moves
    /// the surface in an offset and `waterSwellNormal` lights it in the
    /// colour: the offset takes the normal in the node's space, the colour
    /// in the lit one. A shader that calls either carries the swell's lanes
    /// after its own parameters, which the engine fills from the water the
    /// surface is on; one that calls neither is the size it always was.
    #[test]
    fn a_water_shader_reads_the_swell_and_others_do_not_pay_for_it() {
        let lanes = crate::transpile::SWELL_LANES;
        let src = "shader sea {\n  stage fragment\n  uniform tint: color = #204060\n  texture foam\n  \
                   let n = waterSwellNormal(objectPos)\n  output color = vec4(litSurface(tint.rgb, n), 1)\n  \
                   output offset = waterSwell(objectPos) + waterSwellNormal(objectPos) * 0.0\n}\n";
        let c = crate::compile_fragment(src).expect("a water shader compiles");
        // One uniform and one slot's two tiling lanes come first.
        assert_eq!(c.swell_offset, Some(3 * 16));
        assert_eq!(c.param_block_size(), ((3 + lanes) * 16) as u64);
        assert!(c.chunk.contains(&format!("swell: array<vec4<f32>, {lanes}>")), "{}", c.chunk);
        let at = c.chunk.find("fn flsl_offset(").expect("the offset function");
        let off = &c.chunk[at..at + c.chunk[at..].find("\n}").expect("its end")];
        assert!(off.contains("flsl_swell_offset(opos)") && off.contains("flsl_swell_normal_local(opos, onrm)"), "{off}");
        assert!(c.chunk.contains("flsl_swell_normal(in.lpos, in.normal)"), "the colour lights the swell's normal in its own space:\n{}", c.chunk);
        // The lanes land after the shader's own, and nowhere else.
        let mut params = c.pack_params(&|_| None, &|_| None);
        let before = params.clone();
        let mut block = [[0.0f32; 4]; crate::transpile::SWELL_LANES];
        block[0] = [8.0, 0.0, 0.0, 0.0];
        block[lanes - 1] = [1.0, 2.0, 3.0, 4.0];
        c.write_swell(&mut params, &block);
        assert_eq!(params[..48], before[..48]);
        assert_eq!(params[48..52], 8.0f32.to_le_bytes());
        assert_eq!(params[params.len() - 4..], 4.0f32.to_le_bytes());

        // No swell op, no lanes: the block an older shader always had.
        let plain = crate::compile_fragment("shader s {\n  stage fragment\n  uniform tint: color = #204060\n  output color = tint\n}\n").unwrap();
        assert_eq!((plain.swell_offset, plain.param_block_size()), (None, 16));
        assert!(!plain.chunk.contains("swell"));
        let mut untouched = plain.pack_params(&|_| None, &|_| None);
        plain.write_swell(&mut untouched, &block);
        assert_eq!(untouched, plain.pack_params(&|_| None, &|_| None));

        // A sky has no water under it.
        let err = crate::compile_sky("shader s {\n  stage sky\n  output color = waterSwell(vec3(1, 2, 3))\n}\n").expect_err("a sky shader");
        assert!(err.contains("waterSwell"), "{err}");
    }
}

#[cfg(test)]
mod screen_pass_tests {
    fn naga(compiled: &crate::CompiledPost) {
        let prelude = format!("{}\n{}", crate::transpile::POST_PRELUDE, crate::transpile::POST_FIELD_SHIM);
        crate::transpile::validate(&prelude, &compiled.chunk)
            .unwrap_or_else(|e| panic!("naga rejects: {}\n{}", e.message, compiled.chunk));
    }

    fn f32s(bytes: &[u8]) -> Vec<f32> {
        bytes.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect()
    }

    /// **A screen shader reads a picture.** A slot it declares is bound in its
    /// own group beside its uniforms, with one sampler of its own, and both
    /// `sample` and `sampleTriplanar` reach it.
    #[test]
    fn a_screen_shader_declares_and_samples_texture_slots() {
        let src = "shader caustics {\n  stage post\n  uniform strength: float = 0.5\n  \
                   texture pattern = \"textures/caustic.png\"\n  texture ramp\n  \
                   let world = cameraPosition() + sceneRay() * sceneDepth()\n  \
                   let c = sample(pattern, uv * 4).rgb + sampleTriplanar(ramp, world, vec3(0, 1, 0)).rgb\n  \
                   output color = sceneColor().rgb + c * strength\n}\n";
        let c = crate::compile_post(src).expect("a screen shader may declare textures");
        assert_eq!(c.textures, ["pattern", "ramp"]);
        assert_eq!(c.texture_defaults, [Some("textures/caustic.png".to_string()), None]);
        assert!(c.chunk.contains("@group(2) @binding(1) var flsl_tex0: texture_2d<f32>;"), "{}", c.chunk);
        assert!(c.chunk.contains("@group(2) @binding(4) var flsl_samp1: sampler;"), "{}", c.chunk);
        naga(&c);
        // One vec4 for the uniform, then two lanes of tiling a slot: the
        // image as it is, and a triplanar tile one unit wide.
        let p = f32s(&c.pack_params(&|_| None));
        assert_eq!(p.len(), (1 + 2 * 2) * 4);
        assert_eq!(p[0], 0.5);
        assert_eq!(&p[4..12], &[1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 4.0]);
        // More slots than a pass can bind is said, not left to the pipeline.
        let many: String = (0..9).map(|i| format!("  texture t{i}\n")).collect();
        let err = crate::compile_post(&format!("shader s {{\n  stage post\n{many}  output color = sceneColor()\n}}\n"))
            .expect_err("nine slots");
        assert!(err.contains("at most 8 texture slots"), "{err}");
    }

    /// **A screen shader reads a list a script fills.** `uniform bodies:
    /// vec4[8]` is eight elements after the uniforms in the param block, and
    /// `bodies[i]` reads one: any number, rounded down and held in range.
    #[test]
    fn a_screen_shader_reads_a_uniform_array() {
        let src = "shader bodies {\n  stage post\n  uniform gain: float = 2\n  uniform bodies: vec4[8]\n  \
                   uniform levels: float[3]\n  \
                   let near = bodies[0].xyz - bodies[uv.x * 8].xyz\n  \
                   output color = sceneColor().rgb * levels[2] * gain + near\n}\n";
        let c = crate::compile_post(src).expect("a screen shader may declare arrays");
        assert_eq!(c.arrays.len(), 2);
        assert!(c.chunk.contains("a0: array<vec4<f32>, 8>"), "{}", c.chunk);
        assert!(c.chunk.contains("P.a0[u32(clamp((in.uv.x * 8.0), 0.0, 7.0))].xyz"), "{}", c.chunk);
        assert!(c.chunk.contains("P.a1[u32(clamp(2.0, 0.0, 2.0))].x"), "{}", c.chunk);
        naga(&c);

        let bodies = [[1.0, 2.0, 3.0, 4.0], [5.0, 6.0, 7.0, 8.0]];
        let levels = [[9.0, 0.0, 0.0, 0.0]; 5];
        let p = f32s(&c.pack_params_with(&|_| None, &|name| match name {
            "bodies" => Some(&bodies[..]),
            "levels" => Some(&levels[..]),
            _ => None,
        }));
        assert_eq!(p.len(), (1 + 8 + 3) * 4, "one uniform, eight bodies, three levels");
        assert_eq!(&p[4..12], &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]);
        assert!(p[12..36].iter().all(|v| *v == 0.0), "elements nobody wrote are zero");
        assert_eq!(&p[36..], &[9.0, 0.0, 0.0, 0.0, 9.0, 0.0, 0.0, 0.0, 9.0, 0.0, 0.0, 0.0], "a longer list is cut");

        // The text round-trips, so a graph edit (which re-prints) keeps it.
        let ir = crate::parse(src).expect("parses");
        let printed = crate::print(&ir);
        assert!(printed.contains("uniform bodies: vec4[8]") && printed.contains("bodies[uv.x * 8]"), "{printed}");
        assert!(crate::parse(&printed).expect("re-parses").same_shader(&ir), "{printed}");
        // And the graph opens it.
        let ck = crate::ir::check(&ir).expect("checks");
        assert!(crate::graph::build_view(&ir, Some(&ck)).iter().any(|n| n.op_label() == "bodies[ ]"));
    }

    /// What an array cannot be is said where it is written.
    #[test]
    fn a_bad_uniform_array_is_refused_with_a_reason() {
        for (body, says) in [
            ("uniform a: vec4[0]\n  output color = sceneColor()", "1 to 64"),
            ("uniform a: vec4[65]\n  output color = sceneColor()", "1 to 64"),
            ("uniform a: color[4]\n  output color = sceneColor()", "not color"),
            ("uniform a: vec4[4] = 1\n  output color = sceneColor()", "no default"),
            ("uniform a: vec4[4]\n  output color = sceneColor() * a", "is an array"),
            ("uniform a: vec4[4]\n  output color = a[uv]", "one number"),
            ("uniform a: vec4[64]\n  uniform b: vec4[64]\n  uniform c: vec4[64]\n  uniform d: vec4[64]\n  \
              uniform e: vec4[1]\n  output color = sceneColor()", "at most 256"),
        ] {
            let src = format!("shader s {{\n  stage post\n  {body}\n}}\n");
            let err = crate::compile_post(&src).expect_err(body);
            assert!(err.contains(says), "{body}: {err}");
        }
        let err = crate::compile_fragment("shader s {\n  stage fragment\n  uniform a: vec4[4]\n  output color = a[0]\n}\n")
            .expect_err("arrays are for screen shaders");
        assert!(err.contains("screen shaders"), "{err}");
    }

    /// `sceneWaterDepth()` is a screen op: a number, with nothing to pass it.
    #[test]
    fn a_screen_shader_reads_how_deep_the_camera_is() {
        let c = crate::compile_post(
            "shader under {\n  stage post\n  output color = mix(sceneColor().rgb, vec3(0, 0.2, 0.3), saturate(sceneWaterDepth()))\n}\n",
        )
        .expect("compiles");
        assert!(c.chunk.contains("flsl_post_water_depth()"), "{}", c.chunk);
        naga(&c);
        let err = crate::compile_fragment("shader s {\n  stage fragment\n  output color = vec3(sceneWaterDepth())\n}\n")
            .expect_err("post only");
        assert!(err.contains("sceneWaterDepth"), "{err}");
    }
}
