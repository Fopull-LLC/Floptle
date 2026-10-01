//! The typefaces a theme can name, and the egui font stack built from them.
//!
//! Floptle ships the brand's three (IBM Plex Sans, IBM Plex Mono, Bricolage
//! Grotesque) and Atkinson Hyperlegible, which the high-contrast themes use
//! because it was drawn to keep similar letters apart. All four are OFL 1.1;
//! the licences are beside them in `fonts/`.
//!
//! **The chosen face goes first and egui's own faces stay behind it.** The
//! editor draws its icons from glyphs that Plex does not have (`icons.rs`
//! guards every one against the stack's character maps), so a stack of only
//! the theme's face would turn the tab bar into boxes. Falling through to
//! Ubuntu, the emoji fonts and Hack keeps every icon where it was.

use std::sync::Arc;

use egui::{FontData, FontDefinitions, FontFamily};

static PLEX_SANS: &[u8] = include_bytes!("../fonts/IBMPlexSans.ttf");
static PLEX_MONO: &[u8] = include_bytes!("../fonts/IBMPlexMono-Regular.ttf");
static BRICOLAGE: &[u8] = include_bytes!("../fonts/BricolageGrotesque.ttf");
static ATKINSON: &[u8] = include_bytes!("../fonts/AtkinsonHyperlegibleNext.ttf");

/// The faces a theme can name without shipping a file.
pub const BUILTIN_FACES: &[&str] = &[
    "IBM Plex Sans",
    "IBM Plex Mono",
    "Bricolage Grotesque",
    "Atkinson Hyperlegible",
    "Ubuntu",
    "Hack",
];

/// The family egui knows the theme's display face by. `look::title` draws in it.
pub const DISPLAY: &str = "floptle-display";

/// The display family, or the ordinary one while `ctx` does not have it yet.
///
/// `set_fonts` takes effect on the frame after it is called, so a program's
/// very first frame has egui's stock fonts, and naming a family they lack
/// panics inside egui. Before the first frame there are no fonts at all, and
/// even *asking* panics, so that frame is answered without asking. This is
/// the only way any of this crate names the family.
pub fn display_family(ctx: &egui::Context) -> FontFamily {
    if ctx.cumulative_pass_nr() == 0 {
        return FontFamily::Proportional;
    }
    let fam = FontFamily::Name(DISPLAY.into());
    if ctx.fonts(|f| f.definitions().families.contains_key(&fam)) { fam } else { FontFamily::Proportional }
}

pub fn is_builtin(face: &str) -> bool {
    BUILTIN_FACES.contains(&face)
}

/// Does this face have a weight axis? Then `ui_weight`/`display_weight`, and
/// a heavier run for a primary button, mean something.
pub fn is_variable(face: &str) -> bool {
    matches!(face, "IBM Plex Sans" | "Bricolage Grotesque" | "Atkinson Hyperlegible")
}

/// Does this look like a font file? egui **panics** on bytes it cannot parse,
/// so a theme's font is checked here first and refused with a sentence.
pub fn is_font(bytes: &[u8]) -> bool {
    matches!(bytes.get(..4), Some(b"\x00\x01\x00\x00") | Some(b"true") | Some(b"ttcf") | Some(b"OTTO"))
}

/// egui's own name for a built-in face it already carries, if it is one.
fn egui_name(face: &str) -> Option<&'static str> {
    match face {
        "Ubuntu" => Some("Ubuntu-Light"),
        "Hack" => Some("Hack"),
        _ => None,
    }
}

fn data_for(face: &str, theme: &crate::Theme, weight: f32) -> Option<FontData> {
    let bytes: Option<FontData> = match face {
        "IBM Plex Sans" => Some(FontData::from_static(PLEX_SANS)),
        "IBM Plex Mono" => Some(FontData::from_static(PLEX_MONO)),
        "Bricolage Grotesque" => Some(FontData::from_static(BRICOLAGE)),
        "Atkinson Hyperlegible" => Some(FontData::from_static(ATKINSON)),
        _ => theme
            .assets
            .files
            .get(face)
            .filter(|b| is_font(b))
            .map(|b| FontData::from_owned(b.to_vec())),
    };
    bytes.map(|d| {
        let mut tweak = egui::epaint::text::FontTweak::default();
        if is_variable(face) || !is_builtin(face) {
            tweak.coords.push(b"wght", weight);
        }
        // Bricolage's optical-size axis: the display cut, made for big type.
        if face == "Bricolage Grotesque" {
            tweak.coords.push(b"opsz", 32.0);
        }
        d.tweak(tweak)
    })
}

/// The whole stack for `theme`: its three faces, each falling back through
/// egui's defaults. Hosts that add faces of their own (the editor's packages)
/// start from this.
pub fn definitions(theme: &crate::Theme) -> FontDefinitions {
    let mut defs = FontDefinitions::default();
    let base_prop = defs.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let base_mono = defs.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();

    let mut slot = |key: &str, face: &str, weight: f32| -> Option<String> {
        if let Some(n) = egui_name(face) {
            return Some(n.to_string());
        }
        let d = data_for(face, theme, weight)?;
        defs.font_data.insert(key.to_string(), Arc::new(d));
        Some(key.to_string())
    };
    let ui = slot("floptle-ui", &theme.fonts.ui, theme.fonts.ui_weight);
    let mono = slot("floptle-mono", &theme.fonts.mono, 400.0);
    let display = slot("floptle-display-face", &theme.fonts.display, theme.fonts.display_weight);

    let chain = |first: Option<String>, rest: &[String]| {
        let mut v: Vec<String> = first.into_iter().collect();
        for r in rest {
            if !v.contains(r) {
                v.push(r.clone());
            }
        }
        v
    };
    let mut prop_rest = base_prop.clone();
    // Hack carries the arrows and geometry the editor's icons use.
    if !prop_rest.iter().any(|n| n == "Hack") {
        prop_rest.push("Hack".into());
    }
    let prop = chain(ui, &prop_rest);
    defs.families.insert(FontFamily::Proportional, prop.clone());
    defs.families.insert(FontFamily::Monospace, chain(mono, &base_mono));
    defs.families.insert(FontFamily::Name(DISPLAY.into()), chain(display, &prop));
    defs
}

/// A value that changes whenever [`definitions`] would, so a host calls
/// `set_fonts` (which rebuilds the glyph atlas) only when it must.
pub fn fingerprint(theme: &crate::Theme) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let f = &theme.fonts;
    (f.ui.as_str(), f.mono.as_str(), f.display.as_str()).hash(&mut h);
    (f.ui_weight.to_bits(), f.display_weight.to_bits()).hash(&mut h);
    for face in [&f.ui, &f.mono, &f.display] {
        if !is_builtin(face) {
            theme.id.hash(&mut h);
            if let Some(b) = theme.assets.files.get(face.as_str()) {
                b.len().hash(&mut h);
            }
        }
    }
    h.finish()
}
