//! The brand's components (contract §5), for the Hub and the editor alike.
//!
//! Four type steps (title, section, body, fine), mono for identity and data,
//! one panel treatment, one filled primary action per view. Every colour here
//! is read from the applied theme, so each of these looks right in all of
//! them; the three [`crate::signal`] colours are the only fixed ones.

use egui::{Color32, CornerRadius, FontId, RichText, Stroke, Ui};

use crate::color::Rgba;

fn base(ui: &Ui) -> f32 {
    ui.style().text_styles.get(&egui::TextStyle::Body).map_or(13.0, |f| f.size)
}

fn tokens(ui: &Ui) -> std::sync::Arc<crate::Theme> {
    crate::paint::theme(ui.ctx())
}

/// The theme's accent.
pub fn accent(ui: &Ui) -> Color32 {
    tokens(ui).tokens.accent.to_egui()
}

/// A token of the applied theme, as egui wants it.
pub fn token(ui: &Ui, f: impl Fn(&crate::Tokens) -> Rgba) -> Color32 {
    f(&tokens(ui).tokens).to_egui()
}

/// The one biggest thing in a view, in the display face.
pub fn title(ui: &Ui, s: impl Into<String>) -> RichText {
    RichText::new(s)
        .font(FontId::new((base(ui) * 1.5).round(), crate::fonts::display_family(ui.ctx())))
        .color(token(ui, |t| t.text))
}

/// What a group of rows is.
pub fn section(ui: &Ui, s: impl Into<String>) -> RichText {
    RichText::new(s).size((base(ui) * 1.15).round()).variation("wght", 600.0).color(token(ui, |t| t.text))
}

/// Labels, counts, timestamps: the quiet half of a row.
pub fn fine(ui: &Ui, s: impl Into<String>) -> RichText {
    RichText::new(s).size((base(ui) * 0.85).round()).color(token(ui, |t| t.dim))
}

/// Fine print that is the point: a refusal, a permission.
pub fn fine_strong(ui: &Ui, s: impl Into<String>) -> RichText {
    RichText::new(s).size((base(ui) * 0.85).round()).variation("wght", 600.0).color(token(ui, |t| t.text))
}

/// Identity and data: ids, versions, paths, codes. Mono, in the text colour,
/// because an id is content.
pub fn data(ui: &Ui, s: impl Into<String>) -> RichText {
    RichText::new(s).size((base(ui) * 0.85).round()).monospace().color(token(ui, |t| t.text))
}

/// The small uppercase tracked label (`INSTALL BY HAND`). Six words, not
/// three lines.
pub fn label(ui: &Ui, s: impl Into<String>) -> RichText {
    RichText::new(s.into().to_uppercase())
        .size((base(ui) * 0.72).round().max(8.0))
        .monospace()
        .extra_letter_spacing(1.4)
        .color(token(ui, |t| t.faint))
}

/// One hairline.
pub fn hairline(ui: &Ui) -> Stroke {
    let t = tokens(ui);
    Stroke::new(t.shape.stroke, t.tokens.hairline.to_egui())
}

/// **The** panel: one fill step up from the ground, a hairline, the theme's
/// radius.
pub fn panel(ui: &Ui) -> egui::Frame {
    let t = tokens(ui);
    egui::Frame::new()
        .inner_margin(10)
        .corner_radius(CornerRadius::same(t.shape.radius.round() as u8))
        .fill(t.tokens.surface.to_egui())
        .stroke(Stroke::new(t.shape.stroke, t.tokens.hairline.to_egui()))
}

/// The panel that is chosen: the accent's edge and wash, not a different
/// treatment.
pub fn panel_selected(ui: &Ui) -> egui::Frame {
    let t = tokens(ui);
    panel(ui)
        .fill(t.tokens.accent_wash.over(t.tokens.solid(t.tokens.surface)).to_egui())
        .stroke(Stroke::new(t.shape.stroke, t.tokens.accent_edge.to_egui()))
}

/// **The one primary action in a view**: filled accent, `on_accent` text.
/// Everything else is an ordinary button. Every theme declares `on_accent`,
/// which is what makes a fill safe to use (§5).
pub fn primary(ui: &Ui, text: impl Into<String>) -> egui::Button<'static> {
    let t = tokens(ui);
    egui::Button::new(RichText::new(text).variation("wght", 600.0).color(t.tokens.on_accent.to_egui()))
        .fill(t.tokens.accent.to_egui())
        .stroke(Stroke::new(t.shape.stroke, t.tokens.accent.to_egui()))
        .corner_radius(CornerRadius::same(t.shape.radius.round() as u8))
}

/// What a chip is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chip {
    Neutral,
    On,
    Good,
    Warn,
    Bad,
}

/// A chip: mono, small, a hairline and the small radius. Neutral by default;
/// the others tint text, edge and a wash.
pub fn chip(ui: &mut Ui, text: impl Into<String>, kind: Chip) -> egui::Response {
    let t = tokens(ui);
    let k = &t.tokens;
    let tint = match kind {
        Chip::Neutral => None,
        Chip::On => Some(k.accent.to_egui()),
        Chip::Good => Some(crate::signal::GOOD),
        Chip::Warn => Some(crate::signal::WARN),
        Chip::Bad => Some(crate::signal::BAD),
    };
    let (fg, edge, fill) = match tint {
        None => (k.dim.to_egui(), k.hairline.to_egui(), k.surface_2.to_egui()),
        Some(c) => (c, c.gamma_multiply(0.5), c.gamma_multiply(0.1)),
    };
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(6, 1))
        .corner_radius(CornerRadius::same(t.shape.radius_small.round() as u8))
        .fill(fill)
        .stroke(Stroke::new(1.0, edge))
        .show(ui, |ui| ui.label(RichText::new(text).size((base(ui) * 0.8).round()).monospace().color(fg)))
        .response
}

/// The active-tab treatment: plain text, the active one in the text colour
/// over a 2px accent underline (the site's section bar).
pub fn tab(ui: &mut Ui, selected: bool, text: impl Into<String>) -> egui::Response {
    let t = tokens(ui);
    let color = if selected { t.tokens.text.to_egui() } else { t.tokens.dim.to_egui() };
    let r = ui.add(
        egui::Button::new(RichText::new(text).color(color).variation("wght", if selected { 560.0 } else { 420.0 }))
            .frame(false)
            .min_size(egui::vec2(0.0, ui.spacing().interact_size.y + 6.0)),
    );
    let r = if r.hovered() && !selected {
        ui.painter().hline(r.rect.x_range(), r.rect.bottom() - 1.0, Stroke::new(1.0, t.tokens.hairline.to_egui()));
        r.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        r
    };
    if selected {
        ui.painter().hline(r.rect.x_range(), r.rect.bottom() - 1.0, Stroke::new(2.0, t.tokens.accent.to_egui()));
    }
    r
}

/// The logo's soft glow (contract §5): a radial wash of the accent behind
/// `rect`, never a tile.
pub fn glow(ui: &Ui, rect: egui::Rect, strength: f32) {
    let t = tokens(ui);
    let c = t.tokens.accent;
    let center = rect.center();
    let r = rect.size().max_elem() * 0.9;
    let mut mesh = egui::Mesh::default();
    const N: u32 = 48;
    mesh.colored_vertex(center, c.with_alpha_mul(0.28 * strength).to_egui());
    for i in 0..=N {
        let a = i as f32 / N as f32 * std::f32::consts::TAU;
        mesh.colored_vertex(center + egui::vec2(a.cos(), a.sin()) * r, Color32::TRANSPARENT);
    }
    for i in 1..=N {
        mesh.add_triangle(0, i, i + 1);
    }
    ui.painter().add(egui::Shape::mesh(mesh));
}
