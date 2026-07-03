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
use std::collections::HashSet;
use std::sync::Arc;

use floptle_core::ParticleSystem;
use floptle_core::World;
use floptle_scene::{
    VfxBurstDoc, VfxClipDoc, VfxCurveDoc, VfxEffectDoc, VfxEndDoc, VfxExtrapolateDoc, VfxInterpDoc,
    VfxKeyDoc, VfxLaneDoc, VfxLaneTargetDoc, VfxPlaybackDoc, VfxPropDoc, VfxTrackDoc, VfxValueDoc,
};
use floptle_vfx::EffectInstance;

use crate::timeline::{draw_ruler, snap_time, TimelineView, ACCENT, EVENT_COLOR, KEY_COLOR, PLAYHEAD};
use crate::vfx::{curve_from_doc, effect_from_doc, starter_effect_doc, VfxPreview, VFX_GRAVITY};
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
    /// Tracks whose lanes are drawn (expanded) on the timeline.
    pub expanded_tracks: HashSet<usize>,
    /// The selected breakpoint `(track, lane, key)` — highlighted on the timeline;
    /// its exact value/colour edits in the Inspector.
    pub auto_sel: Option<(usize, LaneRef, usize)>,
    /// The lane whose scalar point is mid-drag, and the value-axis range frozen for
    /// that drag (auto-fit lanes only — so lifting a point can't stretch the axis).
    pub lane_drag: Option<(usize, LaneRef)>,
    pub lane_vrange: Option<(f32, f32)>,
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
            expanded_tracks: HashSet::new(),
            auto_sel: None,
            lane_drag: None,
            lane_vrange: None,
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
            self.expanded_tracks.clear();
            self.auto_sel = None;
            self.lane_drag = None;
            self.lane_vrange = None;
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

/// One automation-lane strip's height (drawn under an expanded track).
const LANE_H: f32 = 36.0;
const LANE_PAD: f32 = 3.0;

/// Every automation target, for the track's "Add automation" menu.
const ALL_TARGETS: [VfxLaneTargetDoc; 6] = [
    VfxLaneTargetDoc::Rate,
    VfxLaneTargetDoc::Count,
    VfxLaneTargetDoc::Speed,
    VfxLaneTargetDoc::Size,
    VfxLaneTargetDoc::Tint,
    VfxLaneTargetDoc::ShapeScale,
];

/// A human label for an automation target (shared with the Inspector's point editor).
pub(crate) fn lane_label(t: VfxLaneTargetDoc) -> &'static str {
    match t {
        VfxLaneTargetDoc::Rate => "× rate",
        VfxLaneTargetDoc::Count => "× burst count",
        VfxLaneTargetDoc::Speed => "× speed",
        VfxLaneTargetDoc::Size => "× size",
        VfxLaneTargetDoc::Tint => "× tint",
        VfxLaneTargetDoc::ShapeScale => "× shape scale",
    }
}

/// The FIXED vertical range of a scalar automation lane. Being fixed (not auto-fit)
/// is what makes the timeline DAW-like — and structurally rules out the value-axis
/// feedback loop that crashed the old inspector curve editor. Shared with the
/// Inspector's precise point editor so both clamp identically.
pub(crate) fn lane_vrange(t: VfxLaneTargetDoc) -> (f32, f32) {
    match t {
        VfxLaneTargetDoc::Rate | VfxLaneTargetDoc::Count => (0.0, 4.0),
        VfxLaneTargetDoc::Speed | VfxLaneTargetDoc::Size | VfxLaneTargetDoc::ShapeScale => {
            (0.0, 3.0)
        }
        VfxLaneTargetDoc::Tint => (0.0, 1.0),
    }
}

/// A fresh flat lane — a no-op multiplier of 1 (white for tint) spanning the timeline,
/// which the artist then shapes by dragging its breakpoints.
fn starter_lane(target: VfxLaneTargetDoc, dur: f32) -> VfxLaneDoc {
    let v = if target == VfxLaneTargetDoc::Tint {
        VfxValueDoc::Rgba([1.0, 1.0, 1.0, 1.0])
    } else {
        VfxValueDoc::F32(1.0)
    };
    let key = |t: f32| VfxKeyDoc { t, v, interp: VfxInterpDoc::Linear, in_tan: 0.0, out_tan: 0.0 };
    VfxLaneDoc {
        target,
        curve: VfxCurveDoc { keys: vec![key(0.0), key(dur.max(CLIP_MIN_LEN))], extrapolate: VfxExtrapolateDoc::Clamp },
    }
}

// ---------------------------------------------------------------------------
// Unified timeline lanes: a track's animatable curves, whether they shape a
// particle over its LIFE (velocity/size/rotation/colour) or the emitter over the
// effect's TIMELINE (the automation multipliers). Both are `VfxCurveDoc`s; the
// timeline draws and edits them the same way, differing only in x-domain + range.
// ---------------------------------------------------------------------------

/// A curve-shaped property of a track, addressable as one timeline lane.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum LaneRef {
    /// A per-particle life-curve (x-domain = the particle's life, 0→1).
    Life(LifeProp),
    /// An automation multiplier over effect time (index into `track.automation`).
    Auto(usize),
}

/// Which per-particle property a [`LaneRef::Life`] shapes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum LifeProp {
    Velocity,
    Size,
    Rotation,
    AngularVelocity,
    Color,
}

const LIFE_PROPS: [LifeProp; 5] = [
    LifeProp::Velocity,
    LifeProp::Size,
    LifeProp::Rotation,
    LifeProp::AngularVelocity,
    LifeProp::Color,
];

/// How a lane's value is drawn + edited.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LaneVis {
    /// One value: a draggable point curve (y = value).
    Scalar,
    /// A 3-vector: three channel lines + time-only stops (value edits in Inspector).
    Vec3,
    /// RGBA: a gradient strip + time-only stops (colour edits in Inspector).
    Color,
}

fn life_prop_name(p: LifeProp) -> &'static str {
    match p {
        LifeProp::Velocity => "velocity",
        LifeProp::Size => "size",
        LifeProp::Rotation => "rotation",
        LifeProp::AngularVelocity => "angular vel",
        LifeProp::Color => "colour",
    }
}

fn life_prop_doc(track: &VfxTrackDoc, p: LifeProp) -> &VfxPropDoc {
    match p {
        LifeProp::Velocity => &track.velocity,
        LifeProp::Size => &track.size,
        LifeProp::Rotation => &track.rotation,
        LifeProp::AngularVelocity => &track.angular_velocity,
        LifeProp::Color => &track.color,
    }
}

fn life_prop_doc_mut(track: &mut VfxTrackDoc, p: LifeProp) -> &mut VfxPropDoc {
    match p {
        LifeProp::Velocity => &mut track.velocity,
        LifeProp::Size => &mut track.size,
        LifeProp::Rotation => &mut track.rotation,
        LifeProp::AngularVelocity => &mut track.angular_velocity,
        LifeProp::Color => &mut track.color,
    }
}

fn value_kind(v: &VfxValueDoc) -> LaneVis {
    match v {
        VfxValueDoc::F32(_) => LaneVis::Scalar,
        VfxValueDoc::Vec3(_) => LaneVis::Vec3,
        VfxValueDoc::Rgba(_) => LaneVis::Color,
    }
}

/// The lanes shown under an expanded track, in draw order: life-curves first
/// (only those actually animated, i.e. currently a `Curve`), then automation.
fn visible_lanes(track: &VfxTrackDoc) -> Vec<LaneRef> {
    let mut out = Vec::new();
    for p in LIFE_PROPS {
        if matches!(life_prop_doc(track, p), VfxPropDoc::Curve(_)) {
            out.push(LaneRef::Life(p));
        }
    }
    out.extend((0..track.automation.len()).map(LaneRef::Auto));
    out
}

/// The curve backing a lane (read) — `None` if a life prop isn't a curve yet.
fn lane_curve(track: &VfxTrackDoc, lref: LaneRef) -> Option<&VfxCurveDoc> {
    match lref {
        LaneRef::Auto(i) => track.automation.get(i).map(|l| &l.curve),
        LaneRef::Life(p) => match life_prop_doc(track, p) {
            VfxPropDoc::Curve(c) => Some(c),
            _ => None,
        },
    }
}

pub(crate) fn lane_curve_mut(track: &mut VfxTrackDoc, lref: LaneRef) -> Option<&mut VfxCurveDoc> {
    match lref {
        LaneRef::Auto(i) => track.automation.get_mut(i).map(|l| &mut l.curve),
        LaneRef::Life(p) => match life_prop_doc_mut(track, p) {
            VfxPropDoc::Curve(c) => Some(c),
            _ => None,
        },
    }
}

/// A lane's display label (with its x-domain, so `life` vs `time` reads clearly).
pub(crate) fn lane_ref_label(track: &VfxTrackDoc, lref: LaneRef) -> String {
    match lref {
        LaneRef::Auto(i) => {
            format!("{} · time", track.automation.get(i).map(|l| lane_label(l.target)).unwrap_or(""))
        }
        LaneRef::Life(p) => format!("{} · life", life_prop_name(p)),
    }
}

/// Automation lanes run over effect time; life-curves over the particle's life.
fn lane_is_time(lref: LaneRef) -> bool {
    matches!(lref, LaneRef::Auto(_))
}

/// The fixed value range of a lane, or `None` to auto-fit (life scalars have
/// arbitrary magnitude — e.g. rotation in radians — so they can't be pinned).
pub(crate) fn lane_fixed_range(track: &VfxTrackDoc, lref: LaneRef) -> Option<(f32, f32)> {
    match lref {
        LaneRef::Auto(i) => track.automation.get(i).map(|l| lane_vrange(l.target)),
        LaneRef::Life(_) => None,
    }
}

/// Auto-fit a curve's value range across `chans` channels, sampled, with padding.
fn auto_fit_range(rt: &floptle_vfx::Curve, chans: usize) -> (f32, f32) {
    let (mut lo, mut hi) = (f32::MAX, f32::MIN);
    for i in 0..=24 {
        let c = rt.eval(i as f32 / 24.0);
        for &v in c.iter().take(chans) {
            lo = lo.min(v);
            hi = hi.max(v);
        }
    }
    if !lo.is_finite() || !hi.is_finite() {
        return (0.0, 1.0);
    }
    let pad = ((hi - lo) * 0.15).max(0.25);
    (lo - pad, hi + pad)
}

/// A fresh flat life-curve at the property's current constant (domain = life 0→1),
/// so promoting a property to a lane starts as a visible no-op the artist shapes.
fn promote_to_curve(prop: &mut VfxPropDoc) {
    let seed = match prop {
        VfxPropDoc::Const(v) => *v,
        VfxPropDoc::Range(a, _) => *a,
        VfxPropDoc::Curve(_) => return,
    };
    let key = |t: f32| VfxKeyDoc { t, v: seed, interp: VfxInterpDoc::Linear, in_tan: 0.0, out_tan: 0.0 };
    *prop = VfxPropDoc::Curve(VfxCurveDoc {
        keys: vec![key(0.0), key(1.0)],
        extrapolate: VfxExtrapolateDoc::Clamp,
    });
}

/// The pixel height a track occupies, including its expanded lanes.
fn track_block_h(track: &VfxTrackDoc, expanded: bool) -> f32 {
    ROW_H
        + if expanded {
            // At least one row so an empty expanded track shows its "add" hint.
            visible_lanes(track).len().max(1) as f32 * (LANE_H + LANE_PAD) + LANE_PAD
        } else {
            0.0
        }
}

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

        // The preview emitter = the scene node carrying this effect (static while
        // editing), so World-space tracks preview at the node rather than the origin.
        let emitter = anchor_for(self.world, &key)
            .map(|e| floptle_core::world_transform(self.world, e))
            .unwrap_or(floptle_core::transform::Transform::IDENTITY);
        let stale = st.preview_rev != st.doc_rev
            || self.vfx.preview.as_ref().is_none_or(|p| p.key != key);
        if stale {
            let fx = Arc::new(effect_from_doc(&doc).compile());
            let mut inst = EffectInstance::new(fx, 1);
            inst.simulate_to_at(st.playhead, VFX_GRAVITY, emitter);
            self.vfx.preview = Some(VfxPreview { key: key.clone(), inst, anchor: None });
            st.preview_rev = st.doc_rev;
            st.sim_t = st.playhead;
        } else if let Some(p) = self.vfx.preview.as_mut() {
            if st.playhead >= st.sim_t {
                let d = st.playhead - st.sim_t;
                if d > 0.0 {
                    p.inst.advance_at(d, VFX_GRAVITY, emitter);
                }
            } else {
                // Backward scrub / loop wrap: deterministic re-sim from zero.
                p.inst.simulate_to_at(st.playhead, VFX_GRAVITY, emitter);
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
            "Open one from a Particle System component (✏ Edit effect), double-click a \
             .vfx.ron asset, or pick below. \"Add Component › Particle System (new)\" \
             creates a starter effect on the selected node.",
        );
        ui.add_space(6.0);
        let keys: Vec<String> = self.vfx.effects.iter().map(|(k, _)| k.clone()).collect();
        for k in keys {
            if ui.button(format!("✨  {k}")).clicked() {
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
        ui.label(egui::RichText::new(format!("✨ {}", doc.name)).strong());
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
                VfxPlaybackDoc::OneShot => "➡ One-shot",
            })
            .show_ui(ui, |ui| {
                for (v, l) in [
                    (VfxPlaybackDoc::Looping, "⟲ Looping"),
                    (VfxPlaybackDoc::OneShot, "➡ One-shot"),
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

        if ui.button("+ Track").clicked() {
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

/// A fresh track for "+ Track": one clip spanning the whole timeline, so it
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
    let body_h = RULER_H
        + doc
            .tracks
            .iter()
            .enumerate()
            .map(|(ti, t)| track_block_h(t, st.expanded_tracks.contains(&ti)))
            .sum::<f32>()
            .max(ROW_H)
        + 8.0;

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
        let mut y = rows_top;
        let mut deferred: Option<DeferredEdit> = None;
        for (ti, track) in doc.tracks.iter().enumerate() {
            let expanded = st.expanded_tracks.contains(&ti);
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

            // Expand toggle (⏷/⏵) — reveals this track's automation lanes below it.
            let tri = Rect::from_min_size(Pos2::new(full.left() + 2.0, y), egui::vec2(14.0, ROW_H));
            let tresp = ui.interact(tri, ui.id().with(("vfx-expand", ti)), Sense::click());
            painter.text(
                tri.center(),
                Align2::CENTER_CENTER,
                if expanded { "⏷" } else { "⏵" },
                FontId::proportional(11.0),
                if track.automation.is_empty() && !expanded {
                    ui.visuals().weak_text_color()
                } else {
                    ui.visuals().text_color()
                },
            );
            if tresp.clicked() {
                deferred = Some(DeferredEdit::ToggleExpand(ti));
            }

            // Label area: mute toggle + name (click selects the track).
            let mute = Rect::from_min_size(Pos2::new(full.left() + 18.0, y + 7.0), egui::vec2(16.0, 16.0));
            let mresp = ui.interact(mute, ui.id().with(("vfx-mute", ti)), Sense::click());
            painter.text(
                mute.center(),
                Align2::CENTER_CENTER,
                if track.enabled { "▣" } else { "☐" },
                FontId::proportional(12.0),
                if track.enabled { ui.visuals().text_color() } else { ui.visuals().weak_text_color() },
            );
            if mresp.clicked() {
                deferred = Some(DeferredEdit::ToggleMute(ti));
            }
            let label = Rect::from_min_size(
                Pos2::new(mute.right() + 4.0, y),
                egui::vec2(LABEL_W - 42.0, ROW_H),
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
            let present: Vec<VfxLaneTargetDoc> = track.automation.iter().map(|l| l.target).collect();
            let life_avail: Vec<LifeProp> = LIFE_PROPS
                .iter()
                .copied()
                .filter(|&p| !matches!(life_prop_doc(track, p), VfxPropDoc::Curve(_)))
                .collect();
            lane_resp.context_menu(|ui| {
                if ui.button("✳ Add burst here").clicked() {
                    deferred = Some(DeferredEdit::AddBurst(ti, ctx_t));
                    ui.close();
                }
                ui.separator();
                ui.weak("📈 Animate over each particle's life");
                for p in &life_avail {
                    if ui.button(format!("{} · life", life_prop_name(*p))).clicked() {
                        deferred = Some(DeferredEdit::AddLifeLane(ti, *p));
                        ui.close();
                    }
                }
                ui.weak("📈 Automate over the timeline");
                let avail: Vec<VfxLaneTargetDoc> =
                    ALL_TARGETS.iter().copied().filter(|t| !present.contains(t)).collect();
                for t in &avail {
                    if ui.button(format!("{} · time", lane_label(*t))).clicked() {
                        deferred = Some(DeferredEdit::AddAutoLane(ti, *t));
                        ui.close();
                    }
                }
                if avail.is_empty() && life_avail.is_empty() {
                    ui.weak("  (all lanes added)");
                }
                ui.separator();
                if ui.button("📋 Duplicate track").clicked() {
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

            // ---- expanded lanes: every animated curve of this track ----
            if expanded {
                let lanes = visible_lanes(track);
                let mut ly = y + ROW_H + LANE_PAD;
                if lanes.is_empty() {
                    painter.text(
                        Pos2::new(view.left + 8.0, ly + LANE_H * 0.5),
                        Align2::LEFT_CENTER,
                        "no curves — animate a property in the Inspector (📈), or right-click the track to add a lane",
                        FontId::proportional(10.5),
                        ui.visuals().weak_text_color(),
                    );
                } else {
                    for lref in lanes {
                        let label_area = Rect::from_min_size(
                            Pos2::new(full.left() + 18.0, ly),
                            egui::vec2(LABEL_W - 20.0, LANE_H),
                        );
                        let strip = Rect::from_min_size(
                            Pos2::new(view.left, ly + 2.0),
                            egui::vec2(dur * px, LANE_H - 4.0),
                        );
                        curve_lane_ui(ui, &painter, &view, st, ti, lref, track, label_area, strip, dur, &mut deferred);
                        ly += LANE_H + LANE_PAD;
                    }
                }
            }

            y += track_block_h(track, expanded);
        }

        if doc.tracks.is_empty() {
            painter.text(
                Pos2::new(view.left + 12.0, rows_top + ROW_H * 0.7),
                Align2::LEFT_CENTER,
                "no tracks yet — + Track adds one (a track = one visual layer of the effect)",
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
        if released {
            // End any lane-point drag: let the value axis re-fit again next frame.
            st.lane_drag = None;
            st.lane_vrange = None;
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

/// Draw + edit ONE lane over the timeline (DAW-style) — a property shaped over the
/// particle's LIFE (velocity/size/rotation/colour) or an automation multiplier over
/// the effect TIMELINE. Scalar lanes are a draggable point-curve on a fixed (auto)
/// or auto-fit (life) range; vector/colour lanes draw their channels/gradient with
/// time-only stops (exact values edit in the Inspector). All edits are deferred (the
/// caller owns `doc`); the 🔑 button drops a keyframe at the playhead.
#[allow(clippy::too_many_arguments)]
fn curve_lane_ui(
    ui: &egui::Ui,
    painter: &egui::Painter,
    view: &TimelineView,
    st: &mut VfxUiState,
    ti: usize,
    lref: LaneRef,
    track: &VfxTrackDoc,
    label_area: Rect,
    strip: Rect,
    dur: f32,
    deferred: &mut Option<DeferredEdit>,
) {
    let Some(curve) = lane_curve(track, lref) else { return };
    let kind = curve.keys.first().map(|k| value_kind(&k.v)).unwrap_or(LaneVis::Scalar);
    let is_stops = matches!(kind, LaneVis::Color | LaneVis::Vec3); // time-only handles
    let dmax = if lane_is_time(lref) { dur } else { 1.0 }; // x-axis maximum
    let is_time = lane_is_time(lref);
    let snap = st.snap_fps;
    let rt = curve_from_doc(curve);

    // Value range: fixed for automation multipliers; auto-fit for life scalars/vec3,
    // frozen while THIS lane's scalar point is dragged so the fit can't feed back.
    let chans = if kind == LaneVis::Vec3 { 3 } else { 1 };
    let (lo, hi) = match lane_fixed_range(track, lref) {
        Some(r) => r,
        None if st.lane_drag == Some((ti, lref)) => {
            st.lane_vrange.unwrap_or_else(|| auto_fit_range(&rt, chans))
        }
        None => auto_fit_range(&rt, chans),
    };
    let to_x = |t: f32| strip.left() + (t / dmax).clamp(0.0, 1.0) * strip.width();
    let v_to_y = |v: f32| strip.bottom() - ((v - lo) / (hi - lo).max(1e-4)) * strip.height();
    let y_to_v = |yy: f32| (lo + (strip.bottom() - yy) / strip.height() * (hi - lo)).clamp(lo, hi);
    // Pointer x → this lane's t: snapped seconds for a timeline lane, raw 0..1 for life.
    let x_to_t = |x: f32| {
        if is_time {
            snap_time(view.x_to_time(x), snap).clamp(0.0, dmax)
        } else {
            ((x - strip.left()) / strip.width().max(1.0)).clamp(0.0, 1.0)
        }
    };

    // ---- left label + add-key-at-playhead (🔑) + remove-lane (🗑) ----
    painter.text(
        Pos2::new(label_area.left() + 2.0, label_area.center().y),
        Align2::LEFT_CENTER,
        lane_ref_label(track, lref),
        FontId::proportional(10.0),
        ui.visuals().text_color(),
    );
    let key_btn = Rect::from_min_size(
        Pos2::new(label_area.right() - 34.0, label_area.center().y - 8.0),
        egui::vec2(15.0, 16.0),
    );
    let kresp = ui.interact(key_btn, ui.id().with(("vfx-lane-key", ti, lref)), Sense::click());
    painter.text(
        key_btn.center(),
        Align2::CENTER_CENTER,
        "🔑",
        FontId::proportional(10.0),
        if kresp.hovered() { ACCENT } else { ui.visuals().weak_text_color() },
    );
    if kresp.on_hover_text("add a keyframe at the playhead").clicked() {
        *deferred = Some(DeferredEdit::AddKeyAtPlayhead(ti, lref));
    }
    let del = Rect::from_min_size(
        Pos2::new(label_area.right() - 16.0, label_area.center().y - 8.0),
        egui::vec2(14.0, 16.0),
    );
    let dresp = ui.interact(del, ui.id().with(("vfx-lane-del", ti, lref)), Sense::click());
    painter.text(
        del.center(),
        Align2::CENTER_CENTER,
        "🗑",
        FontId::proportional(11.0),
        if dresp.hovered() { ACCENT } else { ui.visuals().weak_text_color() },
    );
    if dresp.on_hover_text("remove this lane").clicked() {
        *deferred = Some(DeferredEdit::DelLane(ti, lref));
    }

    // ---- strip background + empty double-click-to-add (registered BEFORE the
    // handles so the handles, drawn after, win the pointer where they overlap) ----
    painter.rect_filled(strip, 3.0, ui.visuals().extreme_bg_color);
    let sresp = ui.interact(strip, ui.id().with(("vfx-lane-strip", ti, lref)), Sense::click());
    if sresp.double_clicked()
        && let Some(p) = sresp.interact_pointer_pos()
    {
        let t = x_to_t(p.x);
        let sc = rt.eval(t);
        let v = match kind {
            LaneVis::Scalar => VfxValueDoc::F32(y_to_v(p.y)),
            LaneVis::Vec3 => VfxValueDoc::Vec3([sc[0], sc[1], sc[2]]),
            LaneVis::Color => VfxValueDoc::Rgba([sc[0], sc[1], sc[2], sc[3]]),
        };
        *deferred = Some(DeferredEdit::AddKey(ti, lref, t, v));
    }

    // ---- the curve / channels / gradient ----
    match kind {
        LaneVis::Color => {
            let n = strip.width().max(2.0) as usize;
            for i in 0..n {
                let u = i as f32 / (n - 1).max(1) as f32;
                let c = rt.eval(u * dmax);
                let x = strip.left() + u * strip.width();
                painter.line_segment(
                    [Pos2::new(x, strip.top()), Pos2::new(x, strip.bottom())],
                    Stroke::new(1.5, Color32::from_rgb((c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8)),
                );
            }
        }
        LaneVis::Vec3 => {
            let cols = [
                Color32::from_rgb(230, 120, 120),
                Color32::from_rgb(120, 220, 120),
                Color32::from_rgb(120, 160, 240),
            ];
            for (ch, col) in cols.iter().enumerate() {
                let mut pts = Vec::new();
                let n = 40;
                for i in 0..=n {
                    let u = i as f32 / n as f32;
                    let v = rt.eval(u * dmax)[ch];
                    pts.push(Pos2::new(strip.left() + u * strip.width(), v_to_y(v).clamp(strip.top(), strip.bottom())));
                }
                painter.add(egui::Shape::line(pts, Stroke::new(1.0, *col)));
            }
        }
        LaneVis::Scalar => {
            if lo < 1.0 && hi > 1.0 {
                let y1 = v_to_y(1.0);
                painter.line_segment(
                    [Pos2::new(strip.left(), y1), Pos2::new(strip.right(), y1)],
                    Stroke::new(0.5, ui.visuals().weak_text_color().gamma_multiply(0.4)),
                );
            }
            let mut pts = Vec::new();
            let n = 48;
            for i in 0..=n {
                let u = i as f32 / n as f32;
                let v = rt.eval(u * dmax)[0];
                pts.push(Pos2::new(strip.left() + u * strip.width(), v_to_y(v).clamp(strip.top(), strip.bottom())));
            }
            painter.add(egui::Shape::line(pts, Stroke::new(1.5, ACCENT)));
        }
    }

    // ---- breakpoints / stops ----
    for (ki, k) in curve.keys.iter().enumerate() {
        let x = to_x(k.t.clamp(0.0, dmax));
        let sel = st.auto_sel == Some((ti, lref, ki));
        let hit = if is_stops {
            painter.line_segment(
                [Pos2::new(x, strip.top()), Pos2::new(x, strip.bottom())],
                Stroke::new(if sel { 2.0 } else { 1.0 }, if sel { ACCENT } else { Color32::from_black_alpha(150) }),
            );
            painter.circle(Pos2::new(x, strip.bottom()), if sel { 4.0 } else { 3.0 }, if sel { ACCENT } else { KEY_COLOR }, Stroke::new(1.0, Color32::from_black_alpha(160)));
            Rect::from_center_size(Pos2::new(x, strip.center().y), egui::vec2(12.0, strip.height()))
        } else {
            let v = match k.v {
                VfxValueDoc::F32(x) => x,
                _ => 0.0,
            };
            let c = Pos2::new(x, v_to_y(v).clamp(strip.top(), strip.bottom()));
            painter.circle(c, if sel { 5.0 } else { 4.0 }, if sel { ACCENT } else { KEY_COLOR }, Stroke::new(1.0, Color32::from_black_alpha(160)));
            Rect::from_center_size(c, egui::vec2(12.0, 12.0))
        };
        let resp = ui.interact(hit, ui.id().with(("vfx-lane-pt", ti, lref, ki)), Sense::click_and_drag());
        if resp.clicked() || resp.drag_started() {
            st.auto_sel = Some((ti, lref, ki));
            st.sel_track = Some(ti);
            // Freeze an auto-fit lane's value axis for the whole scalar drag.
            if resp.drag_started() && !is_stops && lane_fixed_range(track, lref).is_none() {
                st.lane_drag = Some((ti, lref));
                st.lane_vrange = Some((lo, hi));
            }
        }
        if resp.dragged()
            && let Some(p) = resp.interact_pointer_pos()
        {
            let t = x_to_t(p.x);
            let v = if is_stops { k.v } else { VfxValueDoc::F32(y_to_v(p.y)) };
            *deferred = Some(DeferredEdit::MoveKey(ti, lref, ki, t, v));
        }
        resp.context_menu(|ui| {
            if !is_stops {
                for (iv, lbl) in [
                    (VfxInterpDoc::Constant, "hold"),
                    (VfxInterpDoc::Linear, "linear"),
                    (VfxInterpDoc::Bezier, "smooth"),
                ] {
                    if ui.button(lbl).clicked() {
                        *deferred = Some(DeferredEdit::SetInterp(ti, lref, ki, iv));
                        ui.close();
                    }
                }
                ui.separator();
            }
            if curve.keys.len() > 1 && ui.button("🗑 delete point").clicked() {
                *deferred = Some(DeferredEdit::DelKey(ti, lref, ki));
                ui.close();
            }
        });
    }
}

/// Structural edits deferred out of the per-track iteration borrow.
enum DeferredEdit {
    ToggleMute(usize),
    ToggleExpand(usize),
    AddClip(usize, f32),
    AddBurst(usize, f32),
    DelClip(usize, usize),
    DelBurst(usize, usize),
    DupTrack(usize),
    DelTrack(usize),
    AddAutoLane(usize, VfxLaneTargetDoc),
    AddLifeLane(usize, LifeProp),
    DelLane(usize, LaneRef),
    AddKey(usize, LaneRef, f32, VfxValueDoc),
    AddKeyAtPlayhead(usize, LaneRef),
    MoveKey(usize, LaneRef, usize, f32, VfxValueDoc),
    DelKey(usize, LaneRef, usize),
    SetInterp(usize, LaneRef, usize, VfxInterpDoc),
}

fn apply_deferred(edit: DeferredEdit, st: &mut VfxUiState, doc: &mut VfxEffectDoc, dur: f32) {
    match edit {
        DeferredEdit::ToggleMute(ti) => {
            if let Some(t) = doc.tracks.get_mut(ti) {
                t.enabled = !t.enabled;
            }
        }
        DeferredEdit::ToggleExpand(ti) => {
            if !st.expanded_tracks.remove(&ti) {
                st.expanded_tracks.insert(ti);
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
                // Track indices shifted — collapse lanes so no stale index draws.
                st.expanded_tracks.clear();
                st.auto_sel = None;
            }
        }
        DeferredEdit::DelTrack(ti) => {
            if ti < doc.tracks.len() {
                doc.tracks.remove(ti);
                st.sel_track = None;
                st.sel = None;
                st.expanded_tracks.clear();
                st.auto_sel = None;
            }
        }
        DeferredEdit::AddAutoLane(ti, target) => {
            if let Some(t) = doc.tracks.get_mut(ti) {
                t.automation.push(starter_lane(target, dur));
                st.expanded_tracks.insert(ti); // reveal it so the artist can shape it
            }
        }
        DeferredEdit::AddLifeLane(ti, prop) => {
            if let Some(t) = doc.tracks.get_mut(ti) {
                promote_to_curve(life_prop_doc_mut(t, prop));
                st.expanded_tracks.insert(ti);
            }
        }
        DeferredEdit::DelLane(ti, lref) => {
            if let Some(t) = doc.tracks.get_mut(ti) {
                match lref {
                    LaneRef::Auto(li) if li < t.automation.len() => {
                        t.automation.remove(li);
                    }
                    LaneRef::Life(p) => {
                        // "Removing" a life lane = stop animating it: collapse to a
                        // constant (the value the curve held at birth).
                        let prop = life_prop_doc_mut(t, p);
                        if let VfxPropDoc::Curve(c) = prop {
                            let v0 = c.keys.first().map(|k| k.v).unwrap_or(VfxValueDoc::F32(0.0));
                            *prop = VfxPropDoc::Const(v0);
                        }
                    }
                    LaneRef::Auto(_) => {}
                }
                st.auto_sel = None;
            }
        }
        DeferredEdit::AddKey(ti, lref, t, v) => {
            if let Some(c) = doc.tracks.get_mut(ti).and_then(|tr| lane_curve_mut(tr, lref)) {
                c.keys.push(VfxKeyDoc { t, v, interp: VfxInterpDoc::Linear, in_tan: 0.0, out_tan: 0.0 });
                c.keys.sort_by(|a, b| a.t.total_cmp(&b.t));
                st.auto_sel = c.keys.iter().position(|k| k.t == t).map(|ki| (ti, lref, ki));
            }
        }
        DeferredEdit::AddKeyAtPlayhead(ti, lref) => {
            // Insert a keyframe at the playhead (mapped into the lane's domain) holding
            // the curve's current value there — a clean insert that doesn't reshape it.
            let dmax = if lane_is_time(lref) { dur } else { 1.0 };
            let t = if lane_is_time(lref) {
                match doc.playback {
                    VfxPlaybackDoc::Looping => st.playhead.rem_euclid(dur),
                    VfxPlaybackDoc::OneShot => st.playhead.min(dur),
                }
            } else {
                (st.playhead / dur).clamp(0.0, 1.0)
            }
            .clamp(0.0, dmax);
            if let Some(c) = doc.tracks.get_mut(ti).and_then(|tr| lane_curve_mut(tr, lref)) {
                let sc = curve_from_doc(c).eval(t);
                let v = match c.keys.first().map(|k| k.v) {
                    Some(VfxValueDoc::Vec3(_)) => VfxValueDoc::Vec3([sc[0], sc[1], sc[2]]),
                    Some(VfxValueDoc::Rgba(_)) => VfxValueDoc::Rgba([sc[0], sc[1], sc[2], sc[3]]),
                    _ => VfxValueDoc::F32(sc[0]),
                };
                c.keys.push(VfxKeyDoc { t, v, interp: VfxInterpDoc::Linear, in_tan: 0.0, out_tan: 0.0 });
                c.keys.sort_by(|a, b| a.t.total_cmp(&b.t));
                st.auto_sel = c.keys.iter().position(|k| k.t == t).map(|ki| (ti, lref, ki));
            }
        }
        DeferredEdit::MoveKey(ti, lref, ki, t, v) => {
            let dmax = if lane_is_time(lref) { dur } else { 1.0 };
            if let Some(c) = doc.tracks.get_mut(ti).and_then(|tr| lane_curve_mut(tr, lref)) {
                // Clamp between neighbours so keys never reorder mid-drag — the key
                // index stays valid, so no re-sort (and no selection jump) is needed.
                let n = c.keys.len();
                let lo = if ki > 0 { c.keys[ki - 1].t + 1e-3 } else { 0.0 };
                let hi = if ki + 1 < n { c.keys[ki + 1].t - 1e-3 } else { dmax };
                let (a, b) = (lo.min(hi), lo.max(hi));
                if let Some(k) = c.keys.get_mut(ki) {
                    k.t = t.clamp(a, b);
                    k.v = v;
                }
            }
        }
        DeferredEdit::DelKey(ti, lref, ki) => {
            if let Some(c) = doc.tracks.get_mut(ti).and_then(|tr| lane_curve_mut(tr, lref))
                && ki < c.keys.len()
                && c.keys.len() > 1
            {
                c.keys.remove(ki);
                st.auto_sel = None;
            }
        }
        DeferredEdit::SetInterp(ti, lref, ki, iv) => {
            if let Some(c) = doc.tracks.get_mut(ti).and_then(|tr| lane_curve_mut(tr, lref))
                && let Some(k) = c.keys.get_mut(ki)
            {
                k.interp = iv;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_lanes_include_life_curves_and_automation() {
        let mut t = starter_effect_doc("x").tracks.remove(0);
        // The starter animates size + colour over life → two life lanes, no automation.
        let base = visible_lanes(&t);
        assert!(base.contains(&LaneRef::Life(LifeProp::Size)));
        assert!(base.contains(&LaneRef::Life(LifeProp::Color)));
        assert!(base.iter().all(|l| matches!(l, LaneRef::Life(_))), "no automation yet");
        // A collapsed track is one row; expanding reserves a strip per visible lane.
        assert_eq!(track_block_h(&t, false), ROW_H);
        let h0 = track_block_h(&t, true);
        t.automation.push(starter_lane(VfxLaneTargetDoc::Rate, 2.0));
        assert_eq!(visible_lanes(&t).len(), base.len() + 1, "automation adds a lane");
        assert!(track_block_h(&t, true) > h0, "the extra lane adds height");
    }

    #[test]
    fn promote_to_curve_seeds_a_flat_life_curve() {
        let mut prop = VfxPropDoc::Const(VfxValueDoc::F32(0.7));
        promote_to_curve(&mut prop);
        match prop {
            VfxPropDoc::Curve(c) => {
                assert_eq!(c.keys.len(), 2);
                assert_eq!((c.keys[0].t, c.keys[1].t), (0.0, 1.0));
                assert!(matches!(c.keys[0].v, VfxValueDoc::F32(v) if v == 0.7));
            }
            _ => panic!("promote must produce a curve"),
        }
    }

    #[test]
    fn starter_lane_spans_the_timeline_and_is_a_flat_no_op() {
        // Keys authored in SECONDS across [0, dur] — the domain the bake expects.
        let l = starter_lane(VfxLaneTargetDoc::Rate, 2.0);
        assert_eq!(l.curve.keys.len(), 2);
        assert_eq!(l.curve.keys[0].t, 0.0);
        assert_eq!(l.curve.keys[1].t, 2.0);
        assert!(matches!(l.curve.keys[0].v, VfxValueDoc::F32(v) if v == 1.0), "flat ×1 (no-op)");
        // A tint lane starts white so it multiplies colour by 1 until shaped.
        let tint = starter_lane(VfxLaneTargetDoc::Tint, 1.0);
        assert!(matches!(tint.curve.keys[0].v, VfxValueDoc::Rgba(c) if c == [1.0; 4]));
    }

    #[test]
    fn scalar_lane_ranges_are_fixed_and_tint_is_unit() {
        assert_eq!(lane_vrange(VfxLaneTargetDoc::Rate), (0.0, 4.0));
        assert_eq!(lane_vrange(VfxLaneTargetDoc::Size), (0.0, 3.0));
        assert_eq!(lane_vrange(VfxLaneTargetDoc::Tint), (0.0, 1.0));
    }
}

