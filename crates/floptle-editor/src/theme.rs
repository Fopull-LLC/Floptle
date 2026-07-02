//! Editor theming: engine (chrome) themes, code-editor themes, and the Lua
//! syntax highlighter the Scripting tab renders with.

use crate::ide::{LUA_API_WORDS, LUA_KEYWORDS};

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