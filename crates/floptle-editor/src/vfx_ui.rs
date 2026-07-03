//! The Particles tab — the video-editor timeline for particle effects.
//!
//! One row per track. On a row: **clips** (ranged emission spans — drag the body
//! to move, the edges to trim, double-click empty lane to add) and **bursts**
//! (instant-emit diamonds, dragged like anim event flags). The playhead drives a
//! deterministic preview instance rendered live in the Scene viewport, anchored
//! to a scene node carrying the edited effect. Every edit recompiles the preview
//! and marks the doc dirty; saves coalesce to pointer release (the anim-editor
//! discipline). Curve drawing and automation-lane UI arrive in phase 3.

use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Stroke};
use std::sync::Arc;

use floptle_core::ParticleSystem;
use floptle_core::World;
use floptle_scene::{VfxBurstDoc, VfxClipDoc, VfxEffectDoc, VfxEndDoc, VfxPlaybackDoc, VfxTrackDoc};
use floptle_vfx::EffectInstance;

use crate::timeline::{draw_ruler, snap_time, TimelineView, ACCENT, EVENT_COLOR, PLAYHEAD};
use crate::vfx::{effect_from_doc, starter_effect_doc, VfxPreview, VFX_GRAVITY};
use crate::EditorTabViewer;

/// What's selected on the timeline (drives the side panel's detail section).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum VfxSel {
    Clip(usize, usize),
    Burst(usize, usize),
}

/// A drag in progress on the timeline canvas.
#[derive(Clone, Copy)]
enum VfxDrag {
    /// Moving a clip body; `grab` = pointer offset from the clip start.
    ClipMove { track: usize, idx: usize, grab: f32 },
    /// Trimming a clip edge (`right` = the end handle).
    ClipTrim { track: usize, idx: usize, right: bool },
    Burst { track: usize, idx: usize },
}

/// UI state of the Particles tab. One field on `Editor`.
pub(crate) struct VfxUiState {
    /// The effect asset being edited (registry key).
    pub open_key: Option<String>,
    /// Working copy — edited live, saved (coalesced) back through the registry.
    pub doc: Option<VfxEffectDoc>,
    pub dirty: bool,
    /// Bumped on every edit; the preview recompiles when it trails behind.
    doc_rev: u64,
    preview_rev: u64,
    /// Preview transport. `playhead` runs in effect seconds (may exceed the
    /// lifetime while a one-shot's particle tails play out).
    pub playhead: f32,
    pub playing: bool,
    /// The effect time the preview instance is currently simulated to.
    sim_t: f32,
    /// Timeline zoom, px per second.
    pub zoom: f32,
    pub snap_fps: f32,
    pub sel_track: Option<usize>,
    pub sel: Option<VfxSel>,
    drag: Option<VfxDrag>,
    /// Timeline position captured at right-click, for "add burst here".
    ctx_t: f32,
    /// Which property's curve editor is expanded in the Inspector (by label), and
    /// the selected key within it — the value-or-curve affordance's state.
    pub expanded_prop: Option<String>,
    pub sel_key: Option<usize>,
    /// The curve editor's frozen value-axis range, held for the duration of a key
    /// drag so the auto-fit can't feed back on itself (see `curve_edit`).
    pub curve_vrange: Option<(f32, f32)>,
}

impl Default for VfxUiState {
    fn default() -> Self {
        Self {
            open_key: None,
            doc: None,
            dirty: false,
            doc_rev: 0,
            preview_rev: u64::MAX, // force first compile
            playhead: 0.0,
            playing: true, // auto-play on open: see it live immediately
            sim_t: 0.0,
            zoom: 220.0,
            snap_fps: 0.0,
            sel_track: None,
            sel: None,
            drag: None,
            ctx_t: 0.0,
            expanded_prop: None,
            sel_key: None,
            curve_vrange: None,
        }
    }
}

impl VfxUiState {
    /// Open `key` for editing (working copy loads lazily from the registry).
    pub fn open(&mut self, key: String) {
        if self.open_key.as_deref() != Some(key.as_str()) {
            self.open_key = Some(key);
            self.doc = None;
            self.dirty = false;
            self.playhead = 0.0;
            self.sim_t = 0.0;
            self.sel_track = None;
            self.sel = None;
            self.drag = None;
            self.expanded_prop = None;
            self.sel_key = None;
            self.curve_vrange = None;
            self.bump();
        }
    }

    fn bump(&mut self) {
        self.doc_rev = self.doc_rev.wrapping_add(1);
    }

    /// Mark the working doc edited: schedule a coalesced save + a preview
    /// recompile. Used by the Inspector's track editor (a different module).
    pub(crate) fn mark_dirty(&mut self) {
        self.dirty = true;
        self.bump();
    }
}

/// Track-lane geometry.
const LABEL_W: f32 = 150.0;
const ROW_H: f32 = 30.0;
const RULER_H: f32 = 22.0;
/// Clip edge-trim hit zone (px).
const EDGE_W: f32 = 6.0;
const CLIP_FILL: Color32 = Color32::from_rgb(80, 130, 190);
const CLIP_MIN_LEN: f32 = 0.02;

impl EditorTabViewer<'_> {
    pub(crate) fn particles_ui(&mut self, ui: &mut egui::Ui) {
        // Coalesced save: commit the working copy once the pointer is up.
        if self.vfx_ui.dirty && !self.pointer_down {
            if let (Some(k), Some(d)) = (self.vfx_ui.open_key.clone(), self.vfx_ui.doc.clone()) {
                self.vfx.save(self.project_root, &k, &d);
            }
            self.vfx_ui.dirty = false;
        }

        // Lazy-load the working copy from the registry.
        if self.vfx_ui.doc.is_none()
            && let Some(k) = self.vfx_ui.open_key.clone()
        {
            self.vfx_ui.doc = self.vfx.doc(&k).cloned();
        }
        if self.vfx_ui.open_key.is_none() || self.vfx_ui.doc.is_none() {
            self.vfx.preview = None;
            self.particles_empty_ui(ui);
            return;
        }

        let st = &mut *self.vfx_ui;
        let key = st.open_key.clone().expect("checked above");
        // Take the working copy for the frame (returned below) so the UI can
        // borrow it and the state struct independently.
        let mut doc = st.doc.take().expect("checked above");
        let mut dirty = false;

        transport_ui(ui, st, &mut doc, &mut dirty);
        ui.separator();
        ui.small(
            "Select a track to edit it in the Inspector →   ·   double-click a lane = clip, \
             right-click = burst.",
        );
        // The timeline canvas is full-width; track settings live in the Inspector.
        canvas_ui(ui, st, &mut doc, &mut dirty);

        if dirty {
            st.dirty = true;
            st.bump();
        }

        // ---- preview upkeep: advance/scrub the deterministic instance ----
        let lifetime = doc.lifetime.max(1e-3);
        // A one-shot previews one full lifetime PLUS its longest particle tail,
        // so fades past the timeline end are visible; loops preview seamlessly.
        let period = match doc.playback {
            VfxPlaybackDoc::Looping => f32::INFINITY,
            VfxPlaybackDoc::OneShot => {
                let tail = doc
                    .tracks
                    .iter()
                    .map(|t| t.particle_lifetime * (1.0 + t.lifetime_jitter))
                    .fold(0.0f32, f32::max);
                lifetime + tail.max(0.2)
            }
        };
        if st.playing {
            let dt = ui.input(|i| i.stable_dt).min(0.1);
            st.playhead += dt;
            if st.playhead > period {
                st.playhead = 0.0;
            }
            ui.ctx().request_repaint();
        }

        let stale = st.preview_rev != st.doc_rev
            || self.vfx.preview.as_ref().is_none_or(|p| p.key != key);
        if stale {
            let fx = Arc::new(effect_from_doc(&doc).compile());
            let mut inst = EffectInstance::new(fx, 1);
            inst.simulate_to(st.playhead, VFX_GRAVITY);
            self.vfx.preview = Some(VfxPreview { key: key.clone(), inst, anchor: None });
            st.preview_rev = st.doc_rev;
            st.sim_t = st.playhead;
        } else if let Some(p) = self.vfx.preview.as_mut() {
            if st.playhead >= st.sim_t {
                let d = st.playhead - st.sim_t;
                if d > 0.0 {
                    p.inst.advance(d, VFX_GRAVITY);
                }
            } else {
                // Backward scrub / loop wrap: deterministic re-sim from zero.
                p.inst.simulate_to(st.playhead, VFX_GRAVITY);
            }
            st.sim_t = st.playhead;
        }
        // Anchor the preview to a scene node carrying this effect (world origin
        // otherwise) — you see it exactly where the game will play it.
        if let Some(p) = self.vfx.preview.as_mut() {
            p.anchor = anchor_for(self.world, &key);
        }

        self.vfx_ui.doc = Some(doc);
    }

    /// Shown when no effect is open: pick one, or point at the create flows.
    fn particles_empty_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.label("No effect open.");
        ui.small(
            "Open one from a Particle System component (✎ Edit effect), double-click a \
             .vfx.ron asset, or pick below. \"Add Component → Particle System (new)\" \
             creates a starter effect on the selected node.",
        );
        ui.add_space(6.0);
        let keys: Vec<String> = self.vfx.effects.iter().map(|(k, _)| k.clone()).collect();
        for k in keys {
            if ui.button(format!("❋  {k}")).clicked() {
                self.vfx_ui.open(k);
            }
        }
    }
}

/// The first scene node whose ParticleSystem references `key`.
fn anchor_for(world: &World, key: &str) -> Option<floptle_core::Entity> {
    world.query::<ParticleSystem>().find(|(_, ps)| ps.asset == key).map(|(e, _)| e)
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

fn transport_ui(ui: &mut egui::Ui, st: &mut VfxUiState, doc: &mut VfxEffectDoc, dirty: &mut bool) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(format!("❋ {}", doc.name)).strong());
        ui.separator();
        if ui.button("⏮").on_hover_text("to start").clicked() {
            st.playhead = 0.0;
        }
        let play_lbl = if st.playing { "⏸" } else { "⏵" };
        if ui.button(play_lbl).clicked() {
            st.playing = !st.playing;
        }
        if ui.button("⏹").clicked() {
            st.playing = false;
            st.playhead = 0.0;
        }
        let shown = match doc.playback {
            VfxPlaybackDoc::Looping => st.playhead % doc.lifetime.max(1e-3),
            VfxPlaybackDoc::OneShot => st.playhead,
        };
        ui.monospace(format!("{shown:>5.2}s / {:.2}s", doc.lifetime));
        ui.separator();

        ui.label("lifetime");
        let mut lt = doc.lifetime;
        if ui
            .add(egui::DragValue::new(&mut lt).speed(0.02).range(0.05..=600.0).suffix("s"))
            .changed()
        {
            doc.lifetime = lt;
            *dirty = true;
        }
        egui::ComboBox::from_id_salt("vfx_playback")
            .selected_text(match doc.playback {
                VfxPlaybackDoc::Looping => "⟲ Looping",
                VfxPlaybackDoc::OneShot => "→ One-shot",
            })
            .show_ui(ui, |ui| {
                for (v, l) in [
                    (VfxPlaybackDoc::Looping, "⟲ Looping"),
                    (VfxPlaybackDoc::OneShot, "→ One-shot"),
                ] {
                    if ui.selectable_label(doc.playback == v, l).clicked() && doc.playback != v {
                        doc.playback = v;
                        *dirty = true;
                    }
                }
            });
        // End behavior only means something for a one-shot (proposal §3).
        if doc.playback == VfxPlaybackDoc::OneShot {
            egui::ComboBox::from_id_salt("vfx_end")
                .selected_text(match doc.end {
                    VfxEndDoc::Destroy => "end: destroy",
                    VfxEndDoc::Persist => "end: persist",
                })
                .show_ui(ui, |ui| {
                    for (v, l) in
                        [(VfxEndDoc::Destroy, "destroy"), (VfxEndDoc::Persist, "persist")]
                    {
                        if ui.selectable_label(doc.end == v, l).clicked() && doc.end != v {
                            doc.end = v;
                            *dirty = true;
                        }
                    }
                });
        }
        if ui.button("🎲").on_hover_text("re-roll the effect's random seed").clicked() {
            doc.seed = doc.seed.wrapping_mul(1664525).wrapping_add(1013904223).max(1);
            *dirty = true;
        }
        ui.separator();

        if ui.button("＋ Track").clicked() {
            let mut t = starter_track(doc);
            t.name = format!("Track {}", doc.tracks.len() + 1);
            doc.tracks.push(t);
            st.sel_track = Some(doc.tracks.len() - 1);
            st.sel = None;
            *dirty = true;
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let mut z = st.zoom.ln();
            if ui
                .add(egui::Slider::new(&mut z, 30f32.ln()..=800f32.ln()).show_value(false))
                .on_hover_text("zoom")
                .changed()
            {
                st.zoom = z.exp();
            }
            ui.label("🔍");
            egui::ComboBox::from_id_salt("vfx_snap")
                .width(64.0)
                .selected_text(if st.snap_fps > 0.0 {
                    format!("{} fps", st.snap_fps as u32)
                } else {
                    "free".into()
                })
                .show_ui(ui, |ui| {
                    for (v, l) in [
                        (0.0, "free"),
                        (8.0, "8 fps"),
                        (12.0, "12 fps"),
                        (24.0, "24 fps"),
                        (30.0, "30 fps"),
                        (60.0, "60 fps"),
                    ] {
                        if ui.selectable_label(st.snap_fps == v, l).clicked() {
                            st.snap_fps = v;
                        }
                    }
                });
            ui.label("snap");
        });
    });
}

/// A fresh track for "＋ Track": one clip spanning the whole timeline, so it
/// visibly emits the moment it exists.
fn starter_track(doc: &VfxEffectDoc) -> VfxTrackDoc {
    let mut t = starter_effect_doc("x").tracks.remove(0);
    t.clips = vec![VfxClipDoc { start: 0.0, end: doc.lifetime }];
    t
}

// ---------------------------------------------------------------------------
// Timeline canvas
// ---------------------------------------------------------------------------

fn canvas_ui(ui: &mut egui::Ui, st: &mut VfxUiState, doc: &mut VfxEffectDoc, dirty: &mut bool) {
    let dur = doc.lifetime.max(0.01);
    let px = st.zoom;
    let n_rows = doc.tracks.len();
    let body_h = RULER_H + (n_rows.max(1) as f32) * ROW_H + 8.0;

    egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
        let want_w = (LABEL_W + dur * px + 140.0).max(ui.available_width());
        let want_h = body_h.max(ui.available_height());
        let (full, _) = ui.allocate_exact_size(egui::vec2(want_w, want_h), Sense::hover());
        let painter = ui.painter_at(full);
        let view =
            TimelineView { left: full.left() + LABEL_W, px_per_s: px, duration: dur };

        // ---- ruler (scrub) ----
        let ruler = Rect::from_min_size(
            Pos2::new(view.left, full.top()),
            egui::vec2(dur * px + 100.0, RULER_H),
        );
        let rresp = ui.interact(ruler, ui.id().with("vfx-ruler"), Sense::click_and_drag());
        if (rresp.dragged() || rresp.clicked())
            && let Some(p) = rresp.interact_pointer_pos()
        {
            st.playhead = snap_time(view.x_to_time(p.x), st.snap_fps);
            st.playing = false;
        }
        painter.rect_filled(ruler, 0.0, ui.visuals().extreme_bg_color);

        // ---- track rows ----
        let rows_top = full.top() + RULER_H;
        let mut deferred: Option<DeferredEdit> = None;
        for (ti, track) in doc.tracks.iter().enumerate() {
            let y = rows_top + ti as f32 * ROW_H;
            let row = Rect::from_min_size(Pos2::new(full.left(), y), egui::vec2(full.width(), ROW_H));
            let selected = st.sel_track == Some(ti);
            if selected {
                painter.rect_filled(row, 0.0, ACCENT.gamma_multiply(0.08));
            } else if ti % 2 == 0 {
                painter.rect_filled(
                    Rect::from_min_size(Pos2::new(view.left, y), egui::vec2(dur * px, ROW_H)),
                    0.0,
                    ui.visuals().faint_bg_color.gamma_multiply(0.6),
                );
            }

            // Label area: mute toggle + name (click selects the track).
            let mute = Rect::from_min_size(Pos2::new(full.left() + 4.0, y + 7.0), egui::vec2(16.0, 16.0));
            let mresp = ui.interact(mute, ui.id().with(("vfx-mute", ti)), Sense::click());
            painter.text(
                mute.center(),
                Align2::CENTER_CENTER,
                if track.enabled { "▣" } else { "□" },
                FontId::proportional(12.0),
                if track.enabled { ui.visuals().text_color() } else { ui.visuals().weak_text_color() },
            );
            if mresp.clicked() {
                deferred = Some(DeferredEdit::ToggleMute(ti));
            }
            let label = Rect::from_min_size(
                Pos2::new(mute.right() + 4.0, y),
                egui::vec2(LABEL_W - 28.0, ROW_H),
            );
            let lresp = ui.interact(label, ui.id().with(("vfx-label", ti)), Sense::click());
            let name_col = if selected {
                ACCENT
            } else if track.enabled {
                ui.visuals().text_color()
            } else {
                ui.visuals().weak_text_color()
            };
            painter.text(
                Pos2::new(label.left(), row.center().y),
                Align2::LEFT_CENTER,
                &track.name,
                FontId::proportional(11.5),
                name_col,
            );
            if lresp.clicked() {
                st.sel_track = Some(ti);
                st.sel = None;
            }

            // The lane itself: double-click = new clip, right-click = add burst.
            let lane = Rect::from_min_size(Pos2::new(view.left, y), egui::vec2(dur * px, ROW_H));
            let lane_resp = ui.interact(lane, ui.id().with(("vfx-lane", ti)), Sense::click());
            if lane_resp.double_clicked()
                && let Some(p) = lane_resp.interact_pointer_pos()
            {
                let t0 = snap_time(view.x_to_time(p.x), st.snap_fps);
                deferred = Some(DeferredEdit::AddClip(ti, t0));
            }
            // Capture where the right-click landed — by the time a context-menu
            // button is clicked the pointer is over the menu, not the lane.
            if lane_resp.secondary_clicked()
                && let Some(p) = lane_resp.interact_pointer_pos()
            {
                st.ctx_t = snap_time(view.x_to_time(p.x), st.snap_fps);
            }
            let ctx_t = st.ctx_t;
            lane_resp.context_menu(|ui| {
                if ui.button("✦ Add burst here").clicked() {
                    deferred = Some(DeferredEdit::AddBurst(ti, ctx_t));
                    ui.close();
                }
                if ui.button("⧉ Duplicate track").clicked() {
                    deferred = Some(DeferredEdit::DupTrack(ti));
                    ui.close();
                }
                if ui.button("🗑 Delete track").clicked() {
                    deferred = Some(DeferredEdit::DelTrack(ti));
                    ui.close();
                }
            });

            // ---- clips ----
            for (ci, clip) in track.clips.iter().enumerate() {
                let x0 = view.time_to_x(clip.start.clamp(0.0, dur));
                let x1 = view.time_to_x(clip.end.clamp(0.0, dur));
                let body = Rect::from_min_max(Pos2::new(x0, y + 4.0), Pos2::new(x1, y + ROW_H - 4.0));
                let sel = st.sel == Some(VfxSel::Clip(ti, ci));
                let fill = if sel { ACCENT.gamma_multiply(0.85) } else { CLIP_FILL };
                painter.rect_filled(body, 4.0, fill.gamma_multiply(if track.enabled { 1.0 } else { 0.4 }));
                painter.rect_stroke(
                    body,
                    4.0,
                    Stroke::new(1.0, Color32::from_black_alpha(140)),
                    egui::StrokeKind::Inside,
                );

                let wide = body.width() > EDGE_W * 2.0 + 4.0;
                let left_h = Rect::from_min_max(body.min, Pos2::new(body.left() + EDGE_W, body.bottom()));
                let right_h = Rect::from_min_max(Pos2::new(body.right() - EDGE_W, body.top()), body.max);
                let mid = if wide {
                    Rect::from_min_max(
                        Pos2::new(body.left() + EDGE_W, body.top()),
                        Pos2::new(body.right() - EDGE_W, body.bottom()),
                    )
                } else {
                    body
                };

                // Edges first (they sit on top of the body's hit area).
                if wide {
                    for (h, right) in [(left_h, false), (right_h, true)] {
                        let id = ui.id().with(("vfx-clip-edge", ti, ci, right));
                        let resp = ui.interact(h, id, Sense::drag());
                        if resp.hovered() || resp.dragged() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                        }
                        if resp.drag_started() {
                            st.drag = Some(VfxDrag::ClipTrim { track: ti, idx: ci, right });
                            st.sel = Some(VfxSel::Clip(ti, ci));
                            st.sel_track = Some(ti);
                        }
                    }
                }
                let id = ui.id().with(("vfx-clip", ti, ci));
                let resp = ui.interact(mid, id, Sense::click_and_drag());
                if resp.clicked() {
                    st.sel = Some(VfxSel::Clip(ti, ci));
                    st.sel_track = Some(ti);
                }
                if resp.drag_started()
                    && let Some(p) = resp.interact_pointer_pos()
                {
                    st.drag = Some(VfxDrag::ClipMove {
                        track: ti,
                        idx: ci,
                        grab: view.x_to_time(p.x) - clip.start,
                    });
                    st.sel = Some(VfxSel::Clip(ti, ci));
                    st.sel_track = Some(ti);
                }
                resp.context_menu(|ui| {
                    if ui.button("🗑 Delete clip").clicked() {
                        deferred = Some(DeferredEdit::DelClip(ti, ci));
                        ui.close();
                    }
                });
            }

            // ---- bursts (over clips) ----
            for (bi, b) in track.bursts.iter().enumerate() {
                let x = view.time_to_x(b.t.clamp(0.0, dur));
                let c = Pos2::new(x, row.center().y);
                let sel = st.sel == Some(VfxSel::Burst(ti, bi));
                let col = if sel { ACCENT } else { EVENT_COLOR };
                let s = 5.5;
                painter.add(egui::Shape::convex_polygon(
                    vec![
                        Pos2::new(c.x, c.y - s),
                        Pos2::new(c.x + s, c.y),
                        Pos2::new(c.x, c.y + s),
                        Pos2::new(c.x - s, c.y),
                    ],
                    col,
                    Stroke::new(1.0, Color32::from_black_alpha(160)),
                ));
                painter.text(
                    Pos2::new(c.x + 6.0, c.y - 6.0),
                    Align2::LEFT_CENTER,
                    format!("×{}", b.count),
                    FontId::proportional(9.0),
                    col.gamma_multiply(0.9),
                );
                let hit = Rect::from_center_size(c, egui::vec2(13.0, 13.0));
                let id = ui.id().with(("vfx-burst", ti, bi));
                let resp = ui.interact(hit, id, Sense::click_and_drag());
                if resp.clicked() {
                    st.sel = Some(VfxSel::Burst(ti, bi));
                    st.sel_track = Some(ti);
                }
                if resp.drag_started() {
                    st.drag = Some(VfxDrag::Burst { track: ti, idx: bi });
                    st.sel = Some(VfxSel::Burst(ti, bi));
                    st.sel_track = Some(ti);
                }
                resp.context_menu(|ui| {
                    if ui.button("🗑 Delete burst").clicked() {
                        deferred = Some(DeferredEdit::DelBurst(ti, bi));
                        ui.close();
                    }
                });
            }
        }

        if doc.tracks.is_empty() {
            painter.text(
                Pos2::new(view.left + 12.0, rows_top + ROW_H * 0.7),
                Align2::LEFT_CENTER,
                "no tracks yet — ＋ Track adds one (a track = one visual layer of the effect)",
                FontId::proportional(11.5),
                ui.visuals().weak_text_color(),
            );
        }

        // ---- live drag application ----
        let pointer = ui.ctx().pointer_interact_pos();
        let released = ui.input(|i| i.pointer.any_released());
        if let (Some(drag), Some(p)) = (st.drag, pointer) {
            let t = snap_time(view.x_to_time(p.x), st.snap_fps);
            match drag {
                VfxDrag::ClipMove { track, idx, grab } => {
                    if let Some(c) = doc.tracks.get_mut(track).and_then(|tr| tr.clips.get_mut(idx)) {
                        let len = (c.end - c.start).max(CLIP_MIN_LEN);
                        let start = (t - grab).clamp(0.0, (dur - len).max(0.0));
                        if (start - c.start).abs() > 1e-6 {
                            c.start = start;
                            c.end = start + len;
                            *dirty = true;
                        }
                    }
                }
                VfxDrag::ClipTrim { track, idx, right } => {
                    if let Some(c) = doc.tracks.get_mut(track).and_then(|tr| tr.clips.get_mut(idx)) {
                        if right {
                            let e = t.clamp(c.start + CLIP_MIN_LEN, dur);
                            if (e - c.end).abs() > 1e-6 {
                                c.end = e;
                                *dirty = true;
                            }
                        } else {
                            let s = t.clamp(0.0, c.end - CLIP_MIN_LEN);
                            if (s - c.start).abs() > 1e-6 {
                                c.start = s;
                                *dirty = true;
                            }
                        }
                    }
                }
                VfxDrag::Burst { track, idx } => {
                    if let Some(b) = doc.tracks.get_mut(track).and_then(|tr| tr.bursts.get_mut(idx))
                        && (b.t - t).abs() > 1e-6
                    {
                        b.t = t.clamp(0.0, dur);
                        *dirty = true;
                    }
                }
            }
        }
        if released && st.drag.take().is_some() {
            // Keep authored order canonical for the sim (clips sorted by start).
            for tr in &mut doc.tracks {
                tr.clips.sort_by(|a, b| a.start.total_cmp(&b.start));
                tr.bursts.sort_by(|a, b| a.t.total_cmp(&b.t));
            }
            st.sel = None; // indices may have shifted under the sort
            *dirty = true;
        }

        if let Some(edit) = deferred {
            apply_deferred(edit, st, doc, dur);
            *dirty = true;
        }

        // ---- playhead + end marker over everything, ruler ticks on top ----
        let shown = match doc.playback {
            VfxPlaybackDoc::Looping => st.playhead % dur,
            VfxPlaybackDoc::OneShot => st.playhead.min(dur),
        };
        let xp = view.time_to_x(shown);
        painter.line_segment(
            [Pos2::new(xp, full.top()), Pos2::new(xp, full.bottom())],
            Stroke::new(1.5, PLAYHEAD),
        );
        draw_ruler(
            &painter,
            Rect::from_min_size(Pos2::new(view.left, full.top()), egui::vec2(dur * px, RULER_H)),
            dur,
            shown,
            px,
        );
    });
}

/// Structural edits deferred out of the per-track iteration borrow.
enum DeferredEdit {
    ToggleMute(usize),
    AddClip(usize, f32),
    AddBurst(usize, f32),
    DelClip(usize, usize),
    DelBurst(usize, usize),
    DupTrack(usize),
    DelTrack(usize),
}

fn apply_deferred(edit: DeferredEdit, st: &mut VfxUiState, doc: &mut VfxEffectDoc, dur: f32) {
    match edit {
        DeferredEdit::ToggleMute(ti) => {
            if let Some(t) = doc.tracks.get_mut(ti) {
                t.enabled = !t.enabled;
            }
        }
        DeferredEdit::AddClip(ti, t0) => {
            if let Some(t) = doc.tracks.get_mut(ti) {
                let start = t0.clamp(0.0, (dur - CLIP_MIN_LEN).max(0.0));
                let end = (t0 + 0.5).min(dur).max(start + CLIP_MIN_LEN);
                t.clips.push(VfxClipDoc { start, end });
                t.clips.sort_by(|a, b| a.start.total_cmp(&b.start));
                let ci = t.clips.iter().position(|c| c.start == start).unwrap_or(0);
                st.sel = Some(VfxSel::Clip(ti, ci));
                st.sel_track = Some(ti);
            }
        }
        DeferredEdit::AddBurst(ti, t0) => {
            if let Some(t) = doc.tracks.get_mut(ti) {
                t.bursts.push(VfxBurstDoc { t: t0.clamp(0.0, dur), count: 10 });
                t.bursts.sort_by(|a, b| a.t.total_cmp(&b.t));
                let bi = t.bursts.iter().position(|b| b.t == t0.clamp(0.0, dur)).unwrap_or(0);
                st.sel = Some(VfxSel::Burst(ti, bi));
                st.sel_track = Some(ti);
            }
        }
        DeferredEdit::DelClip(ti, ci) => {
            if let Some(t) = doc.tracks.get_mut(ti)
                && ci < t.clips.len()
            {
                t.clips.remove(ci);
                st.sel = None;
            }
        }
        DeferredEdit::DelBurst(ti, bi) => {
            if let Some(t) = doc.tracks.get_mut(ti)
                && bi < t.bursts.len()
            {
                t.bursts.remove(bi);
                st.sel = None;
            }
        }
        DeferredEdit::DupTrack(ti) => {
            if let Some(t) = doc.tracks.get(ti) {
                let mut c = t.clone();
                c.name = format!("{} copy", c.name);
                doc.tracks.insert(ti + 1, c);
                st.sel_track = Some(ti + 1);
                st.sel = None;
            }
        }
        DeferredEdit::DelTrack(ti) => {
            if ti < doc.tracks.len() {
                doc.tracks.remove(ti);
                st.sel_track = None;
                st.sel = None;
            }
        }
    }
}

