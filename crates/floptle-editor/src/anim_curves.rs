//! The Animating tab's curve view: one node's position, rotation and scale
//! drawn as values over time, with the keys as points you drag in both time
//! and value.
//!
//! The dopesheet answers *when*; this answers *how much* and *how fast*. The
//! curves are sampled through the same runtime tracks the game plays, so an
//! eased or smooth key draws exactly the motion it produces. Rotation is shown
//! as Euler degrees, unwrapped along time so a turn past 180° does not fold.

use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Stroke};
use floptle_core::math::{EulerRot, Quat, Vec3};
use floptle_scene::{key_mode_at, same_key_time, set_key_mode, AnimClipDoc, AnimInterpDoc};

use crate::anim::{track3_from_doc, track4_from_doc};
use crate::anim_ui::{interp_menu, key_diamond, key_hit, key_size, lane_of, lane_of_mut, Lane};
use crate::timeline::{draw_ruler, nice_step, snap_time, TimelineView, ACCENT, PLAYHEAD};

/// The view's own state, on [`crate::anim_ui::AnimUiState`].
#[derive(Default)]
pub(crate) struct CurveView {
    /// Curves instead of the dopesheet.
    pub on: bool,
    /// The channel (node name) whose curves are shown; `None` = the first.
    pub channel: Option<String>,
    /// Which lanes are drawn: position, rotation, scale.
    pub lanes: [bool; 3],
    /// The value axis: `None` fits the visible curves; set by a zoom or a pan.
    pub vrange: Option<(f32, f32)>,
    /// A key being dragged: (lane, component, original time, previewed time,
    /// the key's displayed triple with the dragged component moved).
    pub drag: Option<(Lane, usize, f32, f32, [f32; 3])>,
}

impl CurveView {
    pub(crate) fn new() -> Self {
        Self { lanes: [true, true, true], ..Default::default() }
    }
}

/// What the view read from the frame that the sheet's owner applies afterwards.
#[derive(Default)]
pub(crate) struct CurveEdits {
    /// Move a key on one lane: (channel, lane, old time, new time).
    pub retime: Option<(usize, Lane, f32, f32)>,
    /// Set a key's value from its displayed triple: (channel, lane, time, xyz
    /// or Euler degrees).
    pub set_value: Option<(usize, Lane, f32, [f32; 3])>,
    /// A key's interpolation: (channel, lane, time, mode).
    pub mode: Option<(usize, Lane, f32, Option<AnimInterpDoc>)>,
    /// Insert a key at a time on one lane, holding the curve's value there.
    pub insert: Option<(usize, Lane, f32)>,
    /// Delete a key on one lane.
    pub delete: Option<(usize, Lane, f32)>,
    /// A key the pointer chose, for the shared selection: (channel, time).
    pub select: Option<(usize, f32)>,
}

const COMPONENT_COLORS: [Color32; 3] =
    [Color32::from_rgb(235, 100, 90), Color32::from_rgb(110, 210, 110), Color32::from_rgb(100, 150, 240)];
const COMPONENT_NAMES: [&str; 3] = ["x", "y", "z"];
const SAMPLES_PER_PX: f32 = 0.5;

fn euler_deg(q: Quat) -> [f32; 3] {
    let (x, y, z) = q.to_euler(EulerRot::XYZ);
    [x.to_degrees(), y.to_degrees(), z.to_degrees()]
}

/// Bring `deg` within 180° of `prev`, so a curve of angles reads as one line.
fn unwrap_deg(deg: f32, prev: f32) -> f32 {
    let mut d = deg;
    while d - prev > 180.0 {
        d -= 360.0;
    }
    while prev - d > 180.0 {
        d += 360.0;
    }
    d
}

/// The Euler triple for `q` nearest to `prev`. Every rotation has two XYZ
/// spellings — `(x, y, z)` and `(x+180, 180−y, z+180)` — and the one to draw is
/// whichever continues the curve, or a plain turn about one axis would fold
/// at 90° into a jump on all three lines.
fn euler_near(q: Quat, prev: Option<[f32; 3]>) -> [f32; 3] {
    let e = euler_deg(q);
    let Some(prev) = prev else { return e };
    let alt = [e[0] + 180.0, 180.0 - e[1], e[2] + 180.0];
    let near = |c: [f32; 3]| [unwrap_deg(c[0], prev[0]), unwrap_deg(c[1], prev[1]), unwrap_deg(c[2], prev[2])];
    let (a, b) = (near(e), near(alt));
    let dist = |c: [f32; 3]| (c[0] - prev[0]).abs() + (c[1] - prev[1]).abs() + (c[2] - prev[2]).abs();
    if dist(b) < dist(a) { b } else { a }
}

/// One lane's curves and keys as drawn.
struct LaneCurves {
    /// `n` samples per component across `[0, dur]`.
    samples: [Vec<f32>; 3],
    /// Each key's time and displayed triple, in the same continuity as the samples.
    keys: Vec<(f32, [f32; 3])>,
}

/// One lane sampled across `[0, dur]`, `n` points each, with its keys read in
/// the same walk so a key sits on its curve.
fn sample_lane(doc: &AnimClipDoc, ci: usize, lane: Lane, dur: f32, n: usize) -> Option<LaneCurves> {
    let ch = doc.channels.get(ci)?;
    let key_times: Vec<f32> = lane_of(ch, lane)?.times().to_vec();
    // The walk: sample times and key times together, in order.
    let mut walk: Vec<(f32, bool)> = (0..n).map(|i| (dur * i as f32 / (n - 1).max(1) as f32, false)).collect();
    walk.extend(key_times.iter().map(|&t| (t, true)));
    walk.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut samples = [Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n)];
    let mut keys = Vec::with_capacity(key_times.len());
    match lane {
        Lane::Translation | Lane::Scale => {
            let l = if lane == Lane::Translation { ch.translation.as_ref()? } else { ch.scale.as_ref()? };
            let track = track3_from_doc(l);
            for (t, is_key) in walk {
                let v = track.sample(t).unwrap_or(Vec3::ZERO).to_array();
                if is_key {
                    // The key's own value, not a sample beside it.
                    let i = l.times.iter().position(|&x| same_key_time(x, t));
                    keys.push((t, i.map(|i| l.values[i]).unwrap_or(v)));
                } else {
                    for c in 0..3 {
                        samples[c].push(v[c]);
                    }
                }
            }
        }
        Lane::Rotation => {
            let l = ch.rotation.as_ref()?;
            let track = track4_from_doc(l);
            let mut prev: Option<[f32; 3]> = None;
            for (t, is_key) in walk {
                let q = if is_key {
                    let i = l.times.iter().position(|&x| same_key_time(x, t));
                    i.map(|i| Quat::from_array(l.values[i]).normalize())
                        .unwrap_or_else(|| track.sample(t).unwrap_or(Quat::IDENTITY))
                } else {
                    track.sample(t).unwrap_or(Quat::IDENTITY)
                };
                let e = euler_near(q, prev);
                prev = Some(e);
                if is_key {
                    keys.push((t, e));
                } else {
                    for c in 0..3 {
                        samples[c].push(e[c]);
                    }
                }
            }
        }
    }
    Some(LaneCurves { samples, keys })
}

/// Set a key's value from its displayed triple: xyz, or Euler degrees.
pub(crate) fn set_key_triple(doc: &mut AnimClipDoc, ci: usize, lane: Lane, t: f32, v: [f32; 3]) -> bool {
    let Some(ch) = doc.channels.get_mut(ci) else { return false };
    match lane {
        Lane::Translation | Lane::Scale => {
            let Some(l) = (if lane == Lane::Translation { ch.translation.as_mut() } else { ch.scale.as_mut() }) else {
                return false;
            };
            let Some(i) = l.times.iter().position(|&x| same_key_time(x, t)) else { return false };
            l.values[i] = v;
            true
        }
        Lane::Rotation => {
            let Some(l) = ch.rotation.as_mut() else { return false };
            let Some(i) = l.times.iter().position(|&x| same_key_time(x, t)) else { return false };
            let q = Quat::from_euler(EulerRot::XYZ, v[0].to_radians(), v[1].to_radians(), v[2].to_radians());
            l.values[i] = q.normalize().to_array();
            true
        }
    }
}

/// Insert a key on one lane at `t` holding the curve's value there, so the
/// motion is unchanged until the key is moved.
pub(crate) fn insert_key_on_curve(doc: &mut AnimClipDoc, ci: usize, lane: Lane, t: f32) -> bool {
    let Some(ch) = doc.channels.get_mut(ci) else { return false };
    match lane {
        Lane::Translation | Lane::Scale => {
            let Some(l) = (if lane == Lane::Translation { ch.translation.as_mut() } else { ch.scale.as_mut() }) else {
                return false;
            };
            if l.times.iter().any(|&x| same_key_time(x, t)) {
                return false;
            }
            let v = track3_from_doc(l).sample(t).unwrap_or(Vec3::ZERO);
            let at = l.times.partition_point(|&x| x < t);
            l.times.insert(at, t);
            l.values.insert(at, v.to_array());
            true
        }
        Lane::Rotation => {
            let Some(l) = ch.rotation.as_mut() else { return false };
            if l.times.iter().any(|&x| same_key_time(x, t)) {
                return false;
            }
            let q = track4_from_doc(l).sample(t).unwrap_or(Quat::IDENTITY);
            let at = l.times.partition_point(|&x| x < t);
            l.times.insert(at, t);
            l.values.insert(at, q.to_array());
            true
        }
    }
}

/// The value axis' padded fit over what is drawn.
fn fit_range(curves: &[(Lane, LaneCurves)], lanes: [bool; 3]) -> (f32, f32) {
    let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
    for (lane, lc) in curves {
        if !lanes[*lane as usize] {
            continue;
        }
        for c in &lc.samples {
            for &v in c {
                lo = lo.min(v);
                hi = hi.max(v);
            }
        }
    }
    if !lo.is_finite() || !hi.is_finite() {
        return (-1.0, 1.0);
    }
    let pad = ((hi - lo) * 0.12).max(0.5);
    (lo - pad, hi + pad)
}

/// Draw the view into the space left in `ui`. Returns the edits the frame asked
/// for; the caller applies them to the doc it lent us.
#[allow(clippy::too_many_arguments)]
pub(crate) fn curves_ui(
    ui: &mut egui::Ui,
    cv: &mut CurveView,
    doc: &AnimClipDoc,
    dur: f32,
    px: f32,
    snap_fps: f32,
    label_w: f32,
    playhead: &mut f32,
    sel_keys: &[(usize, f32)],
) -> CurveEdits {
    let mut edits = CurveEdits::default();
    let ruler_h = 22.0;
    let avail = ui.available_size();
    let body_h = avail.y.max(160.0);
    let want_w = (label_w + dur * px + 140.0).max(avail.x);

    // Which channel: the chosen one if it still exists, else the one holding
    // the selection, else the first.
    let names: Vec<String> = doc.channels.iter().map(|c| c.node.clone()).collect();
    let ci = cv
        .channel
        .as_ref()
        .and_then(|n| names.iter().position(|x| x == n))
        .or_else(|| sel_keys.first().map(|&(ci, _)| ci).filter(|&ci| ci < names.len()))
        .unwrap_or(0);
    if names.is_empty() {
        ui.add_space(12.0);
        ui.vertical_centered(|ui| {
            ui.weak("no keys yet — ● Record then pose the node, and its curves appear here");
        });
        return edits;
    }
    cv.channel = Some(names[ci].clone());

    let area = egui::ScrollArea::horizontal().auto_shrink([false, false]).max_height(avail.y);
    let out = area.show(ui, |ui| {
        let (full, bg) = ui.allocate_exact_size(egui::vec2(want_w, body_h), Sense::click_and_drag());
        let painter = ui.painter_at(full);
        let graph = Rect::from_min_max(
            Pos2::new(full.left() + label_w, full.top() + ruler_h),
            Pos2::new(full.right(), full.bottom() - 4.0),
        );
        let view = TimelineView { left: graph.left(), px_per_s: px, duration: dur };
        painter.rect_filled(graph, 0.0, ui.visuals().extreme_bg_color.gamma_multiply(0.7));

        // ---- left column: the node and its lanes ----
        let mut y = full.top() + 6.0;
        let font = FontId::proportional(11.0);
        let label = if names[ci].is_empty() { "(this node)".to_string() } else { names[ci].clone() };
        painter.text(Pos2::new(full.left() + 6.0, y + 8.0), Align2::LEFT_CENTER, &label, font.clone(), ui.visuals().strong_text_color());
        y += 22.0;
        for lane in Lane::ALL {
            if lane_of(&doc.channels[ci], lane).is_none() {
                continue;
            }
            let r = Rect::from_min_size(Pos2::new(full.left() + 4.0, y), egui::vec2(label_w - 8.0, 18.0));
            let resp = ui.interact(r, ui.id().with(("curve-lane", lane as u8)), Sense::click());
            if resp.clicked() {
                cv.lanes[lane as usize] = !cv.lanes[lane as usize];
                cv.vrange = None;
            }
            let on = cv.lanes[lane as usize];
            let col = if on { lane.color() } else { ui.visuals().weak_text_color() };
            let style = match lane {
                Lane::Translation => "━",
                Lane::Rotation => "┅",
                Lane::Scale => "╍",
            };
            painter.text(
                Pos2::new(r.left() + 4.0, r.center().y),
                Align2::LEFT_CENTER,
                format!("{} {} {}", if on { "◉" } else { "○" }, style, lane.label()),
                font.clone(),
                col,
            );
            y += 20.0;
        }
        y += 6.0;
        for (c, name) in COMPONENT_NAMES.iter().enumerate() {
            painter.text(
                Pos2::new(full.left() + 12.0 + c as f32 * 18.0, y + 8.0),
                Align2::LEFT_CENTER,
                *name,
                font.clone(),
                COMPONENT_COLORS[c],
            );
        }

        // ---- the curves, sampled through the runtime ----
        let n = ((dur * px * SAMPLES_PER_PX) as usize).clamp(2, 4096);
        let mut curves: Vec<(Lane, LaneCurves)> = Vec::new();
        for lane in Lane::ALL {
            if let Some(s) = sample_lane(doc, ci, lane, dur, n) {
                curves.push((lane, s));
            }
        }
        // The value axis. One lane shown: its real values, with a grid, and a
        // zoom or pan sticks. Several lanes shown: units differ (metres against
        // degrees), so each lane fills the height on its own range, named at the
        // left in its colour.
        let shown: Vec<Lane> = curves.iter().map(|(l, _)| *l).filter(|l| cv.lanes[*l as usize]).collect();
        let single = shown.len() == 1;
        let range_of = |lane: Lane| -> (f32, f32) {
            if single {
                cv.vrange.unwrap_or_else(|| fit_range(&curves, cv.lanes))
            } else {
                let mut only = [false; 3];
                only[lane as usize] = true;
                fit_range(&curves, only)
            }
        };
        let to_y = |lane: Lane, v: f32| {
            let (lo, hi) = range_of(lane);
            graph.bottom() - (v - lo) / (hi - lo).max(1e-6) * graph.height()
        };
        let to_v = |lane: Lane, yy: f32| {
            let (lo, hi) = range_of(lane);
            lo + (graph.bottom() - yy) / graph.height() * (hi - lo).max(1e-6)
        };
        if let Some(&lane) = shown.first().filter(|_| single) {
            let (lo, hi) = range_of(lane);
            let vspan = (hi - lo).max(1e-6);
            let vstep = nice_step(vspan / (graph.height() / 40.0).max(1.0));
            let mut v = (lo / vstep).ceil() * vstep;
            while v <= hi {
                let yy = to_y(lane, v);
                let zero = v.abs() < 1e-6;
                painter.line_segment(
                    [Pos2::new(graph.left(), yy), Pos2::new(graph.right(), yy)],
                    Stroke::new(if zero { 1.0 } else { 0.5 }, Color32::from_gray(if zero { 110 } else { 60 })),
                );
                painter.text(
                    Pos2::new(graph.left() + 3.0, yy - 1.0),
                    Align2::LEFT_BOTTOM,
                    format!("{v:.3}").trim_end_matches('0').trim_end_matches('.').to_string(),
                    FontId::proportional(9.0),
                    Color32::from_gray(130),
                );
                v += vstep;
            }
        } else {
            for (i, &lane) in shown.iter().enumerate() {
                let (lo, hi) = range_of(lane);
                let unit = if lane == Lane::Rotation { "°" } else { "" };
                let x = graph.left() + 3.0 + i as f32 * 64.0;
                painter.text(
                    Pos2::new(x, graph.top() + 2.0),
                    Align2::LEFT_TOP,
                    format!("{hi:.2}{unit}"),
                    FontId::proportional(9.0),
                    lane.color(),
                );
                painter.text(
                    Pos2::new(x, graph.bottom() - 2.0),
                    Align2::LEFT_BOTTOM,
                    format!("{lo:.2}{unit}"),
                    FontId::proportional(9.0),
                    lane.color(),
                );
                let zero_y = to_y(lane, 0.0);
                if zero_y >= graph.top() && zero_y <= graph.bottom() {
                    painter.line_segment(
                        [Pos2::new(graph.left(), zero_y), Pos2::new(graph.right(), zero_y)],
                        Stroke::new(0.5, lane.color().gamma_multiply(0.35)),
                    );
                }
            }
        }

        for (lane, lc) in &curves {
            if !cv.lanes[*lane as usize] {
                continue;
            }
            for (c, vals) in lc.samples.iter().enumerate() {
                let pts: Vec<Pos2> = vals
                    .iter()
                    .enumerate()
                    .map(|(i, &vv)| Pos2::new(view.time_to_x(dur * i as f32 / (n - 1).max(1) as f32), to_y(*lane, vv)))
                    .collect();
                // Position solid, rotation dotted, scale dashed: the component
                // colours are the same on every lane, so the line says the lane.
                let stroke = Stroke::new(1.5, COMPONENT_COLORS[c]);
                match lane {
                    Lane::Translation => painter.add(egui::Shape::line(pts, stroke)),
                    Lane::Rotation => painter.add(egui::Shape::dashed_line(&pts, stroke, 1.5, 3.0)),
                    Lane::Scale => painter.add(egui::Shape::dashed_line(&pts, stroke, 6.0, 4.0)),
                };
            }
        }

        // ---- ruler + playhead ----
        let ruler = Rect::from_min_size(Pos2::new(graph.left(), full.top()), egui::vec2(dur * px + 100.0, ruler_h));
        let rresp = ui.interact(ruler, ui.id().with("curve-ruler"), Sense::click_and_drag());
        if (rresp.dragged() || rresp.clicked())
            && let Some(p) = rresp.interact_pointer_pos()
        {
            *playhead = snap_time(view.x_to_time(p.x), snap_fps);
        }
        painter.rect_filled(ruler, 0.0, ui.visuals().extreme_bg_color);
        draw_ruler(&painter, Rect::from_min_size(Pos2::new(graph.left(), full.top()), egui::vec2(dur * px, ruler_h)), dur, playhead.min(dur), px, snap_fps);
        let xp = view.time_to_x(playhead.min(dur));
        painter.line_segment([Pos2::new(xp, graph.top()), Pos2::new(xp, graph.bottom())], Stroke::new(1.5, PLAYHEAD));

        // ---- the keys, as points on their curves ----
        for (lane, lc) in &curves {
            let lane = *lane;
            if !cv.lanes[lane as usize] {
                continue;
            }
            let Some(l) = lane_of(&doc.channels[ci], lane) else { continue };
            let modes = l.modes().to_vec();
            let step = l.step();
            for (ki, &(t, vals)) in lc.keys.iter().enumerate() {
                for (c, &val) in vals.iter().enumerate() {
                    let dragging = cv.drag.is_some_and(|(dl, dc, ot, _, _)| dl == lane && dc == c && same_key_time(ot, t));
                    let (draw_t, draw_v) = if dragging {
                        let d = cv.drag.unwrap();
                        (d.3, d.4[c])
                    } else {
                        (t, val)
                    };
                    let centre = Pos2::new(view.time_to_x(draw_t), to_y(lane, draw_v));
                    let id = ui.id().with(("curve-key", lane as u8, c, ki));
                    let resp = ui.interact(Rect::from_center_size(centre, key_hit(12.0)), id, Sense::click_and_drag());
                    let selected = sel_keys.iter().any(|&(sc, st)| sc == ci && same_key_time(st, t));
                    let col = if resp.hovered() || dragging || selected { ACCENT } else { COMPONENT_COLORS[c] };
                    key_diamond(&painter, centre, col, key_mode_at(&modes, t), key_size(20.0));
                    if resp.hovered() || dragging {
                        let unit = if lane == Lane::Rotation { "°" } else { "" };
                        painter.text(
                            centre + egui::vec2(8.0, -8.0),
                            Align2::LEFT_BOTTOM,
                            format!("{} {} = {:.3}{unit} @ {:.2}s", lane.label(), COMPONENT_NAMES[c], draw_v, draw_t),
                            FontId::proportional(10.0),
                            ui.visuals().strong_text_color(),
                        );
                    }
                    if resp.clicked() {
                        edits.select = Some((ci, t));
                    }
                    if resp.drag_started() {
                        cv.drag = Some((lane, c, t, t, vals));
                    }
                    if resp.dragged()
                        && let Some(p) = resp.interact_pointer_pos()
                        && let Some(d) = cv.drag.as_mut()
                        && d.0 == lane
                        && d.1 == c
                        && same_key_time(d.2, t)
                    {
                        // Shift keeps the time, so a value can be nudged without
                        // sliding the key off its frame.
                        let shift = ui.input(|i| i.modifiers.shift);
                        d.3 = if shift { t } else { snap_time(view.x_to_time(p.x), snap_fps) };
                        d.4[c] = to_v(lane, p.y);
                    }
                    if resp.drag_stopped()
                        && let Some((dl, dc, ot, nt, nv)) = cv.drag.take()
                        && dl == lane
                        && dc == c
                        && same_key_time(ot, t)
                    {
                        edits.set_value = Some((ci, lane, ot, nv));
                        if !same_key_time(nt, ot) {
                            edits.retime = Some((ci, lane, ot, nt));
                        }
                    }
                    resp.context_menu(|ui| {
                        if ui.button("🗑 Delete key").clicked() {
                            edits.delete = Some((ci, lane, t));
                            ui.close();
                        }
                        ui.separator();
                        interp_menu(ui, key_mode_at(&modes, t), step, &mut |mode| {
                            edits.mode = Some((ci, lane, t, mode));
                        });
                    });
                }
            }
        }

        // Empty graph: right-click inserts a key on a lane at that time.
        if graph.contains(bg.interact_pointer_pos().unwrap_or(Pos2::ZERO)) {
            bg.context_menu(|ui| {
                let mx = ui.min_rect().left();
                let t = snap_time(view.x_to_time(mx), snap_fps);
                for lane in Lane::ALL {
                    if lane_of(&doc.channels[ci], lane).is_some()
                        && ui.button(format!("⏺ Key {} here", lane.label())).clicked()
                    {
                        edits.insert = Some((ci, lane, t));
                        ui.close();
                    }
                }
            });
        }
        // Alt+wheel over the graph zooms the value axis about the cursor;
        // Shift+wheel pans it. One lane at a time: with several shown each has
        // its own fit and there is no one axis to move.
        if single
            && let Some(&lane) = shown.first()
            && let Some(p) = ui.ctx().pointer_hover_pos()
            && graph.contains(p)
        {
            let (scroll, mods) = ui.input(|i| (i.smooth_scroll_delta, i.modifiers));
            if scroll.y.abs() > 0.5 && (mods.alt || mods.shift) {
                let (lo, hi) = range_of(lane);
                let vspan = (hi - lo).max(1e-6);
                let at = to_v(lane, p.y);
                let (mut nlo, mut nhi) = (lo, hi);
                if mods.alt {
                    let z = (-scroll.y * 0.0015).exp();
                    nlo = at + (lo - at) * z;
                    nhi = at + (hi - at) * z;
                } else {
                    let dv = scroll.y / graph.height() * vspan;
                    nlo += dv;
                    nhi += dv;
                }
                cv.vrange = Some((nlo, nhi));
            }
        }
    });
    let _ = out;
    edits
}

/// Apply a frame's edits to the doc. Returns whether anything changed.
pub(crate) fn apply_curve_edits(doc: &mut AnimClipDoc, edits: &CurveEdits) -> bool {
    let mut changed = false;
    if let Some((ci, lane, t, v)) = edits.set_value {
        changed |= set_key_triple(doc, ci, lane, t, v);
    }
    if let Some((ci, lane, old, new)) = edits.retime
        && let Some(l) = lane_of_mut(&mut doc.channels[ci], lane)
    {
        l.retime(old, new.max(0.0));
        changed = true;
    }
    if let Some((ci, lane, t, mode)) = edits.mode
        && let Some(l) = lane_of_mut(&mut doc.channels[ci], lane)
    {
        set_key_mode(l.modes_mut(), t, mode);
        changed = true;
    }
    if let Some((ci, lane, t)) = edits.insert {
        changed |= insert_key_on_curve(doc, ci, lane, t);
    }
    if let Some((ci, lane, t)) = edits.delete {
        crate::anim_ui::delete_lane_key(&mut doc.channels[ci], lane, t);
        changed = true;
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use floptle_scene::{AnimChannelDoc, AnimTrackDoc3, AnimTrackDoc4};

    fn clip() -> AnimClipDoc {
        AnimClipDoc {
            name: "c".into(),
            duration: 2.0,
            source_model: String::new(),
            channels: vec![AnimChannelDoc {
                node: "Arm".into(),
                translation: Some(AnimTrackDoc3 {
                    times: vec![0.0, 2.0],
                    values: vec![[0.0, 0.0, 0.0], [4.0, 0.0, 0.0]],
                    step: false,
                    modes: Vec::new(),
                    hold_times: Vec::new(),
                }),
                rotation: Some(AnimTrackDoc4 {
                    times: vec![0.0, 2.0],
                    values: vec![Quat::IDENTITY.to_array(), Quat::from_rotation_y(1.0).to_array()],
                    step: false,
                    modes: Vec::new(),
                    hold_times: Vec::new(),
                }),
                scale: None,
                properties: Vec::new(),
            }],
            events: Vec::new(),
        }
    }

    /// A key inserted from the curve holds the motion's value there, so the
    /// curve is unchanged until the key is moved.
    #[test]
    fn a_key_inserted_on_the_curve_does_not_change_it() {
        let mut doc = clip();
        assert!(insert_key_on_curve(&mut doc, 0, Lane::Translation, 1.0));
        let l = doc.channels[0].translation.as_ref().unwrap();
        assert_eq!(l.times, vec![0.0, 1.0, 2.0]);
        assert!((l.values[1][0] - 2.0).abs() < 1e-5, "the midpoint of 0→4 is 2, got {:?}", l.values[1]);
        assert!(!insert_key_on_curve(&mut doc, 0, Lane::Translation, 1.0), "no duplicate key at one time");
    }

    /// A rotation key set from its displayed degrees lands as that rotation.
    #[test]
    fn a_rotation_key_set_in_degrees_reads_back() {
        let mut doc = clip();
        assert!(set_key_triple(&mut doc, 0, Lane::Rotation, 2.0, [30.0, 57.2958, 0.0]));
        let s = sample_lane(&doc, 0, Lane::Rotation, 2.0, 3).unwrap();
        let got = s.keys[1].1;
        assert!((got[0] - 30.0).abs() < 1e-2, "{got:?}");
        assert!((got[1] - 57.2958).abs() < 1e-2, "{got:?}");
        assert!(got[2].abs() < 1e-2, "{got:?}");
    }

    /// A key sits on its curve: the key's displayed value is the curve's value
    /// at its time.
    #[test]
    fn a_key_sits_on_its_curve() {
        let doc = clip();
        let s = sample_lane(&doc, 0, Lane::Translation, 2.0, 5).unwrap();
        assert_eq!(s.keys.len(), 2);
        assert_eq!(s.keys[1].1, [4.0, 0.0, 0.0]);
        assert!((s.samples[0][4] - 4.0).abs() < 1e-5);
    }

    /// A turn about one axis past 90° draws as one line: the Euler spelling
    /// that continues the curve is the one drawn, and angles unwrap past 180°.
    #[test]
    fn a_turn_past_90_degrees_does_not_fold() {
        let mut doc = clip();
        doc.channels[0].rotation = Some(AnimTrackDoc4 {
            times: vec![0.0, 1.0, 2.0],
            values: vec![
                Quat::IDENTITY.to_array(),
                Quat::from_rotation_y(2.5).to_array(),
                Quat::from_rotation_y(-2.5).to_array(), // = +3.78 rad the long way
            ],
            step: false,
            modes: Vec::new(),
            hold_times: Vec::new(),
        });
        let s = sample_lane(&doc, 0, Lane::Rotation, 2.0, 81).unwrap();
        for (name, samples) in COMPONENT_NAMES.iter().zip(&s.samples) {
            for w in samples.windows(2) {
                assert!((w[1] - w[0]).abs() < 30.0, "a fold on {name} between {} and {}", w[0], w[1]);
            }
        }
        // …and it is the y line that moves, through 143° and on to 217°.
        let y = &s.samples[1];
        assert!((y[0]).abs() < 1.0 && (y[80] - 216.6).abs() < 2.0, "y runs {} → {}", y[0], y[80]);
    }
}
