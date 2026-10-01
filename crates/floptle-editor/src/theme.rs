//! Editor theming: engine (chrome) themes, code-editor themes, and the Lua
//! syntax highlighter the Scripting tab renders with.

use crate::ide::{LUA_API_WORDS, LUA_KEYWORDS};

/// The three colours that mean something, shared with fopull.com. They live
/// in `floptle-theme` now, beside the themes they are deliberately not part
/// of, so the Hub reads the same three.
pub(crate) use floptle_theme::signal;

/// The type scale, and the one panel treatment, that the package browser is
/// built from.
///
/// **Four steps and no more** — title, section, body, fine — with weight and
/// colour doing everything else. The browser had `heading`, `strong`, `small`
/// and a bare `label` used ad hoc at forty-odd call sites, which is not a scale
/// but four unrelated decisions repeated; a reader gets no help from it about
/// what to read first.
///
/// The components themselves are `floptle_theme::look`, shared with the Hub
/// so the two cannot drift. These are the editor's names for them.
///
/// **Nothing here names a colour of its own.** Ground, surface, text and
/// hairline are the user's theme; the three [`signal`] colours are the only
/// pinned values, because good, warn and bad are facts about the thing rather
/// than about the chrome.
pub(crate) mod look {
    use egui::{Color32, RichText, Ui};
    use floptle_theme::look as shared;

    /// The one biggest thing in a view, and there is one. A package's name.
    pub(crate) fn title(ui: &Ui, s: impl Into<String>) -> RichText {
        shared::title(ui, s)
    }

    /// What a group of rows is. Sits above a run of body text and stops.
    pub(crate) fn section(ui: &Ui, s: impl Into<String>) -> RichText {
        shared::section(ui, s)
    }

    /// The words. The default, and the step that needs no helper — it is here
    /// so the four steps can be named in one place and counted.
    pub(crate) fn body(s: impl Into<String>) -> RichText {
        RichText::new(s)
    }

    /// Labels, counts, timestamps, the quiet half of a row.
    pub(crate) fn fine(ui: &Ui, s: impl Into<String>) -> RichText {
        shared::fine(ui, s)
    }

    /// …in the full text colour, for fine print that is the point rather than
    /// the aside — a refusal, a permission, the line under a primary action.
    pub(crate) fn fine_strong(ui: &Ui, s: impl Into<String>) -> RichText {
        shared::fine_strong(ui, s)
    }

    /// **Identity and data**: package ids, versions, engine ranges, revisions,
    /// file paths, URLs. Monospace, in the ordinary text colour.
    pub(crate) fn data(ui: &Ui, s: impl Into<String>) -> RichText {
        shared::data(ui, s)
    }

    /// The accent, which is the theme's. "Switched on", focus, and the one
    /// primary action all read it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn accent(ui: &Ui) -> Color32 {
        shared::accent(ui)
    }

    /// A panel: one fill step up from the ground it sits on, a hairline.
    pub(crate) fn panel(ui: &Ui) -> egui::Frame {
        shared::panel(ui)
    }

    /// …the one you have chosen, marked by the accent rather than by a
    /// different treatment.
    pub(crate) fn panel_selected(ui: &Ui) -> egui::Frame {
        shared::panel_selected(ui)
    }

    /// **The one primary action in a view**: filled with the accent, its text
    /// in the theme's `on_accent`. Every theme declares that colour, which is
    /// what makes a fill safe under all of them. A view with two of these has
    /// a design problem rather than a styling one.
    pub(crate) fn primary(ui: &Ui, label: impl Into<String>) -> egui::Button<'static> {
        shared::primary(ui, label)
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

/// The selectable code-editor themes (Preferences → Code colours). Index 0
/// is "the engine theme's own", whose colours come from the theme at draw
/// time ([`code_theme`]); the values written here are only its fallback.
/// Saved by name, never by position.
pub(crate) const CODE_THEMES: &[CodeTheme] = &[
    CodeTheme {
        name: "Match the theme",
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

/// The code editor's colours: the engine theme's own (index 0, the default),
/// or one of the classic editor palettes.
pub(crate) fn code_theme(index: usize, ctx: &egui::Context) -> CodeTheme {
    if index == 0 || index >= CODE_THEMES.len() {
        let t = floptle_theme::theme(ctx);
        let c = &t.code;
        let rgb = |r: floptle_theme::Rgba| {
            let o = t.tokens.solid(r).0;
            [o[0], o[1], o[2]]
        };
        return CodeTheme {
            name: CODE_THEMES[0].name,
            bg: rgb(c.background),
            gutter: rgb(c.gutter),
            kw: rgb(c.keyword),
            api: rgb(c.api),
            string: rgb(c.string),
            num: rgb(c.number),
            comment: rgb(c.comment),
            text: rgb(c.text),
            cur_line: c.current_line.0,
        };
    }
    CODE_THEMES[index]
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
            // A #rrggbb[AA] color literal reads as one "number".
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
/// The dock in the theme's terms: tab bars and bodies are left clear so the
/// regions painted under them show (`tab_bar`, `tab.<name>`); tabs are plain
/// text, the active one outlined by a hairline, the focused panel's active
/// tab in the accent's wash and edge.
pub(crate) fn dock_style(ui: &egui::Ui) -> egui_dock::Style {
    use egui::Color32;
    let t = floptle_theme::theme(ui.ctx());
    let k = &t.tokens;
    let mut s = egui_dock::Style::from_egui(ui.style());
    let r = egui::CornerRadius { nw: t.shape.radius_small as u8, ne: t.shape.radius_small as u8, sw: 0, se: 0 };
    s.tab_bar.bg_fill = Color32::TRANSPARENT;
    s.tab_bar.hline_color = k.hairline.to_egui();
    s.tab_bar.corner_radius = egui::CornerRadius::ZERO;
    s.tab_bar.height = (t.fonts.size * 2.0).round().max(22.0);
    s.tab.tab_body.bg_fill = t.tokens.solid(k.ground).to_egui();
    s.tab.tab_body.stroke = egui::Stroke::new(t.shape.stroke, k.hairline.to_egui());
    s.tab.tab_body.corner_radius = egui::CornerRadius::ZERO;
    let plain = |text: Color32, bg: Color32, outline: Color32| egui_dock::TabInteractionStyle {
        outline_color: outline,
        corner_radius: r,
        bg_fill: bg,
        text_color: text,
    };
    s.tab.inactive = plain(k.dim.to_egui(), Color32::TRANSPARENT, Color32::TRANSPARENT);
    s.tab.inactive_with_kb_focus = plain(k.text.to_egui(), Color32::TRANSPARENT, k.accent_edge.to_egui());
    s.tab.hovered = plain(k.text.to_egui(), k.surface_2.to_egui(), Color32::TRANSPARENT);
    s.tab.active = plain(k.text.to_egui(), t.tokens.solid(k.ground).to_egui(), k.hairline.to_egui());
    s.tab.active_with_kb_focus = plain(k.text.to_egui(), t.tokens.solid(k.ground).to_egui(), k.accent_edge.to_egui());
    s.tab.focused = plain(k.text.to_egui(), k.accent_wash.over(t.tokens.solid(k.ground)).to_egui(), k.accent_edge.to_egui());
    s.tab.focused_with_kb_focus = s.tab.focused.clone();
    s.separator.color_idle = k.hairline.to_egui();
    s.separator.color_hovered = k.accent_edge.to_egui();
    s.separator.color_dragged = k.accent.to_egui();
    s.overlay.selection_color = k.accent.to_egui().gamma_multiply(0.4);
    s.overlay.button_color = k.surface_2.to_egui();
    s.main_surface_border_stroke = egui::Stroke::NONE;
    s
}

/// The theme region a dock tab's body is: `tab.<name>`, falling back to the
/// panel's.
pub(crate) fn tab_region(tab: crate::dock::EditorTab) -> &'static str {
    use crate::dock::EditorTab as T;
    match tab {
        T::Hierarchy => "tab.hierarchy",
        T::Inspector => "tab.inspector",
        T::Terrain => "tab.terrain",
        T::Map => "tab.model",
        T::Tiles => "tab.tiles",
        T::Assets => "tab.assets",
        T::Console => "tab.console",
        T::Scene => "tab.scene",
        T::Game => "tab.game",
        T::Scripting => "tab.scripting",
        T::Animation => "tab.animation",
        T::AnimGraph => "tab.controller",
        T::Particles => "tab.particles",
        T::Mixer => "tab.mixer",
        T::ShaderGraph => "tab.shaders",
        T::Paint => "tab.paint",
        T::Image => "tab.image",
        T::UiDesign => "tab.ui",
        T::Learn => "tab.learn",
        T::Settings => "tab.settings",
        T::Packages => "tab.packages",
        T::Package(_) => "tab.package",
    }
}

/// A top or bottom panel's frame with no fill of its own, and the region to
/// paint under it with [`paint_panel`].
pub(crate) fn clear_panel_frame(ctx: &egui::Context) -> egui::Frame {
    egui::Frame::side_top_panel(&ctx.global_style()).fill(egui::Color32::TRANSPARENT)
}

/// Paint `region` under a panel whose frame is [`clear_panel_frame`]. Call
/// first thing inside the panel, before any widget.
pub(crate) fn paint_panel(ui: &egui::Ui, region: &str) {
    let frame = clear_panel_frame(ui.ctx());
    let rect = ui.max_rect() + frame.inner_margin;
    floptle_theme::paint_region(ui, region, rect);
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

    /// Chrome comes from the user's theme and nowhere else. A panel fill
    /// written down as a hex is one that looks borrowed in every other
    /// theme, which is how the browser came to ignore the theme in the first
    /// place.
    ///
    /// Checked across every built-in: the panel is the theme's surface with
    /// its hairline, and the accent *varies*, because it is the one value a
    /// theme actually chooses and two themes must not answer the same.
    #[test]
    fn the_panel_and_the_accent_follow_the_users_theme() {
        let lib = floptle_theme::Library::scan(None, &[]);
        let mut seen = Vec::new();
        for e in &lib.entries {
            let t = std::sync::Arc::new(lib.load(&e.id).expect("a built-in loads"));
            let ctx = egui::Context::default();
            floptle_theme::apply(&ctx, t.clone(), &floptle_theme::Prefs::default());
            let _ = ctx.run_ui(crate::icons::test_input(), |ui| {
                let p = look::panel(ui);
                assert_eq!(p.fill, t.tokens.surface.to_egui(), "{}: the panel fill is written down", e.id);
                assert_eq!(p.stroke.color, t.tokens.hairline.to_egui(), "{}: the hairline is written down", e.id);
                assert_eq!(p.shadow, egui::epaint::Shadow::NONE, "hairlines, not shadows");
                seen.push(look::accent(ui));
            });
        }
        assert!(seen.len() >= 5, "the built-ins did not load: {seen:?}");
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
    /// the one nobody remembered to look at.
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

