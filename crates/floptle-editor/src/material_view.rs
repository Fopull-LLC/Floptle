//! Materials are edited in a view of their own.
//!
//! Everywhere a material is used — a node, a model's part, a map mesh's face
//! slot — the Inspector shows it as a chip: a swatch of the material and its
//! name. Clicking the chip turns the Inspector into that material's editor,
//! with a Back button to what it was showing. So the Inspector reads as a list
//! of what a node is made of rather than a wall of every material control at
//! once, and every material is edited in the same place, the same way.
//!
//! The view is also where a material is tied to the project's materials (see
//! `material_bank.rs`): follow one, make a copy of one's own, or save one for
//! the whole project to use.

use crate::EditorTabViewer;
use crate::assets_ui::paint_swatch;
use crate::material_bank::{MaterialTarget, bank_link, bank_name, target_material};
use floptle_core::{Entity, Material, Matter, ObjectMaterials};

/// The open material view, and what the Inspector was showing when it opened.
/// When the selection moves on, the view closes: a material view left over
/// from a node you are no longer looking at would edit the wrong thing.
#[derive(Clone, Debug)]
pub(crate) struct MaterialView {
    pub(crate) target: MaterialTarget,
    selection: Vec<Entity>,
    bone: Option<(Entity, usize)>,
    asset: Option<String>,
}

impl MaterialView {
    pub(crate) fn new(
        target: MaterialTarget,
        selection: &[Entity],
        bone: Option<(Entity, usize)>,
        asset: Option<String>,
    ) -> Self {
        Self { target, selection: selection.to_vec(), bone, asset }
    }

    /// The Inspector still shows what it showed when the view opened.
    pub(crate) fn still_current(
        &self,
        selection: &[Entity],
        bone: Option<(Entity, usize)>,
        asset: Option<&String>,
    ) -> bool {
        self.selection == selection && self.bone == bone && self.asset.as_ref() == asset
    }
}

/// A material shown as a clickable chip: its swatch, its name, and a line
/// saying what it is. `follows` marks a material tied to a project material.
pub(crate) fn material_chip(
    ui: &mut egui::Ui,
    m: &Material,
    tex: Option<&egui::TextureHandle>,
    title: &str,
    subtitle: &str,
    selected: bool,
) -> egui::Response {
    // As wide as the panel allows, up to a comfortable row — never wider
    // than the panel, however thin it gets.
    let width = ui.available_width().min(420.0).max(1.0);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, 40.0), egui::Sense::click());
    let p = ui.painter_at(rect);
    let v = ui.visuals();
    let narrow = rect.width() < 120.0;
    let fill = if resp.hovered() { v.widgets.hovered.bg_fill } else { v.widgets.inactive.weak_bg_fill };
    p.rect_filled(rect, 6.0, fill);
    if selected || resp.hovered() {
        let stroke = if selected { v.selection.stroke } else { v.widgets.hovered.bg_stroke };
        p.rect_stroke(rect, 6.0, stroke, egui::StrokeKind::Inside);
    }
    paint_swatch(&p, egui::pos2(rect.left() + 22.0, rect.center().y), 13.0, m.color, tex);
    let text_x = rect.left() + 44.0;
    p.text(
        egui::pos2(text_x, rect.top() + 12.0),
        egui::Align2::LEFT_CENTER,
        title,
        egui::FontId::proportional(13.0),
        v.strong_text_color(),
    );
    let sub_color = if m.source.is_some() { egui::Color32::from_rgb(245, 160, 80) } else { v.weak_text_color() };
    p.text(
        egui::pos2(text_x, rect.top() + 28.0),
        egui::Align2::LEFT_CENTER,
        subtitle,
        egui::FontId::proportional(10.5),
        sub_color,
    );
    if !narrow {
        p.text(
            egui::pos2(rect.right() - 12.0, rect.center().y),
            egui::Align2::RIGHT_CENTER,
            "▸",
            egui::FontId::proportional(14.0),
            v.weak_text_color(),
        );
    }
    resp.on_hover_text("edit this material")
}

/// The line under a chip's name: which project material it follows, or that
/// it is its own.
pub(crate) fn chip_subtitle(m: &Material) -> String {
    match m.source.as_deref().and_then(bank_name) {
        Some(n) => format!("↔ project material · {n}"),
        None => "its own material".into(),
    }
}

impl EditorTabViewer<'_> {
    /// The thumbnail of a material's base texture, when it has one.
    pub(crate) fn material_thumb(&mut self, ui: &egui::Ui, m: &Material) -> Option<egui::TextureHandle> {
        let t = m.texture.as_deref()?;
        self.asset_thumbs.get(ui.ctx(), &self.project_root.join(t))
    }

    /// A chip for the material `target` names; clicking it opens the view.
    pub(crate) fn material_target_chip(
        &mut self,
        ui: &mut egui::Ui,
        target: MaterialTarget,
        title: &str,
        selected: bool,
    ) {
        let Some(m) = target_material(self.world, &target).map(|m| m.clone()) else { return };
        let tex = self.material_thumb(ui, &m);
        let title = match m.source.as_deref().and_then(bank_name) {
            Some(n) if title.is_empty() => n.to_string(),
            _ if title.is_empty() => "Material".to_string(),
            _ => title.to_string(),
        };
        if material_chip(ui, &m, tex.as_ref(), &title, &chip_subtitle(&m), selected).clicked() {
            self.cmd.open_material = Some(target);
        }
    }

    /// The project's materials as a row of swatches; returns the one clicked.
    pub(crate) fn bank_palette(&mut self, ui: &mut egui::Ui, current: Option<&str>) -> Option<String> {
        let mut picked = None;
        let entries: Vec<(String, Material)> =
            self.materials.iter().map(|(n, d)| (n.clone(), d.to_material())).collect();
        ui.horizontal_wrapped(|ui| {
            for (name, m) in entries {
                let tex = self.material_thumb(ui, &m);
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(64.0, 58.0), egui::Sense::click());
                let p = ui.painter_at(rect);
                let on = current == Some(name.as_str());
                if on || resp.hovered() {
                    p.rect_filled(rect, 5.0, ui.visuals().widgets.hovered.bg_fill);
                }
                if on {
                    p.rect_stroke(rect, 5.0, ui.visuals().selection.stroke, egui::StrokeKind::Inside);
                }
                paint_swatch(&p, egui::pos2(rect.center().x, rect.top() + 21.0), 15.0, m.color, tex.as_ref());
                let mut job = egui::text::LayoutJob::single_section(
                    name.clone(),
                    egui::TextFormat {
                        font_id: egui::FontId::proportional(10.0),
                        color: ui.visuals().text_color(),
                        ..Default::default()
                    },
                );
                job.wrap = egui::text::TextWrapping::truncate_at_width(rect.width() - 4.0);
                job.halign = egui::Align::Center;
                let galley = ui.fonts_mut(|f| f.layout_job(job));
                p.galley(egui::pos2(rect.center().x, rect.bottom() - 15.0), galley, ui.visuals().text_color());
                if resp.on_hover_text(format!("use the project material {name}")).clicked() {
                    picked = Some(name);
                }
            }
        });
        if self.materials.is_empty() {
            ui.weak("No project materials yet. Save one from any material with \"Save as project material\".");
        }
        picked
    }

    /// The Inspector as one material's editor. Returns false when the view
    /// has closed itself (the Back button, or its material is gone).
    pub(crate) fn material_view_ui(&mut self, ui: &mut egui::Ui) -> bool {
        let Some(view) = self.material_view.clone() else { return false };
        let target = view.target.clone();
        if target_material(self.world, &target).is_none() {
            *self.material_view = None;
            return false;
        }
        let e = target.entity();
        let node_name = self
            .entity_names
            .iter()
            .find(|(x, _)| *x == e)
            .map(|(_, n)| n.clone())
            .unwrap_or_else(|| "node".into());

        let mut back = false;
        ui.horizontal(|ui| {
            back = ui.button("⬅ Back").on_hover_text("back to what the Inspector was showing").clicked();
            ui.strong("◑ Material");
        });
        ui.separator();

        // Who this material belongs to, and what it is.
        let m = target_material(self.world, &target).map(|m| m.clone()).unwrap_or_default();
        let linked = m.source.as_deref().and_then(bank_name).map(str::to_string);
        let (title, whose) = match &target {
            MaterialTarget::Node(_) => (linked.clone().unwrap_or_else(|| format!("{node_name}'s material")), format!("on {node_name}")),
            MaterialTarget::Part(_, key) => (linked.clone().unwrap_or_else(|| key.clone()), format!("{key} · {node_name}")),
        };
        let tex = self.material_thumb(ui, &m);
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(48.0, 48.0), egui::Sense::hover());
            paint_swatch(ui.painter(), rect.center(), 21.0, m.color, tex.as_ref());
            ui.vertical(|ui| {
                ui.heading(&title);
                ui.weak(whose);
            });
        });

        // ---- where the material comes from ----
        let mut link_to: Option<String> = None;
        let mut unlink = false;
        let mut save_as: Option<String> = None;
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            match &linked {
                Some(name) => {
                    ui.colored_label(egui::Color32::from_rgb(245, 160, 80), format!("↔ Project material · {name}"));
                    ui.small("Changes here are saved to the project material and restyle everything that uses it.");
                    if ui
                        .button("Make it this one's own")
                        .on_hover_text("keep the look, but stop following the project material: edits then change only this")
                        .clicked()
                    {
                        unlink = true;
                    }
                }
                None => {
                    ui.small("A material of its own — changes here affect only this.");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(self.mat_name_buf)
                                .desired_width(140.0)
                                .hint_text("name"),
                        );
                        if ui
                            .button("Save as project material")
                            .on_hover_text("add it to the project's materials, which any node, model part or map face can use, and follow it from here")
                            .clicked()
                        {
                            save_as = Some(self.mat_name_buf.trim().to_string());
                        }
                    });
                }
            }
            egui::CollapsingHeader::new("Use a project material")
                .id_salt("mat_view_bank")
                .default_open(linked.is_none() && !self.materials.is_empty())
                .show(ui, |ui| {
                    if let Some(name) = self.bank_palette(ui, linked.as_deref()) {
                        link_to = Some(name);
                    }
                });
        });

        // ---- the material itself ----
        ui.add_space(4.0);
        let sprite_cell = match (&target, self.world.get::<Matter>(e)) {
            (MaterialTarget::Node(_), Some(Matter::Sprite { cell, .. })) => Some(*cell),
            _ => None,
        };
        let mut changed_doc = None;
        let mut picked_cell = None;
        let mut remove = false;
        if let Some(mat) = target_material(self.world, &target) {
            // **On a ▫ Sprite the node owns the cell, not the material** — seed
            // the picker from the node and hand a change back to it, so the
            // one control people reach for is the one that draws.
            if let Some(c) = sprite_cell {
                mat.cell = c;
            }
            let res = crate::inspector::material_core_ui(
                ui,
                mat,
                self.asset_tree,
                self.project_root,
                self.flsl_cache,
                self.sdf_cache,
                self.texture_settings,
            );
            picked_cell = Some(mat.cell);
            self.cmd.inspector_changed |= res.changed;
            self.cmd.open_shader_graph = res.open_shader.or(self.cmd.open_shader_graph.take());
            if res.changed && linked.is_some() {
                changed_doc = Some(floptle_scene::MaterialDoc::from_material(mat));
            }
            ui.separator();
            remove = ui.button("🗑 Remove material").clicked();
        }
        if let (Some(before), Some(after)) = (sprite_cell, picked_cell)
            && before != after
            && let Some(Matter::Sprite { cell, .. }) = self.world.get_mut::<Matter>(e)
        {
            *cell = after;
            self.cmd.inspector_changed = true;
        }

        // ---- apply what was asked ----
        if let (Some(name), Some(doc)) = (&linked, changed_doc) {
            self.cmd.bank_store = Some((name.clone(), doc));
        }
        if unlink && let Some(mat) = target_material(self.world, &target) {
            mat.source = None;
            self.cmd.inspector_changed = true;
        }
        if let Some(name) = link_to
            && let Some((_, doc)) = self.materials.iter().find(|(n, _)| *n == name)
            && let Some(mat) = target_material(self.world, &target)
        {
            let mut fresh = doc.to_material();
            fresh.source = Some(bank_link(&name));
            *mat = fresh;
            self.cmd.inspector_changed = true;
        }
        if let Some(name) = save_as {
            let name = crate::material_bank::unique_name(
                if name.is_empty() { &title } else { &name },
                |n| self.materials.iter().any(|(m, _)| m == n),
            );
            if let Some(mat) = target_material(self.world, &target) {
                mat.source = Some(bank_link(&name));
                self.cmd.bank_store = Some((name, floptle_scene::MaterialDoc::from_material(mat)));
                self.cmd.inspector_changed = true;
            }
            self.mat_name_buf.clear();
        }
        if remove {
            match &target {
                MaterialTarget::Node(e) => self.cmd.remove_material = Some(*e),
                MaterialTarget::Part(e, key) => {
                    if let Some(om) = self.world.get_mut::<ObjectMaterials>(*e) {
                        om.0.remove(key);
                        if om.0.is_empty() {
                            self.world.remove::<ObjectMaterials>(*e);
                        }
                    }
                    self.cmd.inspector_changed = true;
                }
            }
            back = true;
        }
        if back {
            *self.material_view = None;
        }
        !back
    }
}
