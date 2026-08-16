//! The editor's icon glyphs, in one place.
//!
//! egui's bundled font stack (Ubuntu + a *subset* of NotoEmoji + an icon font,
//! plus Hack for geometry) covers far less than the emoji block suggests. A
//! glyph it lacks renders as a **tofu square**, and nothing warns you — the
//! label just looks broken to whoever opens the editor.
//!
//! So every icon lives here as a named constant, and [`ALL`] names the ones
//! shared across tabs. But the test that guards them ([`tests`]) does NOT read
//! that list: it scans **every string literal in every source file**, because
//! the icon that ships broken is always the one nobody remembered to register.
//!
//! ## Ask the charmaps, not egui
//!
//! `epaint::Fonts::has_glyph` is not usable for this. It answers by resolving
//! the character to a font face and comparing that face against the one holding
//! the replacement glyph — so a character whose real home happens to *be* that
//! face reports missing (upstream marks the case with a TODO). An earlier pass
//! trusted it and swapped away icons that were fine: 🎮 ⚠ 🔍 ✏ ✨ 🎧 ✔ were all
//! recorded here as dead, and all of them draw correctly.
//!
//! The test below instead reads each bundled font's **character map** with
//! `skrifa` — the same crate epaint uses to resolve a glyph — which is the fact
//! the renderer actually acts on. To settle a case by eye rather than by
//! assertion, `cargo run -p floptle-editor --example glyph_probe` draws every
//! glyph in the editor to `target/glyph_probe.png` above a control row of known
//! -missing codepoints, so "broken" has a reference to be compared against.

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

/// Build the editor's exact font stack — the same builder `main.rs` calls, so
/// a test cannot draw in a stack the editor does not have.
#[cfg(test)]
pub(crate) fn test_context() -> egui::Context {
    let ctx = egui::Context::default();
    ctx.set_fonts(crate::fonts::definitions(&[]));
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

    /// Which characters the editor's font stack can actually draw, read from
    /// the fonts' own character maps.
    ///
    /// This is the ground truth the renderer acts on. See the module docs for
    /// why `Fonts::has_glyph` can't be used instead.
    fn drawable() -> std::collections::HashSet<char> {
        // The editor's real stack, from the one builder that makes it.
        let defs = crate::fonts::definitions(&[]);
        let names: Vec<String> = defs
            .families
            .get(&egui::FontFamily::Proportional)
            .cloned()
            .unwrap_or_default();

        let mut out = std::collections::HashSet::new();
        for name in &names {
            let data = defs
                .font_data
                .get(name)
                .unwrap_or_else(|| panic!("the font stack names {name:?} but egui has no such font"));
            let font = skrifa::FontRef::from_index(&data.font, data.index)
                .unwrap_or_else(|e| panic!("{name} is not a readable font: {e}"));
            let charmap = skrifa::MetadataProvider::charmap(&font);
            for (cp, _) in charmap.mappings() {
                if let Some(c) = char::from_u32(cp) {
                    out.insert(c);
                }
            }
        }
        out
    }

    /// **Every** non-ASCII character in a string literal anywhere in the editor
    /// must be drawable.
    ///
    /// [`ALL`] only covers icons someone remembered to register, and the icons
    /// that ship broken are the ones typed straight into a button label and
    /// never registered anywhere. So scan the sources instead of trusting a
    /// list — all of them, not a chosen few. That partial coverage is exactly
    /// how ⛰ ⬢ 🥊 ⬡ 🖌 ＋ and eighteen others reached users as empty boxes,
    /// across 88 call sites, while this module's test was passing.
    #[test]
    fn every_glyph_in_the_editor_renders() {
        let ok = drawable();
        let mut tofu: Vec<String> = Vec::new();
        for (path, src) in sources() {
            for c in string_literal_chars(&src) {
                if !c.is_ascii() && !ok.contains(&c) {
                    tofu.push(format!("{c:?} (U+{:04X}) in {path}", c as u32));
                }
            }
        }
        tofu.sort();
        tofu.dedup();
        assert!(
            tofu.is_empty(),
            "these draw as empty boxes — no bundled font maps them:\n  {}\n\
             Pick a replacement that IS mapped; `cargo run -p floptle-editor \
             --example glyph_probe` draws the whole set to a PNG.",
            tofu.join("\n  ")
        );
    }

    /// Every `.rs` file under `src/`, read at test time.
    ///
    /// Read from disk rather than `include_str!` so that adding a module can
    /// never quietly fall outside the check.
    fn sources() -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut stack = vec![std::path::PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src"
        ))];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let name = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
                    out.push((name, std::fs::read_to_string(&p).unwrap_or_default()));
                }
            }
        }
        assert!(out.len() > 40, "found only {} sources — the walk is wrong", out.len());
        out
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

    /// Every registered icon must be drawable.
    #[test]
    fn every_icon_has_a_glyph() {
        let ok = drawable();
        let tofu: Vec<String> = ALL
            .iter()
            .flat_map(|(name, glyph)| glyph.chars().map(move |c| (*name, *glyph, c)))
            .filter(|(_, _, c)| !ok.contains(c))
            .map(|(name, glyph, c)| format!("{name} = {glyph:?} (U+{:04X})", c as u32))
            .collect();
        assert!(tofu.is_empty(), "icons draw as empty boxes:\n  {}", tofu.join("\n  "));
    }

    /// Every dock tab title must render too — titles embed icons directly, and
    /// a broken one is the most-seen pixel in the editor.
    #[test]
    fn every_tab_title_renders() {
        let ok = drawable();
        let tofu: Vec<String> = crate::dock::EditorTab::ALL
            .iter()
            .flat_map(|tab| tab.title().chars().map(move |c| (tab, c)))
            .filter(|(_, c)| !ok.contains(c))
            .map(|(tab, c)| format!("{:?} title {:?} (U+{:04X})", tab, tab.title(), c as u32))
            .collect();
        assert!(tofu.is_empty(), "tab titles draw as empty boxes:\n  {}", tofu.join("\n  "));
    }
}
