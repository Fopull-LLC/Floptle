//! Built-in `stage ui` effect shaders — the one-click juice for UI elements.
//!
//! Each is a small, self-contained `.flsl` that draws OVER an element's shape and
//! (thanks to `transpile_ui`) is automatically clipped to the element's rounded
//! corners. They're seeded into every project's `shaders/examples/ui/` folder
//! (like the material examples) and surfaced in the UI-element inspector's
//! ✨ Effect dropdown. Editing the `.flsl` hot-reloads; the same file is a worked
//! example a designer can copy to author their own.
//!
//! UI shaders only see `uv` (0..1 across the rect), `time` (seconds) and
//! `instanceColor` (the element tint × opacity), plus their own uniforms — no
//! textures, no `palette`/`hueShift` (those are fragment/sky-only). Effects that
//! must render OUTSIDE the rect (drop shadow) are built-in `ShapeSpec` features
//! instead — see `floptle_ui::ShadowSpec`.

/// `(menu label, file stem, source)` for each built-in UI effect. The dropdown
/// and the seeder both read this list, so they never drift.
pub(crate) const UI_EFFECTS: &[(&str, &str, &str)] = &[
    (
        "Outline (pulse)",
        "ui_outline",
        r#"// Glowing edge outline that gently pulses. Sits just inside the rect edge
// and follows the element's rounded corners.
shader ui_outline {
  stage ui
  uniform tint: color = #55E0FF
  uniform width: float = 0.06
  uniform speed: float = 2.0
  uniform strength: float = 0.9
  let edge = min(min(uv.x, 1 - uv.x), min(uv.y, 1 - uv.y))
  let ring = 1 - smoothstep(0.0, width, edge)
  let pulse = 0.65 + 0.35 * sin(time * speed)
  output color = vec4(tint.rgb, ring * pulse * strength) * instanceColor
}
"#,
    ),
    (
        "Gradient fill",
        "ui_gradient",
        r#"// A two-color linear gradient fill. `angle` is in degrees (90 = top→bottom).
shader ui_gradient {
  stage ui
  uniform top: color = #3A7BFF
  uniform bottom: color = #0B1B33
  uniform angle: float = 90.0
  let a = angle * 0.0174533
  let dir = vec2(cos(a), sin(a))
  let t = saturate(dot(uv - vec2(0.5, 0.5), dir) + 0.5)
  output color = vec4(mix(top.rgb, bottom.rgb, t), 1.0) * instanceColor
}
"#,
    ),
    (
        "Gloss sweep",
        "ui_gloss",
        r#"// A diagonal highlight that sweeps across — the classic glass shine.
shader ui_gloss {
  stage ui
  uniform tint: color = #FFFFFF
  uniform speed: float = 0.5
  uniform width: float = 0.16
  uniform strength: float = 0.55
  let sweep = fract(time * speed) * 1.6 - 0.3
  let pos = (uv.x + uv.y) * 0.5
  let streak = 1 - smoothstep(0.0, width, abs(pos - sweep))
  output color = vec4(tint.rgb, streak * strength) * instanceColor
}
"#,
    ),
    (
        "Glow (edge)",
        "ui_glow",
        r#"// A soft glow that brightens toward the edges and pulses — good for
// "active"/selected states.
shader ui_glow {
  stage ui
  uniform tint: color = #FF5FA2
  uniform speed: float = 2.0
  uniform strength: float = 0.7
  let c = distance(uv, vec2(0.5, 0.5))
  let ring = smoothstep(0.15, 0.62, c)
  let pulse = 0.5 + 0.5 * sin(time * speed)
  output color = vec4(tint.rgb, ring * pulse * strength) * instanceColor
}
"#,
    ),
    (
        "Wobble shimmer",
        "ui_wobble",
        r#"// A wobbling interference shimmer — a lively, jelly-ish overlay.
shader ui_wobble {
  stage ui
  uniform tint: color = #7CF6C4
  uniform amp: float = 0.5
  uniform freq: float = 7.0
  uniform speed: float = 3.0
  let wx = sin(uv.x * freq + time * speed) * 0.5 + 0.5
  let wy = sin(uv.y * freq - time * speed * 0.8) * 0.5 + 0.5
  output color = vec4(tint.rgb, wx * wy * amp) * instanceColor
}
"#,
    ),
    (
        "Scanlines (CRT)",
        "ui_scanline",
        r#"// Scrolling CRT scanlines with a subtle flicker.
shader ui_scanline {
  stage ui
  uniform tint: color = #34FF9E
  uniform count: float = 140.0
  uniform strength: float = 0.35
  uniform speed: float = 6.0
  let line = sin(uv.y * count + time * speed) * 0.5 + 0.5
  let flick = 0.92 + 0.08 * sin(time * 24.0)
  output color = vec4(tint.rgb, line * strength * flick) * instanceColor
}
"#,
    ),
    (
        "Holographic",
        "ui_holo",
        r#"// An iridescent holo sheen — a drifting rainbow band across the surface.
shader ui_holo {
  stage ui
  uniform speed: float = 0.4
  uniform strength: float = 0.55
  uniform scale: float = 3.0
  let t = (uv.x + uv.y) * scale + time * speed
  let phase = vec3(t, t + 0.33, t + 0.67) * 6.2831853
  let col = vec3(0.5, 0.5, 0.5) + vec3(0.5, 0.5, 0.5) * cos(phase)
  output color = vec4(col, strength) * instanceColor
}
"#,
    ),
    (
        "Film grain",
        "ui_grain",
        r#"// Animated film grain — a touch of texture over flat panels.
shader ui_grain {
  stage ui
  uniform strength: float = 0.2
  uniform scale: float = 220.0
  let n = noise(uv * scale + vec2(time * 11.0, time * 7.0))
  let g = n * 0.5 + 0.5
  output color = vec4(vec3(g, g, g), strength) * instanceColor
}
"#,
    ),
];

/// The project-relative path a seeded effect lives at.
pub(crate) fn effect_path(stem: &str) -> String {
    format!("shaders/examples/ui/{stem}.flsl")
}

/// A friendly name for whatever a UI element's `shader` field points at: the
/// built-in effect's menu label, or the bare filename for a custom shader.
pub(crate) fn effect_label(shader_path: &str) -> String {
    if shader_path.is_empty() {
        return "None".to_string();
    }
    for (label, stem, _) in UI_EFFECTS {
        if shader_path == effect_path(stem) {
            return (*label).to_string();
        }
    }
    format!("Custom: {}", shader_path.rsplit('/').next().unwrap_or(shader_path))
}

/// Write the built-in UI effect shaders into `<project>/shaders/examples/ui/`,
/// skipping any that already exist (so a designer's edits are never clobbered and
/// engine updates can add new effects). Mirrors `seed_example_shaders`.
pub(crate) fn seed_ui_effects(project_root: &std::path::Path) {
    let dir = project_root.join("shaders").join("examples").join("ui");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    for (_, stem, src) in UI_EFFECTS {
        let path = dir.join(format!("{stem}.flsl"));
        if !path.exists() {
            let _ = std::fs::write(&path, src);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every built-in UI effect must compile as a `stage ui` shader — a broken
    /// one would only fail when a user assigns it, so catch it here.
    #[test]
    fn all_ui_effects_compile() {
        for (label, stem, src) in UI_EFFECTS {
            match floptle_shader::compile_ui(src) {
                Ok(_) => {}
                Err(e) => panic!("built-in UI effect '{label}' ({stem}.flsl) failed to compile: {e:?}"),
            }
        }
    }

    #[test]
    fn effect_label_round_trips() {
        assert_eq!(effect_label(""), "None");
        assert_eq!(effect_label(&effect_path("ui_glow")), "Glow (edge)");
        assert_eq!(effect_label("shaders/mine.flsl"), "Custom: mine.flsl");
    }
}
