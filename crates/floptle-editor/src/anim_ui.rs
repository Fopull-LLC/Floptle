//! Animation editor UI: the model-asset animations panel (Inspector), the
//! AnimationController component section, the **Animation Controller graph
//! window** (states as draggable nodes, transitions as arrows with editable
//! fades), and the **Animating tab** (dopesheet timeline: scrub, preview,
//! record, keys, events).
//!
//! All persistent UI state lives in [`AnimUiState`] (one field on `Editor`,
//! borrowed into `EditorTabViewer`); asset mutations edit a working copy and
//! save on pointer-release (drags coalesce into one disk write).

use std::collections::HashMap;
use std::path::Path;

use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2 as EVec2};
use floptle_anim::TransformTRS;
use floptle_core::{AnimController, Entity, Matter, Name};
use floptle_scene::{
    AnimClipDoc, AnimControllerDoc, AnimEventDoc, AnimPropTrackDoc, AnimPropValueDoc, AnimStateDoc,
    AnimTrackDoc3, AnimTrackDoc4, AnimTransitionDoc, ANIM_CLIP_EXT,
};

use crate::anim;
use crate::{AssetPayload, EditorCmd, EditorTabViewer};

/// Persistent animation-UI state (a field on `Editor`).
pub struct AnimUiState {
    // ---- Inspector: model asset panel ----
    /// Cached animation names per model path (cheap glTF header probe).
    pub probes: HashMap<String, Vec<String>>,

    // ---- Controller graph tab ----
    /// Controller asset key being edited.
    pub graph_key: Option<String>,
    /// Working copy (edits land here; saved on release).
    pub graph_doc: Option<AnimControllerDoc>,
    pub graph_dirty: bool,
    pub graph_layer: usize,
    pub sel_state: Option<String>,
    pub sel_trans: Option<(String, String)>,
    /// In-progress transition drag (from state name).
    pub drag_from: Option<String>,
    pub graph_pan: EVec2,
    /// New-controller name prompt buffer (`Some` = prompt open).
    pub new_ctl_buf: Option<String>,
    /// Attach the newly created controller to this node (Add Component flow).
    pub new_ctl_attach: Option<Entity>,
    /// Folder (project-relative) the new controller is created in.
    pub new_ctl_dir: Option<String>,
    /// Focus the open name prompt once (not every frame — that eats Escape).
    pub focus_prompt: bool,
    /// Layer index awaiting delete confirmation in the graph window.
    pub confirm_delete_layer: Option<usize>,

    // ---- Animating tab ----
    /// The bound node (must carry a controller or a rigged mesh).
    pub target: Option<Entity>,
    /// Selected state name in the target's controller.
    pub sel_anim: Option<String>,
    /// Working copy of the selected state's clip doc (key, doc).
    pub clip_doc: Option<(String, AnimClipDoc)>,
    pub clip_dirty: bool,
    pub playhead: f32,
    pub preview_playing: bool,
    pub record: bool,
    /// Timeline zoom (pixels per second).
    pub zoom: f32,
    /// Vertical (row-height) zoom multiplier — Alt+scroll over the dopesheet.
    pub row_scale: f32,
    /// Last frame's ScrollArea offset (the anchor for cursor-centred zoom).
    pub scroll_off: egui::Vec2,
    /// A scroll offset to force next frame (cursor-anchored zoom / Fit).
    pub scroll_target: Option<egui::Vec2>,
    /// A pending Fit (the F key / Fit button) — zoom the whole clip into view.
    pub fit_pending: bool,
    /// Key-snap grid (frames/sec); 0 = off.
    pub snap_fps: f32,
    pub sel_event: Option<usize>,
    /// The Animating tab drew last frame (gates the render-loop preview).
    pub tab_visible: bool,
    /// In-flight key drag: (channel, original time, previewed time). The doc
    /// is only retimed on release, so egui ids stay stable through the drag.
    pub key_drag: Option<(usize, f32, f32)>,
    /// Selected property key `(channel, track, key index)` — its value is edited
    /// inline above the dopesheet (a texture picker for image lanes, else a number).
    pub sel_prop: Option<(usize, usize, usize)>,
    /// In-flight property-key drag: (channel, track, original time, previewed time).
    pub prop_key_drag: Option<(usize, usize, f32, f32)>,
    /// Pre-record transforms of the target subtree — restored when ● Record
    /// turns off, so recording authors the CLIP, never the scene.
    pub record_restore: Vec<(Entity, floptle_core::Transform)>,
    /// Last-seen local TRS of the target's descendants (record-mode diffing).
    pub last_scene_local: HashMap<Entity, TransformTRS>,
    /// Last-seen numeric component fields of the subtree (record-mode property
    /// diffing) — entity → component → field → value. A change since last frame
    /// auto-keys the field (e.g. a spritesheet `cell`).
    pub last_scene_props: HashMap<Entity, HashMap<String, HashMap<String, f64>>>,
    /// Pre-record numeric property values, re-applied when ● Record turns off so
    /// recording authors the CLIP not the scene: (entity, component, field, value).
    pub record_restore_props: Vec<(Entity, String, String, f64)>,
    /// New-animation name prompt buffer (`Some` = prompt open).
    pub new_anim_buf: Option<String>,

    // ---- Property-track builder (Animating tab) ----
    /// The "add property track" picker's node name ("" = the animated node).
    pub prop_node: String,
    /// The picker's component + field.
    pub prop_comp: String,
    pub prop_field: String,
}

impl Default for AnimUiState {
    fn default() -> Self {
        Self {
            probes: HashMap::new(),
            graph_key: None,
            graph_doc: None,
            graph_dirty: false,
            graph_layer: 0,
            sel_state: None,
            sel_trans: None,
            drag_from: None,
            graph_pan: EVec2::ZERO,
            new_ctl_buf: None,
            new_ctl_attach: None,
            new_ctl_dir: None,
            focus_prompt: false,
            confirm_delete_layer: None,
            target: None,
            sel_anim: None,
            clip_doc: None,
            clip_dirty: false,
            playhead: 0.0,
            preview_playing: false,
            record: false,
            zoom: 120.0,
            row_scale: 1.0,
            scroll_off: egui::Vec2::ZERO,
            scroll_target: None,
            fit_pending: false,
            snap_fps: 0.0,
            sel_event: None,
            tab_visible: false,
            key_drag: None,
            sel_prop: None,
            prop_key_drag: None,
            record_restore: Vec::new(),
            last_scene_local: HashMap::new(),
            last_scene_props: HashMap::new(),
            record_restore_props: Vec::new(),
            new_anim_buf: None,
            prop_node: String::new(),
            prop_comp: "UiElement".into(),
            prop_field: "image".into(),
        }
    }
}

/// `path` is a baked animation clip asset.
pub fn is_anim_clip(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(".anim.ron")
}

/// `path` is an animation controller asset.
pub fn is_anim_ctl(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(".actl.ron")
}

use crate::timeline::{draw_ruler, ACCENT, EVENT_COLOR, KEY_COLOR, PLAYHEAD};

impl EditorTabViewer<'_> {
    // =========================================================================
    // Inspector: selected MODEL asset — list packaged animations + extract.
    // =========================================================================
    pub fn model_asset_anim_ui(&mut self, ui: &mut egui::Ui, path: &str) {
        let names = self
            .anim_ui
            .probes
            .entry(path.to_string())
            .or_insert_with(|| floptle_assets::probe_animations(Path::new(path)))
            .clone();
        if names.is_empty() {
            ui.small("no animations in this model");
            return;
        }
        ui.separator();
        ui.strong(format!("▶ Animations ({})", names.len()));
        for n in &names {
            ui.label(format!("   ▶ {n}"));
        }
        // Which clips are already extracted?
        let stem = Path::new(path)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let extracted = names
            .iter()
            .filter(|n| {
                let safe: String = n
                    .chars()
                    .map(|c| if c == '/' || c == '\\' || c == ':' { '_' } else { c })
                    .collect();
                self.anim.clip(&format!("animations/{stem}/{safe}")).is_some()
            })
            .count();
        if extracted == names.len() {
            ui.small(format!("all extracted → animations/{stem}/"));
        }
        if ui
            .button("⬇ Extract animations")
            .on_hover_text(format!(
                "bake each clip to its own .anim.ron under animations/{stem}/ — standalone \
                 files you can organize freely, add to controllers, and put events on"
            ))
            .clicked()
        {
            self.cmd.extract_anims = Some(path.to_string());
        }
    }

    // =========================================================================
    // Inspector: selected CLIP asset — summary + events hint.
    // =========================================================================
    pub fn clip_asset_ui(&mut self, ui: &mut egui::Ui, path: &str) {
        let key = anim::asset_key(Path::new(path), self.project_root, ANIM_CLIP_EXT);
        ui.label("animation clip");
        if let Some(doc) = self.anim.clip(&key) {
            ui.small(format!(
                "{} · {:.2}s · {} channel(s) · {} event(s)",
                doc.name,
                doc.duration,
                doc.channels.len(),
                doc.events.len()
            ));
            if !doc.source_model.is_empty() {
                ui.small(format!("from {}", doc.source_model));
            }
            ui.small("add it to a controller (drag into the graph window), then edit keys/events in the Animating tab");
        } else {
            ui.small("(unreadable clip file)");
        }
    }

    // =========================================================================
    // Inspector: selected CONTROLLER asset.
    // =========================================================================
    pub fn ctl_asset_ui(&mut self, ui: &mut egui::Ui, path: &str) {
        let key = anim::asset_key(Path::new(path), self.project_root, floptle_scene::ANIM_CTL_EXT);
        ui.label("animation controller");
        if let Some(doc) = self.anim.controller(&key) {
            let states: usize = doc.layers.iter().map(|l| l.states.len()).sum();
            ui.small(format!("{} layer(s) · {states} state(s)", doc.layers.len()));
        }
        if ui.button("◎ Open in graph editor").clicked() {
            self.cmd.open_anim_graph = Some(key);
        }
    }

}

// =============================================================================
// Inspector: the Animation Controller COMPONENT section on a node. A free
// function (not a viewer method) so the Inspector can call it while its own
// `world`/`cmd` reborrows are live.
// =============================================================================
pub fn anim_component_ui(
    ui: &mut egui::Ui,
    e: Entity,
    world: &floptle_core::World,
    anim: &anim::AnimSystem,
    anim_ui: &mut AnimUiState,
    cmd: &mut EditorCmd,
) {
    let Some(key) = world.get::<AnimController>(e).map(|c| c.asset.clone()) else {
        return;
    };
    let (_, _, remove) = crate::inspector::component_header(ui, "▶ Animation Controller", false, true);
    if remove {
        cmd.set_anim_controller = Some((e, None));
        return;
    }
    ui.indent("anim_ctl_props", |ui| {
        let missing = anim.controller(&key).is_none();
        ui.horizontal(|ui| {
            ui.label("controller");
            let label = if missing { format!("⚠ {key}") } else { key.clone() };
            egui::ComboBox::from_id_salt("anim-ctl-pick")
                .selected_text(label)
                .show_ui(ui, |ui| {
                    for (k, _) in anim.controllers.iter() {
                        if ui.selectable_label(*k == key, k).clicked() && *k != key {
                            cmd.set_anim_controller = Some((e, Some(k.clone())));
                        }
                    }
                });
        });
        if missing {
            ui.colored_label(
                Color32::from_rgb(230, 160, 80),
                "controller asset not found — pick another or create one",
            );
        }
        ui.horizontal(|ui| {
            if ui.button("◎ Edit graph").clicked() {
                cmd.open_anim_graph = Some(key.clone());
            }
            if ui
                .button("✏ Animate")
                .on_hover_text("open the Animating timeline bound to this node")
                .clicked()
            {
                anim_ui.target = Some(e);
                cmd.focus_animating = true;
            }
        });
        if let Some(doc) = anim.controller(&key) {
            let states: usize = doc.layers.iter().map(|l| l.states.len()).sum();
            ui.small(format!(
                "{} layer(s) · {states} state(s) · default fade {:.2}s{}",
                doc.layers.len(),
                doc.default_fade,
                doc.sample_fps.map(|f| format!(" · stepped {f:.0} fps")).unwrap_or_default()
            ));
        }
    });
    ui.add_space(4.0);
}

impl EditorTabViewer<'_> {

    // =========================================================================
    // The Animation Controller graph tab (dockable — resize/move it freely).
    // =========================================================================
    pub fn anim_graph_tab_ui(&mut self, ui: &mut egui::Ui) {
        // Sync the working copy with the selected key.
        if self.anim_ui.graph_doc.is_none()
            && let Some(key) = self.anim_ui.graph_key.clone() {
                self.anim_ui.graph_doc = self.anim.controller(&key).cloned();
            }
        self.anim_graph_ui(ui);
    }

    fn flush_graph(&mut self, force: bool) {
        if self.anim_ui.graph_dirty
            && (force || !self.pointer_down)
        {
            if let (Some(key), Some(doc)) = (&self.anim_ui.graph_key, &self.anim_ui.graph_doc) {
                self.anim.save_controller(self.project_root, key, doc);
            }
            self.anim_ui.graph_dirty = false;
        }
    }

    fn anim_graph_ui(&mut self, ui: &mut egui::Ui) {
        let st = &mut *self.anim_ui;
        // ---- header: controller pick / create + controller-wide knobs ----
        ui.horizontal(|ui| {
            ui.label("controller");
            let cur = st.graph_key.clone().unwrap_or_else(|| "(pick)".into());
            egui::ComboBox::from_id_salt("graph-ctl")
                .selected_text(cur.clone())
                .show_ui(ui, |ui| {
                    let keys: Vec<String> =
                        self.anim.controllers.iter().map(|(k, _)| k.clone()).collect();
                    for k in keys {
                        if ui.selectable_label(Some(&k) == st.graph_key.as_ref(), &k).clicked() {
                            st.graph_key = Some(k.clone());
                            st.graph_doc = self.anim.controller(&k).cloned();
                            st.graph_dirty = false;
                            st.graph_layer = 0;
                            st.sel_state = None;
                            st.sel_trans = None;
                        }
                    }
                });
            if ui.button("✚ New…").clicked() {
                st.new_ctl_buf = Some(String::new());
                st.focus_prompt = true;
            }
            if let Some(doc) = st.graph_doc.as_mut() {
                ui.separator();
                ui.label("default fade");
                if ui
                    .add(egui::DragValue::new(&mut doc.default_fade).speed(0.01).range(0.0..=5.0).suffix("s"))
                    .changed()
                {
                    st.graph_dirty = true;
                }
                let mut stepped = doc.sample_fps.is_some();
                if ui
                    .checkbox(&mut stepped, "stepped")
                    .on_hover_text("retro choppy playback: sample every animation on a fixed frame grid (states can override with their own fps)")
                    .changed()
                {
                    doc.sample_fps = if stepped { Some(12.0) } else { None };
                    st.graph_dirty = true;
                }
                if let Some(fps) = doc.sample_fps.as_mut()
                    && ui.add(egui::DragValue::new(fps).speed(0.2).range(1.0..=60.0).suffix(" fps")).changed() {
                        st.graph_dirty = true;
                    }
            }
        });
        // New-controller prompt.
        if let Some(buf) = st.new_ctl_buf.as_mut() {
            let mut done = false;
            let mut cancel = ui.input(|i| i.key_pressed(egui::Key::Escape));
            ui.horizontal(|ui| {
                ui.label("name");
                let resp = ui.text_edit_singleline(buf);
                if st.focus_prompt {
                    resp.request_focus();
                    st.focus_prompt = false;
                }
                if (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                    || ui.button("Create").clicked()
                {
                    done = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
                if let Some(e) = st.new_ctl_attach {
                    let n = self
                        .entity_names
                        .iter()
                        .find(|(x, _)| *x == e)
                        .map(|(_, n)| n.clone())
                        .unwrap_or_else(|| "the selected node".into());
                    ui.small(format!("→ will attach to \"{n}\""));
                }
                if let Some(d) = &st.new_ctl_dir {
                    ui.small(format!("in {d}/"));
                }
            });
            if done && !buf.trim().is_empty() {
                let name = buf.trim().to_string();
                let key =
                    anim::new_controller_key(self.project_root, st.new_ctl_dir.as_deref(), &name);
                let doc = AnimControllerDoc::default();
                self.anim.save_controller(self.project_root, &key, &doc);
                // Add-Component flow: attach the fresh controller to the node.
                if let Some(e) = st.new_ctl_attach.take() {
                    self.cmd.set_anim_controller = Some((e, Some(key.clone())));
                }
                self.cmd.refresh_assets = true; // show the new file in the browser
                st.graph_key = Some(key);
                st.graph_doc = Some(doc);
                st.graph_dirty = false;
                st.graph_layer = 0;
                st.new_ctl_buf = None;
            } else if cancel || done {
                st.new_ctl_buf = None;
                st.new_ctl_attach = None;
                st.new_ctl_dir = None;
            }
        }
        let Some(doc) = st.graph_doc.as_mut() else {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.weak("Pick a controller above, or create one — then drag clips from the Assets browser into the graph.");
            });
            return;
        };
        ui.separator();

        // ---- layers strip ----
        ui.horizontal(|ui| {
            ui.label("layers");
            ui.small("(left = base, right = higher priority)").on_hover_text(
                "a playing state on a higher layer overrides the nodes it animates; \
                 everything else shows the layers below",
            );
            let mut remove: Option<usize> = None;
            for i in 0..doc.layers.len() {
                let sel = st.graph_layer == i;
                let resp = ui.selectable_label(sel, &doc.layers[i].name);
                if resp.clicked() {
                    st.graph_layer = i;
                    st.sel_state = None;
                    st.sel_trans = None;
                }
                resp.context_menu(|ui| {
                    if doc.layers.len() > 1 && ui.button("🗑 Delete layer…").clicked() {
                        remove = Some(i); // asks for confirmation below
                        ui.close();
                    }
                    if i > 0 && ui.button("⬅ Move down (lower priority)").clicked() {
                        doc.layers.swap(i, i - 1);
                        // keep the selection on the layer the user had selected
                        if st.graph_layer == i {
                            st.graph_layer = i - 1;
                        } else if st.graph_layer == i - 1 {
                            st.graph_layer = i;
                        }
                        st.graph_dirty = true;
                        ui.close();
                    }
                    if i + 1 < doc.layers.len() && ui.button("➡ Move up (higher priority)").clicked() {
                        doc.layers.swap(i, i + 1);
                        if st.graph_layer == i {
                            st.graph_layer = i + 1;
                        } else if st.graph_layer == i + 1 {
                            st.graph_layer = i;
                        }
                        st.graph_dirty = true;
                        ui.close();
                    }
                });
            }
            if let Some(i) = remove {
                st.confirm_delete_layer = Some(i);
            }
            if ui.button("✚").on_hover_text("add a layer (e.g. an Attack layer above Movement)").clicked() {
                let n = doc.layers.len();
                doc.layers.push(floptle_scene::AnimLayerDoc {
                    name: format!("Layer{n}"),
                    weight: 1.0,
                    states: Vec::new(),
                    default_state: None,
                    transitions: Vec::new(),
                });
                st.graph_layer = doc.layers.len() - 1;
                st.graph_dirty = true;
            }
        });
        // Deleting a layer erases all its states + transitions from the asset —
        // confirm first (asset edits are outside scene undo).
        if let Some(i) = st.confirm_delete_layer {
            if i >= doc.layers.len() {
                st.confirm_delete_layer = None;
            } else {
                let name = doc.layers[i].name.clone();
                ui.horizontal(|ui| {
                    ui.colored_label(
                        Color32::from_rgb(230, 160, 80),
                        format!("Delete layer \"{name}\" and everything in it? This edits the asset file and can't be undone."),
                    );
                    if ui.button("🗑 Delete").clicked() {
                        doc.layers.remove(i);
                        if st.graph_layer >= i && st.graph_layer > 0 {
                            st.graph_layer -= 1;
                        }
                        st.sel_state = None;
                        st.sel_trans = None;
                        st.graph_dirty = true;
                        st.confirm_delete_layer = None;
                    }
                    if ui.button("Cancel").clicked() {
                        st.confirm_delete_layer = None;
                    }
                });
            }
        }
        st.graph_layer = st.graph_layer.min(doc.layers.len().saturating_sub(1));
        let li = st.graph_layer;
        ui.horizontal(|ui| {
            let layer = &mut doc.layers[li];
            ui.label("name");
            if ui.add(egui::TextEdit::singleline(&mut layer.name).desired_width(110.0)).changed() {
                st.graph_dirty = true;
            }
            ui.label("weight");
            if ui.add(egui::Slider::new(&mut layer.weight, 0.0..=1.0)).changed() {
                st.graph_dirty = true;
            }
            if let Some(d) = &layer.default_state {
                ui.small(format!("default: {d}"));
            } else {
                ui.small("no default state (right-click a node)");
            }
        });

        // ---- canvas (left) + selection panel (right) ----
        let panel_w = 210.0;
        ui.horizontal_top(|ui| {
            let canvas_w = (ui.available_width() - panel_w - 8.0).max(200.0);
            let canvas_h = ui.available_height().max(200.0);
            let (canvas_rect, canvas_resp) =
                ui.allocate_exact_size(egui::vec2(canvas_w, canvas_h), Sense::click_and_drag());
            self.graph_canvas(ui, canvas_rect, canvas_resp);
            ui.separator();
            ui.vertical(|ui| {
                ui.set_width(panel_w - 12.0);
                self.graph_side_panel(ui);
            });
        });

        self.flush_graph(false);
    }

    /// The node-graph canvas for the selected layer.
    fn graph_canvas(&mut self, ui: &mut egui::Ui, rect: Rect, bg_resp: egui::Response) {
        let cmd = &mut *self.cmd;
        let anim: &anim::AnimSystem = &*self.anim;
        let world: &floptle_core::World = &*self.world;
        let project_root = self.project_root;
        let st = &mut *self.anim_ui;
        let Some(doc) = st.graph_doc.as_mut() else { return };
        let default_fade = doc.default_fade;
        let li = st.graph_layer.min(doc.layers.len().saturating_sub(1));
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 4.0, ui.visuals().extreme_bg_color);

        // Pan with a background drag (nodes consume their own drags first).
        if bg_resp.dragged() && st.drag_from.is_none() {
            st.graph_pan += bg_resp.drag_delta();
        }
        if bg_resp.clicked() {
            st.sel_state = None;
            st.sel_trans = None;
        }

        let node_size = egui::vec2(150.0, 52.0);
        let origin = rect.min + st.graph_pan + egui::vec2(16.0, 16.0);
        let node_rect = |pos: [f32; 2]| Rect::from_min_size(origin + EVec2::from(pos), node_size);

        let layer = &mut doc.layers[li];
        // ---- transitions (under nodes) ----
        for tr in layer.transitions.iter() {
            let (Some(a), Some(b)) = (
                layer.states.iter().find(|s| s.name == tr.from),
                layer.states.iter().find(|s| s.name == tr.to),
            ) else {
                continue;
            };
            let (ra, rb) = (node_rect(a.pos), node_rect(b.pos));
            let (mut pa, mut pb) = (ra.center(), rb.center());
            // Offset paired A↔B arrows so both stay visible.
            let dir = (pb - pa).normalized();
            let perp = egui::vec2(-dir.y, dir.x) * 7.0;
            if layer.transitions.iter().any(|o| o.from == tr.to && o.to == tr.from) {
                pa += perp;
                pb += perp;
            }
            let selected = st.sel_trans.as_ref() == Some(&(tr.from.clone(), tr.to.clone()));
            let fade_in =
                layer.states.iter().find(|s| s.name == tr.to).and_then(|s| s.fade_in);
            let eff_fade = fade_in.unwrap_or(tr.fade);
            let col = if selected {
                ACCENT
            } else if eff_fade <= 0.0 || fade_in.is_some() {
                Color32::from_rgb(235, 170, 90)
            } else {
                ui.visuals().weak_text_color()
            };
            painter.line_segment([pa, pb], Stroke::new(if selected { 2.5 } else { 1.5 }, col));
            // Arrowhead at 62% along (so double arrows read directionally).
            let tip = pa + (pb - pa) * 0.62;
            let d = dir * 9.0;
            let p = egui::vec2(-dir.y, dir.x) * 5.0;
            painter.line_segment([tip, tip - d + p], Stroke::new(2.0, col));
            painter.line_segment([tip, tip - d - p], Stroke::new(2.0, col));
            let label = match fade_in {
                Some(f) if f <= 0.0 => "⚡ instant".to_string(),
                Some(f) => format!("⇥ {f:.2}s"), // state override wins
                None => format!("{:.2}s", tr.fade),
            };
            painter.text(
                pa + (pb - pa) * 0.5 + egui::vec2(0.0, -10.0),
                Align2::CENTER_CENTER,
                label,
                FontId::proportional(10.5),
                col,
            );
            // Click-select an arrow (distance to segment).
            if bg_resp.clicked()
                && let Some(cur) = ui.ctx().pointer_interact_pos()
                    && seg_dist2(cur, pa, pb) < 7.0 {
                        st.sel_trans = Some((tr.from.clone(), tr.to.clone()));
                        st.sel_state = None;
                    }
            // (Deletion goes through the side panel's 🗑 button — a raw Delete
            // keypress here would collide with the scene's delete shortcut.)
        }

        // ---- nodes ----
        let mut hovered_node: Option<String> = None;
        for si in 0..layer.states.len() {
            let (name, clip, fade_in, fps, looped) = {
                let s = &layer.states[si];
                (s.name.clone(), s.clip.clone(), s.fade_in, s.fps, s.looped)
            };
            let r = node_rect(layer.states[si].pos);
            if !rect.intersects(r) {
                // keep interactions sane; still draw (egui clips to painter rect)
            }
            let id = ui.id().with(("anim-state", li, si));
            let resp = ui.interact(r, id, Sense::click_and_drag());
            if resp.hovered() {
                hovered_node = Some(name.clone());
            }
            if resp.dragged() && st.drag_from.is_none() {
                layer.states[si].pos[0] += resp.drag_delta().x;
                layer.states[si].pos[1] += resp.drag_delta().y;
                st.graph_dirty = true;
            }
            if resp.clicked() {
                st.sel_state = Some(name.clone());
                st.sel_trans = None;
            }
            let is_default = layer.default_state.as_ref() == Some(&name);
            let selected = st.sel_state.as_ref() == Some(&name);
            let missing = anim.clip(&clip).is_none();
            let fill = if selected {
                ui.visuals().selection.bg_fill.gamma_multiply(0.6)
            } else {
                ui.visuals().widgets.inactive.bg_fill
            };
            painter.rect_filled(r, 6.0, fill);
            let stroke_col = if selected {
                ACCENT
            } else if is_default {
                Color32::from_rgb(120, 200, 140)
            } else {
                ui.visuals().widgets.inactive.fg_stroke.color
            };
            painter.rect_stroke(r, 6.0, Stroke::new(if selected { 2.0 } else { 1.2 }, stroke_col), StrokeKind::Inside);
            let title = if is_default { format!("▶ {name}") } else { name.clone() };
            painter.text(
                r.min + egui::vec2(8.0, 10.0),
                Align2::LEFT_CENTER,
                title,
                FontId::proportional(13.0),
                ui.visuals().strong_text_color(),
            );
            let mut sub = clip.rsplit('/').next().unwrap_or(&clip).to_string();
            if missing {
                sub = format!("⚠ {sub}");
            }
            let mut badges = String::new();
            match fade_in {
                Some(f) if f <= 0.0 => badges.push('⚡'),
                Some(f) => badges.push_str(&format!("⇥{f:.2}s")),
                None => {}
            }
            if let Some(f) = fps {
                badges.push_str(&format!(" {f:.0}fps"));
            }
            if !looped {
                badges.push_str(" ⏹once");
            }
            painter.text(
                r.min + egui::vec2(8.0, 28.0),
                Align2::LEFT_CENTER,
                sub,
                FontId::proportional(10.5),
                if missing { Color32::from_rgb(230, 150, 90) } else { ui.visuals().weak_text_color() },
            );
            if !badges.is_empty() {
                painter.text(
                    r.min + egui::vec2(8.0, 42.0),
                    Align2::LEFT_CENTER,
                    badges,
                    FontId::proportional(10.0),
                    Color32::from_rgb(235, 170, 90),
                );
            }
            // Transition port: a circle on the right edge; drag it to another
            // node to create a transition.
            let port = Pos2::new(r.right() - 8.0, r.center().y);
            let port_r = Rect::from_center_size(port, egui::vec2(16.0, 16.0));
            let port_resp = ui.interact(port_r, id.with("port"), Sense::click_and_drag());
            painter.circle_filled(port, 5.0, if port_resp.hovered() { ACCENT } else { stroke_col });
            if port_resp.drag_started() {
                st.drag_from = Some(name.clone());
            }
            resp.context_menu(|ui| {
                if ui.button("▶ Set as default state").clicked() {
                    layer.default_state = Some(name.clone());
                    st.graph_dirty = true;
                    ui.close();
                }
                if ui.button("✏ Edit keys/events (Animating)").clicked() {
                    // Bind the Animating tab to a scene node actually using this
                    // controller, so the jump lands on the right timeline.
                    if let Some(key) = &st.graph_key {
                        let stem = key.rsplit('/').next().unwrap_or(key);
                        let target = world.query::<AnimController>().find_map(|(e, c)| {
                            (c.asset == *key
                                || c.asset.rsplit('/').next() == Some(stem))
                            .then_some(e)
                        });
                        if let Some(t) = target {
                            st.target = Some(t);
                        }
                    }
                    st.sel_anim = Some(name.clone());
                    st.clip_doc = None;
                    cmd.focus_animating = true;
                    ui.close();
                }
                if ui.button("🗑 Delete state").clicked() {
                    let nm = name.clone();
                    layer.states.retain(|s| s.name != nm);
                    layer.transitions.retain(|t| t.from != nm && t.to != nm);
                    if layer.default_state.as_ref() == Some(&nm) {
                        layer.default_state = None;
                    }
                    st.sel_state = None;
                    st.graph_dirty = true;
                    ui.close();
                }
            });
        }

        // ---- live transition drag ----
        if let Some(from) = st.drag_from.clone() {
            if let Some(cur) = ui.ctx().pointer_hover_pos()
                && let Some(s) = layer.states.iter().find(|s| s.name == from) {
                    let r = node_rect(s.pos);
                    painter.line_segment(
                        [Pos2::new(r.right() - 8.0, r.center().y), cur],
                        Stroke::new(2.0, ACCENT),
                    );
                }
            if ui.input(|i| i.pointer.any_released()) {
                if let Some(to) = hovered_node.filter(|n| *n != from)
                    && !layer.transitions.iter().any(|t| t.from == from && t.to == to) {
                        layer.transitions.push(AnimTransitionDoc {
                            from: from.clone(),
                            to: to.clone(),
                            fade: default_fade,
                        });
                        st.sel_trans = Some((from, to));
                        st.sel_state = None;
                        st.graph_dirty = true;
                    }
                st.drag_from = None;
            }
        }

        // ---- drop a clip asset onto the canvas → a new state ----
        let payload = egui::DragAndDrop::payload::<AssetPayload>(ui.ctx());
        if let Some(p) = payload
            && is_anim_clip(&p.path)
                && let Some(cur) = ui.ctx().pointer_hover_pos()
                    && rect.contains(cur) {
                        painter.rect_stroke(rect.shrink(2.0), 4.0, Stroke::new(2.0, ACCENT), StrokeKind::Inside);
                        painter.text(
                            rect.center(),
                            Align2::CENTER_CENTER,
                            "drop to add state",
                            FontId::proportional(14.0),
                            ACCENT,
                        );
                        if ui.input(|i| i.pointer.any_released()) {
                            let key =
                                anim::asset_key(Path::new(&p.path), project_root, ANIM_CLIP_EXT);
                            let base = key.rsplit('/').next().unwrap_or("Anim").to_string();
                            let mut name = base.clone();
                            let mut n = 2;
                            while layer.states.iter().any(|s| s.name == name) {
                                name = format!("{base}{n}");
                                n += 1;
                            }
                            let drop_pos = cur - origin - node_size * 0.5;
                            layer.states.push(AnimStateDoc {
                                name: name.clone(),
                                clip: key,
                                speed: 1.0,
                                looped: true,
                                fade_in: None,
                                fps: None,
                                pos: [drop_pos.x, drop_pos.y],
                            });
                            if layer.default_state.is_none() {
                                layer.default_state = Some(name.clone());
                            }
                            st.sel_state = Some(name);
                            st.graph_dirty = true;
                            egui::DragAndDrop::clear_payload(ui.ctx());
                        }
                    }

        if layer.states.is_empty() {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                "drag animation clips (▶ .anim.ron) from Assets here",
                FontId::proportional(13.0),
                ui.visuals().weak_text_color(),
            );
        }
    }

    /// Right-hand properties panel of the graph window (selected state /
    /// transition).
    fn graph_side_panel(&mut self, ui: &mut egui::Ui) {
        let st = &mut *self.anim_ui;
        let Some(doc) = st.graph_doc.as_mut() else { return };
        let li = st.graph_layer.min(doc.layers.len().saturating_sub(1));
        let layer = &mut doc.layers[li];
        if let Some(sel) = st.sel_state.clone() {
            let Some(si) = layer.states.iter().position(|s| s.name == sel) else {
                st.sel_state = None;
                return;
            };
            ui.strong("State");
            let mut rename: Option<(String, String)> = None;
            {
                let s = &mut layer.states[si];
                ui.horizontal(|ui| {
                    ui.label("name");
                    let mut name = s.name.clone();
                    let resp = ui.add(egui::TextEdit::singleline(&mut name).desired_width(120.0));
                    if resp.changed() && !name.trim().is_empty() {
                        rename = Some((s.name.clone(), name.trim().to_string()));
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("clip");
                    let cur = s.clip.rsplit('/').next().unwrap_or(&s.clip).to_string();
                    egui::ComboBox::from_id_salt("state-clip")
                        .selected_text(cur)
                        .width(130.0)
                        .show_ui(ui, |ui| {
                            let keys: Vec<String> =
                                self.anim.clips.iter().map(|(k, _)| k.clone()).collect();
                            for k in keys {
                                if ui.selectable_label(s.clip == k, &k).clicked() {
                                    s.clip = k.clone();
                                    st.graph_dirty = true;
                                }
                            }
                        });
                });
                if ui
                    .add(egui::DragValue::new(&mut s.speed).speed(0.02).range(0.05..=8.0).prefix("speed "))
                    .changed()
                {
                    st.graph_dirty = true;
                }
                if ui.checkbox(&mut s.looped, "looped").changed() {
                    st.graph_dirty = true;
                }
                let mut override_in = s.fade_in.is_some();
                if ui
                    .checkbox(&mut override_in, "⇥ override incoming fades")
                    .on_hover_text("EVERY transition into this state uses this fade time instead of the arrows/default. Set it to 0 for a guaranteed instant snap — even with stepped playback it lands exactly on frame 0.")
                    .changed()
                {
                    s.fade_in = if override_in { Some(0.0) } else { None };
                    st.graph_dirty = true;
                }
                if let Some(f) = s.fade_in.as_mut() {
                    if ui
                        .add(egui::DragValue::new(f).speed(0.01).range(0.0..=5.0).prefix("fade in ").suffix("s"))
                        .changed()
                    {
                        st.graph_dirty = true;
                    }
                    if *f <= 0.0 {
                        ui.small("⚡ 0 = always instant");
                    }
                }
                let mut own_fps = s.fps.is_some();
                if ui
                    .checkbox(&mut own_fps, "own stepped fps")
                    .on_hover_text("override the controller-wide stepped playback for just this state")
                    .changed()
                {
                    s.fps = if own_fps { Some(8.0) } else { None };
                    st.graph_dirty = true;
                }
                if let Some(f) = s.fps.as_mut()
                    && ui.add(egui::DragValue::new(f).speed(0.2).range(1.0..=60.0).suffix(" fps")).changed() {
                        st.graph_dirty = true;
                    }
            }
            if let Some((old, new)) = rename
                && !layer.states.iter().any(|s| s.name == new) {
                    if let Some(s) = layer.states.iter_mut().find(|s| s.name == old) {
                        s.name = new.clone();
                    }
                    for t in layer.transitions.iter_mut() {
                        if t.from == old {
                            t.from = new.clone();
                        }
                        if t.to == old {
                            t.to = new.clone();
                        }
                    }
                    if layer.default_state.as_ref() == Some(&old) {
                        layer.default_state = Some(new.clone());
                    }
                    st.sel_state = Some(new);
                    st.graph_dirty = true;
                }
            ui.separator();
            if ui.button("▶ Set as default").clicked() {
                layer.default_state = Some(sel.clone());
                st.graph_dirty = true;
            }
            ui.small("drag the ○ port onto another state to add a transition");
        } else if let Some((from, to)) = st.sel_trans.clone() {
            ui.strong("Transition");
            ui.label(format!("{from} → {to}"));
            let target_fade_in =
                layer.states.iter().find(|s| s.name == to).and_then(|s| s.fade_in);
            if let Some(f) = target_fade_in {
                ui.colored_label(
                    Color32::from_rgb(235, 170, 90),
                    if f <= 0.0 {
                        "⚡ the target overrides ALL incoming fades to 0 (instant) — this arrow's fade is ignored".to_string()
                    } else {
                        format!("⇥ the target overrides ALL incoming fades to {f:.2}s — this arrow's fade is ignored")
                    },
                );
            }
            if let Some(tr) = layer.transitions.iter_mut().find(|t| t.from == from && t.to == to) {
                if ui
                    .add(egui::DragValue::new(&mut tr.fade).speed(0.01).range(0.0..=5.0).prefix("fade ").suffix("s"))
                    .changed()
                {
                    st.graph_dirty = true;
                }
                ui.small("0 = snap. States without an explicit transition use the controller's default fade.");
            }
            if ui.button("🗑 Delete transition").clicked() {
                layer.transitions.retain(|t| !(t.from == from && t.to == to));
                st.sel_trans = None;
                st.graph_dirty = true;
            }
        } else {
            ui.weak("Click a state or a transition arrow to edit it.");
            ui.add_space(6.0);
            ui.small("• drag a ▶ clip from Assets onto the canvas to add a state");
            ui.small("• drag the ○ port between states to add a transition");
            ui.small("• right-click a state for default / delete");
            ui.small("• fades: default on the controller, per-arrow overrides, ⇥ per-state override (0 = ⚡ instant)");
        }
    }

    // =========================================================================
    // The Animating tab (dopesheet timeline).
    // =========================================================================
    pub fn animating_ui(&mut self, ui: &mut egui::Ui) {
        self.anim_ui.tab_visible = true;
        // ---- resolve the bound node ----
        let valid = |e: Entity, viewer: &Self| {
            viewer.world.get::<AnimController>(e).is_some()
                || matches!(viewer.world.get::<Matter>(e), Some(Matter::Mesh { asset_path })
                    if viewer.mesh_registry.get(asset_path).is_some_and(|m| m.rig.is_some()))
        };
        if let Some(t) = self.anim_ui.target
            && !valid(t, self) {
                self.anim_ui.target = None;
            }
        if self.anim_ui.target.is_none()
            && let Some(&sel) = self.selection.last()
                && valid(sel, self) {
                    self.anim_ui.target = Some(sel);
                }
        let candidates: Vec<(Entity, String)> = self
            .entity_names
            .iter()
            .filter(|(e, _)| valid(*e, self))
            .map(|(e, n)| (*e, n.clone()))
            .collect();
        let Some(target) = self.anim_ui.target else {
            ui.add_space(12.0);
            ui.vertical_centered(|ui| {
                ui.weak("Select a node with an Animation Controller (or a rigged model) to animate.");
                if candidates.is_empty() {
                    ui.small("add one via Inspector ➕ Add Component ⏵ Animation Controller");
                } else {
                    ui.horizontal(|ui| {
                        ui.label("or pick:");
                        for (e, n) in &candidates {
                            if ui.button(n).clicked() {
                                self.anim_ui.target = Some(*e);
                            }
                        }
                    });
                }
            });
            return;
        };

        // The controller doc + available states.
        let ctl_key = self.world.get::<AnimController>(target).map(|c| c.asset.clone());
        let states: Vec<(String, String)> = match &ctl_key {
            Some(k) => self
                .anim
                .controller(k)
                .map(|d| {
                    d.layers
                        .iter()
                        .flat_map(|l| l.states.iter().map(|s| (s.name.clone(), s.clip.clone())))
                        .collect()
                })
                .unwrap_or_default(),
            // Rig without a controller: embedded clip names. Using the NAME as the
            // clip key lets the registry's stem-fallback find an extracted
            // `.anim.ron` of the same name → full editable timeline; names with no
            // extracted file fall through to the read-only banner below.
            None => match self.world.get::<Matter>(target) {
                Some(Matter::Mesh { asset_path }) => self
                    .mesh_registry
                    .get(asset_path)
                    .and_then(|m| m.rig.as_ref())
                    .map(|r| r.clips.iter().map(|c| (c.name.clone(), c.name.clone())).collect())
                    .unwrap_or_default(),
                _ => Vec::new(),
            },
        };
        if self.anim_ui.sel_anim.as_ref().is_none_or(|s| !states.iter().any(|(n, _)| n == s)) {
            self.anim_ui.sel_anim = states.first().map(|(n, _)| n.clone());
            self.anim_ui.clip_doc = None;
            self.anim_ui.playhead = 0.0;
        }

        // ---- top bar ----
        let tname = self
            .entity_names
            .iter()
            .find(|(e, _)| *e == target)
            .map(|(_, n)| n.clone())
            .unwrap_or_default();
        ui.horizontal(|ui| {
            ui.label("node");
            egui::ComboBox::from_id_salt("animating-target")
                .selected_text(tname)
                .show_ui(ui, |ui| {
                    for (e, n) in &candidates {
                        if ui.selectable_label(*e == target, n).clicked() && *e != target {
                            self.anim.restore_preview(self.world);
                            self.anim_ui.target = Some(*e);
                            self.anim_ui.sel_anim = None;
                            self.anim_ui.clip_doc = None;
                            self.anim_ui.sel_prop = None;
                            self.anim_ui.last_scene_local.clear();
                        }
                    }
                });
            ui.label("animation");
            let cur = self.anim_ui.sel_anim.clone().unwrap_or_else(|| "—".into());
            egui::ComboBox::from_id_salt("animating-state")
                .selected_text(cur.clone())
                .show_ui(ui, |ui| {
                    for (n, _) in &states {
                        if ui.selectable_label(Some(n) == self.anim_ui.sel_anim.as_ref(), n).clicked()
                        {
                            self.anim_ui.sel_anim = Some(n.clone());
                            self.anim_ui.clip_doc = None;
                            self.anim_ui.playhead = 0.0;
                            self.anim_ui.sel_event = None;
                            self.anim_ui.sel_prop = None;
                        }
                    }
                });
            if ctl_key.is_some() && ui.button("✚ New…").on_hover_text("create a new empty animation clip and add it to this controller").clicked() {
                self.anim_ui.new_anim_buf = Some(String::new());
                self.anim_ui.focus_prompt = true;
            }
            ui.separator();
            if self.playing {
                ui.colored_label(
                    Color32::from_rgb(230, 180, 90),
                    "⏵ Play mode — preview & record paused",
                );
            }
            ui.add_enabled_ui(!self.playing, |ui| {
                // Transport.
                if ui.button("⏮").on_hover_text("to start").clicked() {
                    self.anim_ui.playhead = 0.0;
                }
                let play_lbl = if self.anim_ui.preview_playing { "⏸" } else { "⏵" };
                if ui.button(play_lbl).on_hover_text("preview play/pause").clicked() {
                    self.anim_ui.preview_playing = !self.anim_ui.preview_playing;
                    if self.anim_ui.preview_playing && self.anim_ui.record {
                        stop_record_ui(self.world, self.anim_ui);
                    }
                }
                if ui.button("⏹").on_hover_text("stop preview (restore the scene pose)").clicked() {
                    self.anim_ui.preview_playing = false;
                    self.anim_ui.playhead = 0.0;
                    if self.anim_ui.record {
                        stop_record_ui(self.world, self.anim_ui);
                    }
                    self.anim.restore_preview(self.world);
                    self.anim.poses.remove(&target);
                }
                let rec = ui
                    .selectable_label(self.anim_ui.record, "● Record")
                    .on_hover_text(
                        "key on move: pose this node's children with the gizmo/Inspector and keys \
                         are written at the playhead. Scrubbing previews what you've keyed; turning \
                         record off restores the scene pose (recording edits the CLIP, not the scene).",
                    );
                if rec.clicked() {
                    if self.anim_ui.record {
                        stop_record_ui(self.world, self.anim_ui);
                    } else {
                        self.anim_ui.record = true;
                        self.anim_ui.preview_playing = false;
                        // Snapshot the subtree so turning record off restores it.
                        self.anim_ui.record_restore = scene_channel_names(self.world, target)
                            .iter()
                            .filter_map(|(e, _)| {
                                self.world
                                    .get::<floptle_core::Transform>(*e)
                                    .map(|t| (*e, *t))
                            })
                            .collect();
                        // Snapshot pre-record property values too (for restore on stop).
                        self.anim_ui.record_restore_props = scene_channel_names(self.world, target)
                            .into_iter()
                            .flat_map(|(e, _)| {
                                numeric_props_of(self.world, e)
                                    .into_iter()
                                    .map(move |(c, f, v)| (e, c.to_string(), f.to_string(), v))
                            })
                            .collect();
                        refresh_record_baseline(self.world, self.anim_ui, target);
                    }
                }
            });
            ui.separator();
            ui.label(format!("{:.2}s", self.anim_ui.playhead));
            ui.label("snap");
            egui::ComboBox::from_id_salt("anim-snap")
                .selected_text(if self.anim_ui.snap_fps <= 0.0 {
                    "off".to_string()
                } else {
                    format!("{:.0} fps", self.anim_ui.snap_fps)
                })
                .width(70.0)
                .show_ui(ui, |ui| {
                    for f in [0.0, 8.0, 12.0, 24.0, 30.0, 60.0] {
                        let lbl = if f <= 0.0 { "off".to_string() } else { format!("{f:.0} fps") };
                        if ui.selectable_label(self.anim_ui.snap_fps == f, lbl).clicked() {
                            self.anim_ui.snap_fps = f;
                        }
                    }
                });
            ui.add(
                egui::Slider::new(&mut self.anim_ui.zoom, ANIM_ZOOM_MIN..=ANIM_ZOOM_MAX)
                    .logarithmic(true)
                    .show_value(false)
                    .text("zoom"),
            )
            .on_hover_text(
                "over the dopesheet: scroll = zoom · Alt+scroll = row height · Shift+scroll = pan · \
                 Space play · ←/→ step · Home/End · F fit · Del remove event",
            );
            if ui.button("Fit").on_hover_text("zoom to fit the whole clip (F)").clicked() {
                self.anim_ui.fit_pending = true;
            }
        });

        // New-animation prompt.
        if let Some(buf) = self.anim_ui.new_anim_buf.as_mut() {
            let mut done = false;
            let mut cancel = ui.input(|i| i.key_pressed(egui::Key::Escape));
            ui.horizontal(|ui| {
                ui.label("new animation name");
                let resp = ui.text_edit_singleline(buf);
                if self.anim_ui.focus_prompt {
                    resp.request_focus();
                    self.anim_ui.focus_prompt = false;
                }
                if (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                    || ui.button("Create").clicked()
                {
                    done = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
            if done && !buf.trim().is_empty() {
                let name = buf.trim().to_string();
                let clip_key = anim::new_clip_key(self.project_root, &name);
                let doc = AnimClipDoc {
                    name: name.clone(),
                    duration: 2.0,
                    source_model: String::new(),
                    channels: Vec::new(),
                    events: Vec::new(),
                };
                self.anim.save_clip(self.project_root, &clip_key, &doc);
                if let Some(k) = &ctl_key {
                    // The graph window may hold a working copy of this same
                    // controller — flush its edits first, then force it to
                    // reload after we add the state (no silent lost updates).
                    if self.anim_ui.graph_key.as_ref() == Some(k) {
                        self.flush_graph(true);
                    }
                    if let Some(mut cdoc) = self.anim.controller(k).cloned()
                        && let Some(layer) = cdoc.layers.first_mut() {
                            let mut sname = name.clone();
                            let mut n = 2;
                            while layer.states.iter().any(|s| s.name == sname) {
                                sname = format!("{name}{n}");
                                n += 1;
                            }
                            layer.states.push(AnimStateDoc {
                                name: sname.clone(),
                                clip: clip_key.clone(),
                                speed: 1.0,
                                looped: true,
                                fade_in: None,
                                fps: None,
                                pos: [40.0 + 30.0 * layer.states.len() as f32, 40.0],
                            });
                            if layer.default_state.is_none() {
                                layer.default_state = Some(sname.clone());
                            }
                            self.anim.save_controller(self.project_root, k, &cdoc);
                            if self.anim_ui.graph_key.as_ref() == Some(k) {
                                self.anim_ui.graph_doc = None; // reload w/ new state
                                self.anim_ui.graph_dirty = false;
                            }
                            self.anim_ui.sel_anim = Some(sname);
                            self.anim_ui.clip_doc = None;
                        }
                }
                self.anim_ui.new_anim_buf = None;
            } else if cancel || done {
                self.anim_ui.new_anim_buf = None;
            }
        }

        let Some(sel_anim) = self.anim_ui.sel_anim.clone() else {
            ui.add_space(10.0);
            ui.weak("No animations yet — extract some from a model, or ✚ New to author one.");
            return;
        };

        // Resolve the editable clip doc for the selected state.
        let clip_key: Option<String> = states
            .iter()
            .find(|(n, _)| *n == sel_anim)
            .map(|(_, c)| c.clone())
            .filter(|c| !c.is_empty());
        // Resolve to the REGISTRY key (handles stem-fallback for moved files) so
        // edits save onto the right file, and reload the working copy on change.
        let resolved = clip_key.as_ref().and_then(|k| self.anim.resolve_clip_key(k));
        if self.anim_ui.clip_doc.as_ref().map(|(k, _)| k.as_str()) != resolved.as_deref() {
            self.anim_ui.clip_doc = resolved
                .as_ref()
                .and_then(|k| Some((k.clone(), self.anim.clip(k)?.clone())));
            self.anim_ui.sel_event = None;
            self.anim_ui.sel_prop = None;
        }

        ui.separator();
        if self.anim_ui.clip_doc.is_some() {
            self.timeline_ui(ui, target);
            self.property_tracks_ui(ui, target);
        } else {
            ui.weak(
                "This animation is embedded in the model. ⬇ Extract animations (select the model \
                 asset in the browser) to edit keys and events.",
            );
            // Still allow preview scrubbing of embedded clips via a bare ruler.
            self.bare_ruler_ui(ui, target, &sel_anim);
        }

        // (Record diffing runs in the render loop BEFORE the preview re-applies
        // the clip — see anim_ui::record_scan.)

        // Save coalescing for clip edits.
        if self.anim_ui.clip_dirty && !self.pointer_down {
            if let Some((k, d)) = self.anim_ui.clip_doc.clone() {
                self.anim.save_clip(self.project_root, &k, &d);
            }
            self.anim_ui.clip_dirty = false;
        }
    }

    /// Scrub-only ruler for embedded (un-extracted) clips.
    fn bare_ruler_ui(&mut self, ui: &mut egui::Ui, _target: Entity, sel: &str) {
        let dur = match self.world.get::<Matter>(_target) {
            Some(Matter::Mesh { asset_path }) => self
                .mesh_registry
                .get(asset_path)
                .and_then(|m| m.rig.as_ref())
                .and_then(|r| r.clips.iter().find(|c| c.name == sel))
                .map(|c| c.duration)
                .unwrap_or(1.0),
            _ => 1.0,
        };
        let h = 26.0;
        let w = ui.available_width();
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 3.0, ui.visuals().extreme_bg_color);
        draw_ruler(&painter, rect, dur, self.anim_ui.playhead, rect.width() / dur.max(0.01));
        if (resp.dragged() || resp.clicked())
            && let Some(p) = resp.interact_pointer_pos() {
                let t = ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0) * dur;
                self.anim_ui.playhead = self.snap_time(t);
                self.anim_ui.preview_playing = false;
            }
        if self.anim_ui.preview_playing && self.anim_ui.playhead > dur {
            self.anim_ui.playhead %= dur.max(1e-3);
        }
    }

    fn snap_time(&self, t: f32) -> f32 {
        crate::timeline::snap_time(t, self.anim_ui.snap_fps)
    }

}

/// Dopesheet zoom bounds (pixels/sec) + row-height multiplier bounds — same feel as
/// the particle timeline.
const ANIM_ZOOM_MIN: f32 = 6.0;
const ANIM_ZOOM_MAX: f32 = 3000.0;
const ANIM_ROW_MIN: f32 = 0.5;
const ANIM_ROW_MAX: f32 = 4.0;
const ANIM_LABEL_W: f32 = 130.0;
/// Property-lane key + label colours — a teal, distinct from the amber transform
/// keys, so a node's property lanes read apart from its transform lane.
const PROP_KEY_COLOR: Color32 = Color32::from_rgb(120, 210, 175);
const PROP_LABEL_COLOR: Color32 = Color32::from_rgb(150, 200, 185);

/// Draw a dopesheet key diamond centred at `c`.
fn key_diamond(painter: &egui::Painter, c: Pos2, col: Color32) {
    let s = 4.5;
    painter.add(egui::Shape::convex_polygon(
        vec![
            Pos2::new(c.x, c.y - s),
            Pos2::new(c.x + s, c.y),
            Pos2::new(c.x, c.y + s),
            Pos2::new(c.x - s, c.y),
        ],
        col,
        Stroke::new(1.0, Color32::from_black_alpha(120)),
    ));
}

/// Scroll-wheel navigation for the dopesheet, mirroring the particle timeline: plain
/// wheel zooms X about the cursor (keeping the time under the pointer fixed), Alt+wheel
/// zooms Y (row height), Shift+wheel falls through to the ScrollArea to pan, and a
/// pending Fit sizes the whole clip to the view. Runs BEFORE the `clip_doc` borrow
/// (it mutates disjoint `st` fields), so `dur` is passed in.
fn handle_anim_wheel(ui: &egui::Ui, st: &mut AnimUiState, dur: f32) {
    let region = ui.available_rect_before_wrap();
    let body_w = (region.width() - ANIM_LABEL_W - 16.0).max(50.0);
    if st.fit_pending {
        st.zoom = (body_w / dur).clamp(ANIM_ZOOM_MIN, ANIM_ZOOM_MAX);
        st.scroll_target = Some(egui::Vec2::ZERO);
        st.fit_pending = false;
    }
    let Some(p) = ui.ctx().pointer_hover_pos() else { return };
    if !region.contains(p) {
        return;
    }
    let (scroll, mods) = ui.input(|i| (i.smooth_scroll_delta, i.modifiers));
    let wheel = if scroll.y.abs() >= scroll.x.abs() { scroll.y } else { scroll.x };
    if wheel.abs() < 0.5 || mods.shift {
        return; // Shift+wheel: let the ScrollArea pan (don't consume).
    }
    let z = (wheel * 0.0015).exp();
    if mods.alt {
        st.row_scale = (st.row_scale * z).clamp(ANIM_ROW_MIN, ANIM_ROW_MAX);
    } else {
        let px = st.zoom;
        let off = st.scroll_off;
        let vrel = p.x - region.left();
        let time = ((vrel + off.x - ANIM_LABEL_W) / px).max(0.0);
        let new_px = (px * z).clamp(ANIM_ZOOM_MIN, ANIM_ZOOM_MAX);
        let new_off_x = (ANIM_LABEL_W + time * new_px - vrel).max(0.0);
        st.zoom = new_px;
        st.scroll_target = Some(egui::vec2(new_off_x, off.y));
    }
    ui.input_mut(|i| i.smooth_scroll_delta = egui::Vec2::ZERO);
}

/// The value editor a property field needs.
#[derive(Clone, Copy, PartialEq)]
enum PropKind {
    Float,
    /// A path/text field — image swap (the headline case), material texture, text.
    Text,
}

/// The component fields the Animating tab can key with a property track, grouped
/// by component. Mirrors the ECS setters in `floptle_script::apply_component_field`
/// (+ `_str`); a field here must have a matching arm there or the key is inert.
const ANIMATABLE_PROPS: &[(&str, &[(&str, PropKind)])] = &[
    (
        "UiElement",
        &[
            ("image", PropKind::Text), // sprite-swap: the texture path, frame by frame
            ("opacity", PropKind::Float),
            ("visible", PropKind::Float),
            ("posX", PropKind::Float),
            ("posY", PropKind::Float),
            ("width", PropKind::Float),
            ("height", PropKind::Float),
            ("radius", PropKind::Float),
            ("border", PropKind::Float),
            ("fillR", PropKind::Float),
            ("fillG", PropKind::Float),
            ("fillB", PropKind::Float),
            ("fillA", PropKind::Float),
            ("textSize", PropKind::Float),
            ("textR", PropKind::Float),
            ("textG", PropKind::Float),
            ("textB", PropKind::Float),
            ("textA", PropKind::Float),
            ("tintR", PropKind::Float),
            ("tintG", PropKind::Float),
            ("tintB", PropKind::Float),
            ("tintA", PropKind::Float),
            ("cell", PropKind::Float), // spritesheet frame index — key with Step
            ("text", PropKind::Text),
        ],
    ),
    (
        "UiSlider",
        &[("value", PropKind::Float), ("min", PropKind::Float), ("max", PropKind::Float)],
    ),
    (
        "PointLight",
        &[
            ("intensity", PropKind::Float),
            ("range", PropKind::Float),
            ("r", PropKind::Float),
            ("g", PropKind::Float),
            ("b", PropKind::Float),
        ],
    ),
    ("Material", &[("texture", PropKind::Text)]),
    ("Camera", &[("fovY", PropKind::Float)]),
];

fn prop_kind(component: &str, field: &str) -> PropKind {
    ANIMATABLE_PROPS
        .iter()
        .find(|(c, _)| *c == component)
        .and_then(|(_, fs)| fs.iter().find(|(f, _)| *f == field))
        .map(|(_, k)| *k)
        .unwrap_or(PropKind::Float)
}

fn fields_for(component: &str) -> &'static [(&'static str, PropKind)] {
    ANIMATABLE_PROPS.iter().find(|(c, _)| *c == component).map(|(_, fs)| *fs).unwrap_or(&[])
}

impl EditorTabViewer<'_> {
    /// Node names in `target`'s subtree (itself + descendants), for the property
    /// track's node picker. The animated node ("") maps to the subtree root.
    fn subtree_names(&self, target: Entity) -> Vec<String> {
        let mut kids: HashMap<Entity, Vec<Entity>> = HashMap::new();
        for (e, p) in self.world.query::<floptle_core::Parent>() {
            kids.entry(p.0).or_default().push(e);
        }
        let mut out = Vec::new();
        let mut stack = vec![target];
        while let Some(e) = stack.pop() {
            if let Some(n) = self.world.get::<Name>(e)
                && !n.0.is_empty()
                && !out.contains(&n.0)
            {
                out.push(n.0.clone());
            }
            if let Some(cs) = kids.get(&e) {
                stack.extend(cs.iter().copied());
            }
        }
        out.sort();
        out
    }

    /// Property-track authoring, paired with the dopesheet above: the currently
    /// selected key's value is edited HERE (its keys live on the timeline as
    /// draggable diamonds under their node), plus an add-track builder and compact
    /// per-track controls. Numeric fields interpolate; image/text fields step.
    fn property_tracks_ui(&mut self, ui: &mut egui::Ui, target: Entity) {
        // Immutable data first, before borrowing the clip doc mutably.
        let tree = self.asset_tree;
        let subtree = self.subtree_names(target);

        let st = &mut *self.anim_ui;
        let playhead = st.playhead;
        let Some((_, doc)) = st.clip_doc.as_mut() else { return };
        let dur = doc.duration.max(0.01);

        // ---- selected key: edit its value + time inline (image picker / number) ----
        let mut del_selected: Option<(usize, usize, usize)> = None;
        if let Some((ci, ti, ki)) = st.sel_prop {
            let valid = doc
                .channels
                .get(ci)
                .and_then(|c| c.properties.get(ti))
                .is_some_and(|pt| ki < pt.times.len());
            if !valid {
                st.sel_prop = None; // the track/key it pointed at is gone
            } else {
                let node_label = {
                    let n = &doc.channels[ci].node;
                    if n.is_empty() { "(this node)".to_string() } else { n.clone() }
                };
                let (comp, field) = {
                    let pt = &doc.channels[ci].properties[ti];
                    (pt.component.clone(), pt.field.clone())
                };
                let kind = prop_kind(&comp, &field);
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.colored_label(PROP_KEY_COLOR, format!("◆ {node_label} · {comp}.{field}"));
                        ui.separator();
                        ui.label("time");
                        let mut t = doc.channels[ci].properties[ti].times[ki];
                        if ui
                            .add(egui::DragValue::new(&mut t).speed(0.01).range(0.0..=dur).suffix("s"))
                            .changed()
                        {
                            doc.channels[ci].properties[ti].times[ki] = t;
                            st.clip_dirty = true;
                        }
                        ui.label("value");
                        match &mut doc.channels[ci].properties[ti].values[ki] {
                            AnimPropValueDoc::Float(x) => {
                                // Whole-frame steps feel right for a spritesheet cell.
                                let speed = if comp == "UiElement" && field == "cell" { 0.25 } else { 0.01 };
                                if ui.add(egui::DragValue::new(x).speed(speed)).changed() {
                                    st.clip_dirty = true;
                                }
                            }
                            AnimPropValueDoc::Text(s) => {
                                if kind == PropKind::Text && field != "text" {
                                    let label =
                                        if s.is_empty() { "(pick image)".to_string() } else { s.clone() };
                                    if let Some(pick) = crate::ui_widgets::asset_picker(
                                        ui,
                                        egui::Id::new(("prop-sel-tex", ci, ti)),
                                        &label,
                                        Some("(clear)"),
                                        tree,
                                        crate::assets::is_texture,
                                        190.0,
                                    ) {
                                        *s = pick.unwrap_or_default();
                                        st.clip_dirty = true;
                                    }
                                } else if ui
                                    .add(egui::TextEdit::singleline(s).desired_width(160.0))
                                    .changed()
                                {
                                    st.clip_dirty = true;
                                }
                            }
                        }
                        if ui.button("🗑 key").on_hover_text("delete this key (or press Del)").clicked() {
                            del_selected = Some((ci, ti, ki));
                        }
                    });
                });
            }
        }
        if let Some((ci, ti, ki)) = del_selected {
            let t = doc.channels[ci].properties[ti].times[ki];
            delete_property_key(doc, ci, ti, t);
            st.sel_prop = None;
            st.clip_dirty = true;
        }

        // ---- add / manage tracks ----
        let (add_node, add_comp, add_field) =
            (st.prop_node.clone(), st.prop_comp.clone(), st.prop_field.clone());
        egui::CollapsingHeader::new("▦  Property tracks")
            .id_salt("anim-prop-tracks")
            .default_open(true)
            .show(ui, |ui| {
                ui.small(
                    "Animate a component field — opacity, colors, or a UI image swapping \
                     frame-by-frame. Keys appear on the timeline above under their node; \
                     click one to edit its value here. (● Record + change a property auto-keys it.)",
                );
                // ---- add-track builder row ----
                let mut do_add = false;
                ui.horizontal_wrapped(|ui| {
                    ui.label("node");
                    egui::ComboBox::from_id_salt("prop-node")
                        .selected_text(if add_node.is_empty() { "(animated node)" } else { add_node.as_str() })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut st.prop_node, String::new(), "(animated node)");
                            for n in &subtree {
                                ui.selectable_value(&mut st.prop_node, n.clone(), n);
                            }
                        });
                    ui.label("component");
                    egui::ComboBox::from_id_salt("prop-comp")
                        .selected_text(&add_comp)
                        .show_ui(ui, |ui| {
                            for (c, _) in ANIMATABLE_PROPS {
                                if ui.selectable_value(&mut st.prop_comp, c.to_string(), *c).clicked()
                                {
                                    // Reset the field to the new component's first.
                                    if let Some((f, _)) = fields_for(c).first() {
                                        st.prop_field = f.to_string();
                                    }
                                }
                            }
                        });
                    ui.label("field");
                    egui::ComboBox::from_id_salt("prop-field")
                        .selected_text(&add_field)
                        .show_ui(ui, |ui| {
                            for (f, _) in fields_for(&st.prop_comp) {
                                ui.selectable_value(&mut st.prop_field, f.to_string(), *f);
                            }
                        });
                    if ui
                        .button("＋ Add track")
                        .on_hover_text("add a lane for this field (then ＋ key, or Record)")
                        .clicked()
                    {
                        do_add = true;
                    }
                });

                if do_add {
                    add_property_track(doc, &st.prop_node, &st.prop_comp, &st.prop_field);
                    st.clip_dirty = true;
                }

                ui.separator();

                // ---- compact per-track rows (keys are edited on the timeline) ----
                let mut remove: Option<(usize, usize)> = None; // (channel, track)
                let mut any = false;
                for ci in 0..doc.channels.len() {
                    let node_label = {
                        let n = &doc.channels[ci].node;
                        if n.is_empty() { "(animated node)".to_string() } else { n.clone() }
                    };
                    for ti in 0..doc.channels[ci].properties.len() {
                        any = true;
                        let (comp, field) = {
                            let pt = &doc.channels[ci].properties[ti];
                            (pt.component.clone(), pt.field.clone())
                        };
                        let kind = prop_kind(&comp, &field);
                        ui.horizontal(|ui| {
                            ui.label(format!("{node_label} · {comp}.{field}"));
                            if ui
                                .button("＋ key")
                                .on_hover_text("key the current value at the playhead")
                                .clicked()
                            {
                                key_property_at(
                                    &mut doc.channels[ci].properties[ti],
                                    playhead.min(dur),
                                    kind,
                                );
                                st.clip_dirty = true;
                            }
                            // Step (hold each key) vs interpolate. Text tracks always
                            // step; numeric tracks (opacity, cell…) choose.
                            if kind == PropKind::Text {
                                ui.add_enabled(false, egui::Button::new("step"));
                            } else {
                                let mut step = doc.channels[ci].properties[ti].step;
                                if ui
                                    .selectable_label(step, "step")
                                    .on_hover_text("hold each key (no blend) — use for spritesheet frames")
                                    .clicked()
                                {
                                    step = !step;
                                    doc.channels[ci].properties[ti].step = step;
                                    st.clip_dirty = true;
                                }
                            }
                            let nkeys = doc.channels[ci].properties[ti].times.len();
                            ui.weak(format!("{nkeys} key{}", if nkeys == 1 { "" } else { "s" }));
                            if ui.button("🗑").on_hover_text("remove this whole track").clicked() {
                                remove = Some((ci, ti));
                            }
                        });
                    }
                }
                if !any {
                    ui.weak(
                        "No property tracks yet — add one above (try UiElement · image for a sprite \
                         swap), or ● Record and change a property.",
                    );
                }

                if let Some((ci, ti)) = remove {
                    doc.channels[ci].properties.remove(ti);
                    drop_empty_channel(doc, ci);
                    if st.sel_prop.is_some_and(|(sci, sti, _)| sci == ci && sti == ti) {
                        st.sel_prop = None;
                    }
                    st.clip_dirty = true;
                }
            });
    }

    /// The full dopesheet for an editable clip doc.
    fn timeline_ui(&mut self, ui: &mut egui::Ui, _target: Entity) {
        let playing = self.playing;
        let st = &mut *self.anim_ui;
        // Read `dur` and run the wheel handler BEFORE borrowing `clip_doc` mutably (the
        // handler needs &mut st, which would alias the `doc` borrow).
        let dur = match st.clip_doc.as_ref() {
            Some((_, d)) => d.duration.max(0.01),
            None => return,
        };
        handle_anim_wheel(ui, st, dur);
        let Some((_, doc)) = st.clip_doc.as_mut() else { return };
        let px = st.zoom;
        let label_w = ANIM_LABEL_W;
        let lane_h = 20.0 * st.row_scale;
        let ruler_h = 22.0;
        let event_h = 20.0;

        // Header row: duration + event add + selected-event editor.
        let mut kill_event: Option<usize> = None;
        ui.horizontal(|ui| {
            ui.label("duration");
            let mut d = doc.duration;
            if ui.add(egui::DragValue::new(&mut d).speed(0.02).range(0.05..=600.0).suffix("s")).changed() {
                doc.duration = d;
                st.clip_dirty = true;
            }
            if ui.button("⚑ Add event at playhead").on_hover_text("events call a Lua function (by name) on this node's scripts when the playhead crosses them").clicked() {
                doc.events.push(AnimEventDoc { t: st.playhead.min(doc.duration), func: "onAnimEvent".into() });
                doc.events.sort_by(|a, b| a.t.total_cmp(&b.t));
                st.sel_event = doc
                    .events
                    .iter()
                    .position(|e| (e.t - st.playhead.min(doc.duration)).abs() < 1e-5);
                st.clip_dirty = true;
            }
            if let Some(ei) = st.sel_event {
                if let Some(ev) = doc.events.get_mut(ei) {
                    ui.separator();
                    ui.label("event fn");
                    if ui.add(egui::TextEdit::singleline(&mut ev.func).desired_width(130.0)).changed() {
                        st.clip_dirty = true;
                    }
                    let mut t = ev.t;
                    if ui.add(egui::DragValue::new(&mut t).speed(0.01).range(0.0..=doc.duration).suffix("s")).changed() {
                        ev.t = t;
                        st.clip_dirty = true;
                    }
                    if ui.button("🗑").clicked() {
                        kill_event = Some(ei);
                    }
                } else {
                    st.sel_event = None;
                }
            }
        });
        if let Some(ei) = kill_event {
            doc.events.remove(ei);
            st.sel_event = None;
            st.clip_dirty = true;
        }

        // One lane per channel (its transform union) PLUS one per property track.
        let n_rows: usize = doc.channels.iter().map(|c| 1 + c.properties.len()).sum();
        let body_h = ruler_h + event_h + (n_rows.max(1) as f32) * lane_h + 8.0;
        let mut area = egui::ScrollArea::both().auto_shrink([false, true]).max_height(ui.available_height());
        if let Some(t) = st.scroll_target.take() {
            area = area.scroll_offset(t);
        }
        let out = area.show(ui, |ui| {
            let want_w = (label_w + dur * px + 140.0).max(ui.available_width());
            let (full, _) = ui.allocate_exact_size(egui::vec2(want_w, body_h), Sense::hover());
            let painter = ui.painter_at(full);
            let tl_left = full.left() + label_w;
            let view = crate::timeline::TimelineView { left: tl_left, px_per_s: px, duration: dur };
            let time_to_x = |t: f32| view.time_to_x(t);
            let x_to_time = |x: f32| view.x_to_time(x);

            // ---- ruler (scrub) ----
            let ruler = Rect::from_min_size(Pos2::new(tl_left, full.top()), egui::vec2(dur * px + 100.0, ruler_h));
            let rresp = ui.interact(ruler, ui.id().with("anim-ruler"), Sense::click_and_drag());
            if (rresp.dragged() || rresp.clicked())
                && let Some(p) = rresp.interact_pointer_pos() {
                    st.playhead = crate::timeline::snap_time(x_to_time(p.x), st.snap_fps);
                    st.preview_playing = false;
                }
            painter.rect_filled(ruler, 0.0, ui.visuals().extreme_bg_color);

            // ---- event lane ----
            let ev_rect = Rect::from_min_size(
                Pos2::new(full.left(), full.top() + ruler_h),
                egui::vec2(full.width(), event_h),
            );
            painter.rect_filled(
                Rect::from_min_size(Pos2::new(tl_left, ev_rect.top()), egui::vec2(dur * px, event_h)),
                0.0,
                ui.visuals().faint_bg_color,
            );
            painter.text(
                Pos2::new(full.left() + 4.0, ev_rect.center().y),
                Align2::LEFT_CENTER,
                "⚑ events",
                FontId::proportional(11.0),
                EVENT_COLOR,
            );
            let mut ev_drag: Option<(usize, f32)> = None;
            for (ei, ev) in doc.events.iter().enumerate() {
                let x = time_to_x(ev.t);
                let flag = Rect::from_center_size(Pos2::new(x, ev_rect.center().y), egui::vec2(12.0, event_h));
                let id = ui.id().with(("anim-event", ei));
                let resp = ui.interact(flag, id, Sense::click_and_drag());
                let col = if st.sel_event == Some(ei) { ACCENT } else { EVENT_COLOR };
                painter.line_segment(
                    [Pos2::new(x, ev_rect.top() + 2.0), Pos2::new(x, ev_rect.bottom() - 2.0)],
                    Stroke::new(2.0, col),
                );
                painter.text(
                    Pos2::new(x + 3.0, ev_rect.top() + 4.0),
                    Align2::LEFT_TOP,
                    &ev.func,
                    FontId::proportional(9.0),
                    col.gamma_multiply(0.9),
                );
                if resp.clicked() {
                    st.sel_event = Some(ei);
                    st.sel_prop = None;
                }
                if resp.dragged()
                    && let Some(p) = resp.interact_pointer_pos() {
                        ev_drag = Some((ei, x_to_time(p.x)));
                    }
                resp.context_menu(|ui| {
                    if ui.button("🗑 Delete event").clicked() {
                        ev_drag = Some((ei, f32::NAN)); // NaN = delete
                        ui.close();
                    }
                });
            }
            if let Some((ei, t)) = ev_drag {
                if t.is_nan() {
                    doc.events.remove(ei);
                    st.sel_event = None;
                } else if let Some(ev) = doc.events.get_mut(ei) {
                    ev.t = crate::timeline::snap_time(t, st.snap_fps);
                    st.sel_event = Some(ei);
                }
                st.clip_dirty = true;
            }

            // ---- channel + property rows ----
            // Each channel draws a node lane (the union of its transform keys) then
            // one lane per property track indented beneath it — every lane shares the
            // same time axis and the same draggable diamonds, so a spritesheet `cell`
            // reads as a keyframe under its node, not a separate numeric panel.
            let rows_top = full.top() + ruler_h + event_h;
            let mut retime: Option<(usize, f32, f32)> = None; // transform: (channel, old t, new t)
            let mut delete_key: Option<(usize, f32)> = None;
            let mut prop_retime: Option<(usize, usize, f32, f32)> = None; // (ci, ti, old, new)
            let mut prop_delete: Option<(usize, usize, f32)> = None; // (ci, ti, t)
            let mut prop_select: Option<(usize, usize, usize)> = None; // (ci, ti, ki)
            let mut row_i = 0usize;
            let stripe = |painter: &egui::Painter, row_i: usize, y: f32, ui: &egui::Ui| {
                if row_i.is_multiple_of(2) {
                    painter.rect_filled(
                        Rect::from_min_size(Pos2::new(tl_left, y), egui::vec2(dur * px, lane_h)),
                        0.0,
                        ui.visuals().faint_bg_color.gamma_multiply(0.6),
                    );
                }
            };
            for ci in 0..doc.channels.len() {
                // --- node lane: label + transform-union diamonds ---
                let y = rows_top + row_i as f32 * lane_h;
                stripe(&painter, row_i, y, ui);
                row_i += 1;
                let cy = y + lane_h * 0.5;
                let label = if doc.channels[ci].node.is_empty() {
                    "(this node)"
                } else {
                    doc.channels[ci].node.as_str()
                };
                painter.text(
                    Pos2::new(full.left() + 4.0, cy),
                    Align2::LEFT_CENTER,
                    label,
                    FontId::proportional(11.0),
                    ui.visuals().text_color(),
                );
                let times = union_times(&doc.channels[ci]);
                for (ki, &t) in times.iter().enumerate() {
                    // A drag previews at the pointer but the doc is only retimed on
                    // RELEASE — live-resorting mid-drag would hand it to a neighbour.
                    let dragging_this = st
                        .key_drag
                        .is_some_and(|(dci, ot, _)| dci == ci && (ot - t).abs() < 1e-6);
                    let draw_t = if dragging_this { st.key_drag.unwrap().2 } else { t };
                    let c = Pos2::new(time_to_x(draw_t), cy);
                    let id = ui.id().with(("anim-key", ci, ki));
                    let resp = ui
                        .interact(Rect::from_center_size(c, egui::vec2(12.0, 12.0)), id, Sense::click_and_drag());
                    let col = if resp.hovered() || dragging_this { ACCENT } else { KEY_COLOR };
                    key_diamond(&painter, c, col);
                    if resp.drag_started() {
                        st.key_drag = Some((ci, t, t));
                    }
                    if resp.dragged()
                        && let Some(p) = resp.interact_pointer_pos()
                    {
                        let nt = crate::timeline::snap_time(x_to_time(p.x), st.snap_fps);
                        if let Some(kd) = st.key_drag.as_mut()
                            && kd.0 == ci
                            && (kd.1 - t).abs() < 1e-6
                        {
                            kd.2 = nt;
                        }
                    }
                    if resp.drag_stopped()
                        && let Some((dci, ot, nt)) = st.key_drag.take()
                        && dci == ci
                        && (ot - t).abs() < 1e-6
                        && (nt - ot).abs() > 1e-6
                    {
                        retime = Some((ci, ot, nt));
                    }
                    resp.context_menu(|ui| {
                        if ui.button("🗑 Delete key").clicked() {
                            delete_key = Some((ci, t));
                            ui.close();
                        }
                    });
                }
                // --- property lanes, indented under the node ---
                for ti in 0..doc.channels[ci].properties.len() {
                    let y = rows_top + row_i as f32 * lane_h;
                    stripe(&painter, row_i, y, ui);
                    row_i += 1;
                    let cy = y + lane_h * 0.5;
                    let plabel = {
                        let pt = &doc.channels[ci].properties[ti];
                        format!("   {}.{}", pt.component, pt.field)
                    };
                    painter.text(
                        Pos2::new(full.left() + 10.0, cy),
                        Align2::LEFT_CENTER,
                        plabel,
                        FontId::proportional(10.5),
                        PROP_LABEL_COLOR,
                    );
                    let n_keys = doc.channels[ci].properties[ti].times.len();
                    for ki in 0..n_keys {
                        let t = doc.channels[ci].properties[ti].times[ki];
                        let dragging_this = st.prop_key_drag.is_some_and(|(dci, dti, ot, _)| {
                            dci == ci && dti == ti && (ot - t).abs() < 1e-6
                        });
                        let draw_t = if dragging_this { st.prop_key_drag.unwrap().3 } else { t };
                        let c = Pos2::new(time_to_x(draw_t), cy);
                        let id = ui.id().with(("anim-prop-key", ci, ti, ki));
                        let resp = ui
                            .interact(Rect::from_center_size(c, egui::vec2(11.0, 11.0)), id, Sense::click_and_drag());
                        let selected = st.sel_prop == Some((ci, ti, ki));
                        let col = if resp.hovered() || dragging_this || selected {
                            ACCENT
                        } else {
                            PROP_KEY_COLOR
                        };
                        key_diamond(&painter, c, col);
                        if resp.clicked() {
                            prop_select = Some((ci, ti, ki));
                        }
                        if resp.drag_started() {
                            st.prop_key_drag = Some((ci, ti, t, t));
                            prop_select = Some((ci, ti, ki));
                        }
                        if resp.dragged()
                            && let Some(p) = resp.interact_pointer_pos()
                        {
                            let nt = crate::timeline::snap_time(x_to_time(p.x), st.snap_fps);
                            if let Some(kd) = st.prop_key_drag.as_mut()
                                && kd.0 == ci
                                && kd.1 == ti
                                && (kd.2 - t).abs() < 1e-6
                            {
                                kd.3 = nt;
                            }
                        }
                        if resp.drag_stopped()
                            && let Some((dci, dti, ot, nt)) = st.prop_key_drag.take()
                            && dci == ci
                            && dti == ti
                            && (ot - t).abs() < 1e-6
                            && (nt - ot).abs() > 1e-6
                        {
                            prop_retime = Some((ci, ti, ot, nt));
                        }
                        resp.context_menu(|ui| {
                            if ui.button("🗑 Delete key").clicked() {
                                prop_delete = Some((ci, ti, t));
                                ui.close();
                            }
                        });
                    }
                }
            }
            if doc.channels.is_empty() {
                painter.text(
                    Pos2::new(tl_left + 12.0, rows_top + lane_h * 0.7),
                    Align2::LEFT_CENTER,
                    "no keys yet — ● Record, then pose child nodes or change a property",
                    FontId::proportional(11.5),
                    ui.visuals().weak_text_color(),
                );
            }
            if let Some((ci, old, new)) = retime {
                retime_channel(&mut doc.channels[ci], old, new);
                st.clip_dirty = true;
            }
            if let Some((ci, t)) = delete_key {
                delete_channel_key(&mut doc.channels[ci], t);
                drop_empty_channel(doc, ci);
                st.clip_dirty = true;
            }
            if let Some(sel) = prop_select {
                st.sel_prop = Some(sel);
                st.sel_event = None;
            }
            if let Some((ci, ti, old, new)) = prop_retime {
                retime_property_key(&mut doc.channels[ci].properties[ti], old, new);
                st.sel_prop = None; // key indices shift after a retime
                st.clip_dirty = true;
            }
            if let Some((ci, ti, t)) = prop_delete {
                delete_property_key(doc, ci, ti, t);
                st.sel_prop = None;
                st.clip_dirty = true;
            }

            // ---- keyboard transport (only when no text field is focused, not playing) ----
            if !playing && ui.memory(|m| m.focused().is_none()) {
                let (sp, home, end, left, right, del, fit) = ui.input(|i| {
                    (
                        i.key_pressed(egui::Key::Space),
                        i.key_pressed(egui::Key::Home),
                        i.key_pressed(egui::Key::End),
                        i.key_pressed(egui::Key::ArrowLeft),
                        i.key_pressed(egui::Key::ArrowRight),
                        i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace),
                        i.key_pressed(egui::Key::F),
                    )
                });
                let step = if st.snap_fps > 0.0 { 1.0 / st.snap_fps } else { 0.1 };
                if sp {
                    st.preview_playing = !st.preview_playing;
                }
                if home {
                    st.playhead = 0.0;
                    st.preview_playing = false;
                }
                if end {
                    st.playhead = dur;
                    st.preview_playing = false;
                }
                if left {
                    st.playhead = (st.playhead - step).max(0.0);
                    st.preview_playing = false;
                }
                if right {
                    st.playhead = (st.playhead + step).min(dur);
                    st.preview_playing = false;
                }
                if fit {
                    st.fit_pending = true;
                }
                // Delete removes the selected property key first, else the event.
                if del {
                    if let Some((ci, ti, ki)) = st.sel_prop.take() {
                        let removed = doc
                            .channels
                            .get_mut(ci)
                            .and_then(|c| c.properties.get_mut(ti))
                            .is_some_and(|pt| {
                                if ki < pt.times.len() {
                                    pt.times.remove(ki);
                                    pt.values.remove(ki);
                                    if pt.times.is_empty() {
                                        // fall through to track/channel cleanup below
                                    }
                                    true
                                } else {
                                    false
                                }
                            });
                        if removed {
                            if doc.channels[ci].properties[ti].times.is_empty() {
                                doc.channels[ci].properties.remove(ti);
                            }
                            drop_empty_channel(doc, ci);
                            st.clip_dirty = true;
                        }
                    } else if let Some(ei) = st.sel_event.take()
                        && ei < doc.events.len()
                    {
                        doc.events.remove(ei);
                        st.clip_dirty = true;
                    }
                }
            }

            // ---- playhead over everything ----
            let xp = time_to_x(st.playhead.min(dur));
            painter.line_segment(
                [Pos2::new(xp, full.top()), Pos2::new(xp, full.bottom())],
                Stroke::new(1.5, PLAYHEAD),
            );
            let xe = time_to_x(dur);
            painter.line_segment(
                [Pos2::new(xe, full.top()), Pos2::new(xe, full.bottom())],
                Stroke::new(1.0, Color32::from_rgb(150, 150, 170)),
            );
            // ruler ticks over the top strip
            draw_ruler(&painter, Rect::from_min_size(Pos2::new(tl_left, full.top()), egui::vec2(dur * px, ruler_h)), dur, st.playhead.min(dur), px);
        });
        // Remember the offset so next frame's cursor-anchored zoom has an anchor.
        st.scroll_off = out.state.offset;

        // Loop the preview playhead.
        if st.preview_playing && st.playhead > dur {
            st.playhead %= dur;
        }
    }

}

/// Turn ● Record off and restore the pre-record subtree pose — recording
/// authors the clip, never the scene.
pub fn stop_record_ui(world: &mut floptle_core::World, st: &mut AnimUiState) {
    st.record = false;
    for (e, tr) in st.record_restore.drain(..) {
        if let Some(slot) = world.get_mut::<floptle_core::Transform>(e) {
            *slot = tr;
        }
    }
    // Re-apply the pre-record property values (cell, opacity, colors…) the same way
    // transforms are restored — recording authors the clip, never the scene.
    for (e, comp, field, val) in std::mem::take(&mut st.record_restore_props) {
        floptle_script::apply_component_field(world, e, &comp, &field, val);
    }
    st.last_scene_local.clear();
    st.last_scene_props.clear();
}

/// Record mode (called from the render loop BEFORE the preview applies): any
/// bound descendant whose local transform changed since the last baseline is
/// keyed at the playhead. Returns true when keys were written.
pub fn record_scan(world: &floptle_core::World, st: &mut AnimUiState, target: Entity) -> bool {
    let named = scene_channel_names(world, target);
    let playhead = if st.snap_fps > 0.0 {
        (st.playhead * st.snap_fps).round() / st.snap_fps
    } else {
        st.playhead
    };
    let mut wrote = false;
    for (e, chan_name) in &named {
        // Unnamed children can't be addressed by a name-bound channel — skip
        // (a "" channel means the target node itself).
        if *e != target && chan_name.is_empty() {
            continue;
        }
        // --- transform diff → TRS keys (nodes that carry a Transform) ---
        if let Some(tr) = world.get::<floptle_core::Transform>(*e) {
            let cur = TransformTRS { t: tr.translation.as_vec3(), r: tr.rotation, s: tr.scale };
            if let Some(prev) = st.last_scene_local.get(e)
                && *prev != cur
                && let Some((_, doc)) = st.clip_doc.as_mut()
            {
                write_key(doc, chan_name, playhead, &cur);
                wrote = true;
            }
            st.last_scene_local.insert(*e, cur);
        }
        // --- property diff → auto-key numeric fields (cell, opacity, colors…) that
        // changed since the baseline, creating the track on first touch. This is
        // what makes "record, then change the spritesheet cell" land a key. ---
        let mir = floptle_script::mirror_components(world, *e);
        for (comp, fields) in ANIMATABLE_PROPS.iter() {
            let Some(cm) = mir.get(*comp) else { continue };
            for (field, kind) in fields.iter() {
                if *kind != PropKind::Float {
                    continue;
                }
                let Some(&v) = cm.get(*field) else { continue };
                let prev = st
                    .last_scene_props
                    .get(e)
                    .and_then(|m| m.get(*comp))
                    .and_then(|m| m.get(*field))
                    .copied();
                if let Some(pv) = prev
                    && (pv - v).abs() > 1e-4
                    && let Some((_, doc)) = st.clip_doc.as_mut()
                {
                    write_property_key(doc, chan_name, comp, field, playhead, v);
                    wrote = true;
                }
            }
        }
        st.last_scene_props.insert(*e, mir);
    }
    if wrote {
        st.clip_dirty = true;
    }
    wrote
}

/// Reset the record baseline to the CURRENT transforms (what the preview just
/// applied), so the next scan only sees fresh user edits.
pub fn refresh_record_baseline(
    world: &floptle_core::World,
    st: &mut AnimUiState,
    target: Entity,
) {
    for (e, _) in scene_channel_names(world, target) {
        if let Some(tr) = world.get::<floptle_core::Transform>(e) {
            st.last_scene_local.insert(
                e,
                TransformTRS { t: tr.translation.as_vec3(), r: tr.rotation, s: tr.scale },
            );
        }
        // Seed the property baseline too, so the first scan keys only real edits.
        st.last_scene_props.insert(e, floptle_script::mirror_components(world, e));
    }
}

/// Numeric animatable fields currently on `e` — the mirror intersected with the
/// `ANIMATABLE_PROPS` float fields. Used to snapshot pre-record values for restore.
fn numeric_props_of(world: &floptle_core::World, e: Entity) -> Vec<(&'static str, &'static str, f64)> {
    let mir = floptle_script::mirror_components(world, e);
    let mut out = Vec::new();
    for (comp, fields) in ANIMATABLE_PROPS.iter() {
        let Some(m) = mir.get(*comp) else { continue };
        for (field, kind) in fields.iter() {
            if *kind == PropKind::Float
                && let Some(&v) = m.get(*field)
            {
                out.push((*comp, *field, v));
            }
        }
    }
    out
}

/// Squared-ish distance from `p` to segment `ab` (in points).
fn seg_dist2(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab = b - a;
    let len2 = ab.length_sq().max(1e-6);
    let t = ((p - a).dot(ab) / len2).clamp(0.0, 1.0);
    let proj = a + ab * t;
    (p - proj).length()
}

/// Union of key times across a channel's lanes (deduped, sorted).
fn union_times(ch: &floptle_scene::AnimChannelDoc) -> Vec<f32> {
    let mut v = Vec::new();
    let mut extend = |times: Option<&Vec<f32>>| {
        if let Some(ts) = times {
            for &t in ts {
                if !v.iter().any(|&x: &f32| (x - t).abs() < 1e-4) {
                    v.push(t);
                }
            }
        }
    };
    extend(ch.translation.as_ref().map(|l| &l.times));
    extend(ch.rotation.as_ref().map(|l| &l.times));
    extend(ch.scale.as_ref().map(|l| &l.times));
    v.sort_by(|a, b| a.total_cmp(b));
    v
}

/// Remove channel `ci` if it now has no transform lanes and no property tracks.
fn drop_empty_channel(doc: &mut AnimClipDoc, ci: usize) {
    if let Some(ch) = doc.channels.get(ci)
        && ch.translation.is_none()
        && ch.rotation.is_none()
        && ch.scale.is_none()
        && ch.properties.is_empty()
    {
        doc.channels.remove(ci);
    }
}

/// Move a single property-track key from `old` to `new` (re-sorting; a key already
/// at `new` is merged). The property counterpart to [`retime_channel`].
fn retime_property_key(pt: &mut AnimPropTrackDoc, old: f32, new: f32) {
    if let Some(i) = pt.times.iter().position(|&t| (t - old).abs() < 1e-4) {
        let v = pt.values.remove(i);
        pt.times.remove(i);
        if let Some(j) = pt.times.iter().position(|&t| (t - new).abs() < 1e-4) {
            pt.values[j] = v; // merge onto the existing key
            return;
        }
        let at = pt.times.partition_point(|&t| t < new);
        pt.times.insert(at, new);
        pt.values.insert(at, v);
    }
}

/// Delete the property-track key at `t`, dropping an emptied track and then an
/// emptied channel.
fn delete_property_key(doc: &mut AnimClipDoc, ci: usize, ti: usize, t: f32) {
    {
        let Some(ch) = doc.channels.get_mut(ci) else { return };
        let Some(pt) = ch.properties.get_mut(ti) else { return };
        if let Some(i) = pt.times.iter().position(|&x| (x - t).abs() < 1e-4) {
            pt.times.remove(i);
            pt.values.remove(i);
        }
        if pt.times.is_empty() {
            ch.properties.remove(ti);
        }
    }
    drop_empty_channel(doc, ci);
}

/// Move every lane key at `old` to `new` (keeping lanes sorted). A key already
/// sitting at `new` is replaced (merge), never doubled.
fn retime_channel(ch: &mut floptle_scene::AnimChannelDoc, old: f32, new: f32) {
    fn retime3(l: &mut AnimTrackDoc3, old: f32, new: f32) {
        if let Some(i) = l.times.iter().position(|&t| (t - old).abs() < 1e-4) {
            let v = l.values.remove(i);
            l.times.remove(i);
            if let Some(j) = l.times.iter().position(|&t| (t - new).abs() < 1e-4) {
                l.values[j] = v; // merge onto the existing key
                return;
            }
            let at = l.times.partition_point(|&t| t < new);
            l.times.insert(at, new);
            l.values.insert(at, v);
        }
    }
    fn retime4(l: &mut AnimTrackDoc4, old: f32, new: f32) {
        if let Some(i) = l.times.iter().position(|&t| (t - old).abs() < 1e-4) {
            let v = l.values.remove(i);
            l.times.remove(i);
            if let Some(j) = l.times.iter().position(|&t| (t - new).abs() < 1e-4) {
                l.values[j] = v;
                return;
            }
            let at = l.times.partition_point(|&t| t < new);
            l.times.insert(at, new);
            l.values.insert(at, v);
        }
    }
    if let Some(l) = ch.translation.as_mut() {
        retime3(l, old, new);
    }
    if let Some(l) = ch.rotation.as_mut() {
        retime4(l, old, new);
    }
    if let Some(l) = ch.scale.as_mut() {
        retime3(l, old, new);
    }
}

/// Delete every lane key at `t`; drops emptied lanes.
fn delete_channel_key(ch: &mut floptle_scene::AnimChannelDoc, t: f32) {
    fn del3(l: &mut AnimTrackDoc3, t: f32) -> bool {
        if let Some(i) = l.times.iter().position(|&x| (x - t).abs() < 1e-4) {
            l.times.remove(i);
            l.values.remove(i);
        }
        l.times.is_empty()
    }
    fn del4(l: &mut AnimTrackDoc4, t: f32) -> bool {
        if let Some(i) = l.times.iter().position(|&x| (x - t).abs() < 1e-4) {
            l.times.remove(i);
            l.values.remove(i);
        }
        l.times.is_empty()
    }
    if ch.translation.as_mut().is_some_and(|l| del3(l, t)) {
        ch.translation = None;
    }
    if ch.rotation.as_mut().is_some_and(|l| del4(l, t)) {
        ch.rotation = None;
    }
    if ch.scale.as_mut().is_some_and(|l| del3(l, t)) {
        ch.scale = None;
    }
}

/// Write (or overwrite) a full TRS key for `chan_name` at time `t`.
pub(crate) fn write_key(doc: &mut AnimClipDoc, chan_name: &str, t: f32, trs: &TransformTRS) {
    let ch = match doc.channels.iter_mut().find(|c| c.node == chan_name) {
        Some(c) => c,
        None => {
            doc.channels.push(floptle_scene::AnimChannelDoc {
                node: chan_name.to_string(),
                ..Default::default()
            });
            doc.channels.last_mut().unwrap()
        }
    };
    fn put3(l: &mut Option<AnimTrackDoc3>, t: f32, v: [f32; 3]) {
        let l = l.get_or_insert_with(Default::default);
        if let Some(i) = l.times.iter().position(|&x| (x - t).abs() < 1e-4) {
            l.values[i] = v;
        } else {
            let at = l.times.partition_point(|&x| x < t);
            l.times.insert(at, t);
            l.values.insert(at, v);
        }
    }
    fn put4(l: &mut Option<AnimTrackDoc4>, t: f32, v: [f32; 4]) {
        let l = l.get_or_insert_with(Default::default);
        if let Some(i) = l.times.iter().position(|&x| (x - t).abs() < 1e-4) {
            l.values[i] = v;
        } else {
            let at = l.times.partition_point(|&x| x < t);
            l.times.insert(at, t);
            l.values.insert(at, v);
        }
    }
    put3(&mut ch.translation, t, trs.t.to_array());
    put4(&mut ch.rotation, t, trs.r.to_array());
    put3(&mut ch.scale, t, trs.s.to_array());
    if t > doc.duration {
        doc.duration = t;
    }
}

/// Write (or overwrite) a numeric property key on `(chan_name, comp, field)`,
/// creating the channel and its property track as needed. The recorder's
/// property counterpart to [`write_key`].
fn write_property_key(
    doc: &mut AnimClipDoc,
    chan_name: &str,
    comp: &str,
    field: &str,
    t: f32,
    value: f64,
) {
    let ci = match doc.channels.iter().position(|c| c.node == chan_name) {
        Some(i) => i,
        None => {
            doc.channels
                .push(floptle_scene::AnimChannelDoc { node: chan_name.to_string(), ..Default::default() });
            doc.channels.len() - 1
        }
    };
    let props = &mut doc.channels[ci].properties;
    let ti = match props.iter().position(|p| p.component == comp && p.field == field) {
        Some(i) => i,
        None => {
            props.push(AnimPropTrackDoc {
                component: comp.to_string(),
                field: field.to_string(),
                times: Vec::new(),
                values: Vec::new(),
                // The spritesheet frame index holds each key (no blend).
                step: comp == "UiElement" && field == "cell",
            });
            props.len() - 1
        }
    };
    let pt = &mut props[ti];
    let v = AnimPropValueDoc::Float(value as f32);
    if let Some(i) = pt.times.iter().position(|&x| (x - t).abs() < 1e-4) {
        pt.values[i] = v;
    } else {
        let at = pt.times.partition_point(|&x| x < t);
        pt.times.insert(at, t);
        pt.values.insert(at, v);
    }
    if t > doc.duration {
        doc.duration = t;
    }
}

/// Add a property track for `(node, component, field)` if one doesn't exist,
/// creating the node's channel as needed. Image/text fields are stepped.
fn add_property_track(doc: &mut AnimClipDoc, node: &str, component: &str, field: &str) {
    let ci = match doc.channels.iter().position(|c| c.node == node) {
        Some(i) => i,
        None => {
            doc.channels
                .push(floptle_scene::AnimChannelDoc { node: node.to_string(), ..Default::default() });
            doc.channels.len() - 1
        }
    };
    let props = &mut doc.channels[ci].properties;
    if props.iter().any(|p| p.component == component && p.field == field) {
        return; // already present — don't duplicate
    }
    props.push(AnimPropTrackDoc {
        component: component.to_string(),
        field: field.to_string(),
        times: Vec::new(),
        values: Vec::new(),
        // Text swaps and the spritesheet frame index hold each key (no blend).
        step: prop_kind(component, field) == PropKind::Text
            || (component == "UiElement" && field == "cell"),
    });
}

/// Insert (or overwrite) a key at time `t` on a property track, seeding a
/// sensible default value for its kind (edit it inline afterwards).
fn key_property_at(pt: &mut AnimPropTrackDoc, t: f32, kind: PropKind) {
    let value = match kind {
        PropKind::Float => {
            // Carry the previous key's value forward so a new key doesn't jump.
            let at = pt.times.partition_point(|&x| x < t);
            pt.values
                .get(at.saturating_sub(1))
                .cloned()
                .unwrap_or(AnimPropValueDoc::Float(0.0))
        }
        PropKind::Text => {
            let at = pt.times.partition_point(|&x| x < t);
            pt.values.get(at.saturating_sub(1)).cloned().unwrap_or(AnimPropValueDoc::Text(String::new()))
        }
    };
    if let Some(i) = pt.times.iter().position(|&x| (x - t).abs() < 1e-4) {
        pt.values[i] = value;
    } else {
        let at = pt.times.partition_point(|&x| x < t);
        pt.times.insert(at, t);
        pt.values.insert(at, value);
    }
}

/// (entity, channel name) for the target + every descendant — the same names
/// `anim::scene_skeleton` binds, `""` for the target itself.
fn scene_channel_names(world: &floptle_core::World, root: Entity) -> Vec<(Entity, String)> {
    let mut children: HashMap<Entity, Vec<Entity>> = HashMap::new();
    for (e, p) in world.query::<floptle_core::Parent>() {
        children.entry(p.0).or_default().push(e);
    }
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // The root records as channel "" but still claims its real name, matching
    // anim::scene_skeleton's dedup exactly (a child sharing the root's name
    // must land on the same "#2" suffix in both walks).
    if let Some(n) = world.get::<Name>(root) {
        seen.insert(n.0.clone());
    }
    fn walk(
        world: &floptle_core::World,
        children: &HashMap<Entity, Vec<Entity>>,
        e: Entity,
        root: bool,
        out: &mut Vec<(Entity, String)>,
        seen: &mut std::collections::HashSet<String>,
    ) {
        let mut name = if root {
            String::new()
        } else {
            world.get::<Name>(e).map(|n| n.0.clone()).unwrap_or_default()
        };
        if !root && !seen.insert(name.clone()) {
            let mut i = 2;
            while !seen.insert(format!("{name}#{i}")) {
                i += 1;
            }
            name = format!("{name}#{i}");
        }
        out.push((e, name));
        if let Some(kids) = children.get(&e) {
            for &k in kids {
                walk(world, children, k, false, out, seen);
            }
        }
    }
    walk(world, &children, root, true, &mut out, &mut seen);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_clip() -> AnimClipDoc {
        AnimClipDoc {
            name: "t".into(),
            duration: 2.0,
            source_model: String::new(),
            channels: Vec::new(),
            events: Vec::new(),
        }
    }

    /// The recorder's property key write, the timeline's drag-retime, and the
    /// delete path all round-trip on one lane — including dropping the emptied
    /// track and its now-empty channel.
    #[test]
    fn property_key_write_retime_delete_roundtrip() {
        let mut doc = empty_clip();
        // Two cell keys auto-create the channel + a STEPPED track (no blending frames).
        write_property_key(&mut doc, "Hand", "UiElement", "cell", 0.5, 3.0);
        write_property_key(&mut doc, "Hand", "UiElement", "cell", 1.0, 5.0);
        assert_eq!(doc.channels.len(), 1);
        assert_eq!(doc.channels[0].node, "Hand");
        assert_eq!(doc.channels[0].properties.len(), 1);
        assert!(doc.channels[0].properties[0].step, "a spritesheet cell lane must step");
        assert_eq!(doc.channels[0].properties[0].times, vec![0.5, 1.0]);

        // Re-keying an existing time overwrites in place, never doubles it.
        write_property_key(&mut doc, "Hand", "UiElement", "cell", 0.5, 9.0);
        assert_eq!(doc.channels[0].properties[0].times, vec![0.5, 1.0]);
        assert_eq!(doc.channels[0].properties[0].values[0], AnimPropValueDoc::Float(9.0));

        // Dragging the second key before the first re-sorts the lane.
        retime_property_key(&mut doc.channels[0].properties[0], 1.0, 0.25);
        assert_eq!(doc.channels[0].properties[0].times, vec![0.25, 0.5]);
        assert_eq!(doc.channels[0].properties[0].values[0], AnimPropValueDoc::Float(5.0));

        // Deleting both keys drops the emptied track AND the now-empty channel.
        delete_property_key(&mut doc, 0, 0, 0.25);
        assert_eq!(doc.channels[0].properties[0].times, vec![0.5]);
        delete_property_key(&mut doc, 0, 0, 0.5);
        assert!(doc.channels.is_empty(), "an empty channel is removed");
    }

    /// A channel that still carries a transform lane is NOT dropped when its last
    /// property track goes away.
    #[test]
    fn drop_empty_channel_spares_transform_lanes() {
        let mut doc = empty_clip();
        write_property_key(&mut doc, "N", "PointLight", "intensity", 0.0, 1.0);
        doc.channels[0].translation = Some(floptle_scene::AnimTrackDoc3::default());
        doc.channels[0].properties.clear();
        drop_empty_channel(&mut doc, 0);
        assert_eq!(doc.channels.len(), 1, "a channel with a transform lane survives");
    }
}
