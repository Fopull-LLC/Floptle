//! The value-or-curve affordance and the drawn-curve editor — phase 3 of the
//! particle system (`docs/particle-system-proposal.md` §6.3–6.4).
//!
//! A property is a constant OR a curve over a normalized domain (`[0,1]` — the
//! particle's life, or effect time for automation lanes). Constants edit inline;
//! a `📈` promotes to a curve, which shows a sparkline that expands into the graph
//! editor: draggable keys, per-key interpolation (constant / linear / bezier with
//! tangent handles), click-empty to add, right-click to delete. Colour curves get
//! a gradient strip so colour-shift and alpha-fade edit in one place.
//!
//! Curves are sampled for drawing by converting the DTO to the runtime
//! [`floptle_vfx::Curve`] (one shared, tested evaluator — no duplicate math).

use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};
use floptle_scene::{VfxCurveDoc, VfxExtrapolateDoc, VfxInterpDoc, VfxKeyDoc, VfxPropDoc, VfxValueDoc};

use crate::vfx::curve_from_doc;

/// What a curve's values mean — drives the editor's channels + gradient.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CurveKind {
    /// A single scalar over the domain (size, rotation, a drag/gravity ramp).
    Scalar,
    /// A 3-vector (velocity) — three colour-coded channel lines.
    Vector,
    /// RGBA — a gradient strip for colour + an alpha line underneath.
    Color,
}

const GRAD_H: f32 = 16.0;
const GRAPH_H: f32 = 128.0;
const PAD: f32 = 6.0;

/// One channel's constant value, for the inline editor + curve seeding.
fn channels_of(v: &VfxValueDoc) -> Vec<f32> {
    match v {
        VfxValueDoc::F32(x) => vec![*x],
        VfxValueDoc::Vec3(v) => v.to_vec(),
        VfxValueDoc::Rgba(v) => v.to_vec(),
    }
}

fn kind_of(v: &VfxValueDoc) -> CurveKind {
    match v {
        VfxValueDoc::F32(_) => CurveKind::Scalar,
        VfxValueDoc::Vec3(_) => CurveKind::Vector,
        VfxValueDoc::Rgba(_) => CurveKind::Color,
    }
}

/// The inline value-or-curve row: label + constant editor + `📈` promote, or a
/// sparkline + `→const` demote when it's a curve. When expanded (its label is in
/// `expanded`), the full graph editor is shown below. Returns whether it changed.
pub(crate) fn value_or_curve(
    ui: &mut egui::Ui,
    label: &str,
    prop: &mut VfxPropDoc,
    expanded: &mut Option<String>,
    sel_key: &mut Option<usize>,
    vrange: &mut Option<(f32, f32)>,
    floor: Option<f32>,
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.add_sized([70.0, 18.0], egui::Label::new(label).truncate());
        match prop {
            VfxPropDoc::Const(v) => {
                changed |= const_editor(ui, v);
                // Both promote buttons render every frame; the value is read once
                // afterwards so reassigning `prop` can't conflict with `v`'s borrow.
                let to_range = ui
                    .small_button("🎲")
                    .on_hover_text("randomize per particle between two values")
                    .clicked();
                let to_curve = ui
                    .small_button("📈")
                    .on_hover_text("animate this over the particle's life (make a curve)")
                    .clicked();
                let start = *v;
                if to_range {
                    // Seed both bounds at the current value; the artist widens the high one.
                    *prop = VfxPropDoc::Range(start, start);
                    changed = true;
                } else if to_curve {
                    // Seed a flat curve at the current constant, first channel.
                    *prop = VfxPropDoc::Curve(VfxCurveDoc {
                        keys: vec![
                            VfxKeyDoc { t: 0.0, v: start, interp: VfxInterpDoc::Linear, in_tan: 0.0, out_tan: 0.0 },
                            VfxKeyDoc { t: 1.0, v: start, interp: VfxInterpDoc::Linear, in_tan: 0.0, out_tan: 0.0 },
                        ],
                        extrapolate: VfxExtrapolateDoc::Clamp,
                    });
                    *expanded = Some(label.to_string());
                    *vrange = None;
                    changed = true;
                }
            }
            VfxPropDoc::Range(a, b) => {
                changed |= const_editor(ui, a);
                ui.label("🎲")
                    .on_hover_text("random per particle between the low and high value");
                changed |= const_editor(ui, b);
                let demote = ui
                    .small_button("•")
                    .on_hover_text("back to a single value (the low bound)")
                    .clicked();
                let low = *a;
                if demote {
                    *prop = VfxPropDoc::Const(low);
                    changed = true;
                }
            }
            VfxPropDoc::Curve(c) => {
                let kind = c.keys.first().map(|k| kind_of(&k.v)).unwrap_or(CurveKind::Scalar);
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(90.0, 18.0), Sense::click());
                sparkline(ui, c, kind, rect);
                if resp.on_hover_text("click to edit the curve").clicked() {
                    *vrange = None;
                    *expanded = if expanded.as_deref() == Some(label) {
                        None
                    } else {
                        *sel_key = None;
                        Some(label.to_string())
                    };
                }
                if ui.small_button("•").on_hover_text("back to a constant (value at t=0)").clicked() {
                    let v0 = c.keys.first().map(|k| k.v).unwrap_or(VfxValueDoc::F32(0.0));
                    *prop = VfxPropDoc::Const(v0);
                    if expanded.as_deref() == Some(label) {
                        *expanded = None;
                    }
                    changed = true;
                }
            }
        }
    });
    if let VfxPropDoc::Curve(c) = prop
        && expanded.as_deref() == Some(label)
    {
        changed |= curve_editor(ui, c, sel_key, vrange, floor);
    }
    changed
}

/// Inline editor for a constant value (drag-float / xyz / colour swatch).
fn const_editor(ui: &mut egui::Ui, v: &mut VfxValueDoc) -> bool {
    let mut changed = false;
    match v {
        VfxValueDoc::F32(x) => {
            changed |= ui.add(egui::DragValue::new(x).speed(0.01)).changed();
        }
        VfxValueDoc::Vec3(xyz) => {
            for (i, p) in ["x", "y", "z"].iter().enumerate() {
                changed |= ui.add(egui::DragValue::new(&mut xyz[i]).speed(0.01).prefix(*p)).changed();
            }
        }
        VfxValueDoc::Rgba(rgba) => {
            changed |= ui.color_edit_button_rgba_unmultiplied(rgba).changed();
        }
    }
    changed
}

/// A tiny inline preview of a curve's shape (scalar/vector line, or a colour
/// gradient), drawn in `rect`.
pub(crate) fn sparkline(ui: &egui::Ui, curve: &VfxCurveDoc, kind: CurveKind, rect: Rect) {
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
    let rt = curve_from_doc(curve);
    if kind == CurveKind::Color {
        // Gradient of rgb (alpha shown as a thin line so a fade still reads).
        let n = rect.width().max(2.0) as usize;
        for i in 0..n {
            let u = i as f32 / (n - 1).max(1) as f32;
            let c = rt.eval(u);
            let x = rect.left() + u * rect.width();
            painter.line_segment(
                [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
                Stroke::new(1.5, Color32::from_rgb((c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8)),
            );
        }
        return;
    }
    let chans = curve.keys.first().map(|k| channels_of(&k.v).len()).unwrap_or(1);
    let cols = [Color32::from_rgb(230, 120, 120), Color32::from_rgb(120, 220, 120), Color32::from_rgb(120, 160, 240)];
    let (lo, hi) = value_range(&rt, chans, None);
    for (ch, col) in cols.iter().enumerate().take(chans) {
        let mut pts = Vec::new();
        let n = 24;
        for i in 0..=n {
            let u = i as f32 / n as f32;
            let val = rt.eval(u)[ch];
            let y = rect.bottom() - ((val - lo) / (hi - lo).max(1e-4)) * rect.height();
            pts.push(Pos2::new(rect.left() + u * rect.width(), y.clamp(rect.top(), rect.bottom())));
        }
        painter.add(egui::Shape::line(pts, Stroke::new(1.0, if chans == 1 { crate::timeline::KEY_COLOR } else { *col })));
    }
}

/// Auto-fit value range across all channels of a runtime curve, sampled.
///
/// **Zero is kept in frame when it is anywhere near the data.** A curve that has
/// never been negative used to fit to itself, so there was no zero line to see —
/// and you found out you had crossed zero by the line appearing *after* you did
/// it, which is exactly backwards. Once the curve is a long way from zero (more
/// than its own span away) it is squashed rather than helped by including it, so
/// there the axis labels carry the meaning instead.
fn value_range(rt: &floptle_vfx::Curve, chans: usize, floor: Option<f32>) -> (f32, f32) {
    let (mut lo, mut hi) = (f32::MAX, f32::MIN);
    for i in 0..=32 {
        let c = rt.eval(i as f32 / 32.0);
        for &v in c.iter().take(chans) {
            lo = lo.min(v);
            hi = hi.max(v);
        }
    }
    if !lo.is_finite() || !hi.is_finite() {
        return (0.0, 1.0);
    }
    let span = hi - lo;
    if lo > 0.0 && lo <= span.max(0.25) {
        lo = 0.0;
    } else if hi < 0.0 && -hi <= span.max(0.25) {
        hi = 0.0;
    }
    // A generous minimum span keeps flat/near-flat curves comfortably draggable
    // (a tiny fitted range would otherwise map the whole graph height to a sliver
    // of value, making the key feel stuck).
    let pad = ((hi - lo) * 0.15).max(0.25);
    let (mut lo, hi) = (lo - pad, hi + pad);
    // A property with a floor does not get graph below it — a size axis that
    // shows -0.4 invites a value that means nothing.
    if let Some(f) = floor {
        lo = lo.max(f);
    }
    (lo, hi)
}

/// The full graph editor for one curve. Domain is the normalized `[0,1]`; the
/// value axis auto-fits. Colour curves add a gradient strip + colour-stop row.
pub(crate) fn curve_editor(
    ui: &mut egui::Ui,
    curve: &mut VfxCurveDoc,
    sel_key: &mut Option<usize>,
    vrange: &mut Option<(f32, f32)>,
    // `floor`: a value this property cannot meaningfully go below (a size of
    // -0.4 is not a small particle, it is a wrong one). `None` for genuinely
    // signed properties like velocity and rotation.
    floor: Option<f32>,
) -> bool {
    let mut changed = false;
    let kind = curve.keys.first().map(|k| kind_of(&k.v)).unwrap_or(CurveKind::Scalar);

    // Header: extrapolate mode + selected-key interp + delete.
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("curve_extrap")
            .width(84.0)
            .selected_text(match curve.extrapolate {
                VfxExtrapolateDoc::Clamp => "clamp ends",
                VfxExtrapolateDoc::Repeat => "repeat",
            })
            .show_ui(ui, |ui| {
                for (v, l) in [(VfxExtrapolateDoc::Clamp, "clamp ends"), (VfxExtrapolateDoc::Repeat, "repeat")] {
                    if ui.selectable_label(curve.extrapolate == v, l).clicked() && curve.extrapolate != v {
                        curve.extrapolate = v;
                        changed = true;
                    }
                }
            });
        if let Some(k) = sel_key.and_then(|i| curve.keys.get_mut(i)) {
            egui::ComboBox::from_id_salt("curve_interp")
                .width(72.0)
                .selected_text(match k.interp {
                    VfxInterpDoc::Constant => "hold",
                    VfxInterpDoc::Linear => "linear",
                    VfxInterpDoc::Bezier => "smooth",
                })
                .show_ui(ui, |ui| {
                    for (v, l) in [(VfxInterpDoc::Constant, "hold"), (VfxInterpDoc::Linear, "linear"), (VfxInterpDoc::Bezier, "smooth")] {
                        if ui.selectable_label(k.interp == v, l).clicked() && k.interp != v {
                            k.interp = v;
                            changed = true;
                        }
                    }
                });
        }
        if let Some(i) = *sel_key
            && curve.keys.len() > 1
            && ui.small_button("🗑 key").clicked()
        {
            curve.keys.remove(i);
            *sel_key = None;
            changed = true;
        }
        // Re-fitting is something you ASK for. It used to happen every time the
        // pointer lifted, which is why the same curve kept being drawn at a
        // different scale.
        if ui
            .small_button("⛶")
            .on_hover_text("fit the value axis to the curve — it does not do this on its own")
            .clicked()
        {
            *vrange = None;
        }
    });

    let width = ui.available_width().clamp(160.0, 280.0);
    let total_h = if kind == CurveKind::Color { GRAPH_H + GRAD_H + PAD } else { GRAPH_H };
    let (area, _) = ui.allocate_exact_size(Vec2::new(width, total_h), Sense::hover());
    let painter = ui.painter_at(area);

    let rt = curve_from_doc(curve);
    // Colour: gradient strip on top (rgb), the graph edits alpha below.
    let graph = if kind == CurveKind::Color {
        let strip = Rect::from_min_size(area.min, Vec2::new(area.width(), GRAD_H));
        let n = strip.width().max(2.0) as usize;
        for i in 0..n {
            let u = i as f32 / (n - 1).max(1) as f32;
            let c = rt.eval(u);
            let x = strip.left() + u * strip.width();
            painter.line_segment(
                [Pos2::new(x, strip.top()), Pos2::new(x, strip.bottom())],
                Stroke::new(1.5, Color32::from_rgb((c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8)),
            );
        }
        painter.text(Pos2::new(strip.left() + 3.0, strip.center().y), Align2::LEFT_CENTER, "colour", FontId::proportional(9.0), Color32::from_black_alpha(160));
        Rect::from_min_size(Pos2::new(area.left(), strip.bottom() + PAD), Vec2::new(area.width(), GRAPH_H))
    } else {
        area
    };
    painter.rect_filled(graph, 3.0, ui.visuals().extreme_bg_color);

    // Which channel(s) the graph plots: color edits alpha (channel 3); vector plots xyz; scalar plots 0.
    let plot_chans: Vec<usize> = match kind {
        CurveKind::Scalar => vec![0],
        CurveKind::Color => vec![3],
        CurveKind::Vector => vec![0, 1, 2],
    };
    // ---- the value axis ----------------------------------------------------
    //
    // It used to REFIT every time the pointer lifted, so the same curve was drawn
    // at a different scale after each edit and a key you dragged upward sprang
    // back toward the middle. An axis that moves under you is an axis you cannot
    // read a change against, which was most of "it's hard to tell how my change
    // will affect it".
    //
    // So: fit once, then keep it. It may only GROW, and only when the curve has
    // actually left it — that way an edit can never push a key off-screen, and
    // the graph is never redrawn smaller than it was a moment ago. Frozen
    // outright mid-drag, which is the original NaN guard: an axis that grew while
    // a key was held remapped the same pointer position to an ever-larger value,
    // a runaway that overflowed a screen coordinate and panicked the tessellator.
    let pointer_down = ui.input(|i| i.pointer.any_down());
    let (lo, hi) = match kind {
        CurveKind::Color => (0.0, 1.0),
        _ => {
            let fit = value_range(&rt, plot_chans.len(), floor);
            match *vrange {
                Some(r) if pointer_down => r,
                Some(r) => {
                    let grown = (r.0.min(fit.0), r.1.max(fit.1));
                    *vrange = Some(grown);
                    grown
                }
                None => {
                    *vrange = Some(fit);
                    fit
                }
            }
        }
    };
    let to_y = |val: f32| graph.bottom() - ((val - lo) / (hi - lo).max(1e-4)) * graph.height();
    let to_x = |t: f32| graph.left() + t.clamp(0.0, 1.0) * graph.width();
    let x_to_t = |x: f32| ((x - graph.left()) / graph.width()).clamp(0.0, 1.0);

    // Gridlines + zero line.
    let grid = ui.visuals().weak_text_color().gamma_multiply(0.25);
    for i in 1..4 {
        let x = graph.left() + i as f32 / 4.0 * graph.width();
        painter.line_segment([Pos2::new(x, graph.top()), Pos2::new(x, graph.bottom())], Stroke::new(0.5, grid));
    }
    draw_value_axis(&painter, ui, graph, lo, hi, &to_y);

    // The curve polyline(s).
    let cols = [Color32::from_rgb(230, 120, 120), Color32::from_rgb(120, 220, 120), Color32::from_rgb(120, 160, 240)];
    for (ci, &ch) in plot_chans.iter().enumerate() {
        let mut pts = Vec::new();
        let n = 64;
        for i in 0..=n {
            let u = i as f32 / n as f32;
            pts.push(Pos2::new(to_x(u), to_y(rt.eval(u)[ch]).clamp(graph.top(), graph.bottom())));
        }
        let col = if plot_chans.len() == 1 { crate::timeline::ACCENT } else { cols[ci] };
        painter.add(egui::Shape::line(pts, Stroke::new(1.5, col)));
    }

    // Add a key on empty double-click inside the graph.
    let graph_resp = ui.interact(graph, ui.id().with("curve_graph"), Sense::click());
    if graph_resp.double_clicked()
        && let Some(p) = graph_resp.interact_pointer_pos()
    {
        let t = x_to_t(p.x);
        // New key: primary channel from the click's height, others sampled at t.
        let sampled = rt.eval(t);
        let val_from_y = lo + (graph.bottom() - p.y) / graph.height() * (hi - lo);
        let chans = match kind {
            CurveKind::Scalar => vec![val_from_y],
            CurveKind::Color => vec![sampled[0], sampled[1], sampled[2], val_from_y.clamp(0.0, 1.0)],
            CurveKind::Vector => sampled[..3].to_vec(),
        };
        curve.keys.push(VfxKeyDoc { t, v: value_from_channels(&chans), interp: VfxInterpDoc::Linear, in_tan: 0.0, out_tan: 0.0 });
        curve.keys.sort_by(|a, b| a.t.total_cmp(&b.t));
        *sel_key = curve.keys.iter().position(|k| (k.t - t).abs() < 1e-6);
        changed = true;
    }

    // Draggable key handles (primary channel controls the y; time is shared).
    let prim = plot_chans[0];
    let mut retime: Option<(usize, f32, f32)> = None;
    for i in 0..curve.keys.len() {
        let (kt, kv) = { let k = &curve.keys[i]; (k.t, channels_of(&k.v)) };
        let yv = if prim < kv.len() { kv[prim] } else { 0.0 };
        let c = Pos2::new(to_x(kt), to_y(yv));
        let hit = Rect::from_center_size(c, Vec2::splat(11.0));
        let resp = ui.interact(hit, ui.id().with(("curve_key", i)), Sense::click_and_drag());
        let selected = *sel_key == Some(i);
        painter.circle(c, if selected { 5.0 } else { 4.0 }, if selected { crate::timeline::ACCENT } else { crate::timeline::KEY_COLOR }, Stroke::new(1.0, Color32::from_black_alpha(160)));
        if resp.clicked() {
            *sel_key = Some(i);
        }
        if resp.dragged()
            && let Some(p) = resp.interact_pointer_pos()
        {
            let nt = x_to_t(p.x);
            // Finite clamp is a backstop against a NaN/inf ever reaching the DTO
            // (the frozen range above is the real fix; this just guarantees sanity).
            let nv = (lo + (graph.bottom() - p.y) / graph.height() * (hi - lo)).clamp(
                if kind == CurveKind::Color { 0.0 } else { -1.0e6 },
                if kind == CurveKind::Color { 1.0 } else { 1.0e6 },
            );
            let k = &mut curve.keys[i];
            let mut ch = channels_of(&k.v);
            if prim < ch.len() {
                ch[prim] = nv;
            }
            k.v = value_from_channels(&ch);
            retime = Some((i, k.t, nt));
            *sel_key = Some(i);
            changed = true;
        }
    }
    if let Some((i, _old, nt)) = retime {
        curve.keys[i].t = nt.clamp(0.0, 1.0);
        // Re-sort and follow the moved key so the selection stays on it.
        let moved = curve.keys[i];
        curve.keys.sort_by(|a, b| a.t.total_cmp(&b.t));
        *sel_key = curve.keys.iter().position(|k| k.t == moved.t && k.v == moved.v);
    }

    // Tangent handles for a selected bezier key.
    if let Some(i) = *sel_key
        && let Some(k) = curve.keys.get(i).copied()
        && k.interp == VfxInterpDoc::Bezier
    {
        changed |= tangent_handles(ui, &painter, curve, i, prim, &graph, lo, hi);
    }

    // Per-channel numeric editor for the selected key (so exact values are reachable).
    if let Some(i) = *sel_key
        && let Some(k) = curve.keys.get_mut(i)
    {
        ui.horizontal(|ui| {
            ui.small(format!("key {i} @"));
            changed |= ui.add(egui::DragValue::new(&mut k.t).speed(0.005).range(0.0..=1.0)).changed();
            changed |= const_editor(ui, &mut k.v);
        });
        k.t = k.t.clamp(0.0, 1.0);
    }
    changed
}

/// A number as short as it can be and still be read at 9pt: `0`, `1.5`, `-0.25`.
fn axis_label(v: f32) -> String {
    if v == 0.0 {
        "0".into()
    } else if v.abs() >= 100.0 {
        format!("{v:.0}")
    } else if v.abs() >= 1.0 {
        format!("{v:.2}").trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        format!("{v:.3}").trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// Label the value axis, and say where zero is.
///
/// The graph used to carry no numbers at all — an unlabelled point in an
/// unlabelled box — and drew its zero line only `if lo < 0.0 && hi > 0.0`, i.e.
/// only once the curve had already gone negative. Both halves of "I can't tell
/// what this is going to do" (`floptle/0098`).
///
/// When zero is off the top or bottom, the edge says so rather than the graph
/// staying silent about which side of it you are on.
fn draw_value_axis(
    painter: &egui::Painter,
    ui: &egui::Ui,
    graph: Rect,
    lo: f32,
    hi: f32,
    to_y: &impl Fn(f32) -> f32,
) {
    let faint = ui.visuals().weak_text_color();
    let mark = |v: f32, at: Pos2, anchor: Align2| {
        painter.text(at, anchor, axis_label(v), FontId::proportional(9.0), faint);
    };
    mark(hi, Pos2::new(graph.left() + 3.0, graph.top() + 2.0), Align2::LEFT_TOP);
    mark(lo, Pos2::new(graph.left() + 3.0, graph.bottom() - 2.0), Align2::LEFT_BOTTOM);

    if (lo..=hi).contains(&0.0) {
        let y0 = to_y(0.0);
        painter.line_segment(
            [Pos2::new(graph.left(), y0), Pos2::new(graph.right(), y0)],
            Stroke::new(1.0, faint.gamma_multiply(0.8)),
        );
        // Only worth labelling when it is not already one of the two extents.
        if lo != 0.0 && hi != 0.0 {
            painter.text(
                Pos2::new(graph.right() - 3.0, y0 - 1.0),
                Align2::RIGHT_BOTTOM,
                "0",
                FontId::proportional(9.0),
                faint,
            );
        }
    } else {
        // Off-screen: say which way, so "am I near zero" is still answerable.
        let (at, anchor, text) = if lo > 0.0 {
            (Pos2::new(graph.right() - 3.0, graph.bottom() - 2.0), Align2::RIGHT_BOTTOM, "0 ↓")
        } else {
            (Pos2::new(graph.right() - 3.0, graph.top() + 2.0), Align2::RIGHT_TOP, "0 ↑")
        };
        painter.text(at, anchor, text, FontId::proportional(9.0), faint);
    }
}

/// Drag the in/out tangent handles of a bezier key (slope in value-units per unit t).
#[allow(clippy::too_many_arguments)]
fn tangent_handles(
    ui: &egui::Ui,
    painter: &egui::Painter,
    curve: &mut VfxCurveDoc,
    i: usize,
    ch: usize,
    graph: &Rect,
    lo: f32,
    hi: f32,
) -> bool {
    let mut changed = false;
    let key = curve.keys[i];
    let kv = channels_of(&key.v).get(ch).copied().unwrap_or(0.0);
    let px_t = graph.width().max(1.0);
    let px_v = graph.height().max(1.0) / (hi - lo).max(1e-4);
    let cx = graph.left() + key.t * graph.width();
    let cy = graph.bottom() - (kv - lo) / (hi - lo).max(1e-4) * graph.height();
    // Handles sit a fixed pixel distance out along the tangent slope.
    let handle = |slope: f32, dir: f32| {
        let dt = 0.12 * dir; // in normalized-t
        let dx = dt * px_t;
        let dv = slope * dt;
        let dy = -dv * px_v;
        Pos2::new(cx + dx, cy + dy)
    };
    for (out, dir) in [(true, 1.0f32), (false, -1.0f32)] {
        let slope = if out { key.out_tan } else { key.in_tan };
        let hp = handle(slope, dir);
        painter.line_segment([Pos2::new(cx, cy), hp], Stroke::new(1.0, crate::timeline::ACCENT.gamma_multiply(0.7)));
        let hit = Rect::from_center_size(hp, Vec2::splat(9.0));
        let resp = ui.interact(hit, ui.id().with(("curve_tan", i, out)), Sense::drag());
        painter.circle(hp, 3.0, crate::timeline::ACCENT, Stroke::NONE);
        if resp.dragged()
            && let Some(p) = resp.interact_pointer_pos()
        {
            // Recover slope from the dragged handle position.
            let dx = (p.x - cx) / px_t;
            let dv = -(p.y - cy) / px_v;
            if dx.abs() > 1e-3 {
                let s = dv / dx;
                if out {
                    curve.keys[i].out_tan = s;
                } else {
                    curve.keys[i].in_tan = s;
                }
                changed = true;
            }
        }
    }
    let _ = StrokeKind::Inside; // (silence unused import on some builds)
    changed
}

fn value_from_channels(ch: &[f32]) -> VfxValueDoc {
    match ch.len() {
        1 => VfxValueDoc::F32(ch[0]),
        3 => VfxValueDoc::Vec3([ch[0], ch[1], ch[2]]),
        _ => {
            let mut v = [0.0; 4];
            for (i, c) in ch.iter().take(4).enumerate() {
                v[i] = *c;
            }
            VfxValueDoc::Rgba(v)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use floptle_scene::{VfxCurveDoc, VfxExtrapolateDoc, VfxInterpDoc, VfxKeyDoc, VfxValueDoc};

    fn curve(vals: &[(f32, f32)]) -> VfxCurveDoc {
        VfxCurveDoc {
            keys: vals
                .iter()
                .map(|&(t, v)| VfxKeyDoc {
                    t,
                    v: VfxValueDoc::F32(v),
                    interp: VfxInterpDoc::Linear,
                    in_tan: 0.0,
                    out_tan: 0.0,
                })
                .collect(),
            extrapolate: VfxExtrapolateDoc::Clamp,
        }
    }

    /// The reported bug: a curve that has never been negative got no zero line,
    /// so you crossed zero with no landmark and the line appeared AFTER the
    /// mistake. Zero is in frame while the curve is anywhere near it.
    #[test]
    fn zero_is_in_frame_before_a_curve_goes_negative() {
        let rt = curve_from_doc(&curve(&[(0.0, 1.0), (1.0, 2.0)]));
        let (lo, hi) = value_range(&rt, 1, None);
        assert!(lo <= 0.0 && hi >= 0.0, "zero must be plottable, got {lo}..{hi}");
    }

    /// A curve a long way from zero is squashed rather than helped by including
    /// it — there the axis labels carry the meaning instead.
    #[test]
    fn a_curve_far_from_zero_is_not_squashed_to_reach_it() {
        let rt = curve_from_doc(&curve(&[(0.0, 900.0), (1.0, 901.0)]));
        let (lo, hi) = value_range(&rt, 1, None);
        assert!(lo > 100.0, "a curve at 900 should not plot from 0, got {lo}..{hi}");
        assert!(hi - lo < 50.0, "and should still be readable, got {lo}..{hi}");
    }

    /// A property with a floor gets no graph below it: a size axis showing -0.4
    /// invites a value that means nothing.
    #[test]
    fn a_floored_property_never_shows_axis_below_its_floor() {
        let rt = curve_from_doc(&curve(&[(0.0, 0.0), (1.0, 1.0)]));
        let (lo, _) = value_range(&rt, 1, Some(0.0));
        assert_eq!(lo, 0.0);
        // ...and without a floor the padding below zero is still there, because
        // a velocity curve sitting at 0 genuinely may go either way.
        let (lo, _) = value_range(&rt, 1, None);
        assert!(lo < 0.0);
    }

    /// Every extent gets a label short enough to read at 9pt, and zero says "0"
    /// rather than "0.000".
    #[test]
    fn axis_labels_are_short() {
        assert_eq!(axis_label(0.0), "0");
        assert_eq!(axis_label(1.0), "1");
        assert_eq!(axis_label(1.5), "1.5");
        assert_eq!(axis_label(-0.25), "-0.25");
        assert_eq!(axis_label(1234.0), "1234");
        assert!(axis_label(0.125).len() <= 5);
    }
}
