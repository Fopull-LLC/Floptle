//! The editor's icon glyphs, in one place.
//!
//! egui's bundled font stack (Ubuntu + a *subset* of NotoEmoji + an icon font,
//! plus Hack for geometry) covers far less than the emoji block suggests. A
//! glyph it lacks renders as a **tofu square**, and nothing warns you — the
//! label just looks broken to whoever opens the editor.
//!
//! So every icon lives here as a named constant, and [`ALL`] feeds a test that
//! asserts the real font stack can draw each one. Picking a new icon means
//! adding it here and running `cargo test -p floptle-editor icons`; if it's
//! tofu, the test says so before a user ever sees it.
//!
//! Casualties of that check, for the record: 🎮 ⚠ ⚪ ✓ 🔍 ＋ ⬢ ✏ ✨ 🎧 🖌 —
//! all missing, several of which had been shipping as squares in tab titles.

// Some of these appear only inside `dock.rs`'s title literals (`concat!` can't
// take a const) — they still belong here, because [`ALL`] is what the coverage
// test iterates and that test is the whole point of the module.
#![allow(dead_code)]

// --- devices ------------------------------------------------------------
/// Keyboard binding.
pub(crate) const KEYBOARD: &str = "⌨";
/// Mouse binding.
pub(crate) const MOUSE: &str = "🖱";
/// Gamepad binding. An analog stick, since no gamepad glyph exists in the
/// bundled fonts.
pub(crate) const PAD: &str = "◉";

// --- status -------------------------------------------------------------
/// Something needs attention (a control bound to nothing).
pub(crate) const WARN: &str = "▲";
/// Present and active — a held action in the live tester.
pub(crate) const ON: &str = "●";
/// Present and idle.
pub(crate) const OFF: &str = "○";
/// Defined but nothing references it.
pub(crate) const UNUSED: &str = "◌";

// --- actions ------------------------------------------------------------
/// Add / create.
pub(crate) const ADD: &str = "+";
/// Delete / remove.
pub(crate) const REMOVE: &str = "🗑";
/// Re-scan / refresh.
pub(crate) const RESCAN: &str = "⟲";
/// Opens a menu.
pub(crate) const MENU: &str = "▾";
/// Settings / configuration.
pub(crate) const SETTINGS: &str = "⚙";

// --- tabs ---------------------------------------------------------------
pub(crate) const TERRAIN: &str = "Δ";
pub(crate) const MAP: &str = "▦";
pub(crate) const SCENE: &str = "⌖";
pub(crate) const PLAY: &str = "⏵";
pub(crate) const ANIMATION: &str = "⏱";
pub(crate) const CONTROLLER: &str = "◎";
pub(crate) const PARTICLES: &str = "✱";
pub(crate) const MIXER: &str = "≣";
pub(crate) const SHADERS: &str = "◈";
pub(crate) const PAINT: &str = "◨";
/// The 2D image editor (distinct from ◨ Paint, which is the 3D brush).
/// NOT 🎨 — the proposal suggested it and `every_icon_has_a_glyph` refused it:
/// the bundled NotoEmoji subset has no palette glyph, so it would have shipped
/// as a tofu box in the tab bar. 🖼 renders, and matches the Assets browser's
/// texture icon.
pub(crate) const IMAGE: &str = "🖼";

/// Every icon, for the coverage test.
pub(crate) const ALL: &[(&str, &str)] = &[
    ("KEYBOARD", KEYBOARD),
    ("MOUSE", MOUSE),
    ("PAD", PAD),
    ("WARN", WARN),
    ("ON", ON),
    ("OFF", OFF),
    ("UNUSED", UNUSED),
    ("ADD", ADD),
    ("REMOVE", REMOVE),
    ("RESCAN", RESCAN),
    ("MENU", MENU),
    ("SETTINGS", SETTINGS),
    ("TERRAIN", TERRAIN),
    ("MAP", MAP),
    ("SCENE", SCENE),
    ("PLAY", PLAY),
    ("ANIMATION", ANIMATION),
    ("CONTROLLER", CONTROLLER),
    ("PARTICLES", PARTICLES),
    ("MIXER", MIXER),
    ("SHADERS", SHADERS),
    ("PAINT", PAINT),
    ("IMAGE", IMAGE),
];

/// Build the editor's exact font stack (mirrors the setup in `main.rs`).
#[cfg(test)]
pub(crate) fn test_context() -> egui::Context {
    let ctx = egui::Context::default();
    let mut fonts = egui::FontDefinitions::default();
    if let Some(fam) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
        fam.push("Hack".into());
    }
    ctx.set_fonts(fonts);
    // Fonts don't exist until a frame has run.
    let _ = ctx.run_ui(test_input(), |_| {});
    ctx
}

/// A headless frame with a REAL screen rect.
///
/// `RawInput::default()` has no `screen_rect`, so egui lays out into nothing and
/// every widget is clipped away — which makes a "does it render" test pass
/// while rendering literally zero widgets. Give it a window-sized viewport so
/// the panels actually run.
#[cfg(test)]
pub(crate) fn test_input() -> egui::RawInput {
    egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1280.0, 800.0),
        )),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every icon must actually render in the editor's font stack.
    ///
    /// This is the test that would have caught 🎮 and ⚠ rendering as squares
    /// in the Input settings — and ⬢ ✏ ✨ 🎧 🖌, which had been tofu in the
    /// tab bar for some time.
    #[test]
    fn every_icon_has_a_glyph() {
        let ctx = test_context();
        let id = egui::FontId::proportional(14.0);
        let mut tofu = Vec::new();
        ctx.fonts_mut(|f| {
            for (name, glyph) in ALL {
                for c in glyph.chars() {
                    if !f.has_glyph(&id, c) {
                        tofu.push(format!("{name} = {glyph:?} (U+{:04X})", c as u32));
                    }
                }
            }
        });
        assert!(tofu.is_empty(), "icons render as tofu squares:\n  {}", tofu.join("\n  "));
    }

    /// Every non-ASCII character in a **string literal** anywhere in the 🖼 tab
    /// must have a glyph.
    ///
    /// [`ALL`] only covers icons someone remembered to register. The image
    /// editor's chrome is full of one-off glyphs typed straight into button
    /// labels — ⏶ ⏷ ⎘ 👁 ⛶ ↶ ↷ ⇩ ⊗ — and any one of them silently shipping as a
    /// tofu box is exactly the failure this module exists to prevent. So scan
    /// the source instead of trusting a list.
    #[test]
    fn every_glyph_in_the_image_tab_renders() {
        let sources = [
            ("image_ui.rs", include_str!("image_ui.rs")),
            ("image_edit.rs", include_str!("image_edit.rs")),
            ("image_io.rs", include_str!("image_io.rs")),
        ];
        let ctx = test_context();
        let id = egui::FontId::proportional(14.0);
        let mut tofu = Vec::new();
        ctx.fonts_mut(|f| {
            for (name, src) in sources {
                for c in string_literal_chars(src) {
                    if !c.is_ascii() && !f.has_glyph(&id, c) {
                        tofu.push(format!("{name}: {c:?} (U+{:04X})", c as u32));
                    }
                }
            }
        });
        tofu.sort();
        tofu.dedup();
        assert!(tofu.is_empty(), "the Image tab draws tofu squares:\n  {}", tofu.join("\n  "));
    }

    /// Characters inside double-quoted string literals — skipping comments,
    /// where a glyph is prose rather than something a user will ever see.
    fn string_literal_chars(src: &str) -> Vec<char> {
        #[derive(PartialEq)]
        enum S {
            Code,
            Line,
            Block,
            Str,
        }
        let mut state = S::Code;
        let mut out = Vec::new();
        let mut escaped = false;
        let cs: Vec<char> = src.chars().collect();
        let mut i = 0;
        while i < cs.len() {
            let c = cs[i];
            let next = cs.get(i + 1).copied().unwrap_or('\0');
            match state {
                S::Code => match (c, next) {
                    ('/', '/') => {
                        state = S::Line;
                        i += 1;
                    }
                    ('/', '*') => {
                        state = S::Block;
                        i += 1;
                    }
                    ('"', _) => state = S::Str,
                    _ => {}
                },
                S::Line if c == '\n' => state = S::Code,
                S::Block if c == '*' && next == '/' => {
                    state = S::Code;
                    i += 1;
                }
                S::Str => {
                    if escaped {
                        escaped = false;
                    } else if c == '\\' {
                        escaped = true;
                    } else if c == '"' {
                        state = S::Code;
                    } else {
                        out.push(c);
                    }
                }
                _ => {}
            }
            i += 1;
        }
        out
    }

    /// Every dock tab title must render too — titles embed icons directly.
    #[test]
    fn every_tab_title_renders() {
        let ctx = test_context();
        let id = egui::FontId::proportional(14.0);
        let mut tofu = Vec::new();
        ctx.fonts_mut(|f| {
            for tab in crate::dock::EditorTab::ALL {
                for c in tab.title().chars() {
                    if !f.has_glyph(&id, c) {
                        tofu.push(format!("{:?} title {:?} (U+{:04X})", tab, tab.title(), c as u32));
                    }
                }
            }
        });
        assert!(tofu.is_empty(), "tab titles render as tofu:\n  {}", tofu.join("\n  "));
    }
}
