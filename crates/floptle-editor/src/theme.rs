//! Editor theming: engine (chrome) themes, code-editor themes, and the Lua
//! syntax highlighter the Scripting tab renders with.

use crate::ide::{LUA_API_WORDS, LUA_KEYWORDS};

/// The three colours that mean something, shared with fopull.com.
///
/// **Colour is a signal, not decoration.** A resting panel is monochrome; these
/// appear for a rating, a permission, a compatibility warning, or the one
/// primary action, and nowhere else. That rule is what makes them readable —
/// a page where everything is coloured says nothing.
///
/// They are the same values the website uses, which is deliberate and is why
/// they live in one named place instead of being spelled out at each site. Two
/// of them came *from* this editor and went to the site; `WARN` came back the
/// other way, a more saturated amber than the muddy gold that was here. Change
/// one and it changes everywhere in the editor — and then tell the site, or the
/// two drift and "the same green" stops being the same green.
///
/// Deliberately NOT theme-derived. A rating being good and a package asking for
/// the network are facts about the thing, not about the chrome around it, and
/// they have to keep meaning the same under every theme. Everything that IS
/// about the chrome — panel fills, text, hairlines — must still come from
/// `ui.visuals()`.
///
/// **The accent is the one that stayed with the theme**, and it is the
/// exception that shows the rule. The site picked a teal to match this
/// editor's default look, but an accent is chrome: somebody who chose GitHub
/// Light should get that theme's accent, not a fixed teal sitting in it. So the
/// editor reads `ui.visuals().selection.stroke.color` for "switched on" and
/// "the primary action", and only the three signals below are pinned.
pub(crate) mod signal {
    use egui::Color32;

    /// Good: a healthy rating, a check that passed, a save that landed.
    pub(crate) const GOOD: Color32 = Color32::from_rgb(0x82, 0xd2, 0x96);
    /// Warn: a permission, an unsaved change, something that wants a look.
    pub(crate) const WARN: Color32 = Color32::from_rgb(0xe0, 0xb0, 0x50);
    /// Bad: a failure, an incompatibility, a refusal.
    pub(crate) const BAD: Color32 = Color32::from_rgb(0xe6, 0x78, 0x6e);
}

/// The type scale, and the one panel treatment, that the package browser is
/// built from (`floptle/0135`).
///
/// **Four steps and no more** — title, section, body, fine — with weight and
/// colour doing everything else. The browser had `heading`, `strong`, `small`
/// and a bare `label` used ad hoc at forty-odd call sites, which is not a scale
/// but four unrelated decisions repeated; a reader gets no help from it about
/// what to read first.
///
/// Sizes are derived from the theme's own body text rather than written down,
/// so somebody who has turned the editor's font size up gets a scale that goes
/// up with it instead of a title that ends up smaller than its own body.
///
/// **Nothing here names a colour that is not `ui.visuals()`.** Ground, surface,
/// text and hairline are the user's theme — the editor ships Floptle Dark,
/// Midnight, Slate, Carbon and Light, and hardcoding a hex is exactly how the
/// browser came to look like a stranger inside all five. The three [`signal`]
/// colours above are the only pinned values, because good, warn and bad are
/// facts about the thing rather than about the chrome.
pub(crate) mod look {
    use egui::{Color32, RichText, Stroke, Ui};

    /// The theme's body size — every step is a multiple of it.
    fn base(ui: &Ui) -> f32 {
        ui.style()
            .text_styles
            .get(&egui::TextStyle::Body)
            .map(|f| f.size)
            .unwrap_or(12.5)
    }

    /// The one biggest thing in a view, and there is one. A package's name.
    pub(crate) fn title(ui: &Ui, s: impl Into<String>) -> RichText {
        RichText::new(s).size((base(ui) * 1.5).round()).strong()
    }

    /// What a group of rows is. Sits above a run of body text and stops.
    pub(crate) fn section(ui: &Ui, s: impl Into<String>) -> RichText {
        RichText::new(s).size((base(ui) * 1.15).round()).strong()
    }

    /// The words. The default, and the step that needs no helper — it is here
    /// so the four steps can be named in one place and counted.
    pub(crate) fn body(s: impl Into<String>) -> RichText {
        RichText::new(s)
    }

    /// Labels, counts, timestamps, the quiet half of a row. Dimmed by default:
    /// fine print that is the same colour as the text is not fine print.
    pub(crate) fn fine(ui: &Ui, s: impl Into<String>) -> RichText {
        RichText::new(s).size((base(ui) * 0.85).round()).color(ui.visuals().weak_text_color())
    }

    /// …in the full text colour, for fine print that is the point rather than
    /// the aside — a refusal, a permission, the line under a primary action.
    pub(crate) fn fine_strong(ui: &Ui, s: impl Into<String>) -> RichText {
        RichText::new(s).size((base(ui) * 0.85).round()).strong()
    }

    /// **Identity and data**: package ids, versions, engine ranges, revisions,
    /// file paths, URLs. Monospace, at the fine size, in the ordinary text
    /// colour — an id is content, not an aside, and it is the thing somebody
    /// copies, compares character by character, or reads out loud.
    pub(crate) fn data(ui: &Ui, s: impl Into<String>) -> RichText {
        RichText::new(s).size((base(ui) * 0.85).round()).monospace()
    }

    /// The accent, which stays the user's. "Switched on", focus, and the one
    /// primary action all read it, so a GitHub Light user gets GitHub Light's
    /// accent rather than a fixed teal sitting in a pale panel.
    pub(crate) fn accent(ui: &Ui) -> Color32 {
        ui.visuals().selection.stroke.color
    }

    /// One hairline. How two surfaces are separated — not a shadow, and not a
    /// widget stroke that changes width with the theme's widget styling.
    pub(crate) fn hairline(ui: &Ui) -> Stroke {
        Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color)
    }

    /// A panel: one small fill step up from the ground it sits on, separated by
    /// a hairline. **The** panel — every card, row group and callout in the
    /// browser is this, so there is one treatment to change rather than one per
    /// call site.
    pub(crate) fn panel(ui: &Ui) -> egui::Frame {
        egui::Frame::new()
            .inner_margin(8)
            .corner_radius(3.0)
            .fill(ui.visuals().faint_bg_color)
            .stroke(hairline(ui))
    }

    /// …the one you have chosen, marked by the accent rather than by a
    /// different treatment.
    pub(crate) fn panel_selected(ui: &Ui) -> egui::Frame {
        panel(ui)
            .fill(accent(ui).gamma_multiply(0.10))
            .stroke(Stroke::new(1.0, accent(ui)))
    }

    /// **The one primary action in a view.** Accent-outlined over a wash of it,
    /// rather than accent-filled: a solid fill has to pick its own text colour,
    /// and the colour that reads on one of five themes reads as a bruise on
    /// another. Everything else in the view is an ordinary button, and a view
    /// with two of these has a design problem rather than a styling one.
    pub(crate) fn primary(ui: &Ui, label: impl Into<String>) -> egui::Button<'static> {
        let a = accent(ui);
        egui::Button::new(RichText::new(label).strong())
            .fill(a.gamma_multiply(0.22))
            .stroke(Stroke::new(1.0, a))
    }
}

/// Build a colored layout for Lua source (keywords, strings, numbers, comments,
/// engine API). A simple single-pass tokenizer — good enough for an in-engine IDE.
/// A code-editor color theme: the syntax token colors plus the editor background, gutter
/// and current-line highlight. Colors are raw RGB(A) so the presets can be `const`.
#[derive(Clone, Copy)]
pub(crate) struct CodeTheme {
    pub(crate) name: &'static str,
    pub(crate) bg: [u8; 3],
    pub(crate) gutter: [u8; 3],
    pub(crate) kw: [u8; 3],
    pub(crate) api: [u8; 3],
    pub(crate) string: [u8; 3],
    pub(crate) num: [u8; 3],
    pub(crate) comment: [u8; 3],
    pub(crate) text: [u8; 3],
    /// Current-line highlight (RGBA; alpha is the wash strength).
    pub(crate) cur_line: [u8; 4],
}

impl CodeTheme {
    pub(crate) fn bg32(&self) -> egui::Color32 {
        egui::Color32::from_rgb(self.bg[0], self.bg[1], self.bg[2])
    }
    pub(crate) fn gutter32(&self) -> egui::Color32 {
        egui::Color32::from_rgb(self.gutter[0], self.gutter[1], self.gutter[2])
    }
    pub(crate) fn text32(&self) -> egui::Color32 {
        egui::Color32::from_rgb(self.text[0], self.text[1], self.text[2])
    }
    pub(crate) fn cur_line32(&self) -> egui::Color32 {
        let [r, g, b, a] = self.cur_line;
        egui::Color32::from_rgba_unmultiplied(r, g, b, a)
    }
}

/// The selectable code-editor themes (Preferences → Editor theme). Index 0 is the default.
pub(crate) const CODE_THEMES: &[CodeTheme] = &[
    CodeTheme {
        name: "Floptle Dark",
        bg: [30, 30, 30],
        gutter: [100, 100, 100],
        kw: [86, 156, 214],
        api: [78, 201, 176],
        string: [206, 145, 120],
        num: [181, 206, 168],
        comment: [106, 153, 85],
        text: [212, 212, 212],
        cur_line: [255, 255, 255, 14],
    },
    CodeTheme {
        name: "Monokai",
        bg: [39, 40, 34],
        gutter: [120, 120, 110],
        kw: [249, 38, 114],
        api: [102, 217, 239],
        string: [230, 219, 116],
        num: [174, 129, 255],
        comment: [117, 113, 94],
        text: [248, 248, 242],
        cur_line: [255, 255, 255, 16],
    },
    CodeTheme {
        name: "Dracula",
        bg: [40, 42, 54],
        gutter: [98, 114, 164],
        kw: [255, 121, 198],
        api: [139, 233, 253],
        string: [241, 250, 140],
        num: [189, 147, 249],
        comment: [98, 114, 164],
        text: [248, 248, 242],
        cur_line: [255, 255, 255, 16],
    },
    CodeTheme {
        name: "Solarized Dark",
        bg: [0, 43, 54],
        gutter: [88, 110, 117],
        kw: [133, 153, 0],
        api: [42, 161, 152],
        string: [42, 161, 152],
        num: [211, 54, 130],
        comment: [88, 110, 117],
        text: [147, 161, 161],
        cur_line: [255, 255, 255, 14],
    },
    CodeTheme {
        name: "GitHub Light",
        bg: [255, 255, 255],
        gutter: [160, 160, 160],
        kw: [215, 58, 73],
        api: [0, 92, 197],
        string: [3, 47, 98],
        num: [0, 92, 197],
        comment: [106, 115, 125],
        text: [36, 41, 46],
        cur_line: [0, 0, 0, 14],
    },
];

/// An editor/engine chrome theme (Preferences → Engine theme). Built on egui's dark/light
/// base, then key surfaces are overridden. Index 0 is the default (egui dark).
#[derive(Clone, Copy)]
pub(crate) struct EngineTheme {
    pub(crate) name: &'static str,
    pub(crate) dark: bool,
    /// Override panel/window/extreme backgrounds; `None` keeps the egui base value.
    pub(crate) panel: Option<[u8; 3]>,
    pub(crate) window: Option<[u8; 3]>,
    pub(crate) extreme: Option<[u8; 3]>,
    /// Selection / hyperlink accent.
    pub(crate) accent: Option<[u8; 3]>,
}

pub(crate) const ENGINE_THEMES: &[EngineTheme] = &[
    EngineTheme { name: "Floptle Dark", dark: true, panel: None, window: None, extreme: None, accent: None },
    EngineTheme {
        name: "Midnight",
        dark: true,
        panel: Some([18, 20, 30]),
        window: Some([22, 25, 37]),
        extreme: Some([12, 13, 20]),
        accent: Some([90, 130, 245]),
    },
    EngineTheme {
        name: "Slate",
        dark: true,
        panel: Some([38, 42, 50]),
        window: Some([44, 49, 58]),
        extreme: Some([28, 31, 37]),
        accent: Some([120, 160, 200]),
    },
    EngineTheme {
        name: "Carbon (OLED)",
        dark: true,
        panel: Some([8, 8, 8]),
        window: Some([14, 14, 14]),
        extreme: Some([0, 0, 0]),
        accent: Some([0, 200, 160]),
    },
    EngineTheme { name: "Light", dark: false, panel: None, window: None, extreme: None, accent: None },
];

impl EngineTheme {
    /// The egui visuals for this theme (base + overrides).
    pub(crate) fn visuals(&self) -> egui::Visuals {
        let mut v = if self.dark { egui::Visuals::dark() } else { egui::Visuals::light() };
        let c = |rgb: [u8; 3]| egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
        if let Some(p) = self.panel {
            v.panel_fill = c(p);
        }
        if let Some(w) = self.window {
            v.window_fill = c(w);
            v.widgets.noninteractive.bg_fill = c(w);
        }
        if let Some(e) = self.extreme {
            v.extreme_bg_color = c(e);
        }
        if let Some(a) = self.accent {
            v.selection.bg_fill = c(a).gamma_multiply(0.55);
            v.hyperlink_color = c(a);
        }
        v
    }
}

pub(crate) fn lua_highlight(text: &str, font: egui::FontId, theme: &CodeTheme) -> egui::text::LayoutJob {
    use egui::Color32;
    let rgb = |c: [u8; 3]| Color32::from_rgb(c[0], c[1], c[2]);
    let c_kw = rgb(theme.kw);
    let c_api = rgb(theme.api);
    let c_str = rgb(theme.string);
    let c_num = rgb(theme.num);
    let c_com = rgb(theme.comment);
    let c_def = rgb(theme.text);

    let mut job = egui::text::LayoutJob::default();
    let mut push = |s: &str, color: Color32| {
        job.append(s, 0.0, egui::text::TextFormat { font_id: font.clone(), color, ..Default::default() });
    };

    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        // line comment
        if c == b'-' && i + 1 < b.len() && b[i + 1] == b'-' {
            let s = i;
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            push(&text[s..i], c_com);
        } else if c == b'"' || c == b'\'' {
            // string (single line; handles \" escapes)
            let q = c;
            let s = i;
            i += 1;
            while i < b.len() {
                if b[i] == b'\\' {
                    i = (i + 2).min(b.len());
                    continue;
                }
                if b[i] == q || b[i] == b'\n' {
                    i = (i + 1).min(b.len());
                    break;
                }
                i += 1;
            }
            push(&text[s..i], c_str);
        } else if c.is_ascii_digit() {
            let s = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.') {
                i += 1;
            }
            push(&text[s..i], c_num);
        } else if c.is_ascii_alphabetic() || c == b'_' {
            let s = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            let word = &text[s..i];
            let color = if LUA_KEYWORDS.contains(&word) {
                c_kw
            } else if LUA_API_WORDS.contains(&word) {
                c_api
            } else {
                c_def
            };
            push(word, color);
        } else {
            // one (possibly multibyte) character verbatim
            let ch = text[i..].chars().next().unwrap();
            let l = ch.len_utf8();
            push(&text[i..i + l], c_def);
            i += l;
        }
    }
    job
}

/// A plain monospace layout (no highlighting) — used for non-Lua files (Markdown).
pub(crate) fn plain_job(text: &str, font: egui::FontId, theme: &CodeTheme) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    job.append(
        text,
        0.0,
        egui::text::TextFormat { font_id: font, color: theme.text32(), ..Default::default() },
    );
    job
}

/// `.flsl` structure keywords (declarations + stage/blend names + types).
const FLSL_KEYWORDS: [&str; 15] = [
    "shader", "stage", "blend", "uniform", "texture", "let", "output", "range", "fragment",
    "sdf", "opaque", "alpha", "additive", "float", "color",
];

/// Every stdlib op + built-in input name, straight from the shader registry —
/// autocomplete-grade accuracy with zero duplicated word lists.
fn flsl_api_words() -> &'static std::collections::HashSet<&'static str> {
    static WORDS: std::sync::OnceLock<std::collections::HashSet<&'static str>> =
        std::sync::OnceLock::new();
    WORDS.get_or_init(|| {
        let mut set: std::collections::HashSet<&'static str> =
            floptle_shader::stdlib::OPS.iter().map(|o| o.name).collect();
        for i in floptle_shader::ir::Input::all() {
            set.insert(i.name());
        }
        for v in ["vec2", "vec3", "vec4"] {
            set.insert(v);
        }
        set
    })
}

/// Syntax highlighting for `.flsl` shaders — the Lua highlighter's structure
/// with `//` comments, `#RRGGBB` colors as numbers, and the shader word sets.
pub(crate) fn flsl_highlight(
    text: &str,
    font: egui::FontId,
    theme: &CodeTheme,
) -> egui::text::LayoutJob {
    use egui::Color32;
    let rgb = |c: [u8; 3]| Color32::from_rgb(c[0], c[1], c[2]);
    let c_kw = rgb(theme.kw);
    let c_api = rgb(theme.api);
    let c_str = rgb(theme.string);
    let c_num = rgb(theme.num);
    let c_com = rgb(theme.comment);
    let c_def = rgb(theme.text);

    let mut job = egui::text::LayoutJob::default();
    let mut push = |s: &str, color: Color32| {
        job.append(s, 0.0, egui::text::TextFormat { font_id: font.clone(), color, ..Default::default() });
    };

    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
            let s = i;
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            push(&text[s..i], c_com);
        } else if c == b'"' {
            let s = i;
            i += 1;
            while i < b.len() {
                if b[i] == b'"' || b[i] == b'\n' {
                    i = (i + 1).min(b.len());
                    break;
                }
                i += 1;
            }
            push(&text[s..i], c_str);
        } else if c == b'#' {
            // A #RRGGBB[AA] color literal reads as one "number".
            let s = i;
            i += 1;
            while i < b.len() && (b[i] as char).is_ascii_hexdigit() {
                i += 1;
            }
            push(&text[s..i], c_num);
        } else if c.is_ascii_digit() {
            let s = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.') {
                i += 1;
            }
            push(&text[s..i], c_num);
        } else if c.is_ascii_alphabetic() || c == b'_' {
            let s = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            let word = &text[s..i];
            let color = if FLSL_KEYWORDS.contains(&word) {
                c_kw
            } else if flsl_api_words().contains(word) {
                c_api
            } else {
                c_def
            };
            push(word, color);
        } else {
            let ch = text[i..].chars().next().unwrap();
            let l = ch.len_utf8();
            push(&text[i..i + l], c_def);
            i += l;
        }
    }
    job
}
#[cfg(test)]
mod look_tests {
    use super::*;

    /// Four steps, and each one genuinely a step. A "scale" whose title and
    /// body come out the same height is four names for one size, which is what
    /// the browser had.
    #[test]
    fn the_type_scale_is_four_steps_in_descending_order() {
        let ctx = crate::icons::test_context();
        let mut h = [0.0f32; 4];
        let _ = ctx.run_ui(crate::icons::test_input(), |ui| {
            for (i, t) in [
                look::title(ui, "Ag"),
                look::section(ui, "Ag"),
                look::body("Ag"),
                look::fine(ui, "Ag"),
            ]
            .into_iter()
            .enumerate()
            {
                h[i] = ui.label(t).rect.height();
            }
        });
        assert!(
            h[0] > h[1] && h[1] > h[2] && h[2] > h[3],
            "title/section/body/fine must be four sizes, largest first: {h:?}",
        );
    }

    /// Chrome comes from the user's theme and nowhere else. The editor ships
    /// five, and a panel fill written down as a hex is one that looks borrowed
    /// in four of them — which is how the browser came to ignore the theme in
    /// the first place.
    ///
    /// The accent is checked by *varying*: it is the one a theme actually
    /// chooses, so two themes must not answer the same. The panel is checked
    /// against `visuals` directly, because its fill is egui's own additive
    /// lift — deliberately the same value on every theme, since "one step
    /// brighter than whatever is underneath" is the same instruction
    /// everywhere. What the assertion catches is somebody replacing it with a
    /// number.
    #[test]
    fn the_panel_and_the_accent_follow_the_users_theme() {
        let read = |v: egui::Visuals| {
            let ctx = egui::Context::default();
            ctx.set_visuals(v);
            let mut out = None;
            let _ = ctx.run_ui(crate::icons::test_input(), |ui| {
                let p = look::panel(ui);
                assert_eq!(p.fill, ui.visuals().faint_bg_color, "the panel fill is written down");
                assert_eq!(
                    p.stroke.color,
                    ui.visuals().widgets.noninteractive.bg_stroke.color,
                    "the hairline is written down",
                );
                assert_eq!(p.shadow, egui::epaint::Shadow::NONE, "hairlines, not shadows");
                out = Some(look::accent(ui));
            });
            out.expect("a frame ran")
        };
        let seen: Vec<_> = ENGINE_THEMES.iter().map(|t| read(t.visuals())).collect();
        assert!(
            seen.iter().any(|a| *a != seen[0]),
            "every theme got the same accent — it is not being read from one: {seen:?}",
        );
    }

    /// The three signals are the exception, and they are pinned *because* they
    /// are facts about the thing rather than about the chrome. Shared with
    /// fopull.com — change one here and the two surfaces drift.
    #[test]
    fn the_three_signals_are_the_values_the_site_uses() {
        assert_eq!(signal::GOOD, egui::Color32::from_rgb(0x82, 0xd2, 0x96));
        assert_eq!(signal::WARN, egui::Color32::from_rgb(0xe0, 0xb0, 0x50));
        assert_eq!(signal::BAD, egui::Color32::from_rgb(0xe6, 0x78, 0x6e));
    }

    /// The package browser makes no type or surface decision of its own.
    ///
    /// A source scan rather than a review, for the same reason the glyph
    /// coverage test is one: the ad-hoc `strong` that creeps back in is always
    /// the one nobody remembered to look at. `floptle/0135`.
    #[test]
    fn the_package_browser_makes_no_type_decisions_of_its_own() {
        let src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/packages_ui.rs"
        ))
        .expect("the browser's source");
        for banned in [
            "ui.heading(",
            "ui.strong(",
            "ui.small(",
            "RichText::new(",
            "Frame::group(",
            "ui.group(",
            "FontId::monospace(",
        ] {
            assert!(
                !src.contains(banned),
                "packages_ui.rs uses {banned}…) directly — every type and surface decision \
                 goes through theme::look, so there is one place the browser's look lives",
            );
        }
    }
}
