//! The ◈ Shaders tab — the node-graph view of a `.flsl` (ADR-0007, proposal
//! §10.2). The first box-and-wire canvas in the editor.
//!
//! One source of truth: the graph renders `floptle_shader::graph::build_view`
//! of the parsed file, and EVERY edit is an IR mutation that re-prints the
//! `.flsl` to disk — so the Scripting tab, VSCode and the hot-reload pipeline
//! all see the same shader, and an external text edit re-syncs the graph on
//! its next frame (mtime watch, the house pattern).
//!
//! Interaction scheme (the node-editor standard): wheel zooms about the
//! pointer, middle-drag pans, left-drag on empty canvas box-selects,
//! left-drag on a node moves the whole selection, right-click adds nodes.
//!
//! Nodes NEVER move on their own: `//@layout` wins, and every auto-laid-out
//! position is frozen in a session cache keyed by reparse-stable identities
//! (`graph::stable_keys`) the moment it's first computed.
//!
//! Continuous edits (dragging a knob) mutate the in-memory IR live and flush
//! to disk when the pointer releases (the anim-graph save-on-release
//! pattern); structural edits (wire/add/delete) flush immediately. Undo here
//! is graph-local: a stack of printed sources.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::time::SystemTime;

use egui::{
    Align2, Color32, CornerRadius, CursorIcon, FontId, PointerButton, Pos2, Rect, Sense, Stroke,
    StrokeKind, UiBuilder,
};
use floptle_shader::graph::{
    self, GNode, InlineVal, NodeKey, NodeKind, Site, NODE_HEADER_H, NODE_ROW_H, NODE_W,
};
use floptle_shader::ir::{self, BinOp, Checked, Input, ShaderIr, Stage, Ty};
use floptle_shader::stdlib;

use crate::assets::is_shader;
use crate::ide::IdeState;
use crate::project::resolve_asset_path;
use crate::{Editor, EditorTabViewer};

/// Graph coords (sink at the origin, sources at negative x) sit at this offset
/// inside the egui Scene's positive space.
const ORIGIN: egui::Vec2 = egui::vec2(2400.0, 1200.0);

/// The canvas zoom bounds (scene scale = screen px per graph unit).
const ZOOM_MIN: f32 = 0.25;
const ZOOM_MAX: f32 = 2.0;

pub(crate) struct ShaderGraphState {
    /// The `.flsl` being edited (an asset-tree path, like `Material.shader`).
    pub(crate) path: Option<String>,
    mtime: Option<SystemTime>,
    src: String,
    pub(crate) ir: Option<ShaderIr>,
    ck: Option<Checked>,
    pub(crate) view: Vec<GNode>,
    /// Session positions by reparse-stable node identity — the "nothing moves
    /// unless you move it" guarantee for nodes without a `//@layout` entry.
    pos_cache: HashMap<String, (f32, f32)>,
    /// First parse/type error: banner line + the node it pins to.
    err: Option<String>,
    err_key: Option<NodeKey>,
    /// The egui Scene's pan/zoom state (scene-space view rect).
    scene_rect: Rect,
    sel: BTreeSet<NodeKey>,
    /// An in-flight wire drag.
    wire: Option<WireDrag>,
    /// Nodes being dragged: each key with its live (graph-space) position.
    drag: Option<Vec<(NodeKey, (f32, f32))>>,
    /// An in-flight box selection (scene coords).
    box_sel: Option<(Pos2, Pos2)>,
    /// In-node text editors mid-flight (title rename, uniform name, swizzle…).
    field_buf: Option<(egui::Id, String)>,
    /// The palette popup's search text + spawn position (graph space).
    palette_search: String,
    palette_at: (f32, f32),
    undo: Vec<String>,
    redo: Vec<String>,
    /// The pre-edit source backing the NEXT undo push (set when an edit
    /// stream begins, consumed at flush).
    pending_undo: Option<String>,
    /// The in-memory IR differs from disk (flushed when the pointer is up).
    dirty: bool,
    /// A transient toast: message + seconds left.
    status: Option<(String, f32)>,
    /// Set by the tab each frame it draws; consumed by the preview driver
    /// (the anim-tab visibility pattern) so the atlas only renders when seen.
    pub(crate) tab_visible: bool,
    /// Nodes whose preview thumbnail the user collapsed (session-local).
    pv_hidden: BTreeSet<NodeKey>,
}

impl Default for ShaderGraphState {
    fn default() -> Self {
        Self {
            path: None,
            mtime: None,
            src: String::new(),
            ir: None,
            ck: None,
            view: Vec::new(),
            pos_cache: HashMap::new(),
            err: None,
            err_key: None,
            scene_rect: Rect::ZERO,
            sel: BTreeSet::new(),
            wire: None,
            drag: None,
            box_sel: None,
            field_buf: None,
            palette_search: String::new(),
            palette_at: (0.0, 0.0),
            undo: Vec::new(),
            redo: Vec::new(),
            pending_undo: None,
            dirty: false,
            status: None,
            tab_visible: false,
            pv_hidden: BTreeSet::new(),
        }
    }
}

/// Which nodes carry a live preview thumbnail — MUST mirror
/// [`floptle_shader::preview::preview_targets`]'s skip rule (uniforms and
/// constants already show their value as widgets).
fn previewable(n: &GNode) -> bool {
    !matches!(n.kind, NodeKind::Uniform(_) | NodeKind::Constant(_))
}

/// Thumbnail edge on the node (the atlas tile is 128 px, drawn slightly up).
const PV_SIDE: f32 = 148.0;

/// The per-node pad every layout pass uses: reserve the preview strip on
/// preview-capable nodes. Constant (independent of the 👁 toggle) so fresh
/// layouts are deterministic and never stack nodes into each other.
fn graph_pad(n: &GNode) -> f32 {
    if previewable(n) {
        PV_SIDE + 6.0
    } else {
        0.0
    }
}

enum WireDrag {
    /// Dragging from a node's output — looking for an input port.
    FromOut(NodeKey),
    /// Dragging from an empty input port — looking for a source node.
    FromIn(Site),
}

/// A boxed IR edit queued by the UI pass.
type Edit = Box<dyn FnOnce(&mut ShaderIr) -> Result<(), String>>;

/// Edits the UI pass queues up, applied after all borrows drop.
enum Act {
    /// A structural edit: applied, then flushed to disk immediately.
    /// `strict` reverts the edit if it introduces a type error into a shader
    /// that previously checked (the beginner-friendly wire rule).
    Commit { edit: Edit, strict: bool },
    /// A continuous edit (knob drag): applied in memory, flushed on release.
    Live(Edit),
    /// Replace the selection.
    Select(Vec<NodeKey>),
    /// Extend the selection (shift-box).
    SelectAdd(Vec<NodeKey>),
    /// Toggle one node (ctrl/shift-click).
    SelectToggle(NodeKey),
    /// Persist node positions (release of a node drag; promotes anons).
    Place(Vec<(NodeKey, (f32, f32))>),
    /// Delete every listed node (best-effort — consumers fall to defaults).
    Delete(Vec<NodeKey>),
    /// Duplicate the listed nodes and select the copies.
    Duplicate(Vec<NodeKey>),
    /// Re-run the auto-layout over the whole graph (one undoable commit).
    Arrange,
    Undo,
    Redo,
}

impl Editor {
    /// Open a `.flsl` in the graph tab (assets double-click / `cmd` intents).
    pub(crate) fn open_shader_in_graph(&mut self, path: &str) {
        if self.shader_graph.path.as_deref() != Some(path) {
            self.shader_graph.flush(&self.project_root, &mut self.ide, true, false);
            self.shader_graph =
                ShaderGraphState { path: Some(path.to_string()), ..Default::default() };
            self.shader_graph.reload(&self.project_root);
        }
        if let Some(dock) = self.dock_state.as_mut() {
            crate::dock::focus_shader_graph_tab(dock);
        }
    }
}

impl ShaderGraphState {
    /// Re-read the file and rebuild the whole graph state (parse + check +
    /// view). Keeps the selection when the nodes still exist.
    pub(crate) fn reload(&mut self, project_root: &Path) {
        let Some(path) = self.path.clone() else { return };
        let full = resolve_asset_path(project_root, &path);
        self.mtime = std::fs::metadata(&full).and_then(|m| m.modified()).ok();
        let src = match std::fs::read_to_string(&full) {
            Ok(s) => s,
            Err(e) => {
                self.err = Some(format!("can't read {path}: {e}"));
                self.err_key = None;
                self.ir = None;
                self.ck = None;
                self.view.clear();
                return;
            }
        };
        self.src = src;
        match floptle_shader::parse(&self.src) {
            Err(e) => {
                let (l, c) = floptle_shader::text::line_col(&self.src, e.span.start);
                self.err = Some(format!("{l}:{c}: {} — fix it in the text view", e.message));
                self.err_key = None;
                self.ir = None;
                self.ck = None;
                self.view.clear();
            }
            Ok(ir) => {
                let (ck, err) = match ir::check(&ir) {
                    Ok(ck) => (Some(ck), None),
                    Err(errs) => {
                        let e = &errs[0];
                        let (l, c) = floptle_shader::text::line_col(&self.src, e.span.start);
                        (None, Some((format!("{l}:{c}: {}", e.message), e.span)))
                    }
                };
                self.view = graph::build_view_padded(&ir, ck.as_ref(), &graph_pad);
                self.err_key = err.as_ref().and_then(|(_, span)| {
                    // Pin the error to the smallest node whose span contains it.
                    let mut best: Option<(u32, NodeKey)> = None;
                    for n in &self.view {
                        let root = match &n.key {
                            NodeKey::Anon(id) => Some(*id),
                            NodeKey::Let(name) => {
                                ir.lets.iter().find(|(x, _)| x == name).map(|(_, e)| *e)
                            }
                            _ => None,
                        };
                        if let Some(id) = root {
                            let s = ir.expr(id).span;
                            if s.start <= span.start && span.start < s.end.max(s.start + 1) {
                                let w = s.end.saturating_sub(s.start);
                                if best.as_ref().is_none_or(|(bw, _)| w < *bw) {
                                    best = Some((w, n.key.clone()));
                                }
                            }
                        }
                    }
                    best.map(|(_, k)| k)
                });
                self.err = err.map(|(msg, _)| msg);
                self.ir = Some(ir);
                self.ck = ck;
                self.freeze_positions();
                let view = std::mem::take(&mut self.view);
                self.sel.retain(|k| view.iter().any(|n| n.key == *k));
                self.pv_hidden.retain(|k| view.iter().any(|n| n.key == *k));
                self.view = view;
            }
        }
    }

    /// Pin every node's position for the session: `//@layout` entries refresh
    /// the cache, cached nodes stay exactly where they were, and only a node
    /// seen for the very first time keeps its auto-layout spot (then freezes).
    ///
    /// Two key families cover each other's blind spots: the name-anchored
    /// keys (`graph::stable_keys`) survive rewires, and the name-free
    /// sink-path keys survive renames/promotions — so neither kind of edit
    /// can shuffle bystander nodes.
    fn freeze_positions(&mut self) {
        let named = graph::stable_keys(&self.view);
        let pathed = sink_path_keys(&self.view);
        for n in &mut self.view {
            let nk = named.get(&n.key);
            let pk = pathed.get(&n.key);
            if !n.placed {
                if let Some(&p) = nk.and_then(|k| self.pos_cache.get(k)) {
                    n.pos = p;
                } else if let Some(&p) = pk.and_then(|k| self.pos_cache.get(k)) {
                    n.pos = p;
                }
            }
            if let Some(k) = nk {
                self.pos_cache.insert(k.clone(), n.pos);
            }
            if let Some(k) = pk {
                self.pos_cache.insert(k.clone(), n.pos);
            }
        }
    }

    /// Rebuild the view from the in-memory IR (live knob edits) without
    /// touching disk — positions stay frozen.
    fn rebuild_view(&mut self) {
        if let Some(ir) = &self.ir {
            self.view = graph::build_view_padded(ir, self.ck.as_ref(), &graph_pad);
            self.freeze_positions();
        }
    }

    /// Print the in-memory IR to disk (when dirty), push the undo snapshot,
    /// resync the IDE buffer, and reload fresh spans. `force` flushes even
    /// while the pointer is down (structural edits).
    pub(crate) fn flush(
        &mut self,
        project_root: &Path,
        ide: &mut IdeState,
        force: bool,
        pointer_down: bool,
    ) {
        if !self.dirty || (!force && pointer_down) {
            return;
        }
        let (Some(path), Some(ir)) = (self.path.clone(), self.ir.as_ref()) else {
            self.dirty = false;
            return;
        };
        let printed = floptle_shader::print(ir);
        let full = resolve_asset_path(project_root, &path);
        if let Err(e) = std::fs::write(&full, &printed) {
            self.status = Some((format!("save failed: {e}"), 4.0));
            return;
        }
        if let Some(prev) = self.pending_undo.take()
            && prev != printed
        {
            self.undo.push(prev);
            if self.undo.len() > 64 {
                self.undo.remove(0);
            }
            self.redo.clear();
        }
        self.dirty = false;
        // The Scripting tab's buffer follows (unless the user has unsaved
        // text edits there — those win when they save).
        if let Some(f) = ide.open.iter_mut().find(|f| f.path == path && !f.dirty) {
            f.text = printed;
        }
        self.reload(project_root);
    }

    /// Swap the file to `text` (the undo/redo restore path).
    fn restore(&mut self, project_root: &Path, ide: &mut IdeState, text: String) {
        let Some(path) = self.path.clone() else { return };
        let full = resolve_asset_path(project_root, &path);
        if std::fs::write(&full, &text).is_err() {
            return;
        }
        if let Some(f) = ide.open.iter_mut().find(|f| f.path == path && !f.dirty) {
            f.text = text;
        }
        self.dirty = false;
        self.pending_undo = None;
        self.reload(project_root);
    }

    fn undo(&mut self, project_root: &Path, ide: &mut IdeState) {
        self.flush(project_root, ide, true, false);
        if let Some(prev) = self.undo.pop() {
            let cur = self.src.clone();
            self.redo.push(cur);
            self.restore(project_root, ide, prev);
        }
    }

    fn redo(&mut self, project_root: &Path, ide: &mut IdeState) {
        if let Some(next) = self.redo.pop() {
            let cur = self.src.clone();
            self.undo.push(cur);
            self.restore(project_root, ide, next);
        }
    }

    /// Apply one queued edit.
    fn apply(&mut self, act: Act, project_root: &Path, ide: &mut IdeState) {
        match act {
            Act::Select(keys) => self.sel = keys.into_iter().collect(),
            Act::SelectAdd(keys) => self.sel.extend(keys),
            Act::SelectToggle(k) => {
                if !self.sel.remove(&k) {
                    self.sel.insert(k);
                }
            }
            Act::Undo => self.undo(project_root, ide),
            Act::Redo => self.redo(project_root, ide),
            Act::Live(edit) => {
                let Some(ir) = self.ir.as_mut() else { return };
                if self.pending_undo.is_none() {
                    self.pending_undo = Some(self.src.clone());
                }
                if let Err(e) = edit(ir) {
                    self.status = Some((e, 3.0));
                } else {
                    self.dirty = true;
                    // The view renders from the IR — rebuild so the knob shows
                    // its new value this frame.
                    self.rebuild_view();
                }
            }
            Act::Commit { edit, strict } => {
                let was_ok = self.err.is_none();
                {
                    let Some(ir) = self.ir.as_mut() else { return };
                    let backup = ir.clone();
                    match edit(ir) {
                        Err(e) => {
                            *ir = backup;
                            self.status = Some((e, 3.0));
                            return;
                        }
                        Ok(()) => {
                            if strict && was_ok
                                && let Err(errs) = ir::check(ir)
                            {
                                let msg = errs
                                    .first()
                                    .map(|e| e.message.clone())
                                    .unwrap_or_else(|| "type error".into());
                                *ir = backup;
                                self.status = Some((format!("✋ {msg}"), 3.5));
                                return;
                            }
                        }
                    }
                }
                if self.pending_undo.is_none() {
                    self.pending_undo = Some(self.src.clone());
                }
                self.dirty = true;
                self.flush(project_root, ide, true, false);
            }
            Act::Place(moves) => {
                let placed = self.commit_returning(
                    move |ir| {
                        moves
                            .into_iter()
                            .map(|(k, pos)| graph::set_position(ir, &k, pos))
                            .collect::<Result<Vec<_>, _>>()
                    },
                    project_root,
                    ide,
                );
                if let Some(keys) = placed {
                    self.sel = keys.into_iter().collect();
                }
            }
            Act::Delete(keys) => {
                self.sel.clear();
                self.apply(
                    Act::Commit {
                        // Best-effort: deleting A may take a selected
                        // downstream B's site with it — skip, don't abort.
                        edit: Box::new(move |ir| {
                            for k in &keys {
                                let _ = graph::delete_node(ir, k);
                            }
                            Ok(())
                        }),
                        strict: false,
                    },
                    project_root,
                    ide,
                );
            }
            Act::Arrange => {
                // Fresh positions for everything: drop the session freeze so
                // the new layout isn't overridden by remembered spots.
                self.pos_cache.clear();
                self.apply(
                    Act::Commit {
                        edit: Box::new(|ir| {
                            graph::arrange(ir, None, &graph_pad);
                            Ok(())
                        }),
                        strict: false,
                    },
                    project_root,
                    ide,
                );
            }
            Act::Duplicate(keys) => {
                let copies = self.commit_returning(
                    move |ir| graph::duplicate_nodes(ir, &keys),
                    project_root,
                    ide,
                );
                if let Some(keys) = copies {
                    self.sel = keys.into_iter().collect();
                }
            }
        }
    }

    /// A commit that returns a value (placement/duplication return keys).
    fn commit_returning<T>(
        &mut self,
        edit: impl FnOnce(&mut ShaderIr) -> Result<T, String>,
        project_root: &Path,
        ide: &mut IdeState,
    ) -> Option<T> {
        let out = {
            let ir = self.ir.as_mut()?;
            let backup = ir.clone();
            match edit(ir) {
                Err(e) => {
                    *ir = backup;
                    self.status = Some((e, 3.0));
                    return None;
                }
                Ok(v) => v,
            }
        };
        if self.pending_undo.is_none() {
            self.pending_undo = Some(self.src.clone());
        }
        self.dirty = true;
        self.flush(project_root, ide, true, false);
        Some(out)
    }
}

impl EditorTabViewer<'_> {
    // ---- the tab --------------------------------------------------------------

    pub(crate) fn shader_graph_ui(&mut self, ui: &mut egui::Ui) {
        // Arm the preview driver for next frame + keep animated previews live.
        self.shader_graph.tab_visible = true;
        if self.shader_preview.enabled && self.shader_graph.ir.is_some() {
            ui.ctx().request_repaint();
        }
        // External edits (VSCode, the Scripting tab, hot tools) re-sync the
        // graph — the same mtime watch the shader compiler runs.
        if let Some(path) = self.shader_graph.path.clone() {
            let full = resolve_asset_path(self.project_root, &path);
            let mtime = std::fs::metadata(&full).and_then(|m| m.modified()).ok();
            if mtime != self.shader_graph.mtime && !self.shader_graph.dirty {
                self.shader_graph.reload(self.project_root);
            }
        }

        let mut acts: Vec<Act> = Vec::new();
        self.shader_graph_header(ui, &mut acts);
        ui.separator();

        if self.shader_graph.path.is_none() {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.weak("Pick a shader above, or double-click a ◈ .flsl in the Assets browser.");
                ui.weak("Right-click the canvas to add nodes once one is open.");
            });
            return;
        }
        if self.shader_graph.ir.is_some() {
            self.shader_graph_canvas(ui, &mut acts);
        } else if let Some(err) = self.shader_graph.err.clone() {
            ui.add_space(16.0);
            ui.colored_label(Color32::from_rgb(235, 100, 100), format!("⚠ {err}"));
            if ui.button("</> Open as text").clicked()
                && let Some(p) = self.shader_graph.path.clone()
            {
                self.cmd.open_script_pref = Some(p);
            }
        }

        for act in acts {
            self.shader_graph.apply(act, self.project_root, self.ide);
        }
        self.shader_graph.flush(self.project_root, self.ide, false, self.pointer_down);
        // Toast countdown.
        if let Some((_, ttl)) = &mut self.shader_graph.status {
            *ttl -= ui.input(|i| i.stable_dt).min(0.1);
            if *ttl <= 0.0 {
                self.shader_graph.status = None;
            }
            ui.ctx().request_repaint();
        }
    }

    fn shader_graph_header(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        ui.horizontal(|ui| {
            ui.label("◈");
            let cur = self
                .shader_graph
                .path
                .as_deref()
                .and_then(|p| Path::new(p).file_name())
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "(pick a shader)".into());
            if let Some(Some(p)) = crate::ui_widgets::asset_picker(
                ui,
                egui::Id::new("shader-graph-pick"),
                &cur,
                None,
                self.asset_tree,
                is_shader,
                200.0,
            ) {
                self.cmd.open_shader_graph = Some(p);
            }
            if ui.button("✚ New").on_hover_text("create a shader and open it here").clicked() {
                self.cmd.new_shader_in = Some(String::new());
                self.cmd.new_shader_to_graph = true;
            }
            let Some(ir) = self.shader_graph.ir.as_ref() else {
                if let Some((msg, _)) = &self.shader_graph.status {
                    ui.colored_label(Color32::from_rgb(235, 170, 90), msg.clone());
                }
                return;
            };
            let stage = ir.stage.unwrap_or(Stage::Fragment);
            // ✚ Node — the palette without the right-click (dropped mid-view).
            let btn = ui
                .button("✚ Node")
                .on_hover_text("add a node at the center of the view (or right-click the canvas)");
            let center = {
                let r = self.shader_graph.scene_rect;
                if r.width() > 1.0 {
                    (r.center().x - ORIGIN.x, r.center().y - ORIGIN.y)
                } else {
                    (0.0, -200.0)
                }
            };
            if btn.clicked() {
                self.shader_graph.palette_search.clear();
            }
            egui::Popup::menu(&btn).id(egui::Id::new("sg-add-node")).show(|ui| {
                let mut search = std::mem::take(&mut self.shader_graph.palette_search);
                if let Some(act) = palette_ui(ui, stage, &mut search, center) {
                    acts.push(act);
                    ui.close();
                }
                self.shader_graph.palette_search = search;
            });
            ui.separator();
            match ir.stage {
                Some(Stage::Sdf) => {
                    ui.label(egui::RichText::new("sdf — this shader IS geometry").small())
                        .on_hover_text("assign it to a ◈ Field Shape node (Add → Field Shape)");
                }
                _ => {
                    ui.label(egui::RichText::new("fragment").small())
                        .on_hover_text("a surface look — assign it on a Material's Shader row");
                    let mut blend = ir.blend;
                    egui::ComboBox::from_id_salt("sg-blend")
                        .selected_text(match blend {
                            ir::Blend::Opaque => "opaque",
                            ir::Blend::Alpha => "alpha",
                            ir::Blend::Additive => "additive",
                        })
                        .width(80.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut blend, ir::Blend::Opaque, "opaque");
                            ui.selectable_value(&mut blend, ir::Blend::Alpha, "alpha");
                            ui.selectable_value(&mut blend, ir::Blend::Additive, "additive");
                        });
                    if blend != ir.blend {
                        acts.push(Act::Commit {
                            edit: Box::new(move |ir| {
                                ir.blend = blend;
                                Ok(())
                            }),
                            strict: false,
                        });
                    }
                }
            }
            ui.separator();
            let st = &self.shader_graph;
            if ui
                .add_enabled(!st.undo.is_empty(), egui::Button::new("↶"))
                .on_hover_text("undo (Ctrl+Z over the canvas)")
                .clicked()
            {
                acts.push(Act::Undo);
            }
            if ui
                .add_enabled(!st.redo.is_empty(), egui::Button::new("↷"))
                .on_hover_text("redo")
                .clicked()
            {
                acts.push(Act::Redo);
            }
            if ui.button("⛶").on_hover_text("frame the whole graph").clicked() {
                self.shader_graph.scene_rect = Rect::ZERO;
            }
            if ui
                .button("⇅")
                .on_hover_text("auto-arrange all nodes (spaced for previews; undoable)")
                .clicked()
            {
                acts.push(Act::Arrange);
                // Re-frame after the shuffle so the tidied graph is in view.
                self.shader_graph.scene_rect = Rect::ZERO;
            }
            if ui
                .selectable_label(self.shader_preview.enabled, "👁")
                .on_hover_text("live previews on every node (right-click a node to hide just its own)")
                .clicked()
            {
                self.shader_preview.enabled = !self.shader_preview.enabled;
            }
            let path = self.shader_graph.path.clone().unwrap_or_default();
            if ui
                .button("</>")
                .on_hover_text("edit as text (same shader — the views stay in sync)")
                .clicked()
            {
                self.cmd.open_script_pref = Some(path.clone());
            }
            let st = &self.shader_graph;
            // Status / errors, most urgent first.
            if let Some((msg, _)) = &st.status {
                ui.colored_label(Color32::from_rgb(235, 170, 90), msg.clone());
            } else if let Some(err) = &st.err {
                ui.colored_label(
                    Color32::from_rgb(235, 100, 100),
                    egui::RichText::new(format!("⚠ {err}")).small(),
                );
            } else if let Some(cache_err) = st.path.as_deref().and_then(|p| {
                self.flsl_cache
                    .get(p)
                    .and_then(|e| e.error.clone())
                    .or_else(|| self.sdf_cache.get(p).and_then(|e| e.error.clone()))
            }) {
                ui.colored_label(
                    Color32::from_rgb(235, 100, 100),
                    egui::RichText::new(format!("⚠ {cache_err}")).small(),
                );
            } else if let Some(pe) = self.shader_preview.err.clone() {
                // Preview-only trouble never blocks editing — mention quietly.
                ui.weak(format!("previews paused: {pe}"));
            } else {
                // A gentle nudge when the shader isn't visible anywhere.
                let used = self
                    .world
                    .query::<floptle_core::Material>()
                    .any(|(_, m)| m.shader.as_deref() == Some(path.as_str()));
                if !used {
                    ui.weak("not in the scene yet — assign it on a Material to watch edits live");
                }
            }
        });
    }

    // ---- the canvas ------------------------------------------------------------

    fn shader_graph_canvas(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        // Wheel = zoom about the pointer (the node-editor standard). Steal the
        // scroll before the Scene turns it into a pan; ctrl+wheel and pinch
        // still zoom via the Scene, shift+wheel still pans sideways.
        let canvas_rect = ui.available_rect_before_wrap();
        if ui.rect_contains_pointer(canvas_rect) {
            let (scroll, mods, ptr) =
                ui.input(|i| (i.smooth_scroll_delta.y, i.modifiers, i.pointer.latest_pos()));
            if scroll != 0.0
                && !mods.ctrl
                && !mods.command
                && !mods.shift
                && let Some(ptr) = ptr
            {
                // Consume it the way ScrollArea does, so the Scene can't ALSO
                // pan with the same wheel motion.
                ui.input_mut(|i| i.smooth_scroll_delta = egui::Vec2::ZERO);
                let r = &mut self.shader_graph.scene_rect;
                if r.width() > 0.0 && r.height() > 0.0 {
                    // Mirror the Scene's letterboxed fit to find the anchor.
                    let scale =
                        (canvas_rect.width() / r.width()).min(canvas_rect.height() / r.height());
                    let k = (scroll * 0.0035).exp();
                    let k = (scale * k).clamp(ZOOM_MIN, ZOOM_MAX) / scale;
                    if (k - 1.0).abs() > 1e-4 {
                        let anchor = r.center() + (ptr - canvas_rect.center()) / scale;
                        *r = Rect::from_min_size(anchor + (r.min - anchor) / k, r.size() / k);
                    }
                }
            }
        }

        // Read-only copies so node UIs can borrow freely while we queue edits.
        let view = self.shader_graph.view.clone();
        let ir = self.shader_graph.ir.clone().unwrap_or_default();
        let sel = self.shader_graph.sel.clone();
        let err_key = self.shader_graph.err_key.clone();
        let drag = self.shader_graph.drag.clone();

        // Node rects (graph space + ORIGIN) — the wire endpoints. A node with
        // a live preview reserves the thumbnail strip below its body.
        let pv_on = self.shader_preview.enabled;
        let pv_hidden = self.shader_graph.pv_hidden.clone();
        let pv_extra = move |n: &GNode| -> f32 {
            if pv_on && previewable(n) && !pv_hidden.contains(&n.key) {
                PV_SIDE + 6.0
            } else {
                0.0
            }
        };
        let rect_of = {
            let pv_extra = pv_extra.clone();
            move |n: &GNode| -> Rect {
                let pos = drag
                    .as_deref()
                    .and_then(|d| d.iter().find(|(k, _)| *k == n.key))
                    .map(|(_, p)| *p)
                    .unwrap_or(n.pos);
                Rect::from_min_size(
                    Pos2::new(pos.0, pos.1) + ORIGIN,
                    egui::vec2(NODE_W, graph::node_height(n) + pv_extra(n)),
                )
            }
        };
        let mut rects: HashMap<NodeKey, Rect> = Default::default();
        for n in &view {
            rects.insert(n.key.clone(), rect_of(n));
        }
        let in_port_pos = |r: &Rect, i: usize| {
            Pos2::new(r.left(), r.top() + NODE_HEADER_H + i as f32 * NODE_ROW_H + NODE_ROW_H * 0.5)
        };
        let out_port_pos = |r: &Rect| Pos2::new(r.right(), r.top() + NODE_HEADER_H * 0.5);

        let mut scene_rect = self.shader_graph.scene_rect;
        let mut hover_in: Option<Site> = None;
        let mut hover_out: Option<NodeKey> = None;
        let mut pointer_scene: Option<Pos2> = None;

        let scene = egui::Scene::new()
            .zoom_range(ZOOM_MIN..=ZOOM_MAX)
            .max_inner_size(egui::vec2(6000.0, 3000.0))
            .drag_pan_buttons(egui::DragPanButtons::MIDDLE);
        let resp = scene.show(ui, &mut scene_rect, |ui| {
            // Pointer in scene coords (wire preview + box select).
            if let Some(sp) = ui.input(|i| i.pointer.latest_pos())
                && let Some(t) = ui.ctx().layer_transform_to_global(ui.layer_id())
            {
                pointer_scene = Some(t.inverse() * sp);
            }
            // A soft dot grid so pan/zoom reads spatially.
            let clip = ui.clip_rect();
            let p = ui.painter();
            let step = 32.0;
            let dot = ui.visuals().weak_text_color().gamma_multiply(0.22);
            let mut x = (clip.left() / step).floor() * step;
            while x < clip.right() {
                let mut y = (clip.top() / step).floor() * step;
                while y < clip.bottom() {
                    p.circle_filled(Pos2::new(x, y), 1.0, dot);
                    y += step;
                }
                x += step;
            }

            // ---- wires (under the nodes) ----
            for n in &view {
                let Some(r) = rects.get(&n.key) else { continue };
                for (i, port) in n.inputs.iter().enumerate() {
                    let Some(src) = &port.wired else { continue };
                    let Some(sr) = rects.get(src) else { continue };
                    let a = out_port_pos(sr);
                    let b = in_port_pos(r, i);
                    draw_wire(ui.painter(), a, b, port_color(ui, port.ty, port.is_texture), 2.0);
                }
            }
            // The in-flight wire preview.
            if let (Some(w), Some(ptr)) = (&self.shader_graph.wire, pointer_scene) {
                let (a, b) = match w {
                    WireDrag::FromOut(src) => {
                        (rects.get(src).map(out_port_pos).unwrap_or(ptr), ptr)
                    }
                    WireDrag::FromIn(site) => {
                        let mut at = ptr;
                        'find: for n in &view {
                            for (i, p) in n.inputs.iter().enumerate() {
                                if p.site == *site
                                    && let Some(r) = rects.get(&n.key)
                                {
                                    at = in_port_pos(r, i);
                                    break 'find;
                                }
                            }
                        }
                        (ptr, at)
                    }
                };
                draw_wire(ui.painter(), a, b, ui.visuals().hyperlink_color, 2.5);
            }

            // ---- nodes ----
            for n in &view {
                let r = rects[&n.key];
                self.draw_node(
                    ui,
                    n,
                    r,
                    &ir,
                    &sel,
                    err_key.as_ref() == Some(&n.key),
                    acts,
                    &mut hover_in,
                    &mut hover_out,
                );
            }

            // ---- the box-selection rectangle (over everything) ----
            if let Some((a, b)) = self.shader_graph.box_sel {
                let r = Rect::from_two_pos(a, b);
                let col = ui.visuals().selection.stroke.color;
                ui.painter().rect(
                    r,
                    0.0,
                    col.gamma_multiply(0.12),
                    Stroke::new(1.0, col),
                    StrokeKind::Inside,
                );
            }
        });
        self.shader_graph.scene_rect = scene_rect;

        // ---- wire-drag resolution (release over a compatible port) ----
        let released = ui.input(|i| i.pointer.any_released());
        if released && self.shader_graph.wire.is_some() {
            match self.shader_graph.wire.take() {
                Some(WireDrag::FromOut(src)) => {
                    if let Some(site) = hover_in {
                        acts.push(Act::Commit {
                            edit: Box::new(move |ir| graph::connect(ir, &src, site)),
                            strict: true,
                        });
                    }
                }
                Some(WireDrag::FromIn(site)) => {
                    if let Some(src) = hover_out {
                        acts.push(Act::Commit {
                            edit: Box::new(move |ir| graph::connect(ir, &src, site)),
                            strict: true,
                        });
                    }
                }
                None => {}
            }
        }

        // ---- box select (left-drag on empty canvas) ----
        let bg = &resp.response;
        if bg.drag_started_by(PointerButton::Primary)
            && self.shader_graph.wire.is_none()
            && self.shader_graph.drag.is_none()
            && let Some(ptr) = pointer_scene
        {
            self.shader_graph.box_sel = Some((ptr, ptr));
        }
        if bg.dragged_by(PointerButton::Primary)
            && let (Some(b), Some(ptr)) = (&mut self.shader_graph.box_sel, pointer_scene)
        {
            b.1 = ptr;
        }
        if bg.drag_stopped_by(PointerButton::Primary)
            && let Some((a, b)) = self.shader_graph.box_sel.take()
        {
            let r = Rect::from_two_pos(a, b);
            let hit: Vec<NodeKey> = view
                .iter()
                .filter(|n| r.intersects(rects[&n.key]))
                .map(|n| n.key.clone())
                .collect();
            let additive = ui.input(|i| i.modifiers.shift || i.modifiers.command);
            acts.push(if additive { Act::SelectAdd(hit) } else { Act::Select(hit) });
        }

        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.shader_graph.wire = None;
            self.shader_graph.box_sel = None;
        }

        // ---- canvas-level interactions ----
        let stage = ir.stage.unwrap_or(Stage::Fragment);
        if bg.clicked() && !ui.input(|i| i.modifiers.shift || i.modifiers.command) {
            acts.push(Act::Select(Vec::new()));
        }
        if bg.secondary_clicked()
            && let Some(ptr) = pointer_scene
        {
            self.shader_graph.palette_at = (ptr.x - ORIGIN.x, ptr.y - ORIGIN.y);
            self.shader_graph.palette_search.clear();
        }
        bg.context_menu(|ui| {
            let at = self.shader_graph.palette_at;
            let mut search = std::mem::take(&mut self.shader_graph.palette_search);
            if let Some(act) = palette_ui(ui, stage, &mut search, at) {
                acts.push(act);
                ui.close();
            }
            self.shader_graph.palette_search = search;
        });
        if bg.hovered() {
            let (del, undo, redo, dup) = ui.input(|i| {
                (
                    i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace),
                    i.modifiers.command && !i.modifiers.shift && i.key_pressed(egui::Key::Z),
                    (i.modifiers.command && i.modifiers.shift && i.key_pressed(egui::Key::Z))
                        || (i.modifiers.command && i.key_pressed(egui::Key::Y)),
                    i.modifiers.command && i.key_pressed(egui::Key::D),
                )
            });
            let sel: Vec<NodeKey> = self.shader_graph.sel.iter().cloned().collect();
            if del && self.shader_graph.field_buf.is_none() && !sel.is_empty() {
                acts.push(Act::Delete(sel.clone()));
            }
            if dup && !sel.is_empty() {
                acts.push(Act::Duplicate(sel));
            }
            if undo {
                acts.push(Act::Undo);
            }
            if redo {
                acts.push(Act::Redo);
            }
        }
        // First-visit hint + toast overlay, bottom of the canvas.
        if view.len() <= 1 {
            ui.painter().text(
                bg.rect.center(),
                Align2::CENTER_CENTER,
                "right-click to add nodes",
                FontId::proportional(14.0),
                ui.visuals().weak_text_color(),
            );
        }
        if let Some((msg, _)) = &self.shader_graph.status {
            let at = bg.rect.left_bottom() + egui::vec2(12.0, -20.0);
            ui.painter().text(
                at,
                Align2::LEFT_BOTTOM,
                msg,
                FontId::proportional(13.0),
                Color32::from_rgb(235, 170, 90),
            );
        }
    }

    /// One node: frame, header (drag/select/rename/menu), port rows with
    /// inline editors, port dots (wire drags), body extras per kind.
    #[allow(clippy::too_many_arguments)]
    fn draw_node(
        &mut self,
        ui: &mut egui::Ui,
        n: &GNode,
        r: Rect,
        ir: &ShaderIr,
        sel: &BTreeSet<NodeKey>,
        errored: bool,
        acts: &mut Vec<Act>,
        hover_in: &mut Option<Site>,
        hover_out: &mut Option<NodeKey>,
    ) {
        let id = ui.id().with(("sg-node", &n.key));
        let selected = sel.contains(&n.key);
        let toggle_mod = ui.input(|i| i.modifiers.command || i.modifiers.shift);
        // The whole node selects on click — registered FIRST so the header,
        // rows, dots and widgets all take precedence over it.
        let body_resp = ui.interact(r, id.with("bodyclick"), Sense::click());
        if body_resp.clicked() {
            if toggle_mod {
                acts.push(Act::SelectToggle(n.key.clone()));
            } else {
                acts.push(Act::Select(vec![n.key.clone()]));
            }
        }
        let p = ui.painter();
        let cat = node_color(ui, n);
        let fill = ui.visuals().widgets.inactive.bg_fill.gamma_multiply(1.05);
        p.rect_filled(r, 6.0, fill);
        let header = Rect::from_min_size(r.min, egui::vec2(r.width(), NODE_HEADER_H));
        p.rect_filled(
            header,
            CornerRadius { nw: 6, ne: 6, sw: 0, se: 0 },
            cat.gamma_multiply(0.30),
        );
        let stroke = if errored {
            Stroke::new(2.0, Color32::from_rgb(235, 90, 90))
        } else if selected {
            Stroke::new(2.0, ui.visuals().selection.stroke.color)
        } else {
            Stroke::new(1.0, cat.gamma_multiply(0.8))
        };
        p.rect_stroke(r, 6.0, stroke, StrokeKind::Inside);

        // ---- header: drag to move (the whole selection), click to select,
        // double-click to rename ----
        let hresp = ui
            .interact(header, id.with("drag"), Sense::click_and_drag())
            .on_hover_cursor(CursorIcon::Move);
        if hresp.drag_started() {
            // Dragging an unselected node re-selects just it first.
            let group: Vec<NodeKey> = if selected {
                sel.iter().cloned().collect()
            } else {
                acts.push(Act::Select(vec![n.key.clone()]));
                vec![n.key.clone()]
            };
            let starts = group
                .into_iter()
                .filter_map(|k| {
                    self.shader_graph.view.iter().find(|x| x.key == k).map(|x| (k, x.pos))
                })
                .collect();
            self.shader_graph.drag = Some(starts);
        }
        if hresp.dragged()
            && let Some(d) = &mut self.shader_graph.drag
            && d.iter().any(|(k, _)| *k == n.key)
        {
            let delta = hresp.drag_delta();
            for (_, pos) in d.iter_mut() {
                pos.0 += delta.x;
                pos.1 += delta.y;
            }
        }
        if hresp.drag_stopped()
            && let Some(d) = self.shader_graph.drag.take()
        {
            acts.push(Act::Place(d));
        }
        if hresp.clicked() {
            if toggle_mod {
                acts.push(Act::SelectToggle(n.key.clone()));
            } else {
                acts.push(Act::Select(vec![n.key.clone()]));
            }
        }
        let renamable = matches!(n.key, NodeKey::Let(_) | NodeKey::Anon(_));
        if hresp.double_clicked() && renamable {
            self.shader_graph.field_buf =
                Some((id.with("rename"), n.name.clone().unwrap_or_default()));
        }
        hresp.context_menu(|ui| {
            let multi = selected && sel.len() > 1;
            if renamable && !multi && ui.button("🖊 Rename…").clicked() {
                self.shader_graph.field_buf =
                    Some((id.with("rename"), n.name.clone().unwrap_or_default()));
                ui.close();
            }
            let targets: Vec<NodeKey> =
                if multi { sel.iter().cloned().collect() } else { vec![n.key.clone()] };
            let word = if multi { format!(" {} nodes", targets.len()) } else { String::new() };
            if ui.button(format!("⧉ Duplicate{word}")).on_hover_text("Ctrl+D").clicked() {
                acts.push(Act::Duplicate(targets.clone()));
                ui.close();
            }
            if !matches!(n.key, NodeKey::Out) && ui.button(format!("🗑 Delete{word}")).clicked() {
                acts.push(Act::Delete(targets));
                ui.close();
            }
            if previewable(n) {
                let hidden = self.shader_graph.pv_hidden.contains(&n.key);
                let label = if hidden { "👁 Show preview" } else { "👁 Hide preview" };
                if ui.button(label).clicked() {
                    if hidden {
                        self.shader_graph.pv_hidden.remove(&n.key);
                    } else {
                        self.shader_graph.pv_hidden.insert(n.key.clone());
                    }
                    ui.close();
                }
            }
        });

        // Title (or its rename editor).
        let title_rect = header.shrink2(egui::vec2(8.0, 2.0));
        let renaming = self
            .shader_graph
            .field_buf
            .as_ref()
            .is_some_and(|(fid, _)| *fid == id.with("rename"));
        if renaming {
            let key = n.key.clone();
            self.node_text_editor(ui, title_rect, id.with("rename-ui"), move |name| Act::Commit {
                edit: Box::new(move |ir| match &key {
                    NodeKey::Let(old) => graph::rename_let(ir, old, &name),
                    NodeKey::Anon(e) => {
                        let i = graph::promote_to_let(ir, *e)?;
                        let old = ir.lets[i].0.clone();
                        graph::rename_let(ir, &old, &name)
                    }
                    _ => Err("only value nodes rename".into()),
                }),
                strict: false,
            }, acts);
        } else {
            let strong = ui.visuals().strong_text_color();
            let weak = ui.visuals().weak_text_color();
            let title = n.title();
            p.text(
                title_rect.left_center(),
                Align2::LEFT_CENTER,
                &title,
                FontId::proportional(13.0),
                strong,
            );
            // Named nodes get their op as a right-aligned hint.
            if n.name.is_some() && n.name.as_deref() != Some(n.op_label().as_str()) {
                p.text(
                    title_rect.right_center(),
                    Align2::RIGHT_CENTER,
                    n.op_label(),
                    FontId::proportional(10.0),
                    weak,
                );
            }
            if let NodeKind::Op(spec) = &n.kind {
                hresp.on_hover_text(spec.doc);
            }
        }

        // ---- output port dot ----
        if !matches!(n.key, NodeKey::Out) {
            let at = Pos2::new(r.right(), r.top() + NODE_HEADER_H * 0.5);
            let dot = Rect::from_center_size(at, egui::vec2(16.0, 16.0));
            let dresp = ui
                .interact(dot, id.with("out"), Sense::click_and_drag())
                .on_hover_cursor(CursorIcon::PointingHand);
            let col = port_color(ui, n.ty, matches!(n.kind, NodeKind::Texture(_)));
            let radius = if dresp.hovered() { 6.0 } else { 4.5 };
            ui.painter().circle_filled(at, radius, col);
            if dresp.drag_started() {
                self.shader_graph.wire = Some(WireDrag::FromOut(n.key.clone()));
            }
            if dresp.hovered() {
                *hover_out = Some(n.key.clone());
            }
        }

        // ---- input port rows ----
        for (i, port) in n.inputs.iter().enumerate() {
            let at = Pos2::new(
                r.left(),
                r.top() + NODE_HEADER_H + i as f32 * NODE_ROW_H + NODE_ROW_H * 0.5,
            );
            let dot = Rect::from_center_size(at, egui::vec2(16.0, 16.0));
            let dresp = ui
                .interact(dot, id.with(("in", i)), Sense::click_and_drag())
                .on_hover_cursor(CursorIcon::PointingHand);
            let col = port_color(ui, port.ty, port.is_texture);
            if port.wired.is_some() {
                ui.painter().circle_filled(at, if dresp.hovered() { 6.0 } else { 4.5 }, col);
            } else {
                ui.painter().circle_stroke(
                    at,
                    if dresp.hovered() { 6.0 } else { 4.5 },
                    Stroke::new(1.5, col),
                );
            }
            if dresp.drag_started() {
                match (&port.wired, port.site) {
                    // Grabbing a live wire detaches it — you're now holding
                    // the source's loose end.
                    (Some(src), site) => {
                        let src = src.clone();
                        acts.push(Act::Commit {
                            edit: Box::new(move |ir| graph::disconnect(ir, site)),
                            strict: false,
                        });
                        self.shader_graph.wire = Some(WireDrag::FromOut(src));
                    }
                    (None, site) => self.shader_graph.wire = Some(WireDrag::FromIn(site)),
                }
            }
            if dresp.hovered() {
                *hover_in = Some(port.site);
            }

            // The row: label + inline editor when unwired. Salted child ui so
            // widget ids stay unique even if two nodes momentarily overlap.
            let row = Rect::from_min_size(
                Pos2::new(r.left() + 10.0, r.top() + NODE_HEADER_H + i as f32 * NODE_ROW_H),
                egui::vec2(r.width() - 18.0, NODE_ROW_H),
            );
            ui.scope_builder(UiBuilder::new().max_rect(row).id_salt(id.with(("row", i))), |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(4.0, 0.0);
                ui.horizontal_centered(|ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(&port.label)
                                .small()
                                .color(ui.visuals().text_color()),
                        )
                        .selectable(false),
                    );
                    if let Some(v) = &port.inline {
                        self.inline_editor(ui, id.with(("val", i)), port.site, v, acts);
                    }
                });
            });
        }

        // ---- per-kind body extras ----
        let extra_top = r.top() + NODE_HEADER_H + n.inputs.len() as f32 * NODE_ROW_H;
        let body_row = |k: usize| {
            Rect::from_min_size(
                Pos2::new(r.left() + 10.0, extra_top + k as f32 * NODE_ROW_H),
                egui::vec2(r.width() - 18.0, NODE_ROW_H),
            )
        };
        let salted =
            |k: usize| UiBuilder::new().max_rect(body_row(k)).id_salt(id.with(("body", k)));
        match &n.kind {
            NodeKind::Uniform(u) => {
                self.uniform_body(ui, id, [salted(0), salted(1), salted(2), salted(3)], ir, *u, acts);
            }
            NodeKind::Texture(t) => {
                if let Some(name) = ir.textures.get(*t).cloned() {
                    ui.scope_builder(salted(0), |ui| {
                        ui.horizontal_centered(|ui| {
                            ui.add(
                                egui::Label::new(egui::RichText::new("slot").small())
                                    .selectable(false),
                            );
                            let old = name.clone();
                            self.text_field(ui, id.with("texname"), &name, move |new| Act::Commit {
                                edit: Box::new(move |ir| graph::rename_texture(ir, &old, &new)),
                                strict: false,
                            }, acts);
                        });
                    });
                }
            }
            NodeKind::Swizzle(sw) => {
                let key = n.key.clone();
                ui.scope_builder(salted(0), |ui| {
                    ui.horizontal_centered(|ui| {
                        ui.add(
                            egui::Label::new(egui::RichText::new("take").small()).selectable(false),
                        );
                        self.text_field(ui, id.with("swz"), sw, move |new| Act::Commit {
                            edit: Box::new(move |ir| graph::set_swizzle(ir, &key, &new)),
                            strict: true,
                        }, acts);
                        ui.add(
                            egui::Label::new(egui::RichText::new("of x y z w").small().weak())
                                .selectable(false),
                        );
                    });
                });
            }
            NodeKind::Constant(_) => {
                // A quick type switch: number / vec2 / vec3 / vec4 / color.
                let site = n.inputs.first().map(|p| p.site);
                let cur = n.inputs.first().and_then(|p| p.inline.clone());
                ui.scope_builder(salted(0), |ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(3.0, 0.0);
                    ui.horizontal_centered(|ui| {
                        let variants: [(&str, InlineVal); 5] = [
                            ("#", InlineVal::Num(1.0)),
                            ("v2", InlineVal::Vec { ctor: ir::ExprId(0), lanes: 2, vals: [0.0; 4] }),
                            ("v3", InlineVal::Vec { ctor: ir::ExprId(0), lanes: 3, vals: [0.0; 4] }),
                            ("v4", InlineVal::Vec { ctor: ir::ExprId(0), lanes: 4, vals: [0.0; 4] }),
                            ("🎨", InlineVal::Color([1.0, 1.0, 1.0, 1.0])),
                        ];
                        for (label, proto) in variants {
                            let on = matches!(
                                (&cur, &proto),
                                (Some(InlineVal::Num(_)), InlineVal::Num(_))
                                    | (Some(InlineVal::Color(_)), InlineVal::Color(_))
                            ) || matches!(
                                (&cur, &proto),
                                (Some(InlineVal::Vec { lanes: a, .. }), InlineVal::Vec { lanes: b, .. }) if a == b
                            );
                            if ui
                                .selectable_label(on, egui::RichText::new(label).small())
                                .clicked()
                                && !on
                                && let Some(site) = site
                            {
                                acts.push(Act::Commit {
                                    edit: Box::new(move |ir| graph::set_inline(ir, site, &proto)),
                                    strict: false,
                                });
                            }
                        }
                    });
                });
            }
            _ => {}
        }

        // ---- the live preview thumbnail (what this node LOOKS like) ----
        if self.shader_preview.enabled
            && previewable(n)
            && !self.shader_graph.pv_hidden.contains(&n.key)
        {
            let side = PV_SIDE.min(r.width() - 16.0);
            let prect = Rect::from_min_size(
                Pos2::new(r.center().x - side * 0.5, r.top() + graph::node_height(n) - 2.0),
                egui::vec2(side, side),
            );
            match (self.shader_preview.tex_id, self.shader_preview.tiles.get(&n.key)) {
                (Some(tex), Some(&tile)) => {
                    ui.painter().image(tex, prect, self.shader_preview.tile_uv(tile), Color32::WHITE);
                }
                _ => {
                    // Not compiled yet (fresh node / mid-edit type break):
                    // keep the space so nothing jumps, show a quiet dash.
                    ui.painter().rect_filled(
                        prect,
                        3.0,
                        ui.visuals().extreme_bg_color.gamma_multiply(0.6),
                    );
                    ui.painter().text(
                        prect.center(),
                        Align2::CENTER_CENTER,
                        "—",
                        FontId::proportional(12.0),
                        ui.visuals().weak_text_color(),
                    );
                }
            }
            ui.painter().rect_stroke(
                prect,
                3.0,
                Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
                StrokeKind::Inside,
            );
        }
    }

    /// The uniform node's body: name, type, default value, range — the
    /// declaration IS the Inspector schema, edited right on the node.
    fn uniform_body(
        &mut self,
        ui: &mut egui::Ui,
        id: egui::Id,
        rows: [UiBuilder; 4],
        ir: &ShaderIr,
        u: usize,
        acts: &mut Vec<Act>,
    ) {
        let Some(uni) = ir.uniforms.get(u).cloned() else { return };
        let [name_row, ty_row, default_row, range_row] = rows;
        // name
        ui.scope_builder(name_row, |ui| {
            ui.horizontal_centered(|ui| {
                ui.add(egui::Label::new(egui::RichText::new("knob").small()).selectable(false));
                let old = uni.name.clone();
                self.text_field(ui, id.with("uname"), &uni.name, move |new| Act::Commit {
                    edit: Box::new(move |ir| graph::rename_uniform(ir, &old, &new)),
                    strict: false,
                }, acts);
            });
        });
        // type
        ui.scope_builder(ty_row, |ui| {
            ui.horizontal_centered(|ui| {
                let cur = if uni.is_color { "color" } else { uni.ty.flsl() };
                egui::ComboBox::from_id_salt(id.with("uty"))
                    .selected_text(cur)
                    .width(90.0)
                    .show_ui(ui, |ui| {
                        for (label, ty, is_color) in [
                            ("float", Ty::Float, false),
                            ("vec2", Ty::Vec2, false),
                            ("vec3", Ty::Vec3, false),
                            ("vec4", Ty::Vec4, false),
                            ("color", Ty::Vec4, true),
                        ] {
                            if ui.selectable_label(cur == label, label).clicked() && cur != label {
                                acts.push(Act::Commit {
                                    edit: Box::new(move |ir| {
                                        let uni = ir.uniforms.get_mut(u).ok_or("stale uniform")?;
                                        uni.ty = ty;
                                        uni.is_color = is_color;
                                        if is_color {
                                            uni.default = [1.0, 1.0, 1.0, 1.0];
                                            uni.range = None;
                                        }
                                        Ok(())
                                    }),
                                    strict: true,
                                });
                                ui.close();
                            }
                        }
                    });
            });
        });
        // default value
        ui.scope_builder(default_row, |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(3.0, 0.0);
            ui.horizontal_centered(|ui| {
                ui.add(egui::Label::new(egui::RichText::new("=").small()).selectable(false));
                if uni.is_color {
                    let mut c = uni.default;
                    if ui.color_edit_button_rgba_unmultiplied(&mut c).changed() {
                        acts.push(Act::Live(Box::new(move |ir| {
                            ir.uniforms.get_mut(u).ok_or("stale uniform")?.default = c;
                            Ok(())
                        })));
                    }
                } else {
                    for lane in 0..uni.ty.lanes() as usize {
                        let mut v = uni.default[lane];
                        if ui
                            .add(egui::DragValue::new(&mut v).speed(0.01).max_decimals(3))
                            .changed()
                        {
                            acts.push(Act::Live(Box::new(move |ir| {
                                ir.uniforms.get_mut(u).ok_or("stale uniform")?.default[lane] = v;
                                Ok(())
                            })));
                        }
                    }
                }
            });
        });
        // range (float knobs get Inspector slider bounds)
        if uni.ty == Ty::Float && !uni.is_color {
            ui.scope_builder(range_row, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(3.0, 0.0);
                ui.horizontal_centered(|ui| {
                    ui.add(
                        egui::Label::new(egui::RichText::new("range").small()).selectable(false),
                    );
                    let (mut lo, mut hi) = uni.range.unwrap_or((0.0, 1.0));
                    let mut changed = false;
                    changed |= ui.add(egui::DragValue::new(&mut lo).speed(0.01)).changed();
                    changed |= ui.add(egui::DragValue::new(&mut hi).speed(0.01)).changed();
                    if changed {
                        acts.push(Act::Live(Box::new(move |ir| {
                            ir.uniforms.get_mut(u).ok_or("stale uniform")?.range = Some((lo, hi));
                            Ok(())
                        })));
                    }
                });
            });
        }
    }

    /// The inline editor for one unwired port value.
    fn inline_editor(
        &mut self,
        ui: &mut egui::Ui,
        id: egui::Id,
        site: Site,
        v: &InlineVal,
        acts: &mut Vec<Act>,
    ) {
        match v {
            InlineVal::Num(n) => {
                let mut x = *n;
                if ui
                    .add(egui::DragValue::new(&mut x).speed(0.01).max_decimals(4))
                    .changed()
                {
                    acts.push(Act::Live(Box::new(move |ir| {
                        graph::set_inline(ir, site, &InlineVal::Num(x))
                    })));
                }
            }
            InlineVal::Vec { ctor, lanes, vals } => {
                let ctor = *ctor;
                for (lane, val) in vals.iter().enumerate().take(*lanes as usize) {
                    let mut x = *val;
                    if ui
                        .add(egui::DragValue::new(&mut x).speed(0.01).max_decimals(3))
                        .changed()
                    {
                        acts.push(Act::Live(Box::new(move |ir| {
                            graph::set_vec_component(ir, ctor, lane, x)
                        })));
                    }
                }
            }
            InlineVal::Color(c) => {
                let mut c = *c;
                if ui.color_edit_button_rgba_unmultiplied(&mut c).changed() {
                    acts.push(Act::Live(Box::new(move |ir| {
                        graph::set_inline(ir, site, &InlineVal::Color(c))
                    })));
                }
            }
            InlineVal::Str(s) => {
                egui::ComboBox::from_id_salt(id)
                    .selected_text(s.as_str())
                    .width(84.0)
                    .show_ui(ui, |ui| {
                        for name in stdlib::PALETTE_NAMES {
                            if ui.selectable_label(s == name, *name).clicked() {
                                let name = name.to_string();
                                acts.push(Act::Commit {
                                    edit: Box::new(move |ir| {
                                        graph::set_inline(ir, site, &InlineVal::Str(name))
                                    }),
                                    strict: false,
                                });
                                ui.close();
                            }
                        }
                    });
            }
            InlineVal::Default(d) => {
                let d = *d;
                if ui
                    .add(
                        egui::Label::new(egui::RichText::new(format!("{d}")).small().weak())
                            .selectable(false)
                            .sense(Sense::click()),
                    )
                    .on_hover_text("using the default — click to override")
                    .clicked()
                {
                    acts.push(Act::Commit {
                        edit: Box::new(move |ir| graph::set_inline(ir, site, &InlineVal::Num(d))),
                        strict: false,
                    });
                }
            }
            InlineVal::Missing => {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new("—")
                            .small()
                            .color(Color32::from_rgb(235, 120, 100)),
                    )
                    .selectable(false),
                )
                .on_hover_text("nothing wired in yet");
            }
        }
    }

    /// A one-line text editor whose live buffer survives frames (in
    /// `field_buf`), committing on Enter / focus loss.
    fn text_field(
        &mut self,
        ui: &mut egui::Ui,
        id: egui::Id,
        current: &str,
        commit: impl FnOnce(String) -> Act,
        acts: &mut Vec<Act>,
    ) {
        let editing = self.shader_graph.field_buf.as_ref().is_some_and(|(f, _)| *f == id);
        if !editing {
            let resp = ui
                .add(
                    egui::Label::new(egui::RichText::new(current).small())
                        .selectable(false)
                        .sense(Sense::click()),
                )
                .on_hover_cursor(CursorIcon::Text)
                .on_hover_text("click to edit");
            if resp.clicked() {
                self.shader_graph.field_buf = Some((id, current.to_string()));
            }
            return;
        }
        let (_, buf) = self.shader_graph.field_buf.as_mut().unwrap();
        let resp = ui.add(
            egui::TextEdit::singleline(buf)
                .desired_width(84.0)
                .font(egui::TextStyle::Small),
        );
        if !resp.has_focus() && !resp.lost_focus() {
            resp.request_focus();
        }
        if resp.lost_focus() {
            let (_, buf) = self.shader_graph.field_buf.take().unwrap();
            let cancel = ui.input(|i| i.key_pressed(egui::Key::Escape));
            if !cancel && !buf.trim().is_empty() && buf.trim() != current {
                acts.push(commit(buf.trim().to_string()));
            }
        }
    }

    /// The header-rename variant of [`Self::text_field`] (fills a rect).
    fn node_text_editor(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        salt: egui::Id,
        commit: impl FnOnce(String) -> Act,
        acts: &mut Vec<Act>,
    ) {
        ui.scope_builder(UiBuilder::new().max_rect(rect).id_salt(salt), |ui| {
            let Some((_, buf)) = self.shader_graph.field_buf.as_mut() else { return };
            let resp = ui.add(egui::TextEdit::singleline(buf).desired_width(rect.width()));
            if !resp.has_focus() && !resp.lost_focus() {
                resp.request_focus();
            }
            if resp.lost_focus() {
                let (_, buf) = self.shader_graph.field_buf.take().unwrap();
                let cancel = ui.input(|i| i.key_pressed(egui::Key::Escape));
                if !cancel && !buf.trim().is_empty() {
                    acts.push(commit(buf.trim().to_string()));
                }
            }
        });
    }
}

/// The right-click palette: search + everything addable, grouped the way a
/// beginner scans (values → inputs → operators → stdlib categories).
fn palette_ui(
    ui: &mut egui::Ui,
    stage: Stage,
    search: &mut String,
    at: (f32, f32),
) -> Option<Act> {
    let mut out: Option<Act> = None;
    ui.set_min_width(240.0);
    let resp = ui.add(egui::TextEdit::singleline(search).hint_text("🔍 add node…"));
    if ui.memory(|m| m.focused().is_none()) {
        resp.request_focus();
    }
    ui.separator();
    let q = search.to_lowercase();
    let hit = |name: &str, doc: &str| {
        q.is_empty() || name.to_lowercase().contains(&q) || doc.to_lowercase().contains(&q)
    };
    let mut entry = |ui: &mut egui::Ui, label: String, doc: &str, act: Act| {
        if ui.button(label).on_hover_text(doc).clicked() {
            out = Some(act);
        }
    };

    egui::ScrollArea::vertical().max_height(380.0).min_scrolled_height(380.0).show(ui, |ui| {
        // ---- values ----
        type AddFn = fn(&mut ShaderIr, (f32, f32)) -> Result<NodeKey, String>;
        let values: [(&str, &str, AddFn); 8] = [
            ("knob", "a number the Inspector exposes as a slider on every material using this shader", |ir, at| graph::add_uniform_node(ir, false, at)),
            ("color knob", "a color picker the Inspector exposes", |ir, at| graph::add_uniform_node(ir, true, at)),
            ("texture slot", "a texture the material binds (drag an image onto it in the Inspector)", graph::add_texture_node),
            ("constant", "a fixed number/vector/color", graph::add_constant_node),
            ("combine 2", "build a vec2 from parts", |ir, at| graph::add_vec_node(ir, 2, at)),
            ("combine 3", "build a vec3 from parts", |ir, at| graph::add_vec_node(ir, 3, at)),
            ("combine 4", "build a vec4 from parts", |ir, at| graph::add_vec_node(ir, 4, at)),
            ("split", "take components out of a vector (.x, .rgb…)", graph::add_swizzle_node),
        ];
        if values.iter().any(|(n, d, _)| hit(n, d)) {
            ui.add(egui::Label::new(egui::RichText::new("values").small().weak()).selectable(false));
            for (name, doc, f) in values {
                if hit(name, doc) {
                    entry(ui, name.to_string(), doc, Act::Commit {
                        edit: Box::new(move |ir| f(ir, at).map(|_| ())),
                        strict: false,
                    });
                }
            }
        }
        // ---- inputs ----
        let ins: Vec<Input> = Input::all().iter().copied().filter(|i| i.in_stage(stage)).collect();
        if ins.iter().any(|i| hit(i.name(), "input")) {
            ui.add(egui::Label::new(egui::RichText::new("inputs").small().weak()).selectable(false));
            for i in ins {
                if hit(i.name(), "input") {
                    entry(ui, i.name().to_string(), "a built-in value from the engine", Act::Commit {
                        edit: Box::new(move |ir| graph::add_input_node(ir, i, at).map(|_| ())),
                        strict: false,
                    });
                }
            }
        }
        // ---- operators ----
        let ops: [(&str, BinOp); 4] = [
            ("add  +", BinOp::Add),
            ("subtract  −", BinOp::Sub),
            ("multiply  ×", BinOp::Mul),
            ("divide  ÷", BinOp::Div),
        ];
        if ops.iter().any(|(n, _)| hit(n, "operator math")) {
            ui.add(egui::Label::new(egui::RichText::new("operators").small().weak()).selectable(false));
            for (name, op) in ops {
                if hit(name, "operator math") {
                    entry(ui, name.to_string(), "combine two values", Act::Commit {
                        edit: Box::new(move |ir| graph::add_binary_node(ir, op, at).map(|_| ())),
                        strict: false,
                    });
                }
            }
        }
        // ---- the stdlib, by category ----
        for cat in ["math", "noise", "color", "texture", "sdf", "engine"] {
            let ops: Vec<&'static stdlib::OpSpec> = stdlib::OPS
                .iter()
                .filter(|o| o.category == cat && o.stages.contains(&stage) && hit(o.name, o.doc))
                .collect();
            if ops.is_empty() {
                continue;
            }
            ui.add(egui::Label::new(egui::RichText::new(cat).small().weak()).selectable(false));
            for op in ops {
                entry(ui, op.name.to_string(), op.doc, Act::Commit {
                    edit: Box::new(move |ir| graph::add_op_node(ir, op, at).map(|_| ())),
                    strict: false,
                });
            }
        }
    });
    out
}

/// Name-FREE stable identities: each node keyed by its wire path from the
/// output sink (port indices + op labels). Renaming/promoting a `let` leaves
/// these untouched, so its downstream nodes keep their cached positions.
fn sink_path_keys(view: &[GNode]) -> HashMap<NodeKey, String> {
    let index_of: HashMap<&NodeKey, usize> =
        view.iter().enumerate().map(|(i, n)| (&n.key, i)).collect();
    let mut out: HashMap<NodeKey, String> = HashMap::new();
    // Breadth-first from the sink; the first path to reach a node names it.
    let mut queue: Vec<(usize, String)> = view
        .iter()
        .enumerate()
        .filter(|(_, n)| matches!(n.key, NodeKey::Out))
        .map(|(i, _)| (i, "p".to_string()))
        .collect();
    let mut guard = 0;
    while let Some((i, key)) = queue.pop() {
        guard += 1;
        if guard > 4096 {
            break;
        }
        let n = &view[i];
        if out.contains_key(&n.key) {
            continue;
        }
        out.insert(n.key.clone(), key.clone());
        for (pi, port) in n.inputs.iter().enumerate() {
            if let Some(src) = &port.wired
                && let Some(&si) = index_of.get(src)
                && !out.contains_key(src)
            {
                queue.push((si, format!("{key}/{pi}.{}", view[si].op_label())));
            }
        }
    }
    out
}

/// A wire: a horizontal-tangent cubic from an output to an input.
fn draw_wire(p: &egui::Painter, a: Pos2, b: Pos2, color: Color32, width: f32) {
    let dx = ((b.x - a.x).abs() * 0.5).clamp(24.0, 120.0);
    p.add(egui::epaint::CubicBezierShape::from_points_stroke(
        [a, Pos2::new(a.x + dx, a.y), Pos2::new(b.x - dx, b.y), b],
        false,
        Color32::TRANSPARENT,
        Stroke::new(width, color),
    ));
}

/// Port dot color by value type (the legend beginners learn by osmosis).
fn port_color(ui: &egui::Ui, ty: Option<Ty>, is_texture: bool) -> Color32 {
    if is_texture {
        return Color32::from_rgb(190, 140, 255);
    }
    match ty {
        Some(Ty::Float) => Color32::from_rgb(150, 170, 190),
        Some(Ty::Vec2) => Color32::from_rgb(120, 200, 120),
        Some(Ty::Vec3) => Color32::from_rgb(230, 200, 90),
        Some(Ty::Vec4) => Color32::from_rgb(230, 140, 200),
        None => ui.visuals().weak_text_color(),
    }
}

/// Header tint per node kind/category.
fn node_color(ui: &egui::Ui, n: &GNode) -> Color32 {
    match &n.kind {
        NodeKind::Op(op) => match op.category {
            "math" => Color32::from_rgb(120, 140, 190),
            "noise" => Color32::from_rgb(80, 180, 170),
            "color" => Color32::from_rgb(220, 140, 70),
            "texture" => Color32::from_rgb(190, 140, 255),
            "sdf" => Color32::from_rgb(120, 190, 110),
            "engine" => Color32::from_rgb(220, 190, 90),
            _ => ui.visuals().weak_text_color(),
        },
        NodeKind::VecCtor(_) | NodeKind::Binary(_) | NodeKind::Neg | NodeKind::Swizzle(_) => {
            Color32::from_rgb(120, 140, 190)
        }
        NodeKind::Constant(_) => Color32::from_rgb(150, 150, 160),
        NodeKind::Input(_) => Color32::from_rgb(90, 190, 220),
        NodeKind::Uniform(_) => Color32::from_rgb(230, 120, 160),
        NodeKind::Texture(_) => Color32::from_rgb(190, 140, 255),
        NodeKind::Out => Color32::from_rgb(235, 110, 90),
    }
}
