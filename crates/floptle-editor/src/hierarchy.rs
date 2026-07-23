//! The Hierarchy dock tab: the scene tree (drag to re-parent, right-click
//! menus), plus the shared "New node" creation catalog used by the Hierarchy
//! header, the viewport context menu, and the menu bar.

use std::collections::HashMap;

use floptle_core::{Entity, Matter};
use floptle_scene::MatterDoc;

use crate::assets::{is_script, AssetPayload};
use crate::matter_catalog::{new_capsule, new_cube, new_plane, new_sphere};
use crate::{EditorCmd, EditorTabViewer};

/// What a hierarchy row carries while dragged — its entity, so dropping it on
/// another row re-parents it.
#[derive(Clone)]
pub(crate) struct NodePayload(pub(crate) Entity);
impl<'a> EditorTabViewer<'a> {
    pub(crate) fn hierarchy_ui(&mut self, ui: &mut egui::Ui) {
        // Scene name + save at the top of the hierarchy.
        ui.horizontal(|ui| {
            ui.strong(format!("⎙ {}", self.scene_name));
            if ui.small_button("Save").on_hover_text("Save scene (Ctrl+S)").clicked() {
                self.cmd.save_scene = true;
            }
            ui.label("?").on_hover_text(
                "Right-click here for New ⏵ Cube / Sphere / Folder / Terrain / Camera …\n\
                 Tools: 1 select · 2 move · 3 rotate · 4 scale · 5 sculpt · 6 rect\n\
                 F focus · Q unselect · G grid · ⏶/⏷ step selection · Del delete\n\
                 F1 play · F2 pause · Ctrl+S save · Ctrl+Z/Y undo/redo\n\
                 Viewport: LMB select · Shift+LMB multi · RMB-drag look · RMB-click menu",
            );
            ui.menu_button("✚ New", |ui| self.node_new_menu(ui, None));
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

        egui::ScrollArea::vertical().show(ui, |ui| {
            for r in roots {
                self.hierarchy_node(ui, r, &children, &names, &visible, 0);
            }
            // Empty area below the tree: drop a node here to unparent it, or a
            // prefab asset to place an instance; right-click for the New menu
            // (create at scene root).
            let bg = ui.allocate_response(ui.available_size(), egui::Sense::click());
            if let Some(p) = bg.dnd_release_payload::<NodePayload>() {
                self.cmd.reparent = Some((p.0, None));
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

/// The shared node-creation catalog (Hierarchy ✚ New, ✚ Add child, and the
/// menu-bar Add menu all list the same things).
pub(crate) fn node_new_menu(ui: &mut egui::Ui, cmd: &mut EditorCmd, parent: Option<Entity>) {
        let mut pick: Option<MatterDoc> = None;
        if ui.button("■ Cube").on_hover_text("a box primitive — the go-to building block (floors, walls, crates)").clicked() {
            pick = Some(new_cube());
            ui.close();
        }
        if ui.button("○ Sphere").on_hover_text("a sphere primitive").clicked() {
            pick = Some(new_sphere());
            ui.close();
        }
        if ui.button("▪ Capsule").on_hover_text("a capsule primitive (ideal for a physics character body)").clicked() {
            pick = Some(new_capsule());
            ui.close();
        }
        if ui.button("▭ Plane").on_hover_text("a flat double-sided quad — add a Material to texture it, drop opacity below 1 for transparency").clicked() {
            pick = Some(new_plane());
            ui.close();
        }
        if ui.button("◑ Blob").on_hover_text("an SDF metaball — nearby blobs melt together (organic/surreal shapes)").clicked() {
            pick = Some(MatterDoc::Blob { scale: 1.0 });
            ui.close();
        }
        if ui
            .button("🗀 Empty")
            .on_hover_text("a blank node — just a transform. Build it up with the Inspector's ➕ Add Component (also groups / parents children).")
            .clicked()
        {
            pick = Some(MatterDoc::Empty);
            ui.close();
        }
        ui.separator();
        if ui.button("Δ Terrain").on_hover_text("a sculptable SDF terrain node").clicked() {
            cmd.open_new_terrain = true;
            ui.close();
        }
        if ui.button("⌖ Camera").on_hover_text("a viewpoint you can give play-mode authority").clicked() {
            cmd.add_camera = Some(parent);
            ui.close();
        }
        if ui.button("● Point Light").on_hover_text("a placeable omni light (color / intensity / range)").clicked() {
            pick = Some(MatterDoc::PointLight { color: [1.0, 0.95, 0.85], intensity: 1.0, range: 10.0 });
            ui.close();
        }
        if ui.button("⬇ Gravity Volume").on_hover_text("physics gravity: Down (level) or Radial (planet)").clicked() {
            pick = Some(MatterDoc::GravityVolume { radial: false, strength: 9.81, radius: 20.0 });
            ui.close();
        }
        if ui
            .button("◈ Field Shape")
            .on_hover_text(
                "an authored SDF shape: assign an sdf-stage .flsl on its Material and the \
                 shader IS the geometry, raymarched into the scene field (up to 4 per scene)",
            )
            .clicked()
        {
            pick = Some(MatterDoc::FieldShape { radius: 1.5 });
            ui.close();
        }
        if ui.button("◎ Skybox").on_hover_text("the scene environment background (solid color or equirect texture)").clicked() {
            pick = Some(MatterDoc::from(&Matter::default_skybox()));
            ui.close();
        }
        ui.menu_button("🖼 UI", |ui| {
        for (label, what, hover) in [
            ("Layer", crate::ui_game::AddUi::Layer, "a screen-space UI canvas — elements go inside it"),
            ("Panel", crate::ui_game::AddUi::Panel, "a rounded-rect shape (radius 0 = sharp, high = pill)"),
            ("Text", crate::ui_game::AddUi::Text, "a text label (your fonts later; neutral fallback for now)"),
            ("Image", crate::ui_game::AddUi::Image, "any texture from your assets — the engine ships no UI art"),
            ("Slider", crate::ui_game::AddUi::Slider, "a value-driven bar (health, progress…): track + Fill + Handle parts you retexture and arrange freely"),
            ("Button", crate::ui_game::AddUi::Button, "a clickable element — its scripts get hoverStart/pressed/clicked hooks"),
            ("Scroll View", crate::ui_game::AddUi::Scroll, "a wheel-scrollable viewport — put more content inside than fits and it clips + scrolls"),
        ] {
            if ui.button(label).on_hover_text(hover).clicked() {
                cmd.add_ui = Some(what);
                ui.close();
            }
        }
    });
    if let Some(m) = pick {
            match parent {
                Some(p) => cmd.add_parented = Some((m, p)),
                None => cmd.add = Some(m),
            }
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
        let expandable = (is_folder && has_kids) || has_bones;
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
        } else if has_kids {
            "⏷"
        } else {
            "•"
        };
        let selected = self.selection.contains(&e);

        // A folder with children gets a clickable disclosure triangle.
        let mut toggle = false;
        let resp = ui
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
                let text = if selected {
                    egui::RichText::new(format!("{icon} {name}")).strong().color(ui.visuals().selection.stroke.color)
                } else {
                    egui::RichText::new(format!("{icon} {name}"))
                };
                ui.add(egui::Label::new(text).selectable(false).sense(egui::Sense::click_and_drag()))
            })
            .inner;
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

        // Highlight when a node / script / prefab is dragged over this row.
        if resp.dnd_hover_payload::<NodePayload>().is_some()
            || resp
                .dnd_hover_payload::<AssetPayload>()
                .is_some_and(|p| is_script(&p.path) || crate::assets::is_prefab(&p.path))
        {
            ui.painter().rect_stroke(
                resp.rect,
                3.0,
                egui::Stroke::new(2.0, egui::Color32::from_rgb(120, 230, 140)),
                egui::StrokeKind::Inside,
            );
        }

        if resp.clicked() {
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
        if resp.secondary_clicked() && !selected {
            self.selection.clear();
            self.selection.push(e);
        }
        resp.context_menu(|ui| {
            ui.menu_button("✚ Add child", |ui| self.node_new_menu(ui, Some(e)));
            if self.world.get::<floptle_core::Parent>(e).is_some() && ui.button("⮪ Unparent").clicked() {
                self.cmd.reparent = Some((e, None));
                ui.close();
            }
            if ui
                .button("⬡ Save as Prefab")
                .on_hover_text("save this node (and its children) as a reusable asset in prefabs/ — or drag it into the Assets panel")
                .clicked()
            {
                let roots = if self.selection.len() > 1 { self.selection.clone() } else { vec![e] };
                self.cmd.save_prefab = Some((roots, self.project_root.join("prefabs")));
                ui.close();
            }
            ui.separator();
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
        if let Some(p) = resp.dnd_release_payload::<NodePayload>()
            && p.0 != e {
                self.cmd.reparent = Some((p.0, Some(e)));
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
                let resp = ui
                    .horizontal(|ui| {
                        ui.add_space((depth + 1 + bdepth[i]) as f32 * 14.0 + 12.0);
                        let text = if sel {
                            egui::RichText::new(&label)
                                .strong()
                                .color(ui.visuals().selection.stroke.color)
                        } else {
                            egui::RichText::new(&label).weak()
                        };
                        ui.add(egui::Label::new(text).selectable(false).sense(egui::Sense::click()))
                            .on_hover_text(hover)
                    })
                    .inner;
                if resp.clicked() {
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
