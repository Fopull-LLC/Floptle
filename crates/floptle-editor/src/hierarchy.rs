//! The Hierarchy dock tab: the scene tree (drag to re-parent, right-click
//! menus), plus the shared "New node" creation catalog used by the Hierarchy
//! header, the viewport context menu, and the menu bar.

use std::collections::HashMap;

use floptle_core::{Entity, Matter};

use crate::assets::{is_script, AssetPayload};
use crate::{EditorCmd, EditorTabViewer};

/// What a hierarchy row carries while dragged — its entity, so dropping it on
/// another row re-parents it.
#[derive(Clone)]
pub(crate) struct NodePayload(pub(crate) Entity);

/// Does this row get a disclosure triangle?
///
/// **Anything that can hide children must be able to reveal them.** This used to
/// also require the row to be an Empty — a "folder" — so a Reflection Probe
/// parented to a Plane, or a light parented to a mesh, was folded away by
/// [`fold_all_parents`] on load and then had no triangle to open it again. The
/// children were still there, still in the scene, still saved; there was simply
/// no way left to reach them, and adding another child only added to the pile.
/// A node with children IS a folder, whatever else it also is.
pub(crate) fn row_expandable(has_kids: bool, has_bones: bool) -> bool {
    has_kids || has_bones
}

/// Fold every parent in the tree, once, on the first draw after a scene load, so
/// a freshly opened scene reads as a list of top-level things rather than as
/// everything at once.
///
/// The roots' own top level stays readable: a scene whose nodes all hang off one
/// folder would otherwise open as a single collapsed row, which is the opposite
/// problem.
pub(crate) fn fold_all_parents(
    children: &HashMap<Entity, Vec<Entity>>,
    roots: &[Entity],
    collapsed: &mut std::collections::HashSet<Entity>,
) {
    collapsed.extend(children.iter().filter(|(_, k)| !k.is_empty()).map(|(p, _)| *p));
    if roots.len() == 1 {
        collapsed.remove(&roots[0]);
    }
}

// ---- dragging in a tree that is taller than the panel -----------------------
//
// Two things stopped a drag from reaching a row that was not already on screen,
// and between them they made re-parenting in a real scene a matter of luck.
//
// egui switches a `ScrollArea`'s mouse wheel OFF for as long as anything is
// being dragged — `scroll_area.rs`'s `is_hovering_outer_rect` requires
// `ctx.dragged_id().is_none()` — and it has no edge auto-scroll of its own. So
// picking a node up at the bottom of a long tree left you holding it with no
// way to move the view at all: put it down, scroll, pick it up again, repeat.
//
// The panel therefore drives both itself for the duration of a drag. The wheel
// goes through `Ui::scroll_with_delta`, which the ScrollArea drains from pass
// state without consulting that gate, and the edges scroll on their own at a
// speed that ramps up the closer to the edge the pointer is held.

/// How close to the top or bottom of the tree's viewport a drag has to come
/// before the panel starts scrolling itself, in points.
const DRAG_EDGE_BAND: f32 = 30.0;

/// How fast it scrolls with the pointer held right at the edge, in points per
/// second — ramped down to nothing at the band's inner side.
const DRAG_EDGE_SPEED: f32 = 700.0;

/// How far OUTSIDE the viewport the pointer may stray and still count as asking
/// to scroll. The gesture is "take this somewhere that is not on screen", so
/// overshooting the panel is the ordinary way to perform it rather than a reason
/// to stop; leave the neighbourhood entirely and it does stop.
const DRAG_EDGE_REACH: egui::Vec2 = egui::vec2(72.0, DRAG_EDGE_BAND * 2.0);

/// The scroll a drag hovering at `pointer` asks of a tree whose visible viewport
/// is `view`, over `dt` seconds. Positive scrolls DOWN, further into the tree;
/// zero means the pointer is nowhere near an edge.
pub(crate) fn edge_scroll(view: egui::Rect, pointer: egui::Pos2, dt: f32) -> f32 {
    if !view.expand2(DRAG_EDGE_REACH).contains(pointer) {
        return 0.0;
    }
    // 0 at the band's inner edge, 1 at the viewport's own edge and past it.
    let down = ((pointer.y - (view.bottom() - DRAG_EDGE_BAND)) / DRAG_EDGE_BAND).clamp(0.0, 1.0);
    let up = ((view.top() + DRAG_EDGE_BAND - pointer.y) / DRAG_EDGE_BAND).clamp(0.0, 1.0);
    // A subtraction rather than a branch: in a panel shorter than two bands
    // every point is inside both, and a tree that scrolled up and down at once
    // would be worse than one that did not scroll at all.
    (down - up) * DRAG_EDGE_SPEED * dt
}

/// Scroll the tree while a drag is in flight: the wheel, which egui takes away
/// for the duration, and the edges, which it never had.
///
/// Call this from INSIDE the scroll area's closure — `clip_rect` is the visible
/// viewport there, and `scroll_with_delta` writes to the pass state that the
/// enclosing `ScrollArea` drains when the closure returns.
pub(crate) fn scroll_while_dragging(ui: &egui::Ui) {
    let ctx = ui.ctx();
    if !egui::DragAndDrop::has_any_payload(ctx) {
        return;
    }
    let Some(pointer) = ctx.pointer_latest_pos() else {
        return;
    };
    let view = ui.clip_rect();

    // The wheel, taken back — and zeroed on the way through for the same reason
    // egui zeroes it: whatever sits under this panel must not scroll as well.
    if view.contains(pointer) {
        let wheel = ctx.input_mut(|i| std::mem::replace(&mut i.smooth_scroll_delta.y, 0.0));
        if wheel != 0.0 {
            ui.scroll_with_delta_animation(
                egui::vec2(0.0, wheel),
                egui::style::ScrollAnimation::none(),
            );
        }
    }

    // `stable_dt`, capped: one stalled frame must not teleport the view to the
    // far end of the tree.
    let by = edge_scroll(view, pointer, ctx.input(|i| i.stable_dt).min(1.0 / 30.0));
    if by != 0.0 {
        ui.scroll_with_delta_animation(egui::vec2(0.0, -by), egui::style::ScrollAnimation::none());
        // Nothing else is asking for frames: the pointer can be perfectly still
        // and the view still has to keep moving under it.
        ctx.request_repaint();
    }
}

/// How long a drag has to rest on a folded row before it springs open.
const SPRING_DWELL: f64 = 0.45;

/// A folded row that a drag is currently resting on.
#[derive(Clone, Copy)]
pub(crate) struct Spring {
    pub(crate) row: Entity,
    /// When the rest began.
    pub(crate) since: f64,
    /// The pass it was last renewed on. A drag that leaves every row stops
    /// renewing, so the next folded row it reaches starts its own clock instead
    /// of inheriting one that has already run out.
    pub(crate) pass: u64,
}

/// What a drag resting on folded row `row` does to the dwell timer, and whether
/// the row springs open.
///
/// **A folded row cannot be dropped into** — the children being aimed at are not
/// on screen — so holding a drag over one opens it, the way a file manager opens
/// a folder. Without this, moving a node into a collapsed subtree means putting
/// the drag down, opening the row, and picking the node up again.
pub(crate) fn spring(prev: Option<Spring>, row: Entity, now: f64, pass: u64) -> (Spring, bool) {
    let since =
        prev.filter(|p| p.row == row && p.pass + 1 >= pass).map_or(now, |p| p.since);
    (Spring { row, since, pass }, now - since >= SPRING_DWELL)
}

/// The clickable / droppable extent of a hierarchy row: from the start of its
/// label to the right-hand edge of the panel.
///
/// **The whole row is the target, not just the words.** A node called "Sun" is
/// three characters wide in a panel two hundred points across, and every click
/// and every drop aimed at it had to land on those three characters — the
/// shorter the name, the smaller the target. The indent and the disclosure
/// triangle stay out of it: the triangle keeps its own click, and a drop that
/// lands left of the label is a miss rather than a hit on the wrong row.
pub(crate) fn row_hit_rect(label: egui::Rect, panel_right: f32) -> egui::Rect {
    // `max`, so a name wider than the panel gives back its own width rather than
    // an inside-out rectangle.
    egui::Rect::from_min_max(label.min, egui::pos2(panel_right.max(label.right()), label.max.y))
}

impl EditorTabViewer<'_> {
    /// What a hierarchy drag actually moves: the whole selection when the dragged
    /// row is part of a multi-selection, else just the dragged row (same rule as
    /// the Assets panel's `move_sources`).
    fn drag_sources(&self, dragged: Entity) -> Vec<Entity> {
        if self.selection.len() > 1 && self.selection.contains(&dragged) {
            self.selection.clone()
        } else {
            vec![dragged]
        }
    }

    /// Can `sources` legally be re-parented under `target`?
    ///
    /// [`crate::Editor::reparent_many`] already filters a node dropped onto
    /// itself or onto its own descendant, and filtering it there is right — but
    /// it filters SILENTLY, and a row that lights up green and then does nothing
    /// is this engine's commonest bug shape. The row refuses in red instead.
    fn drop_target_ok(&self, sources: &[Entity], target: Entity) -> bool {
        !sources.contains(&target) && !sources.iter().any(|&s| self.is_under(target, s))
    }

    /// Is `e` somewhere under `ancestor`?
    ///
    /// The walk is bounded by the node count rather than by reaching a root: a
    /// cycle in `Parent` would otherwise hang the editor, and the Hierarchy is
    /// the one surface from which a cycle could be built.
    fn is_under(&self, e: Entity, ancestor: Entity) -> bool {
        let mut cur = e;
        for _ in 0..self.entity_names.len() {
            match self.world.get::<floptle_core::Parent>(cur).copied() {
                Some(floptle_core::Parent(p)) if p == ancestor => return true,
                Some(floptle_core::Parent(p)) => cur = p,
                None => return false,
            }
        }
        false
    }
}
impl<'a> EditorTabViewer<'a> {
    pub(crate) fn hierarchy_ui(&mut self, ui: &mut egui::Ui) {
        // Scene name + save at the top of the hierarchy. A PREFAB open on its
        // own says so, in its own colour and with its own glyph: the editing
        // surface is identical, and a save goes somewhere completely different,
        // so the one place that names what you are editing has to be unambiguous
        // about which it is (`floptle/0090`).
        ui.horizontal(|ui| {
            if self.editing_prefab {
                ui.colored_label(
                    egui::Color32::from_rgb(210, 170, 90),
                    egui::RichText::new(format!("◇ {} (prefab)", self.scene_name)).strong(),
                )
                .on_hover_text(
                    "Editing this prefab on its own. Save writes back to the prefab file.\n\
                     Open any scene in the Assets panel to go back to editing a scene.",
                );
            } else {
                ui.strong(format!("⎙ {}", self.scene_name));
            }
            let save_tip = if self.editing_prefab {
                "Save the prefab, in place (Ctrl+S)"
            } else {
                "Save scene (Ctrl+S)"
            };
            if ui.small_button("Save").on_hover_text(save_tip).clicked() {
                self.cmd.save_scene = true;
            }
            ui.label("?").on_hover_text(
                "Right-click here for New ⏵ Cube / Sphere / Folder / Terrain / Camera …\n\
                 Tools: 1 select · 2 move · 3 rotate · 4 scale · 5 sculpt · 6 rect\n\
                 F focus · Q unselect · G grid · ⏶/⏷ step selection · Del delete\n\
                 F1 play · F2 pause · Ctrl+S save · Ctrl+Z/Y undo/redo\n\
                 Viewport: LMB select · Shift+LMB multi · RMB-drag look · RMB-click menu\n\
                 Drag a row onto another to re-parent it, or below the tree to unparent.\n\
                 While dragging: the wheel still scrolls, the top and bottom edges scroll\n\
                 by themselves, and resting on a folded row opens it.",
            );
            ui.menu_button("✚ New", |ui| self.node_new_menu(ui, None));
        });

        // ---- search ----------------------------------------------------------
        //
        // The scope only bites WHILE SEARCHING, and that is deliberate. Hiding
        // switched-off nodes from the tree itself would take away the only place
        // you can switch them back on — the disease, not the cure. But a search
        // is you asking "where is the thing I am working on", and the thing you
        // are working on is not the camera you retired last week.
        ui.horizontal(|ui| {
            ui.label("🔍");
            let resp = ui.add(
                egui::TextEdit::singleline(self.hier_search)
                    .desired_width(120.0)
                    .hint_text("find a node"),
            );
            if !self.hier_search.is_empty() && ui.small_button("✖").on_hover_text("clear").clicked()
            {
                self.hier_search.clear();
            }
            resp.on_hover_text("filter the tree by name (case-insensitive)");
            let scope = *self.hier_scope;
            egui::ComboBox::from_id_salt("hier_scope")
                .width(78.0)
                .selected_text(match scope {
                    floptle_script::FindScope::Enabled => "enabled",
                    floptle_script::FindScope::All => "all",
                    floptle_script::FindScope::Disabled => "off only",
                })
                .show_ui(ui, |ui| {
                    for (s, label, tip) in [
                        (
                            floptle_script::FindScope::Enabled,
                            "enabled",
                            "skip switched-off nodes — the default, and what find() does in a \
                             script now",
                        ),
                        (floptle_script::FindScope::All, "all", "switched-off nodes too"),
                        (
                            floptle_script::FindScope::Disabled,
                            "off only",
                            "ONLY switched-off nodes — what did I retire and forget about?",
                        ),
                    ] {
                        if ui.selectable_label(scope == s, label).on_hover_text(tip).clicked() {
                            *self.hier_scope = s;
                        }
                    }
                });
        });
        ui.separator();

        // Build the parent⏵children tree from the world (owned copies, so the
        // recursive render can freely borrow `self`).
        let names: HashMap<Entity, String> = self.entity_names.iter().cloned().collect();
        let order: Vec<Entity> = self.entity_names.iter().map(|(e, _)| *e).collect();
        let mut children: HashMap<Entity, Vec<Entity>> = HashMap::new();
        let mut roots: Vec<Entity> = Vec::new();
        for &e in &order {
            match self.world.get::<floptle_core::Parent>(e).copied() {
                Some(floptle_core::Parent(p)) if names.contains_key(&p) => {
                    children.entry(p).or_default().push(e)
                }
                _ => roots.push(e),
            }
        }

        // FOLD EVERY PARENT, ONCE, on the first draw after a scene load. Done here
        // rather than at load time because this is where the parent⏵children map
        // exists — and doing it from the six places that replace the world would be
        // six chances to forget.
        if *self.hier_fold_pending {
            *self.hier_fold_pending = false;
            fold_all_parents(&children, &roots, self.collapsed);
        }

        // The flat VISIBLE row order (DFS, collapsed subtrees skipped) — the
        // range for Shift-click select, matching the Assets browser.
        let mut visible: Vec<Entity> = Vec::new();
        {
            let mut stack: Vec<Entity> = roots.iter().rev().copied().collect();
            while let Some(e) = stack.pop() {
                visible.push(e);
                if !self.collapsed.contains(&e)
                    && let Some(kids) = children.get(&e)
                {
                    stack.extend(kids.iter().rev());
                }
            }
        }

        // A search shows a FLAT list of matches, not a tree with the misses
        // pruned. Pruned-tree filtering keeps the indentation of a structure you
        // are not currently looking at, and a match nine levels down arrives at
        // the right-hand edge of the panel where its name is elided away.
        let query = self.hier_search.trim().to_ascii_lowercase();
        if !query.is_empty() {
            let scope = *self.hier_scope;
            let hits: Vec<Entity> = order
                .iter()
                .copied()
                .filter(|e| {
                    names.get(e).is_some_and(|n| n.to_ascii_lowercase().contains(&query))
                        && match scope {
                            floptle_script::FindScope::All => true,
                            floptle_script::FindScope::Enabled => {
                                !floptle_core::is_disabled(self.world, *e)
                            }
                            floptle_script::FindScope::Disabled => {
                                floptle_core::is_disabled(self.world, *e)
                            }
                        }
                })
                .collect();
            ui.small(match hits.len() {
                0 => "no matches".to_string(),
                1 => "1 match".to_string(),
                n => format!("{n} matches"),
            });
            let empty: HashMap<Entity, Vec<Entity>> = HashMap::new();
            egui::ScrollArea::vertical().show(ui, |ui| {
                scroll_while_dragging(ui);
                for e in &hits {
                    // No children map: a search result is a row, not a subtree —
                    // expanding one here would re-introduce the indentation the
                    // flat list exists to avoid.
                    self.hierarchy_node(ui, *e, &empty, &names, &hits, 0);
                }
            });
            return;
        }

        egui::ScrollArea::vertical().show(ui, |ui| {
            scroll_while_dragging(ui);
            for r in roots {
                self.hierarchy_node(ui, r, &children, &names, &visible, 0);
            }
            // Empty area below the tree: drop a node here to unparent it, or a
            // prefab asset to place an instance; right-click for the New menu
            // (create at scene root).
            let bg = ui.allocate_response(ui.available_size(), egui::Sense::click());
            if let Some(p) = bg.dnd_release_payload::<NodePayload>() {
                self.cmd.reparent = Some((self.drag_sources(p.0), None));
            }
            if let Some(p) = bg.dnd_release_payload::<AssetPayload>()
                && crate::assets::is_prefab(&p.path)
            {
                self.cmd.instantiate_prefab = Some((p.path.clone(), None));
            }
            bg.context_menu(|ui| {
                ui.menu_button("✚ New", |ui| self.node_new_menu(ui, None));
            });
        });
    }

    /// The shared "New node" menu — used by the Hierarchy header, the empty-area
    /// right-click (creates at scene root, `parent = None`), and each node's
    /// "Add child" submenu (`parent = Some(e)`).
    pub(crate) fn node_new_menu(&mut self, ui: &mut egui::Ui, parent: Option<Entity>) {
        node_new_menu(ui, self.cmd, parent);
    }
}

/// The shared node-creation menu (Hierarchy ✚ New, ✚ Add child, and the
/// menu-bar Add menu all list the same things).
///
/// The catalog itself is data — [`crate::matter_catalog::NEW_CATALOG`] — and
/// this only renders it. That split is the point: the menu used to BE the
/// catalog, a flat run of twenty `if ui.button(..)` arms in creation order, and
/// there was nothing to check because a list has no shape. Now the shape is a
/// value, the grouping is a decision written down once, and a new node type that
/// nobody filed under a heading fails the build.
pub(crate) fn node_new_menu(ui: &mut egui::Ui, cmd: &mut EditorCmd, parent: Option<Entity>) {
    use crate::matter_catalog::{NEW_CATALOG, NEW_TOP_LEVEL, NewEntry, NewNode};

    fn entry(ui: &mut egui::Ui, e: &NewEntry, cmd: &mut EditorCmd, parent: Option<Entity>) {
        let mut b = ui.button(e.label);
        if !e.hover.is_empty() {
            b = b.on_hover_text(e.hover);
        }
        if !b.clicked() {
            return;
        }
        match e.make {
            NewNode::Matter(make) => {
                let m = make();
                match parent {
                    Some(p) => cmd.add_parented = Some((m, p)),
                    None => cmd.add = Some(m),
                }
            }
            NewNode::Terrain => cmd.open_new_terrain = true,
            NewNode::Camera => cmd.add_camera = Some(parent),
            NewNode::MapShape(shape) => cmd.add_map_shape = Some(shape),
            NewNode::Ui(what) => cmd.add_ui = Some(what),
        }
        ui.close();
    }

    for e in NEW_TOP_LEVEL {
        entry(ui, e, cmd, parent);
    }
    ui.separator();
    for g in NEW_CATALOG {
        ui.menu_button(g.title, |ui| {
            for e in g.items {
                entry(ui, e, cmd, parent);
            }
        })
        .response
        .on_hover_text(g.hover);
    }
}
impl EditorTabViewer<'_> {
    /// Render one hierarchy row (indented by `depth`) + its children. The row is a
    /// drag source (drop it on another row to re-parent) and a drop target (for a
    /// dragged node or a script).
    pub(crate) fn hierarchy_node(
        &mut self,
        ui: &mut egui::Ui,
        e: Entity,
        children: &HashMap<Entity, Vec<Entity>>,
        names: &HashMap<Entity, String>,
        visible: &[Entity],
        depth: usize,
    ) {
        let name = names.get(&e).cloned().unwrap_or_default();
        let matter = self.world.get::<Matter>(e);
        let is_folder = matches!(matter, Some(Matter::Empty));
        let has_kids = children.get(&e).map(|c| !c.is_empty()).unwrap_or(false);
        // A rigged Mesh expands to reveal its bones/sub-objects as attach targets.
        let has_bones = self.bone_names.contains_key(&e);
        let expandable = row_expandable(has_kids, has_bones);
        let collapsed = self.collapsed.contains(&e);
        let icon = if is_folder {
            "🗀"
        } else if matches!(matter, Some(Matter::Camera { .. })) {
            "⌖"
        } else if matches!(matter, Some(Matter::Terrain { .. })) {
            "Δ"
        } else if matches!(matter, Some(Matter::PointLight { .. })) {
            "●"
        } else if matches!(matter, Some(Matter::GravityVolume { .. })) {
            "⬇"
        } else if matches!(matter, Some(Matter::Skybox { .. })) {
            "◎"
        } else if matches!(matter, Some(Matter::PostProcess { .. })) {
            "✨"
        } else {
            "•"
        };
        let selected = self.selection.contains(&e);

        // The row's own background, reserved BEFORE the row is laid out: egui
        // paints in call order, and by the time we know whether this row is
        // selected or hovered its text has already gone down.
        let band_slot = ui.painter().add(egui::Shape::Noop);

        // A folder with children gets a clickable disclosure triangle.
        let mut toggle = false;
        let label_rect = ui
            .horizontal(|ui| {
                ui.add_space(depth as f32 * 14.0);
                if expandable {
                    let tri = if collapsed { "⏵" } else { "⏷" };
                    let t = ui.add(
                        egui::Label::new(tri).selectable(false).sense(egui::Sense::click()),
                    );
                    if t.clicked() {
                        toggle = true;
                    }
                } else {
                    ui.add_space(12.0);
                }
                // A switched-off node reads as off at a glance, and so does everything
                // under it — otherwise the only clue that a whole subtree is inert is
                // that nothing happens.
                let off_self = self.world.get::<floptle_core::Disabled>(e).is_some();
                let off = off_self || floptle_core::is_disabled(self.world, e);
                let label = if off_self {
                    format!("{icon} {name}  (off)")
                } else {
                    format!("{icon} {name}")
                };
                let text = if selected {
                    egui::RichText::new(label).strong().color(ui.visuals().selection.stroke.color)
                } else {
                    egui::RichText::new(label)
                };
                let text = if off { text.weak().italics() } else { text };
                // The label is now purely what you read. Everything you DO to
                // the row happens on `resp` below, which spans the row's full
                // width — see [`row_hit_rect`].
                ui.add(egui::Label::new(text).selectable(false)).rect
            })
            .inner;
        let resp = ui.interact(
            row_hit_rect(label_rect, ui.max_rect().right().min(ui.clip_rect().right())),
            egui::Id::new(("hierarchy row", e)),
            egui::Sense::click_and_drag(),
        );
        // A selected row reads as a filled band rather than as four coloured
        // characters: in a tree of thirty rows the colour of a short name is not
        // where the eye goes, and a multi-selection has to be countable at a
        // glance. The hover tint shares the slot, so the two cannot disagree.
        let band = if selected {
            Some(ui.visuals().selection.bg_fill.gamma_multiply(0.55))
        } else if resp.hovered() {
            Some(ui.visuals().widgets.hovered.weak_bg_fill.gamma_multiply(0.5))
        } else {
            None
        };
        if let Some(fill) = band {
            ui.painter().set(band_slot, egui::Shape::rect_filled(resp.rect, 3.0, fill));
        }
        if toggle {
            if collapsed {
                self.collapsed.remove(&e);
            } else {
                self.collapsed.insert(e);
            }
        }
        resp.dnd_set_drag_payload(NodePayload(e));

        // Follow the selection: when the PRIMARY changes (a viewport pick, a
        // paste, a duplicate…), scroll its row into view — once, not per frame.
        if selected
            && self.selection.last() == Some(&e)
            && *self.hier_scrolled != Some(e)
        {
            resp.scroll_to_me(Some(egui::Align::Center));
            *self.hier_scrolled = Some(e);
        }

        // Highlight when a node / script / prefab is dragged over this row —
        // green for a drop that will land, red for one that cannot (see
        // [`Self::drop_target_ok`]).
        let node_over = resp.dnd_hover_payload::<NodePayload>();
        let refuses =
            node_over.as_ref().is_some_and(|p| !self.drop_target_ok(&self.drag_sources(p.0), e));
        let accepts = (node_over.is_some() && !refuses)
            || resp
                .dnd_hover_payload::<AssetPayload>()
                .is_some_and(|p| is_script(&p.path) || crate::assets::is_prefab(&p.path));
        if accepts || refuses {
            let color = if refuses {
                egui::Color32::from_rgb(214, 112, 106)
            } else {
                egui::Color32::from_rgb(120, 230, 140)
            };
            ui.painter().rect_stroke(
                resp.rect,
                3.0,
                egui::Stroke::new(2.0, color),
                egui::StrokeKind::Inside,
            );
        }

        // Spring-loaded folders — see [`spring`]. Only for a drop this row would
        // actually accept: opening a folder to refuse the drag afterwards would
        // be two wrong answers instead of one.
        if accepts && collapsed && expandable {
            let (now, pass) = (ui.input(|i| i.time), ui.ctx().cumulative_pass_nr());
            let id = egui::Id::new("hierarchy spring");
            let prev = ui.ctx().data(|d| d.get_temp::<Spring>(id));
            let (next, open) = spring(prev, e, now, pass);
            ui.ctx().data_mut(|d| d.insert_temp(id, next));
            if open {
                self.collapsed.remove(&e);
            }
            // The pointer can be perfectly still while the clock runs.
            ui.ctx().request_repaint();
        }

        // A held selection ignores the click entirely — the row still hovers
        // and still opens its context menu, it just does not become the
        // selection (`Editor::selection_locked`).
        if resp.clicked() && !self.selection_locked {
            *self.selected_asset = None;
            *self.bone_selection = None;
            // Same model as the Assets browser: plain = single, Ctrl/Cmd =
            // toggle, Shift = range from the current primary in visible order.
            let m = ui.input(|i| i.modifiers);
            if m.command || m.ctrl {
                if let Some(pos) = self.selection.iter().position(|x| *x == e) {
                    self.selection.remove(pos);
                } else {
                    self.selection.push(e);
                }
            } else if m.shift
                && let Some(&anchor) = self.selection.last()
                && let (Some(a), Some(b)) = (
                    visible.iter().position(|&x| x == anchor),
                    visible.iter().position(|&x| x == e),
                )
            {
                let (lo, hi) = (a.min(b), a.max(b));
                let mut range = visible[lo..=hi].to_vec();
                // The clicked row becomes the primary (selection order matters).
                if let Some(pos) = range.iter().position(|&x| x == e) {
                    let x = range.remove(pos);
                    range.push(x);
                }
                *self.selection = range;
            } else {
                self.selection.clear();
                self.selection.push(e);
            }
        }
        // Right-click selects the row it opens the menu for — unless the
        // selection is held, in which case the menu still opens and acts on
        // the row it names.
        if resp.secondary_clicked() && !selected && !self.selection_locked {
            self.selection.clear();
            self.selection.push(e);
        }
        resp.context_menu(|ui| {
            ui.menu_button("✚ Add child", |ui| self.node_new_menu(ui, Some(e)));
            if self.world.get::<floptle_core::Parent>(e).is_some() && ui.button("⮪ Unparent").clicked() {
                self.cmd.reparent = Some((vec![e], None));
                ui.close();
            }
            if ui
                .button("◇ Save as Prefab")
                .on_hover_text("save this node (and its children) as a reusable asset in prefabs/ — or drag it into the Assets panel")
                .clicked()
            {
                let roots = if self.selection.len() > 1 { self.selection.clone() } else { vec![e] };
                self.cmd.save_prefab = Some((roots, self.project_root.join("prefabs")));
                ui.close();
            }
            ui.separator();
            // The target state is decided from THIS row and applied to the whole
            // selection, so a mixed selection ends up uniform rather than inverted
            // node by node.
            let targets: Vec<Entity> =
                if self.selection.contains(&e) && self.selection.len() > 1 {
                    self.selection.clone()
                } else {
                    vec![e]
                };
            let off = self.world.get::<floptle_core::Disabled>(e).is_some();
            let label = if off { "◉ Enable" } else { "◎ Disable" };
            if ui
                .button(label)
                .on_hover_text(
                    "a disabled node doesn't draw, doesn't collide and its scripts                      don't run — and neither do anything under it",
                )
                .clicked()
            {
                self.cmd.set_enabled = Some((targets, off));
                ui.close();
            }
            if ui.button("Duplicate  (Ctrl+D)").clicked() {
                self.cmd.duplicate = true;
                ui.close();
            }
            if ui.button("Copy  (Ctrl+C)").clicked() {
                self.cmd.copy = true;
                ui.close();
            }
            if ui.button("Paste  (Ctrl+V)").clicked() {
                self.cmd.paste = true;
                ui.close();
            }
            if ui.button("Delete  (Del)").clicked() {
                self.cmd.delete = true;
                ui.close();
            }
        });
        // Drops: a node re-parents under me; a script attaches to me; a prefab
        // asset spawns an instance as my child.
        if let Some(p) = resp.dnd_release_payload::<NodePayload>() {
            // Dragging a node that's part of a multi-selection re-parents the
            // WHOLE selection under the drop target (a node whose ancestor is
            // also moving is filtered in reparent_many). The row already said in
            // red whether this drop lands; honouring the same rule here is what
            // makes the red mean something.
            let sources = self.drag_sources(p.0);
            if self.drop_target_ok(&sources, e) {
                self.cmd.reparent = Some((sources, Some(e)));
            }
        }
        if let Some(p) = resp.dnd_release_payload::<AssetPayload>() {
            if is_script(&p.path) {
                self.cmd.drop_script_on = Some((p.path.clone(), e));
            } else if crate::assets::is_prefab(&p.path) {
                self.cmd.instantiate_prefab = Some((p.path.clone(), Some(e)));
            }
        }

        // Recurse into children unless this folder is collapsed.
        if !self.collapsed.contains(&e)
            && let Some(kids) = children.get(&e) {
                for &c in kids {
                    self.hierarchy_node(ui, c, children, names, visible, depth + 1);
                }
            }

        // A model's structure — its objects (mesh sub-objects) and bones (rig joints)
        // — shown as a read-only tree (indented by skeleton depth). Select a node to
        // pose/keyframe it in the Inspector, or (for a child parented to this mesh)
        // pick one in the Inspector's 🔗 Bone attachment to ride it. Objects carry ◈,
        // bones 🔗, so a mixed rig reads at a glance.
        if !self.collapsed.contains(&e)
            && let Some(bones) = self.bone_names.get(&e)
        {
            let mut bdepth = vec![0usize; bones.len()];
            for (i, n) in bones.iter().enumerate() {
                bdepth[i] = n.parent.map_or(0, |p| bdepth.get(p).copied().unwrap_or(0) + 1);
            }
            for (i, node) in bones.iter().enumerate() {
                let sel = *self.bone_selection == Some((e, i));
                let (icon, hover) = if node.is_object {
                    ("◈", "model object (mesh sub-object) — click to select + pose/keyframe it in the Inspector")
                } else {
                    ("🔗", "rig bone — click to select + pose/keyframe it in the Inspector")
                };
                let label = format!("{icon} {}", node.name);
                // Same full-row target and selection band as a node row: a rig's
                // joint names are the shortest labels in the panel, and a tree
                // where half the rows answer across their width and half answer
                // only on their text is a tree you have to think about.
                let band_slot = ui.painter().add(egui::Shape::Noop);
                let label_rect = ui
                    .horizontal(|ui| {
                        ui.add_space((depth + 1 + bdepth[i]) as f32 * 14.0 + 12.0);
                        let text = if sel {
                            egui::RichText::new(&label)
                                .strong()
                                .color(ui.visuals().selection.stroke.color)
                        } else {
                            egui::RichText::new(&label).weak()
                        };
                        ui.add(egui::Label::new(text).selectable(false)).rect
                    })
                    .inner;
                let resp = ui
                    .interact(
                        row_hit_rect(label_rect, ui.max_rect().right().min(ui.clip_rect().right())),
                        egui::Id::new(("hierarchy rig row", e, i)),
                        egui::Sense::click(),
                    )
                    .on_hover_text(hover);
                let band = if sel {
                    Some(ui.visuals().selection.bg_fill.gamma_multiply(0.55))
                } else if resp.hovered() {
                    Some(ui.visuals().widgets.hovered.weak_bg_fill.gamma_multiply(0.5))
                } else {
                    None
                };
                if let Some(fill) = band {
                    ui.painter().set(band_slot, egui::Shape::rect_filled(resp.rect, 3.0, fill));
                }
                if resp.clicked() && !self.selection_locked {
                    // Selecting a node clears the node/asset selection so the Inspector
                    // switches to the object/bone editor (they're mutually exclusive).
                    *self.bone_selection = Some((e, i));
                    self.selection.clear();
                    *self.selected_asset = None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use floptle_core::World;
    use std::collections::HashSet;

    /// **Nothing may be hidden that cannot be un-hidden.** The fold collapses
    /// every parent on load; a parent that is not expandable has no triangle to
    /// reopen, so its children leave the panel for good — still in the scene,
    /// still saved, simply unreachable, and a newly added child joins them. That
    /// is what a Reflection Probe parented to a Plane used to do, and it read as
    /// "the children I added just vanished" rather than as a hierarchy bug.
    #[test]
    fn the_fold_never_hides_a_row_it_cannot_reopen() {
        let mut w = World::new();
        let plane = w.spawn(); // a MESH with a child — the shape that broke
        let probe = w.spawn();
        let folder = w.spawn(); // an Empty with a child — the shape that worked
        let inner = w.spawn();
        let lone = w.spawn(); // no children at all

        let mut children: HashMap<Entity, Vec<Entity>> = HashMap::new();
        children.insert(plane, vec![probe]);
        children.insert(folder, vec![inner]);
        let roots = vec![plane, folder, lone];

        let mut collapsed: HashSet<Entity> = HashSet::new();
        fold_all_parents(&children, &roots, &mut collapsed);

        for e in &collapsed {
            let has_kids = children.get(e).is_some_and(|k| !k.is_empty());
            assert!(
                row_expandable(has_kids, false),
                "the fold hid a row with no way to reopen it — its subtree is unreachable"
            );
        }
        assert!(collapsed.contains(&plane), "a non-folder parent must fold like any other");
        assert!(!collapsed.contains(&lone), "a childless row is never folded");
    }

    /// A miniature Hierarchy — a scroll area 200 points tall over eighty rows —
    /// driven headlessly through a real `egui::Context`.
    ///
    /// The arithmetic guards below check [`edge_scroll`] on its own. This checks
    /// the thing that was actually broken: that a `ScrollArea` MOVES, under the
    /// conditions egui puts it in while a drag is in flight. It is the only way
    /// to catch the panel silently going back to not scrolling, because that is
    /// a change in egui's behaviour, not in ours — and there is no arithmetic
    /// error to notice when it happens.
    struct Panel {
        ctx: egui::Context,
        offset: f32,
    }

    impl Panel {
        fn new() -> Self {
            Self { ctx: egui::Context::default(), offset: 0.0 }
        }

        /// One pass. `dragging` reproduces what egui does mid-drag: a payload in
        /// flight AND a `dragged_id`, which is the half that switches the scroll
        /// area's own wheel handling off.
        fn pass(&mut self, events: Vec<egui::Event>, dragging: bool) {
            let offset = std::cell::Cell::new(self.offset);
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(300.0, 400.0),
                )),
                events,
                ..Default::default()
            };
            let _ = self.ctx.run_ui(input, |ui| {
                if dragging {
                    egui::DragAndDrop::set_payload(ui.ctx(), 1u8);
                    ui.ctx().set_dragged_id(egui::Id::new("a dragged row"));
                }
                let out = egui::ScrollArea::vertical().id_salt("probe").max_height(200.0).show(
                    ui,
                    |ui| {
                        scroll_while_dragging(ui);
                        for i in 0..80 {
                            ui.label(format!("row {i}"));
                        }
                    },
                );
                offset.set(out.state.offset.y);
            });
            self.offset = offset.get();
        }

        /// Settle: the scroll lands through the area's own target animation, so
        /// the answer is one pass behind the ask.
        fn settle(&mut self, at: egui::Pos2, dragging: bool) {
            for _ in 0..3 {
                self.pass(vec![egui::Event::PointerMoved(at)], dragging);
            }
        }
    }

    fn wheel(dy: f32) -> egui::Event {
        egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, dy),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::default(),
        }
    }

    /// **The reported bug.** egui switches a scroll area's mouse wheel off for
    /// as long as anything is being dragged, so a node picked up in a long tree
    /// could not be carried to a row that was off screen. The wheel has to keep
    /// working mid-drag.
    #[test]
    fn the_wheel_still_scrolls_the_tree_while_a_node_is_being_dragged() {
        let mut p = Panel::new();
        p.settle(egui::pos2(150.0, 100.0), true);
        let before = p.offset;
        for _ in 0..3 {
            p.pass(vec![egui::Event::PointerMoved(egui::pos2(150.0, 100.0)), wheel(-60.0)], true);
        }
        assert!(
            p.offset > before + 1.0,
            "the wheel did nothing mid-drag: {before} -> {}",
            p.offset
        );
    }

    /// **The other half.** Nothing near an edge, nothing on the wheel: the view
    /// must sit perfectly still. A panel that drifted while you held a node over
    /// the row you were aiming at would be worse than one that never scrolled.
    #[test]
    fn a_drag_held_in_the_middle_leaves_the_tree_alone() {
        let mut p = Panel::new();
        p.settle(egui::pos2(150.0, 100.0), true);
        let before = p.offset;
        p.settle(egui::pos2(150.0, 100.0), true);
        assert_eq!(p.offset, before, "the view drifted under a stationary drag");
    }

    /// A drag carried to the bottom edge scrolls the tree down, and one carried
    /// to the top scrolls it back up.
    #[test]
    fn a_drag_at_an_edge_scrolls_the_tree_that_way() {
        let mut p = Panel::new();
        p.settle(egui::pos2(150.0, 195.0), true);
        let down = p.offset;
        assert!(down > 0.0, "a drag at the bottom edge did not scroll down: {down}");

        p.settle(egui::pos2(150.0, 5.0), true);
        assert!(p.offset < down, "a drag at the top edge did not scroll back up: {}", p.offset);
    }

    /// The edge scroll belongs to the DRAG. A pointer resting at the bottom of
    /// the panel with nothing in hand is just a pointer, and the tree under it
    /// must not run away.
    #[test]
    fn an_idle_pointer_at_the_edge_does_not_scroll_anything() {
        let mut p = Panel::new();
        p.settle(egui::pos2(150.0, 195.0), false);
        assert_eq!(p.offset, 0.0, "the tree scrolled with nothing being dragged");
    }

    /// A view 200 points tall, its top-left at the origin — the shape of a
    /// Hierarchy panel, for the edge-scroll guards.
    fn view() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(200.0, 200.0))
    }

    /// **A drag in the middle of the tree must not move the view.** The whole
    /// point of the edge band is that it is an edge: a panel that crept while
    /// you held a node over the row you were aiming at would be unusable in a
    /// way the old no-scroll-at-all behaviour at least was not.
    #[test]
    fn the_middle_of_the_tree_does_not_scroll() {
        assert_eq!(edge_scroll(view(), egui::pos2(100.0, 100.0), 1.0 / 60.0), 0.0);
    }

    /// Down at the bottom, up at the top, and harder the closer to the edge.
    #[test]
    fn the_edges_scroll_towards_themselves_and_ramp() {
        let dt = 1.0 / 60.0;
        let v = view();
        let near = edge_scroll(v, egui::pos2(100.0, v.bottom() - DRAG_EDGE_BAND * 0.5), dt);
        let at = edge_scroll(v, egui::pos2(100.0, v.bottom()), dt);
        assert!(near > 0.0, "the bottom edge must scroll DOWN, got {near}");
        assert!(at > near, "closer to the edge must be faster: {at} !> {near}");
        assert!((at - DRAG_EDGE_SPEED * dt).abs() < 1e-3, "the edge itself is full speed: {at}");

        let up = edge_scroll(v, egui::pos2(100.0, v.top() + DRAG_EDGE_BAND * 0.5), dt);
        assert!(up < 0.0, "the top edge must scroll UP, got {up}");
    }

    /// **Overshooting the panel is how the gesture is performed**, so the band
    /// reaches past the edge — but only so far. Let go of that and a drag parked
    /// over the Inspector would scroll the tree off its end.
    #[test]
    fn the_band_reaches_past_the_edge_but_not_forever() {
        let dt = 1.0 / 60.0;
        let v = view();
        let just_out = edge_scroll(v, egui::pos2(100.0, v.bottom() + DRAG_EDGE_BAND), dt);
        assert!(just_out > 0.0, "a drag held just past the edge still asks to scroll");
        let far = edge_scroll(v, egui::pos2(100.0, v.bottom() + DRAG_EDGE_REACH.y + 1.0), dt);
        assert_eq!(far, 0.0, "a drag that has left the panel must not keep scrolling it");
        let aside = edge_scroll(v, egui::pos2(v.right() + DRAG_EDGE_REACH.x + 1.0, v.bottom()), dt);
        assert_eq!(aside, 0.0, "sideways counts too");
    }

    /// A panel shorter than two bands is inside both of them at once. It must
    /// settle on one answer rather than fighting itself.
    #[test]
    fn a_panel_shorter_than_two_bands_does_not_scroll_both_ways() {
        let tiny = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(200.0, 20.0));
        let mid = edge_scroll(tiny, egui::pos2(100.0, 10.0), 1.0 / 60.0);
        assert_eq!(mid, 0.0, "dead centre of a tiny panel is a tie, not a jitter");
        assert!(edge_scroll(tiny, egui::pos2(100.0, 19.0), 1.0 / 60.0) > 0.0);
        assert!(edge_scroll(tiny, egui::pos2(100.0, 1.0), 1.0 / 60.0) < 0.0);
    }

    /// **A row is as wide as the panel, not as wide as its name.** Aiming a drop
    /// at a three-character name in a two-hundred-point panel is the thing this
    /// exists to stop.
    #[test]
    fn a_row_is_as_wide_as_the_panel() {
        let label = egui::Rect::from_min_size(egui::pos2(26.0, 40.0), egui::vec2(24.0, 16.0));
        let row = row_hit_rect(label, 200.0);
        assert_eq!(row.right(), 200.0);
        assert_eq!(row.left(), 26.0, "the indent and the triangle stay out of it");
        assert_eq!(row.top(), 40.0);
        assert_eq!(row.bottom(), 56.0);
    }

    /// A name wider than the panel must give back its own width, not an
    /// inside-out rectangle — `Rect` will happily hold one and every hit test
    /// against it then answers no.
    #[test]
    fn a_name_wider_than_the_panel_is_not_inside_out() {
        let label = egui::Rect::from_min_size(egui::pos2(26.0, 40.0), egui::vec2(400.0, 16.0));
        let row = row_hit_rect(label, 200.0);
        assert!(row.right() >= row.left(), "inside-out: {row:?}");
        assert_eq!(row.right(), label.right());
    }

    /// The dwell has to be a dwell: resting on a folded row opens it only after
    /// [`SPRING_DWELL`], and a drag that merely crosses one keeps it shut.
    #[test]
    fn a_folded_row_springs_open_only_after_the_dwell() {
        let mut w = World::new();
        let folder = w.spawn();
        let (state, open) = spring(None, folder, 100.0, 7);
        assert!(!open, "the first frame over a row must not open it");
        let (state, open) = spring(Some(state), folder, 100.0 + SPRING_DWELL * 0.5, 8);
        assert!(!open, "half the dwell is not the dwell");
        let (_, open) = spring(Some(state), folder, 100.0 + SPRING_DWELL, 9);
        assert!(open, "resting for the whole dwell opens the row");
    }

    /// **The clock belongs to the row, not to the drag.** Moving to another
    /// folded row starts that row's own dwell; so does coming back to a row the
    /// drag left, because the passes in between were not spent resting on it.
    #[test]
    fn the_dwell_restarts_when_the_drag_moves_on_or_leaves() {
        let mut w = World::new();
        let (a, b) = (w.spawn(), w.spawn());
        let (state, _) = spring(None, a, 100.0, 7);

        let (_, open) = spring(Some(state), b, 100.0 + SPRING_DWELL, 8);
        assert!(!open, "another row inherited a's clock");

        // Gone for several passes, then back on `a`: a fresh clock, not a
        // finished one.
        let (_, open) = spring(Some(state), a, 100.0 + SPRING_DWELL, 20);
        assert!(!open, "a row the drag had left kept counting while it was away");
    }

    /// An empty child list is not a parent. Left in, such a row would fold to
    /// nothing and take a disclosure triangle that opens onto an empty subtree.
    #[test]
    fn an_empty_child_list_is_not_a_parent() {
        let mut w = World::new();
        let a = w.spawn();
        let b = w.spawn();
        let mut children: HashMap<Entity, Vec<Entity>> = HashMap::new();
        children.insert(a, Vec::new());
        let mut collapsed: HashSet<Entity> = HashSet::new();
        fold_all_parents(&children, &[a, b], &mut collapsed);
        assert!(collapsed.is_empty());
    }
}
