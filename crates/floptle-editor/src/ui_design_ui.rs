//! The ◫ UI tab's egui surface: toolbar, outline panel, and the canvas with
//! its rulers, guides, snapping and direct manipulation.
//!
//! The canvas *image* is the real UI pipeline (see
//! `Editor::update_ui_design_view`); everything here is chrome drawn over it
//! plus the interaction that turns pointer gestures into ordinary component
//! edits. No edit made here does anything a hand-written `.ron` couldn't —
//! which is the property that keeps the tab optional.

use std::collections::{HashMap, HashSet};

use floptle_core::Entity;
use floptle_ui::{ElementSpec, UiState};

use crate::EditorTabViewer;
use crate::ui_design::{
    Align, Drag, RES_PRESETS, Row, SnapCfg, StyleClip, align_moves, distribute_moves,
    layer_rows, place_label, snap_delta,
};

/// Selection outline / handle colours. Deliberately the editor's own palette,
/// not the project's: chrome that borrowed the project's accent colour would
/// vanish the moment someone designed a UI in that colour.
const SEL: egui::Color32 = egui::Color32::from_rgb(255, 180, 60);
const HOT: egui::Color32 = egui::Color32::from_rgb(80, 200, 255);
const SMART: egui::Color32 = egui::Color32::from_rgb(255, 90, 200);
const GUIDE: egui::Color32 = egui::Color32::from_rgb(90, 210, 160);

impl EditorTabViewer<'_> {
    pub(crate) fn ui_design_ui(&mut self, ui: &mut egui::Ui) {
        self.ui_design.tab_visible = true;
        let layers: Vec<(Entity, String, floptle_ui::UiLayer)> = self
            .world
            .query::<floptle_ui::UiLayer>()
            .map(|(e, l)| {
                let name = self
                    .world
                    .get::<floptle_core::Name>(e)
                    .map(|n| n.0.clone())
                    .unwrap_or_else(|| format!("Layer #{}", e.index()));
                (e, name, *l)
            })
            .collect();
        if layers.is_empty() {
            self.ui_design_empty(ui);
            return;
        }
        // Which layer is on the canvas (mirrors `Editor::ui_design_layer`).
        let current = self
            .ui_design
            .layer
            .and_then(|idx| layers.iter().find(|(e, ..)| e.index() == idx))
            .unwrap_or(&layers[0]);
        let (layer_ent, _, layer) = (current.0, current.1.clone(), current.2);
        self.ui_design.layer = Some(layer_ent.index());

        self.ui_design_toolbar(ui, &layers, &layer);
        ui.separator();

        if self.ui_design.outline_panel {
            egui::Panel::left("ui_design_outline")
                .resizable(true)
                .default_size(206.0)
                .size_range(150.0..=380.0)
                .show(ui, |ui| self.ui_design_outline(ui, layer_ent));
        }
        egui::CentralPanel::default().show(ui, |ui| {
            self.ui_design_canvas(ui, layer_ent, &layer);
        });
        self.ui_design_make_style_dialog(ui);
    }

    /// What the tab says when the scene has no UI at all. A dead-end panel that
    /// only says "nothing here" wastes the one moment the user is definitely
    /// looking at it, so it offers the thing they need next.
    fn ui_design_empty(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(60.0);
            ui.heading("No UI layer in this scene");
            ui.add_space(6.0);
            ui.label("A layer is the root of one screen: it owns the design resolution and how");
            ui.label("that resolution maps to whatever window the game ends up in.");
            ui.add_space(14.0);
            if ui.button("➕  Add a UI layer").clicked() {
                self.cmd.add_ui = Some(crate::ui_game::AddUi::Layer);
            }
            ui.add_space(4.0);
            ui.small("or Add ⏵ UI ⏵ Layer from the menu bar");
        });
    }

    // -----------------------------------------------------------------------
    // Toolbar
    // -----------------------------------------------------------------------

    fn ui_design_toolbar(
        &mut self,
        ui: &mut egui::Ui,
        layers: &[(Entity, String, floptle_ui::UiLayer)],
        layer: &floptle_ui::UiLayer,
    ) {
        ui.horizontal_wrapped(|ui| {
            // ---- which layer ----
            let cur = self.ui_design.layer.unwrap_or(0);
            let cur_name = layers
                .iter()
                .find(|(e, ..)| e.index() == cur)
                .map(|(_, n, _)| n.clone())
                .unwrap_or_default();
            egui::ComboBox::from_id_salt("ui_design_layer")
                .selected_text(cur_name)
                .width(150.0)
                .show_ui(ui, |ui| {
                    for (e, name, l) in layers {
                        let label = if l.is_world() {
                            format!("{name}  (world)")
                        } else {
                            name.clone()
                        };
                        if ui.selectable_label(e.index() == cur, label).clicked() {
                            self.ui_design.layer = Some(e.index());
                            self.ui_design.want_fit = true;
                        }
                    }
                })
                .response
                .on_hover_text("which screen you're building");

            ui.separator();

            // ---- preview resolution ----
            let res = self.ui_design.res.min(RES_PRESETS.len() - 1);
            egui::ComboBox::from_id_salt("ui_design_res")
                .selected_text(RES_PRESETS[res].0)
                .width(148.0)
                .show_ui(ui, |ui| {
                    for (i, (name, _)) in RES_PRESETS.iter().enumerate() {
                        if ui.selectable_label(i == res, *name).clicked() {
                            self.ui_design.res = i;
                            self.ui_design.want_fit = true;
                        }
                    }
                })
                .response
                .on_hover_text(
                    "solve the layer at this resolution — the only way to see whether Pin and \
                     Stretch actually hold up",
                );
            if RES_PRESETS[res].0 == "Custom" {
                for a in 0..2 {
                    ui.add(
                        egui::DragValue::new(&mut self.ui_design.custom_res[a])
                            .speed(4.0)
                            .range(64.0..=8192.0),
                    );
                }
            }
            let px = self.ui_design.preview_px(layer);
            ui.small(format!("{}×{} px", px[0] as i32, px[1] as i32))
                .on_hover_text(format!(
                    "{} design units tall · scaler: {:?}",
                    layer.design_height as i32, layer.scale_mode
                ));

            ui.separator();

            // ---- zoom ----
            let z = self.ui_design.zoom;
            ui.label("🔍");
            let mut zpct = z * 100.0;
            if ui
                .add(egui::DragValue::new(&mut zpct).speed(1.0).range(5.0..=800.0).suffix("%"))
                .changed()
            {
                self.ui_design.zoom = zpct / 100.0;
            }
            if ui.small_button("1:1").on_hover_text("100% — one design unit per pixel at the reference resolution").clicked() {
                self.ui_design.zoom = 1.0;
                self.ui_design.pan = egui::Vec2::ZERO;
            }
            if ui.small_button("Fit").on_hover_text("frame the whole canvas").clicked() {
                self.ui_design.want_fit = true;
            }

            ui.separator();

            // ---- snapping ----
            ui.toggle_value(&mut self.ui_design.snap, "🧲")
                .on_hover_text("snapping (guides, sibling edges, then the grid)");
            ui.add_enabled_ui(self.ui_design.snap, |ui| {
                let step = self.ui_design.grid_step(self.ui_tokens);
                let mut grid = self.ui_design.snap_grid;
                let r = ui.add(
                    egui::DragValue::new(&mut grid)
                        .speed(1.0)
                        .range(0.0..=256.0)
                        .prefix("grid ")
                        .custom_formatter(move |v, _| {
                            if v <= 0.0 { format!("{step:.0} ⌁") } else { format!("{v:.0}") }
                        }),
                );
                if r.changed() {
                    self.ui_design.snap_grid = grid;
                }
                r.on_hover_text(
                    "design units. 0 = follow the project's smallest spacing token, so the easy \
                     drag lands on a value from your own scale",
                );
                ui.toggle_value(&mut self.ui_design.snap_guides, "┆").on_hover_text("snap to guides");
                ui.toggle_value(&mut self.ui_design.snap_siblings, "⇹")
                    .on_hover_text("snap to sibling edges and centres");
                ui.toggle_value(&mut self.ui_design.show_grid, "▦").on_hover_text("show the grid");
            });

            ui.separator();

            // ---- align / distribute ----
            let sel: Vec<u32> = self.selection.iter().map(|e| e.index()).collect();
            let has_sel = !sel.is_empty();
            ui.add_enabled_ui(has_sel, |ui| {
                for how in [
                    Align::Left,
                    Align::CenterX,
                    Align::Right,
                    Align::Top,
                    Align::CenterY,
                    Align::Bottom,
                ] {
                    let tip = if sel.len() >= 2 {
                        format!("{} (to the selection's bounds)", how.tip())
                    } else {
                        format!("{} (to the parent)", how.tip())
                    };
                    if ui.button(how.glyph()).on_hover_text(tip).clicked() {
                        self.ui_design_align(how);
                    }
                }
            });
            ui.add_enabled_ui(sel.len() >= 3, |ui| {
                if ui.button("⇿").on_hover_text("distribute horizontally (equal gaps)").clicked() {
                    self.ui_design_distribute(0);
                }
                if ui.button("⇕").on_hover_text("distribute vertically (equal gaps)").clicked() {
                    self.ui_design_distribute(1);
                }
            });

            ui.separator();

            // ---- state preview ----
            ui.label("state");
            let states: [(Option<UiState>, &str, &str); 6] = [
                (None, "live", "however the element actually is right now"),
                (Some(UiState::Hover), "hover", "force the style's hover block on the selection"),
                (Some(UiState::Pressed), "press", "force the style's pressed block"),
                (Some(UiState::Focus), "focus", "force the style's focus block"),
                (Some(UiState::Selected), "sel", "force the style's selected block"),
                (Some(UiState::Disabled), "off", "force the style's disabled block"),
            ];
            for (s, label, tip) in states {
                let on = self.ui_design.state == s;
                if ui.selectable_label(on, label).on_hover_text(tip).clicked() {
                    self.ui_design.state = if on { None } else { s };
                }
            }

            ui.separator();
            ui.toggle_value(&mut self.ui_design.outlines, "⬚").on_hover_text("element outlines");
            ui.toggle_value(&mut self.ui_design.rulers, "📏").on_hover_text("rulers and guides");
            ui.toggle_value(&mut self.ui_design.outline_panel, "☰")
                .on_hover_text("the element outline panel");
            let mut bg = self.ui_design.backdrop;
            if ui
                .color_edit_button_rgb(&mut bg)
                .on_hover_text("what's behind the layer — design against your game's actual background")
                .changed()
            {
                self.ui_design.backdrop = bg;
            }
        });
    }

    // -----------------------------------------------------------------------
    // Outline panel
    // -----------------------------------------------------------------------

    fn ui_design_outline(&mut self, ui: &mut egui::Ui, layer_ent: Entity) {
        ui.horizontal(|ui| {
            ui.strong("Elements");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.small("front ▲").on_hover_text(
                    "the top row draws in FRONT. Drag a row to change depth; inside a Stack the \
                     same drag changes the order children flow in.",
                );
            });
        });
        ui.separator();
        let rows = layer_rows(self.world, layer_ent);
        if rows.is_empty() {
            ui.small("This layer has no elements yet — Add ⏵ UI ⏵ Panel / Text / Image.");
            return;
        }
        // Front-most first: the panel reads like a stack of sheets seen from
        // above, which is the mental model everyone already has.
        let display: Vec<&Row> = rows.iter().rev().collect();
        let sel: HashSet<u32> = self.selection.iter().map(|e| e.index()).collect();
        let mut drop_target: Option<(u32, usize)> = None; // (parent, index among siblings)
        let mut dragged: Option<u32> = None;

        egui::ScrollArea::vertical().show(ui, |ui| {
            for row in display {
                let id = egui::Id::new(("ui_outline", row.id));
                let selected = sel.contains(&row.id);
                let resp = ui
                    .dnd_drag_source(id, row.id, |ui| {
                        ui.horizontal(|ui| {
                            ui.add_space(row.depth as f32 * 12.0);
                            // 👁 visibility — a real scene property.
                            let eye = if row.visible { "👁" } else { "—" };
                            if ui
                                .add(egui::Button::new(eye).frame(false).small())
                                .on_hover_text("visible in the game")
                                .clicked()
                            {
                                self.cmd.ui_set_visible.push((row.id, !row.visible));
                            }
                            // 🔒 lock — an authoring-only property; it stops the
                            // canvas picking this element and is never saved.
                            let locked = self.ui_design.locked.contains(&row.id);
                            if ui
                                .add(egui::Button::new(if locked { "🔒" } else { " " }).frame(false).small())
                                .on_hover_text("lock: ignore this element when picking on the canvas (editor only — never saved)")
                                .clicked()
                            {
                                if locked {
                                    self.ui_design.locked.remove(&row.id);
                                } else {
                                    self.ui_design.locked.insert(row.id);
                                }
                            }
                            let mut text = egui::RichText::new(&row.name);
                            if !row.visible {
                                text = text.weak();
                            }
                            if row.is_stack {
                                text = text.italics();
                            }
                            if selected {
                                text = text.color(SEL);
                            }
                            ui.label(text);
                        });
                    })
                    .response
                    .on_hover_text(format!(
                        "depth {} · {}{}",
                        row.order,
                        if row.is_stack { "arranges its children" } else { "free placement" },
                        if self.ui_design.locked.contains(&row.id) { " · locked" } else { "" },
                    ));
                if resp.clicked() {
                    let additive = ui.input(|i| i.modifiers.ctrl || i.modifiers.shift);
                    if !additive {
                        self.selection.clear();
                    }
                    if !self.selection.contains(&row.entity) {
                        self.selection.push(row.entity);
                    }
                }
                // Drop ABOVE this row (= in front of it, since we draw reversed).
                if let Some(payload) = resp.dnd_release_payload::<u32>() {
                    dragged = Some(*payload);
                    let sibs: Vec<&Row> =
                        rows.iter().filter(|r| r.parent == row.parent).collect();
                    let at = sibs.iter().position(|r| r.id == row.id).unwrap_or(0);
                    drop_target = Some((row.parent, at + 1));
                }
            }
        });

        if let (Some(moved), Some((parent, at))) = (dragged, drop_target) {
            self.ui_design_reorder(&rows, moved, parent, at);
        }
    }

    /// Renumber a sibling run so `moved` lands at index `at` among the children
    /// of `parent`.
    ///
    /// Rewriting the whole run (0, 1, 2, …) rather than nudging one value keeps
    /// the numbers meaningful: a run of ties would make the next drag depend on
    /// scene order again, which is exactly the invisible state `order` exists
    /// to replace.
    fn ui_design_reorder(&mut self, rows: &[Row], moved: u32, parent: u32, at: usize) {
        let sibs: Vec<u32> = rows.iter().filter(|r| r.parent == parent).map(|r| r.id).collect();
        self.cmd.ui_order.extend(crate::ui_design::reorder_run(&sibs, moved, at));
    }

    // -----------------------------------------------------------------------
    // Canvas
    // -----------------------------------------------------------------------

    fn ui_design_canvas(
        &mut self,
        ui: &mut egui::Ui,
        layer_ent: Entity,
        layer: &floptle_ui::UiLayer,
    ) {
        const RULER: f32 = 18.0;
        let full = ui.available_rect_before_wrap();
        let rulers = self.ui_design.rulers;
        let view = if rulers {
            egui::Rect::from_min_max(full.min + egui::vec2(RULER, RULER), full.max)
        } else {
            full
        };
        if view.width() < 8.0 || view.height() < 8.0 {
            return;
        }

        let design_vp = self.ui_design.design_vp;
        let ppp = self.ppp.max(0.1);
        // Points per design unit, from the size the canvas was actually
        // rendered at — so the chrome can never drift from the image.
        let ppd = (self.ui_design.render_scale / ppp).max(0.0001);
        let canvas_size = egui::vec2(design_vp[0] * ppd, design_vp[1] * ppd);

        // ---- framing (never automatic; see UiDesignState::want_fit) ----
        // Only once the canvas has actually rendered: `ppd` comes from the last
        // render, so fitting against the placeholder would frame the wrong size
        // and then never correct itself.
        if self.ui_design.want_fit && self.ui_design.rendered_layer.is_some() {
            self.ui_design.want_fit = false;
            let margin = 24.0;
            let fit = ((view.width() - margin) / (design_vp[0] * ppd / self.ui_design.zoom))
                .min((view.height() - margin) / (design_vp[1] * ppd / self.ui_design.zoom));
            self.ui_design.zoom = (self.ui_design.zoom * fit).clamp(0.05, 8.0);
            self.ui_design.pan = egui::Vec2::ZERO;
        }
        let origin = view.center() - canvas_size * 0.5 + self.ui_design.pan;
        let canvas = egui::Rect::from_min_size(origin, canvas_size);
        // design units ⇄ screen points
        let to_screen = |p: [f32; 2]| canvas.min + egui::vec2(p[0] * ppd, p[1] * ppd);
        let to_design = |p: egui::Pos2| [(p.x - canvas.min.x) / ppd, (p.y - canvas.min.y) / ppd];

        let resp = ui.interact(
            view,
            egui::Id::new("ui_design_canvas"),
            egui::Sense::click_and_drag(),
        );
        let painter = ui.painter_at(view);

        // ---- wheel zoom about the pointer, middle-drag pan ----
        if resp.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 {
                let before = ui.input(|i| i.pointer.hover_pos()).unwrap_or(view.center());
                let anchor = to_design(before);
                let z = (self.ui_design.zoom * (1.0 + scroll * 0.0015)).clamp(0.05, 8.0);
                // Hold the point under the cursor still: the zoom the canvas
                // renders at only updates next frame, so re-derive the pan from
                // the ratio rather than from the (stale) ppd.
                let ratio = z / self.ui_design.zoom;
                let anchor_pt = egui::vec2(anchor[0] * ppd, anchor[1] * ppd);
                self.ui_design.pan += anchor_pt - anchor_pt * ratio;
                self.ui_design.zoom = z;
            }
        }
        if ui.input(|i| i.pointer.middle_down()) && resp.hovered() {
            self.ui_design.pan += ui.input(|i| i.pointer.delta());
        }

        // ---- canvas bounds + grid ----
        painter.rect_stroke(
            canvas,
            0.0,
            egui::Stroke::new(1.0, egui::Color32::from_gray(90)),
            egui::StrokeKind::Outside,
        );
        let step = self.ui_design.grid_step(self.ui_tokens);
        if self.ui_design.show_grid && step * ppd >= 4.0 {
            let col = egui::Color32::from_white_alpha(14);
            let mut x = 0.0;
            while x <= design_vp[0] {
                let sx = canvas.min.x + x * ppd;
                painter.line_segment(
                    [egui::pos2(sx, canvas.min.y), egui::pos2(sx, canvas.max.y)],
                    egui::Stroke::new(1.0, col),
                );
                x += step;
            }
            let mut y = 0.0;
            while y <= design_vp[1] {
                let sy = canvas.min.y + y * ppd;
                painter.line_segment(
                    [egui::pos2(canvas.min.x, sy), egui::pos2(canvas.max.x, sy)],
                    egui::Stroke::new(1.0, col),
                );
                y += step;
            }
        }

        // ---- the render ----
        if let Some(tex) = self.ui_design.tex {
            egui::Image::new((tex, canvas.size())).paint_at(ui, canvas);
        }

        // The layer's REFERENCE box, when the preview resolution makes the
        // solvable area a different shape. This is the scaler made visible: the
        // area outside it is what a 21:9 monitor gives you for free (or what an
        // `Expand` layer letterboxes away), and it's the thing that makes the
        // difference between Free and Stretch obvious without reading a doc.
        let refbox = [layer.reference_width, layer.design_height];
        if (refbox[0] - design_vp[0]).abs() > 1.0 || (refbox[1] - design_vp[1]).abs() > 1.0 {
            let r = egui::Rect::from_min_size(
                canvas.min,
                egui::vec2(refbox[0] * ppd, refbox[1] * ppd),
            );
            let col = egui::Color32::from_rgba_unmultiplied(120, 160, 255, 110);
            painter.rect_stroke(r, 0.0, egui::Stroke::new(1.0, col), egui::StrokeKind::Inside);
            painter.text(
                r.right_top() + egui::vec2(-4.0, 2.0),
                egui::Align2::RIGHT_TOP,
                "reference",
                egui::FontId::proportional(10.0),
                col,
            );
        }

        // ---- solved rects ----
        let placed = self.ui_design.placed.clone();
        let rect_of: HashMap<u32, [f32; 4]> =
            placed.iter().map(|p| (p.id, p.rect)).collect();
        let rows = layer_rows(self.world, layer_ent);
        let parent_of: HashMap<u32, u32> = rows.iter().map(|r| (r.id, r.parent)).collect();
        let stack_parents: HashSet<u32> =
            rows.iter().filter(|r| r.is_stack).map(|r| r.id).collect();
        let sel: Vec<u32> = self.selection.iter().map(|e| e.index()).collect();
        let ent_of: HashMap<u32, Entity> = rows.iter().map(|r| (r.id, r.entity)).collect();

        // ---- outlines ----
        if self.ui_design.outlines {
            for p in &placed {
                if sel.contains(&p.id) {
                    continue;
                }
                let r = egui::Rect::from_min_size(
                    to_screen([p.rect[0], p.rect[1]]),
                    egui::vec2(p.rect[2] * ppd, p.rect[3] * ppd),
                );
                painter.rect_stroke(
                    r,
                    0.0,
                    egui::Stroke::new(1.0, egui::Color32::from_white_alpha(26)),
                    egui::StrokeKind::Inside,
                );
            }
        }

        // ---- rulers + guides ----
        if rulers {
            self.ui_design_rulers(ui, full, view, canvas, ppd, design_vp, RULER);
        }
        self.ui_design_guides(ui, &painter, view, canvas, ppd, design_vp, rulers);

        // ---- interaction ----
        let pointer = ui.input(|i| i.pointer.hover_pos());
        let hot = pointer
            .filter(|p| view.contains(*p))
            .and_then(|p| {
                let d = to_design(p);
                // Last in draw order = front-most, so search backwards.
                placed
                    .iter()
                    .rev()
                    .find(|pl| {
                        !self.ui_design.locked.contains(&pl.id)
                            && d[0] >= pl.rect[0]
                            && d[1] >= pl.rect[1]
                            && d[0] <= pl.rect[0] + pl.rect[2]
                            && d[1] <= pl.rect[1] + pl.rect[3]
                    })
                    .map(|pl| pl.id)
            });

        if let Some(id) = hot.filter(|id| !sel.contains(id))
            && let Some(r) = rect_of.get(&id)
        {
            {
                let rr = egui::Rect::from_min_size(
                    to_screen([r[0], r[1]]),
                    egui::vec2(r[2] * ppd, r[3] * ppd),
                );
                painter.rect_stroke(
                    rr,
                    0.0,
                    egui::Stroke::new(1.0, HOT),
                    egui::StrokeKind::Outside,
                );
            }
        }

        self.ui_design_selection_chrome(ui, &painter, &sel, &rect_of, &stack_parents, &parent_of, ppd, to_screen);

        // Guide dragging owns the pointer while it's active.
        let dragging_guide = matches!(self.ui_design.drag, Drag::Guide { .. });
        if !dragging_guide {
            self.ui_design_pointer(
                ui,
                &resp,
                &painter,
                canvas,
                ppd,
                design_vp,
                &placed,
                &rect_of,
                &parent_of,
                &stack_parents,
                &ent_of,
                &rows,
                hot,
            );
        }
        // Nudge keys are scoped to the canvas: the tab shares its window with
        // the Hierarchy and the Inspector, and arrows that moved elements while
        // you were arrowing through a list would be a trap.
        if resp.hovered() {
            self.ui_design_keys(ui, &sel, &rect_of);
        }
        self.ui_design_context_menu(&resp, &sel, &ent_of);
        self.ui_design_text_overlay(ui, &rect_of, ppd, to_screen);

        // Readout: what's selected and where, in design units.
        if let Some(id) = sel.first()
            && let Some(r) = rect_of.get(id)
        {
            let place = ent_of
                .get(id)
                .and_then(|e| self.world.get::<ElementSpec>(*e))
                .map(|s| place_label(&s.place))
                .unwrap_or("");
            let text = format!(
                "{:.0}, {:.0}   {:.0} × {:.0}   ({place})",
                r[0], r[1], r[2], r[3]
            );
            painter.text(
                view.left_bottom() + egui::vec2(6.0, -6.0),
                egui::Align2::LEFT_BOTTOM,
                text,
                egui::FontId::monospace(11.0),
                egui::Color32::from_gray(190),
            );
        }
        self.ui_design.rect = Some(view);
    }

    /// Selection outlines, resize handles, and the parent/stack cues that
    /// explain why a given element can't just be dragged anywhere.
    #[allow(clippy::too_many_arguments)]
    fn ui_design_selection_chrome(
        &mut self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        sel: &[u32],
        rect_of: &HashMap<u32, [f32; 4]>,
        stack_parents: &HashSet<u32>,
        parent_of: &HashMap<u32, u32>,
        ppd: f32,
        to_screen: impl Fn([f32; 2]) -> egui::Pos2,
    ) {
        for id in sel {
            let Some(r) = rect_of.get(id) else { continue };
            let rr = egui::Rect::from_min_size(
                to_screen([r[0], r[1]]),
                egui::vec2((r[2] * ppd).max(2.0), (r[3] * ppd).max(2.0)),
            );
            painter.rect_stroke(rr, 0.0, egui::Stroke::new(1.5, SEL), egui::StrokeKind::Outside);
            // Inside a stack the parent places this element: say so instead of
            // offering handles that would fight the layout.
            let in_stack = parent_of.get(id).is_some_and(|p| stack_parents.contains(p));
            if in_stack {
                painter.text(
                    rr.left_top() + egui::vec2(3.0, -14.0),
                    egui::Align2::LEFT_TOP,
                    "in stack — drag to re-order",
                    egui::FontId::proportional(10.0),
                    SEL.gamma_multiply(0.8),
                );
                continue;
            }
            if sel.len() == 1 {
                for (k, (pos, hx, hy)) in handles(rr).iter().enumerate() {
                    let hr = egui::Rect::from_center_size(*pos, egui::vec2(9.0, 9.0));
                    let hresp = ui.interact(
                        hr,
                        egui::Id::new(("ui_design_h", *id, k)),
                        egui::Sense::drag(),
                    );
                    painter.rect_filled(
                        hr.shrink(if hresp.hovered() || hresp.dragged() { 0.0 } else { 2.0 }),
                        1.0,
                        SEL,
                    );
                    if hresp.drag_started() {
                        self.ui_design.drag = Drag::Resize { id: *id, hx: *hx, hy: *hy };
                    }
                    if hresp.dragged()
                        && let Drag::Resize { id: did, hx, hy } = self.ui_design.drag
                        && did == *id
                    {
                        let d = hresp.drag_delta() / ppd;
                        let ds = [d.x * hx as f32, d.y * hy as f32];
                        let from_min = [hx < 0, hy < 0];
                        let cur = [r[2], r[3]];
                        match &mut self.cmd.ui_resize {
                            Some((i, acc, fm, c)) if *i == *id => {
                                acc[0] += ds[0];
                                acc[1] += ds[1];
                                *fm = from_min;
                                *c = cur;
                            }
                            slot => *slot = Some((*id, ds, from_min, cur)),
                        }
                    }
                    if hresp.drag_stopped() {
                        self.ui_design.drag = Drag::None;
                    }
                }
            }
        }
        // Multi-selection bounds, so align targets are visible before clicking.
        if sel.len() >= 2 {
            let rs: Vec<[f32; 4]> = sel.iter().filter_map(|id| rect_of.get(id).copied()).collect();
            if let Some(b) = bounds(&rs) {
                let br = egui::Rect::from_min_max(
                    to_screen([b[0], b[1]]),
                    to_screen([b[0] + b[2], b[1] + b[3]]),
                );
                painter.rect_stroke(
                    br,
                    0.0,
                    egui::Stroke::new(1.0, SEL.gamma_multiply(0.5)),
                    egui::StrokeKind::Outside,
                );
            }
        }
    }

    /// Rulers in design units, and pulling a new guide off one.
    #[allow(clippy::too_many_arguments)]
    fn ui_design_rulers(
        &mut self,
        ui: &mut egui::Ui,
        full: egui::Rect,
        view: egui::Rect,
        canvas: egui::Rect,
        ppd: f32,
        design_vp: [f32; 2],
        thickness: f32,
    ) {
        let top = egui::Rect::from_min_max(
            egui::pos2(view.min.x, full.min.y),
            egui::pos2(view.max.x, full.min.y + thickness),
        );
        let left = egui::Rect::from_min_max(
            egui::pos2(full.min.x, view.min.y),
            egui::pos2(full.min.x + thickness, view.max.y),
        );
        let p = ui.painter_at(full);
        let bg = egui::Color32::from_gray(34);
        p.rect_filled(top, 0.0, bg);
        p.rect_filled(left, 0.0, bg);
        // Tick spacing: the smallest of 1/2/5×10ⁿ design units that keeps ticks
        // at least 48 points apart, so the ruler stays readable at any zoom.
        let mut tick = 1.0f32;
        while tick * ppd < 48.0 {
            let m = [2.0, 2.5, 2.0];
            tick *= m[(tick.log10().round() as usize) % 3];
            if tick > 100000.0 {
                break;
            }
        }
        let col = egui::Color32::from_gray(150);
        let font = egui::FontId::monospace(9.0);
        let mut x = 0.0;
        while x <= design_vp[0] {
            let sx = canvas.min.x + x * ppd;
            if sx >= top.min.x && sx <= top.max.x {
                p.line_segment(
                    [egui::pos2(sx, top.max.y - 4.0), egui::pos2(sx, top.max.y)],
                    egui::Stroke::new(1.0, col),
                );
                p.text(
                    egui::pos2(sx + 2.0, top.min.y),
                    egui::Align2::LEFT_TOP,
                    format!("{x:.0}"),
                    font.clone(),
                    col,
                );
            }
            x += tick;
        }
        let mut y = 0.0;
        while y <= design_vp[1] {
            let sy = canvas.min.y + y * ppd;
            if sy >= left.min.y && sy <= left.max.y {
                p.line_segment(
                    [egui::pos2(left.max.x - 4.0, sy), egui::pos2(left.max.x, sy)],
                    egui::Stroke::new(1.0, col),
                );
                p.text(
                    egui::pos2(left.min.x + 1.0, sy + 1.0),
                    egui::Align2::LEFT_TOP,
                    format!("{y:.0}"),
                    font.clone(),
                    col,
                );
            }
            y += tick;
        }
        // Pull a guide off either ruler.
        let layer = self.ui_design.layer.unwrap_or(0);
        for (rect, vertical, salt) in
            [(top, false, "ui_ruler_top"), (left, true, "ui_ruler_left")]
        {
            let r = ui.interact(rect, egui::Id::new(salt), egui::Sense::click_and_drag());
            if r.hovered() {
                ui.ctx().set_cursor_icon(if vertical {
                    egui::CursorIcon::ResizeHorizontal
                } else {
                    egui::CursorIcon::ResizeVertical
                });
            }
            if r.drag_started() {
                // Start the guide under the pointer, so it doesn't flash at 0
                // before the first drag frame moves it.
                let p = ui.input(|i| i.pointer.hover_pos()).unwrap_or(canvas.min);
                let at = if vertical {
                    (p.x - canvas.min.x) / ppd
                } else {
                    (p.y - canvas.min.y) / ppd
                };
                let g = self.ui_design.guides.entry(layer).or_default();
                if vertical {
                    g.x.push(at);
                    self.ui_design.drag = Drag::Guide { vertical, idx: Some(g.x.len() - 1) };
                } else {
                    g.y.push(at);
                    self.ui_design.drag = Drag::Guide { vertical, idx: Some(g.y.len() - 1) };
                }
            }
        }
    }

    /// Draw and drag guides. A guide dragged back onto its ruler is deleted —
    /// the universal gesture, and it means there's nothing to learn.
    #[allow(clippy::too_many_arguments)]
    fn ui_design_guides(
        &mut self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        view: egui::Rect,
        canvas: egui::Rect,
        ppd: f32,
        design_vp: [f32; 2],
        rulers: bool,
    ) {
        let layer = self.ui_design.layer.unwrap_or(0);
        let Some(guides) = self.ui_design.guides.get(&layer).cloned() else {
            self.ui_design_guide_drag(ui, view, canvas, ppd, design_vp, rulers);
            return;
        };
        for (vertical, list) in [(true, &guides.x), (false, &guides.y)] {
            for (i, at) in list.iter().enumerate() {
                let (a, b) = if vertical {
                    let sx = canvas.min.x + at * ppd;
                    (egui::pos2(sx, view.min.y), egui::pos2(sx, view.max.y))
                } else {
                    let sy = canvas.min.y + at * ppd;
                    (egui::pos2(view.min.x, sy), egui::pos2(view.max.x, sy))
                };
                painter.line_segment([a, b], egui::Stroke::new(1.0, GUIDE));
                let grab = if vertical {
                    egui::Rect::from_min_max(
                        egui::pos2(a.x - 3.0, view.min.y),
                        egui::pos2(a.x + 3.0, view.max.y),
                    )
                } else {
                    egui::Rect::from_min_max(
                        egui::pos2(view.min.x, a.y - 3.0),
                        egui::pos2(view.max.x, a.y + 3.0),
                    )
                };
                let r = ui.interact(
                    grab,
                    egui::Id::new(("ui_guide", layer, vertical, i)),
                    egui::Sense::drag(),
                );
                if r.hovered() || r.dragged() {
                    ui.ctx().set_cursor_icon(if vertical {
                        egui::CursorIcon::ResizeHorizontal
                    } else {
                        egui::CursorIcon::ResizeVertical
                    });
                }
                if r.drag_started() {
                    self.ui_design.drag = Drag::Guide { vertical, idx: Some(i) };
                }
            }
        }
        self.ui_design_guide_drag(ui, view, canvas, ppd, design_vp, rulers);
    }

    fn ui_design_guide_drag(
        &mut self,
        ui: &mut egui::Ui,
        view: egui::Rect,
        canvas: egui::Rect,
        ppd: f32,
        design_vp: [f32; 2],
        rulers: bool,
    ) {
        let Drag::Guide { vertical, idx } = self.ui_design.drag else { return };
        let Some(idx) = idx else { return };
        let layer = self.ui_design.layer.unwrap_or(0);
        let released = !ui.input(|i| i.pointer.primary_down());
        let Some(p) = ui.input(|i| i.pointer.hover_pos()) else {
            if released {
                self.ui_design.drag = Drag::None;
            }
            return;
        };
        let at = if vertical {
            (p.x - canvas.min.x) / ppd
        } else {
            (p.y - canvas.min.y) / ppd
        };
        // Dropped back over the ruler (or clean off the canvas) = delete.
        let off = if vertical {
            rulers && p.y < view.min.y
        } else {
            rulers && p.x < view.min.x
        };
        let g = self.ui_design.guides.entry(layer).or_default();
        let list = if vertical { &mut g.x } else { &mut g.y };
        if idx < list.len() {
            let step = 1.0;
            list[idx] = (at / step).round() * step;
        }
        if released {
            if off && idx < list.len() {
                list.remove(idx);
            } else if idx < list.len() {
                let v = list[idx];
                // A guide outside the canvas can never catch anything.
                let span = if vertical { design_vp[0] } else { design_vp[1] };
                if v < -2.0 || v > span + 2.0 {
                    list.remove(idx);
                }
            }
            self.ui_design.drag = Drag::None;
            self.ui_design.guides_dirty = true;
        }
    }

    /// The canvas pointer state machine: pick, marquee, move, stack re-order.
    #[allow(clippy::too_many_arguments)]
    fn ui_design_pointer(
        &mut self,
        ui: &mut egui::Ui,
        resp: &egui::Response,
        painter: &egui::Painter,
        canvas: egui::Rect,
        ppd: f32,
        design_vp: [f32; 2],
        placed: &[floptle_ui::Placed],
        rect_of: &HashMap<u32, [f32; 4]>,
        parent_of: &HashMap<u32, u32>,
        stack_parents: &HashSet<u32>,
        ent_of: &HashMap<u32, Entity>,
        rows: &[Row],
        hot: Option<u32>,
    ) {
        // A resize gesture owns the pointer.
        if matches!(self.ui_design.drag, Drag::Resize { .. }) {
            if !ui.input(|i| i.pointer.primary_down()) {
                self.ui_design.drag = Drag::None;
            }
            return;
        }
        let to_design = |p: egui::Pos2| [(p.x - canvas.min.x) / ppd, (p.y - canvas.min.y) / ppd];
        let additive = ui.input(|i| i.modifiers.shift || i.modifiers.ctrl);

        // ---- double-click a text element to edit it in place ----
        if resp.double_clicked()
            && let Some(id) = hot
            && let Some(e) = ent_of.get(&id)
            && let Some(spec) = self.world.get::<ElementSpec>(*e)
            && let Some(t) = &spec.text
        {
            self.ui_design.text_edit = Some((id, t.text.clone()));
            return;
        }

        // ---- click to select ----
        if resp.clicked() {
            match hot {
                Some(id) => {
                    if let Some(e) = ent_of.get(&id) {
                        if !additive {
                            self.selection.clear();
                        }
                        if let Some(pos) = self.selection.iter().position(|s| s == e) {
                            if additive {
                                self.selection.remove(pos);
                            }
                        } else {
                            self.selection.push(*e);
                        }
                    }
                }
                None if !additive => self.selection.clear(),
                None => {}
            }
        }

        // ---- drag start ----
        if resp.drag_started() {
            let start = ui.input(|i| i.pointer.press_origin()).unwrap_or(resp.rect.center());
            match hot {
                Some(id) => {
                    // Grabbing an unselected element selects it, so any element
                    // can be moved in ONE gesture.
                    if let Some(e) = ent_of.get(&id)
                        && !self.selection.contains(e)
                    {
                        if !additive {
                            self.selection.clear();
                        }
                        self.selection.push(*e);
                    }
                    let in_stack = parent_of.get(&id).is_some_and(|p| stack_parents.contains(p));
                    self.ui_design.drag = if in_stack {
                        Drag::Reorder { parent: parent_of[&id], at: 0 }
                    } else {
                        Drag::Move { applied: [0.0, 0.0], start }
                    };
                }
                None => self.ui_design.drag = Drag::Marquee { start, add: additive },
            }
        }

        // ---- drag update ----
        let sel: Vec<u32> = self.selection.iter().map(|e| e.index()).collect();
        match self.ui_design.drag.clone() {
            Drag::Move { applied, start } if resp.dragged() => {
                let Some(now) = ui.input(|i| i.pointer.hover_pos()) else { return };
                let want = [(now.x - start.x) / ppd, (now.y - start.y) / ppd];
                // Snap the PRIMARY element, then move the whole selection by
                // the same amount: a multi-selection that snapped per-element
                // would silently rearrange itself.
                let primary = sel.first().copied();
                let (want, lines) = match primary.and_then(|id| rect_of.get(&id)) {
                    Some(r) if self.ui_design.snap => {
                        let cfg = self.ui_design_snap_cfg(
                            primary.unwrap(),
                            placed,
                            parent_of,
                            design_vp,
                            ppd,
                        );
                        snap_delta(*r, want, &cfg)
                    }
                    _ => (want, Vec::new()),
                };
                let d = [want[0] - applied[0], want[1] - applied[1]];
                if d[0] != 0.0 || d[1] != 0.0 {
                    for id in &sel {
                        self.cmd.ui_move.push((*id, d));
                    }
                }
                self.ui_design.drag = Drag::Move { applied: want, start };
                // Smart guides.
                for l in lines {
                    let (a, b) = if l.vertical {
                        let sx = canvas.min.x + l.at * ppd;
                        (egui::pos2(sx, canvas.min.y), egui::pos2(sx, canvas.max.y))
                    } else {
                        let sy = canvas.min.y + l.at * ppd;
                        (egui::pos2(canvas.min.x, sy), egui::pos2(canvas.max.x, sy))
                    };
                    painter.line_segment([a, b], egui::Stroke::new(1.0, SMART));
                }
            }
            Drag::Reorder { parent, .. } if resp.dragged() => {
                let Some(now) = ui.input(|i| i.pointer.hover_pos()) else { return };
                let d = to_design(now);
                let sibs: Vec<&Row> = rows.iter().filter(|r| r.parent == parent).collect();
                let dir = ent_of
                    .get(&parent)
                    .and_then(|e| self.world.get::<ElementSpec>(*e))
                    .and_then(|s| s.stack)
                    .map(|s| s.dir)
                    .unwrap_or(floptle_ui::Dir::Column);
                let axis = if dir == floptle_ui::Dir::Row { 0 } else { 1 };
                let mut at = sibs.len();
                for (i, s) in sibs.iter().enumerate() {
                    let Some(r) = rect_of.get(&s.id) else { continue };
                    if d[axis] < r[axis] + r[axis + 2] * 0.5 {
                        at = i;
                        break;
                    }
                }
                // The insertion caret.
                let caret = if at < sibs.len() {
                    rect_of.get(&sibs[at].id).map(|r| (r[axis], *r))
                } else {
                    sibs.last().and_then(|s| rect_of.get(&s.id)).map(|r| (r[axis] + r[axis + 2], *r))
                };
                if let Some((line, r)) = caret {
                    let (a, b) = if axis == 0 {
                        let sx = canvas.min.x + line * ppd;
                        (
                            egui::pos2(sx, canvas.min.y + r[1] * ppd),
                            egui::pos2(sx, canvas.min.y + (r[1] + r[3]) * ppd),
                        )
                    } else {
                        let sy = canvas.min.y + line * ppd;
                        (
                            egui::pos2(canvas.min.x + r[0] * ppd, sy),
                            egui::pos2(canvas.min.x + (r[0] + r[2]) * ppd, sy),
                        )
                    };
                    painter.line_segment([a, b], egui::Stroke::new(2.5, SMART));
                }
                self.ui_design.drag = Drag::Reorder { parent, at };
            }
            Drag::Marquee { start, add } if resp.dragged() => {
                let Some(now) = ui.input(|i| i.pointer.hover_pos()) else { return };
                let band = egui::Rect::from_two_pos(start, now);
                painter.rect_filled(band, 0.0, SEL.gamma_multiply(0.10));
                painter.rect_stroke(
                    band,
                    0.0,
                    egui::Stroke::new(1.0, SEL),
                    egui::StrokeKind::Inside,
                );
                let _ = add;
            }
            _ => {}
        }

        // ---- drag end ----
        if resp.drag_stopped() {
            match self.ui_design.drag.clone() {
                Drag::Marquee { start, add } => {
                    let now = ui.input(|i| i.pointer.interact_pos()).unwrap_or(start);
                    let band = egui::Rect::from_two_pos(start, now);
                    if !add {
                        self.selection.clear();
                    }
                    for p in placed {
                        if self.ui_design.locked.contains(&p.id) {
                            continue;
                        }
                        let r = egui::Rect::from_min_size(
                            canvas.min + egui::vec2(p.rect[0] * ppd, p.rect[1] * ppd),
                            egui::vec2(p.rect[2] * ppd, p.rect[3] * ppd),
                        );
                        // Fully enclosed, not merely touched: a band that
                        // grabbed every parent it clipped would select the
                        // whole screen on the first drag.
                        if band.contains_rect(r)
                            && let Some(e) = ent_of.get(&p.id)
                            && !self.selection.contains(e)
                        {
                            self.selection.push(*e);
                        }
                    }
                }
                Drag::Reorder { parent, at } => {
                    if let Some(moved) = sel.first() {
                        self.ui_design_reorder(rows, *moved, parent, at);
                    }
                }
                _ => {}
            }
            self.ui_design.drag = Drag::None;
        }
    }

    /// The lines a drag can snap to: guides, the parent's box, and every
    /// sibling's edges and centres.
    fn ui_design_snap_cfg(
        &self,
        id: u32,
        placed: &[floptle_ui::Placed],
        parent_of: &HashMap<u32, u32>,
        design_vp: [f32; 2],
        ppd: f32,
    ) -> SnapCfg<'_> {
        let layer = self.ui_design.layer.unwrap_or(0);
        let mut lines: [Vec<f32>; 2] = [Vec::new(), Vec::new()];
        if self.ui_design.snap_siblings {
            let parent = parent_of.get(&id).copied();
            // The container: the parent element's rect, or the whole canvas.
            let container = parent
                .and_then(|p| placed.iter().find(|pl| pl.id == p))
                .map(|pl| pl.rect)
                .unwrap_or([0.0, 0.0, design_vp[0], design_vp[1]]);
            for (a, axis) in lines.iter_mut().enumerate() {
                axis.push(container[a]);
                axis.push(container[a] + container[a + 2] * 0.5);
                axis.push(container[a] + container[a + 2]);
            }
            for pl in placed {
                if pl.id == id || parent_of.get(&pl.id).copied() != parent {
                    continue;
                }
                for (a, axis) in lines.iter_mut().enumerate() {
                    axis.push(pl.rect[a]);
                    axis.push(pl.rect[a] + pl.rect[a + 2] * 0.5);
                    axis.push(pl.rect[a] + pl.rect[a + 2]);
                }
            }
        }
        SnapCfg {
            grid: self.ui_design.grid_step(self.ui_tokens),
            guides: if self.ui_design.snap_guides {
                self.ui_design.guides.get(&layer)
            } else {
                None
            },
            lines,
            // A fixed number of SCREEN points, so snapping feels the same at
            // every zoom instead of getting stickier as you zoom out.
            radius: 6.0 / ppd.max(0.0001),
        }
    }

    /// Arrow keys nudge; shift-arrows nudge by one step of the project's
    /// spacing scale. The whole point is that the keyboard path also lands on
    /// scale values.
    fn ui_design_keys(
        &mut self,
        ui: &mut egui::Ui,
        sel: &[u32],
        rect_of: &HashMap<u32, [f32; 4]>,
    ) {
        if sel.is_empty() || self.ui_design.text_edit.is_some() {
            return;
        }
        let step = self.ui_design.grid_step(self.ui_tokens);
        let mut d = [0.0f32; 2];
        ui.input(|i| {
            let big = i.modifiers.shift;
            let n = if big { step } else { 1.0 };
            if i.key_pressed(egui::Key::ArrowLeft) {
                d[0] -= n;
            }
            if i.key_pressed(egui::Key::ArrowRight) {
                d[0] += n;
            }
            if i.key_pressed(egui::Key::ArrowUp) {
                d[1] -= n;
            }
            if i.key_pressed(egui::Key::ArrowDown) {
                d[1] += n;
            }
        });
        if d != [0.0, 0.0] {
            for id in sel {
                if rect_of.contains_key(id) {
                    self.cmd.ui_move.push((*id, d));
                }
            }
        }
    }

    fn ui_design_context_menu(
        &mut self,
        resp: &egui::Response,
        sel: &[u32],
        ent_of: &HashMap<u32, Entity>,
    ) {
        let sel = sel.to_vec();
        let has = !sel.is_empty();
        resp.clone().context_menu(|ui| {
            ui.add_enabled_ui(has, |ui| {
                if ui.button("Copy style").clicked() {
                    if let Some(spec) =
                        sel.first().and_then(|id| ent_of.get(id)).and_then(|e| self.world.get::<ElementSpec>(*e))
                    {
                        self.ui_design.style_clip = Some(if spec.style.is_empty() {
                            StyleClip::Look(Box::new(spec.clone()))
                        } else {
                            StyleClip::Named(spec.style.clone())
                        });
                    }
                    ui.close();
                }
                let can_paste = self.ui_design.style_clip.is_some();
                ui.add_enabled_ui(can_paste, |ui| {
                    if ui.button("Paste style").clicked() {
                        match self.ui_design.style_clip.clone() {
                            Some(StyleClip::Named(n)) => {
                                for id in &sel {
                                    self.cmd.ui_set_style.push((*id, n.clone()));
                                }
                            }
                            Some(StyleClip::Look(spec)) => {
                                for id in &sel {
                                    self.cmd.ui_paste_look.push((*id, spec.clone()));
                                }
                            }
                            None => {}
                        }
                        ui.close();
                    }
                });
                ui.separator();
                if ui
                    .button("Make this a style…")
                    .on_hover_text("lift this element's look into your project's style sheet")
                    .clicked()
                {
                    if let Some(id) = sel.first() {
                        let name = ent_of
                            .get(id)
                            .and_then(|e| self.world.get::<floptle_core::Name>(*e))
                            .map(|n| n.0.to_lowercase().replace(' ', "-"))
                            .unwrap_or_else(|| "style".into());
                        self.ui_design.sheets = scan_sheets(self.project_root);
                        self.ui_design.make_style = Some((*id, name, 0));
                    }
                    ui.close();
                }
                ui.separator();
                if ui.button("Bring to front").clicked() {
                    self.ui_design_depth(&sel, true);
                    ui.close();
                }
                if ui.button("Send to back").clicked() {
                    self.ui_design_depth(&sel, false);
                    ui.close();
                }
                ui.separator();
                if ui.button("Duplicate").clicked() {
                    self.cmd.duplicate = true;
                    ui.close();
                }
                if ui.button("Delete").clicked() {
                    self.cmd.delete = true;
                    ui.close();
                }
            });
        });
    }

    /// Push elements to the front or back of their sibling runs.
    fn ui_design_depth(&mut self, sel: &[u32], front: bool) {
        let Some(layer) = self.ui_design.layer else { return };
        let Some(layer_ent) =
            self.world.query::<floptle_core::transform::Transform>().map(|(e, _)| e).find(|e| e.index() == layer)
        else {
            return;
        };
        let rows = layer_rows(self.world, layer_ent);
        // Group by parent so a multi-selection spanning several containers
        // still does the obvious thing inside each of them.
        let mut by_parent: HashMap<u32, Vec<u32>> = HashMap::new();
        for id in sel {
            if let Some(r) = rows.iter().find(|r| r.id == *id) {
                by_parent.entry(r.parent).or_default().push(*id);
            }
        }
        for (parent, moved) in by_parent {
            let sibs: Vec<u32> =
                rows.iter().filter(|r| r.parent == parent).map(|r| r.id).collect();
            self.cmd.ui_order.extend(crate::ui_design::depth_run(&sibs, &moved, front));
        }
    }

    fn ui_design_align(&mut self, how: Align) {
        let sel: Vec<u32> = self.selection.iter().map(|e| e.index()).collect();
        let rect_of: HashMap<u32, [f32; 4]> =
            self.ui_design.placed.iter().map(|p| (p.id, p.rect)).collect();
        let Some(layer) = self.ui_design.layer else { return };
        let Some(layer_ent) =
            self.world.query::<floptle_core::transform::Transform>().map(|(e, _)| e).find(|e| e.index() == layer)
        else {
            return;
        };
        let rows = layer_rows(self.world, layer_ent);
        let parent_of: HashMap<u32, u32> = rows.iter().map(|r| (r.id, r.parent)).collect();
        let vp = self.ui_design.design_vp;
        let container = |id: u32| -> [f32; 4] {
            parent_of
                .get(&id)
                .and_then(|p| rect_of.get(p))
                .copied()
                .unwrap_or([0.0, 0.0, vp[0], vp[1]])
        };
        for m in align_moves(&sel, &rect_of, &container, how) {
            self.cmd.ui_move.push(m);
        }
    }

    fn ui_design_distribute(&mut self, axis: usize) {
        let sel: Vec<u32> = self.selection.iter().map(|e| e.index()).collect();
        let rect_of: HashMap<u32, [f32; 4]> =
            self.ui_design.placed.iter().map(|p| (p.id, p.rect)).collect();
        for m in distribute_moves(&sel, &rect_of, axis) {
            self.cmd.ui_move.push(m);
        }
    }

    /// The inline text editor: a real text field parked over the element.
    fn ui_design_text_overlay(
        &mut self,
        ui: &mut egui::Ui,
        rect_of: &HashMap<u32, [f32; 4]>,
        ppd: f32,
        to_screen: impl Fn([f32; 2]) -> egui::Pos2,
    ) {
        let Some((id, mut buf)) = self.ui_design.text_edit.clone() else { return };
        let Some(r) = rect_of.get(&id) else {
            self.ui_design.text_edit = None;
            return;
        };
        let rr = egui::Rect::from_min_size(
            to_screen([r[0], r[1]]),
            egui::vec2((r[2] * ppd).max(80.0), (r[3] * ppd).max(20.0)),
        );
        let mut done = false;
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rr));
        let resp = child.add(
            egui::TextEdit::multiline(&mut buf)
                .desired_width(rr.width())
                .font(egui::TextStyle::Monospace),
        );
        resp.request_focus();
        if resp.lost_focus() || child.input(|i| i.key_pressed(egui::Key::Escape)) {
            done = true;
        }
        if done {
            self.cmd.ui_set_text = Some((id, buf.clone()));
            self.ui_design.text_edit = None;
        } else {
            self.ui_design.text_edit = Some((id, buf));
        }
    }

    /// "Make this a style": name it, pick which sheet it goes in, write it.
    fn ui_design_make_style_dialog(&mut self, ui: &mut egui::Ui) {
        let Some((id, mut name, mut sheet)) = self.ui_design.make_style.clone() else { return };
        let mut open = true;
        let mut commit = false;
        egui::Window::new("Make this a style")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ui.ctx(), |ui| {
                ui.label("Lifts this element's look into a named style. The element keeps");
                ui.label("its own values — a style you can't see the effect of is a trap.");
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("Name");
                    ui.text_edit_singleline(&mut name);
                });
                ui.small("Slashes group names in the picker: \"button/primary\".");
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("Sheet");
                    let sheets = self.ui_design.sheets.clone();
                    let current = sheets
                        .get(sheet)
                        .map(|(_, n)| n.clone())
                        .unwrap_or_else(|| "assets/ui/styles.uistyle.ron  (new)".into());
                    egui::ComboBox::from_id_salt("ui_make_style_sheet")
                        .selected_text(current)
                        .width(280.0)
                        .show_ui(ui, |ui| {
                            for (i, (_, n)) in sheets.iter().enumerate() {
                                if ui.selectable_label(i == sheet, n).clicked() {
                                    sheet = i;
                                }
                            }
                            if ui
                                .selectable_label(
                                    sheet >= sheets.len(),
                                    "assets/ui/styles.uistyle.ron  (new)",
                                )
                                .clicked()
                            {
                                sheet = sheets.len();
                            }
                        });
                });
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    let valid = !name.trim().is_empty()
                        && !self.ui_styles.styles.contains_key(name.trim());
                    ui.add_enabled_ui(valid, |ui| {
                        if ui.button("Create").clicked() {
                            commit = true;
                        }
                    });
                    if !name.trim().is_empty()
                        && self.ui_styles.styles.contains_key(name.trim())
                    {
                        ui.colored_label(
                            egui::Color32::from_rgb(230, 130, 90),
                            "that name is already taken",
                        );
                    }
                });
            });
        if commit {
            let spec = self
                .world
                .query::<floptle_core::transform::Transform>()
                .map(|(e, _)| e)
                .find(|e| e.index() == id)
                .and_then(|e| self.world.get::<ElementSpec>(e))
                .cloned();
            if let Some(spec) = spec {
                let style = floptle_ui::Style {
                    base: crate::ui_design::block_from(&spec),
                    ..Default::default()
                };
                let path = self
                    .ui_design
                    .sheets
                    .get(sheet)
                    .map(|(p, _)| p.clone())
                    .unwrap_or_else(|| {
                        self.project_root.join("assets").join("ui").join("styles.uistyle.ron")
                    });
                match crate::ui_design::append_style(&path, name.trim(), &style) {
                    Ok(()) => {
                        self.cmd.ui_set_style.push((id, name.trim().to_string()));
                        self.cmd.ui_reload_styles = true;
                        self.console.push(
                            floptle_script::LogLevel::Debug,
                            format!("added UI style \"{}\" to {}", name.trim(), path.display()),
                            None,
                        );
                    }
                    Err(e) => self.console.push(
                        floptle_script::LogLevel::Error,
                        format!("could not write {}: {e}", path.display()),
                        None,
                    ),
                }
            }
            open = false;
        }
        self.ui_design.make_style = if open { Some((id, name, sheet)) } else { None };
    }
}

/// The eight resize grips of a rect, with the direction each one grows.
fn handles(r: egui::Rect) -> [(egui::Pos2, i8, i8); 8] {
    [
        (r.left_top(), -1, -1),
        (r.center_top(), 0, -1),
        (r.right_top(), 1, -1),
        (r.left_center(), -1, 0),
        (r.right_center(), 1, 0),
        (r.left_bottom(), -1, 1),
        (r.center_bottom(), 0, 1),
        (r.right_bottom(), 1, 1),
    ]
}

/// The union of some design rects, as `[x, y, w, h]`.
fn bounds(rs: &[[f32; 4]]) -> Option<[f32; 4]> {
    let first = rs.first()?;
    let mut lo = [first[0], first[1]];
    let mut hi = [first[0] + first[2], first[1] + first[3]];
    for r in rs {
        lo[0] = lo[0].min(r[0]);
        lo[1] = lo[1].min(r[1]);
        hi[0] = hi[0].max(r[0] + r[2]);
        hi[1] = hi[1].max(r[1] + r[3]);
    }
    Some([lo[0], lo[1], hi[0] - lo[0], hi[1] - lo[1]])
}

/// Every `.uistyle.ron` in the project, for the "make a style" destination
/// picker. Sorted so the list doesn't reshuffle between openings.
fn scan_sheets(root: &std::path::Path) -> Vec<(std::path::PathBuf, String)> {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>, depth: u32) {
        if depth > 8 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
            if p.is_dir() {
                if name.starts_with('.') || name == "target" || name == "builds" {
                    continue;
                }
                walk(&p, out, depth + 1);
            } else if name.ends_with(".uistyle.ron") {
                out.push(p);
            }
        }
    }
    let mut paths = Vec::new();
    walk(root, &mut paths, 0);
    paths.sort();
    paths
        .into_iter()
        .map(|p| {
            let label =
                p.strip_prefix(root).unwrap_or(&p).to_string_lossy().replace('\\', "/");
            (p, label)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_covers_every_rect() {
        let b = bounds(&[[10.0, 20.0, 5.0, 5.0], [0.0, 40.0, 100.0, 1.0]]).unwrap();
        assert_eq!(b, [0.0, 20.0, 100.0, 21.0]);
        assert!(bounds(&[]).is_none());
    }
}
