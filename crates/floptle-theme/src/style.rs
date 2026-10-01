//! A theme, as egui's `Style`.
//!
//! The mapping is the contract's (§2): ground is the panel fill, surface the
//! window, surface_2 the faint background and a resting button, well the text
//! field. Hairlines and no shadows unless the theme asks for one. Hover takes
//! the accent's edge, press takes its wash, and nothing grows on hover
//! (`expansion` 0), because a widget that changes size under the pointer is a
//! widget that moved on its own.

use egui::{Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle};

use crate::Theme;
use crate::color::Rgba;

/// The three colours that mean something, shared with fopull.com.
///
/// Colour is a signal, not decoration. A resting panel is monochrome; these
/// appear for a rating, a permission, a compatibility warning, and nowhere
/// else. They are **not theme tokens** (contract §7.2): a rating being good and
/// a package asking for the network are facts about the thing, not about the
/// chrome around it, so they mean the same under every theme. Change one here
/// and tell the site, or the same green stops being the same green.
pub mod signal {
    use egui::Color32;

    /// Good: a healthy rating, a check that passed, a save that landed.
    pub const GOOD: Color32 = Color32::from_rgb(0x82, 0xd2, 0x96);
    /// Warn: a permission, an unsaved change, something that wants a look.
    pub const WARN: Color32 = Color32::from_rgb(0xe0, 0xb0, 0x50);
    /// Bad: a failure, an incompatibility, a refusal.
    pub const BAD: Color32 = Color32::from_rgb(0xe6, 0x78, 0x6e);
}

fn mix(a: Rgba, b: Rgba, t: f32) -> Rgba {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Rgba([l(a.0[0], b.0[0]), l(a.0[1], b.0[1]), l(a.0[2], b.0[2]), l(a.0[3], b.0[3])])
}

/// The visuals for `theme`.
pub fn visuals(theme: &Theme) -> egui::Visuals {
    let t = &theme.tokens;
    let sh = &theme.shape;
    let c = |r: Rgba| r.to_egui();
    let solid = |r: Rgba| t.solid(r).to_egui();
    let mut v = if theme.dark { egui::Visuals::dark() } else { egui::Visuals::light() };

    v.dark_mode = theme.dark;
    v.override_text_color = None;
    v.weak_text_color = Some(c(t.dim));
    // Panels are painted solid; a translucent ground reaches the screen only
    // where a host paints a region's backdrop under it (see `paint`).
    v.panel_fill = solid(t.ground);
    v.window_fill = solid(t.surface);
    v.faint_bg_color = c(t.surface_2);
    v.extreme_bg_color = c(t.well);
    v.text_edit_bg_color = Some(c(t.well));
    v.code_bg_color = c(t.well);
    v.hyperlink_color = c(t.accent_hi);
    v.warn_fg_color = signal::WARN;
    v.error_fg_color = signal::BAD;
    v.window_stroke = Stroke::new(sh.stroke, c(t.hairline));
    v.window_corner_radius = CornerRadius::same(sh.radius.round() as u8);
    v.menu_corner_radius = CornerRadius::same(sh.radius.round() as u8);
    let shadow = match &sh.shadow {
        None => egui::Shadow::NONE,
        Some(s) => egui::Shadow {
            offset: [s.offset.0.round() as i8, s.offset.1.round() as i8],
            blur: s.blur.round() as u8,
            spread: s.spread.round() as u8,
            color: c(s.color),
        },
    };
    v.window_shadow = shadow;
    v.popup_shadow = shadow;
    v.window_highlight_topmost = false;

    v.selection.bg_fill = c(t.selection);
    v.selection.stroke = Stroke::new(1.0, c(t.accent));
    v.text_cursor.stroke = Stroke::new(2.0, c(t.accent));

    let wr = CornerRadius::same(sh.widget_radius.round() as u8);
    let br = CornerRadius::same(sh.radius.round() as u8);
    let hair = Stroke::new(sh.stroke, c(t.hairline));
    let text = Stroke::new(1.0, c(t.text));
    let resting = t.surface_2;
    let lifted = mix(t.surface_2, Rgba([t.text.0[0], t.text.0[1], t.text.0[2], t.surface_2.0[3]]), 0.07);

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = c(t.surface);
    w.noninteractive.weak_bg_fill = c(t.surface);
    w.noninteractive.bg_stroke = hair;
    w.noninteractive.fg_stroke = text;
    w.noninteractive.corner_radius = br;
    w.noninteractive.expansion = 0.0;

    w.inactive.bg_fill = c(resting);
    w.inactive.weak_bg_fill = c(resting);
    w.inactive.bg_stroke = hair;
    w.inactive.fg_stroke = text;
    w.inactive.corner_radius = wr;
    w.inactive.expansion = 0.0;

    w.hovered.bg_fill = c(lifted);
    w.hovered.weak_bg_fill = c(lifted);
    w.hovered.bg_stroke = Stroke::new(sh.stroke, c(t.accent_edge));
    w.hovered.fg_stroke = text;
    w.hovered.corner_radius = wr;
    w.hovered.expansion = 0.0;

    let pressed = t.accent_wash.over(t.solid(lifted));
    w.active.bg_fill = c(pressed);
    w.active.weak_bg_fill = c(pressed);
    w.active.bg_stroke = Stroke::new(sh.stroke, c(t.accent));
    w.active.fg_stroke = text;
    w.active.corner_radius = wr;
    w.active.expansion = 0.0;

    w.open.bg_fill = c(lifted);
    w.open.weak_bg_fill = c(lifted);
    w.open.bg_stroke = Stroke::new(sh.stroke, c(t.accent_edge));
    w.open.fg_stroke = text;
    w.open.corner_radius = wr;
    w.open.expansion = 0.0;
    v
}

/// Everything a theme decides about a `Style`: visuals, the type sizes, the
/// spacing, the animation time. The host keeps the rest (its scroll gutter,
/// its interaction settings).
pub fn apply(style: &mut egui::Style, theme: &Theme, display: FontFamily) {
    style.visuals = visuals(theme);
    let f = &theme.fonts;
    let body = f.size;
    style.text_styles = [
        (TextStyle::Small, FontId::new((body * 0.78).round().max(8.0), FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(body, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(body, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(f.mono_size, FontFamily::Monospace)),
        (TextStyle::Heading, FontId::new((body * 1.45).round(), display)),
    ]
    .into();

    let d = theme.shape.density;
    let s = &mut style.spacing;
    s.item_spacing = egui::vec2(8.0 * d, 4.0 * d).round();
    s.button_padding = egui::vec2(6.0 * d, 2.0 * d).round();
    s.interact_size.y = (body * 1.5 * d).round().max(16.0);
    s.window_margin = egui::Margin::same((8.0 * d).round() as i8);
    s.menu_margin = egui::Margin::same((6.0 * d).round() as i8);
    style.animation_time = theme.animation_ms / 1000.0;
}

/// The colour of text drawn on a filled accent.
pub fn on_accent(theme: &Theme) -> Color32 {
    theme.tokens.on_accent.to_egui()
}
