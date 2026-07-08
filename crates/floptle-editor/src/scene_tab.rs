//! The Scene / Game viewport tabs: cache the tab rect the 3D view renders
//! through, the viewport toolbar overlay, drag-and-drop spawning, and the
//! in-viewport context menu.

use floptle_core::math::Vec2;

use crate::dock::AspectMode;
use crate::gizmo::{paint_gizmo, Tool};
use crate::EditorTabViewer;

impl EditorTabViewer<'_> {
    pub(crate) fn scene_ui(&mut self, ui: &mut egui::Ui, game: bool) {
        // This tab's rect IS the 3D viewport. The Scene tab caches it for picking / gizmo
        // gating; the Game tab caches its own rect (so the editor can size the offscreen
        // Game target to it) and, when split, paints that offscreen render over itself.
        let rect = ui.max_rect();
        if game {
            *self.game_rect = Some(rect);
            if self.game_offscreen
                && let Some(tex) = self.game_tex {
                    egui::Image::new((tex, rect.size())).paint_at(ui, rect);
                }
        } else {
            *self.scene_rect = Some(rect);
            // ---- game-UI authoring overlay: element outlines in the Scene view.
            // Click selects the element; drag moves it (Free pos / Pin offset —
            // written back in design units through cmd.ui_move). The Game tab
            // shows the real render; this is the "where is everything" aid.
            if !self.ui_overlay.is_empty() {
                let painter = ui.painter_at(rect);
                for (idx, r, scale) in self.ui_overlay.iter() {
                    let er = egui::Rect::from_min_size(
                        rect.min + egui::vec2(r[0], r[1]),
                        egui::vec2(r[2].max(4.0), r[3].max(4.0)),
                    );
                    if !rect.intersects(er) {
                        continue;
                    }
                    let ent = self
                        .world
                        .query::<floptle_core::transform::Transform>()
                        .map(|(e, _)| e)
                        .find(|e| e.index() == *idx);
                    let selected = ent.map(|e| self.selection.contains(&e)).unwrap_or(false);
                    let color = if selected {
                        egui::Color32::from_rgb(255, 180, 60)
                    } else {
                        egui::Color32::from_rgba_unmultiplied(80, 200, 255, 170)
                    };
                    painter.rect_stroke(
                        er,
                        2.0,
                        egui::Stroke::new(if selected { 2.0 } else { 1.0 }, color),
                        egui::StrokeKind::Outside,
                    );
                    let resp =
                        ui.interact(er, egui::Id::new(("ui_ov", *idx)), egui::Sense::click_and_drag());
                    if resp.clicked()
                        && let Some(e) = ent
                    {
                        self.selection.clear();
                        self.selection.push(e);
                    }
                    if resp.dragged() && selected {
                        let d = resp.drag_delta() / *scale;
                        match &mut self.cmd.ui_move {
                            Some((i, acc)) if *i == *idx => {
                                acc[0] += d.x;
                                acc[1] += d.y;
                            }
                            slot => *slot = Some((*idx, [d.x, d.y])),
                        }
                    }
                }
            }
        }

        // The Game tab is the active-camera gameplay view — no editor tools/gizmos.
        // Warn if there's no active camera (the render falls back to the editor view).
        if game && !self.has_active_camera {
            egui::Area::new(egui::Id::new("game_no_cam"))
                .fixed_pos(rect.left_top() + egui::vec2(8.0, 8.0))
                .show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.colored_label(
                            egui::Color32::from_rgb(235, 200, 90),
                            "Δ no active camera — using editor view",
                        );
                    });
                });
        }

        // Overlay toolbar: tools (left) + resolution simulator (right). Editor view only.
        if !game {
            egui::Area::new(egui::Id::new("scene_toolbar"))
                .fixed_pos(rect.left_top() + egui::vec2(8.0, 8.0))
                .show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            for t in [Tool::Select, Tool::Move, Tool::Rotate, Tool::Scale, Tool::Sculpt] {
                                if ui.selectable_label(self.tool == t, t.label()).clicked() {
                                    self.cmd.set_tool = Some(t);
                                }
                            }
                            ui.separator();
                            egui::ComboBox::from_id_salt("aspect_mode")
                                .selected_text(self.aspect.label())
                                .show_ui(ui, |ui| {
                                    for m in AspectMode::ALL {
                                        if ui.selectable_label(*self.aspect == m, m.label()).clicked() {
                                            *self.aspect = m;
                                        }
                                    }
                                });
                            if self.aspect.ratio().is_some() {
                                ui.add(egui::Slider::new(self.zoom, 0.4..=1.0).text("fit").show_value(false));
                            }
                        });
                    });
                });
        }

        // Gizmos master toggle — top-right of the viewport (editor view only). Off hides
        // every overlay (colliders, camera/light/gravity gizmos, contacts), including the
        // selected node's.
        if !game {
            egui::Area::new(egui::Id::new("gizmo_toggle"))
                .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-8.0, 8.0))
                .show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.toggle_value(self.show_gizmos, "◎ Gizmos")
                            .on_hover_text("show selection/collider/camera/light gizmos in the viewport");
                    });
                });
        }

        // Resolution simulator: a centered device frame for the chosen aspect.
        if let Some(r) = self.aspect.ratio() {
            let avail = rect.shrink(10.0);
            let zoom = self.zoom.clamp(0.2, 1.0);
            let (mut w, mut h) = (avail.width(), avail.height());
            if w / h > r {
                w = h * r;
            } else {
                h = w / r;
            }
            w *= zoom;
            h *= zoom;
            let frame = egui::Rect::from_center_size(rect.center(), egui::vec2(w, h));
            let painter = ui.painter_at(rect);
            // Dim outside the device frame so the framing is obvious.
            let shade = egui::Color32::from_black_alpha(150);
            painter.rect_filled(egui::Rect::from_min_max(rect.left_top(), egui::pos2(rect.right(), frame.top())), 0.0, shade);
            painter.rect_filled(egui::Rect::from_min_max(egui::pos2(rect.left(), frame.bottom()), rect.right_bottom()), 0.0, shade);
            painter.rect_filled(egui::Rect::from_min_max(egui::pos2(rect.left(), frame.top()), egui::pos2(frame.left(), frame.bottom())), 0.0, shade);
            painter.rect_filled(egui::Rect::from_min_max(egui::pos2(frame.right(), frame.top()), egui::pos2(rect.right(), frame.bottom())), 0.0, shade);
            painter.rect_stroke(frame, 2.0, egui::Stroke::new(1.5, egui::Color32::from_gray(180)), egui::StrokeKind::Inside);
        }

        // The gizmo paints on a layer above the scene, clipped to this tab (editor only).
        if let Some(g) = self.gizmo.filter(|_| !game) {
            let painter = ui
                .ctx()
                .layer_painter(egui::LayerId::new(egui::Order::Background, egui::Id::new("gizmo")))
                .with_clip_rect(rect);
            paint_gizmo(&painter, g, self.tool, self.grabbed, self.ppp);
        }

        // Terrain brush telegraph: a ring at the surface + a normal line, so you can
        // see exactly where (and on what facing) a stroke will land.
        if let Some(viz) = self.terrain_viz.filter(|_| !game) {
            let painter = ui
                .ctx()
                .layer_painter(egui::LayerId::new(egui::Order::Background, egui::Id::new("terrain_brush")))
                .with_clip_rect(rect);
            let ppp = self.ppp;
            let pt = |v: Vec2| egui::pos2(v.x / ppp, v.y / ppp);
            if viz.ring.len() >= 2 {
                let mut pts: Vec<egui::Pos2> = viz.ring.iter().map(|v| pt(*v)).collect();
                pts.push(pts[0]); // close the loop
                painter.line(pts, egui::Stroke::new(2.0, egui::Color32::from_rgb(255, 230, 120)));
            }
            if let Some((a, b)) = viz.normal {
                painter.line_segment(
                    [pt(a), pt(b)],
                    egui::Stroke::new(2.0, egui::Color32::from_rgb(120, 200, 255)),
                );
            }
        }

        // Camera frustums (active = bright green, others = dim) so cameras are visible.
        if !game && !self.camera_gizmos.is_empty() {
            let painter = ui
                .ctx()
                .layer_painter(egui::LayerId::new(egui::Order::Background, egui::Id::new("camera_gizmos")))
                .with_clip_rect(rect);
            let ppp = self.ppp;
            let pt = |v: Vec2| egui::pos2(v.x / ppp, v.y / ppp);
            for g in self.camera_gizmos {
                let col = if g.active {
                    egui::Color32::from_rgb(120, 230, 140)
                } else {
                    egui::Color32::from_rgb(150, 160, 175)
                };
                for (a, b) in &g.lines {
                    painter.line_segment([pt(*a), pt(*b)], egui::Stroke::new(1.5, col));
                }
            }
        }

        // Point-light gizmos (a warm cross + range ring) so unselected lights are
        // visible/placeable. Editor view only (the gather is gated on !game_view).
        if !game && !self.light_gizmos.is_empty() {
            let painter = ui
                .ctx()
                .layer_painter(egui::LayerId::new(egui::Order::Background, egui::Id::new("light_gizmos")))
                .with_clip_rect(rect);
            let ppp = self.ppp;
            let pt = |v: Vec2| egui::pos2(v.x / ppp, v.y / ppp);
            let col = egui::Color32::from_rgb(245, 210, 110);
            for lines in self.light_gizmos {
                for (a, b) in lines {
                    painter.line_segment([pt(*a), pt(*b)], egui::Stroke::new(1.5, col));
                }
            }
        }

        // Rigidbody collider outlines (cyan) + collision-contact crosses (orange).
        if !game && (!self.body_gizmos.is_empty() || !self.contact_gizmos.is_empty()) {
            let painter = ui
                .ctx()
                .layer_painter(egui::LayerId::new(egui::Order::Background, egui::Id::new("physics_gizmos")))
                .with_clip_rect(rect);
            let ppp = self.ppp;
            let pt = |v: Vec2| egui::pos2(v.x / ppp, v.y / ppp);
            let body_col = egui::Color32::from_rgb(110, 220, 210);
            for lines in self.body_gizmos {
                for (a, b) in lines {
                    painter.line_segment([pt(*a), pt(*b)], egui::Stroke::new(1.2, body_col));
                }
            }
            let hit_col = egui::Color32::from_rgb(255, 150, 60);
            for (a, b) in self.contact_gizmos {
                painter.line_segment([pt(*a), pt(*b)], egui::Stroke::new(2.0, hit_col));
            }
        }

        // Terrain collider wireframe (where the player can walk) — a soft yellow net.
        if !game && !self.terrain_wire.is_empty() {
            let painter = ui
                .ctx()
                .layer_painter(egui::LayerId::new(egui::Order::Background, egui::Id::new("terrain_wire")))
                .with_clip_rect(rect);
            let ppp = self.ppp;
            let pt = |v: Vec2| egui::pos2(v.x / ppp, v.y / ppp);
            let col = egui::Color32::from_rgba_unmultiplied(235, 225, 120, 130);
            for (a, b) in self.terrain_wire {
                painter.line_segment([pt(*a), pt(*b)], egui::Stroke::new(0.8, col));
            }
        }

        // Script debug gizmos (`gizmo.*`): Scene view only — like every other
        // gizmo, the Game view stays clean (what the player would actually see).
        if !game && !self.script_gizmo_lines.is_empty() {
            let painter = ui
                .ctx()
                .layer_painter(egui::LayerId::new(egui::Order::Background, egui::Id::new("script_gizmos")))
                .with_clip_rect(rect);
            let ppp = self.ppp;
            let pt = |v: Vec2| egui::pos2(v.x / ppp, v.y / ppp);
            for (a, b, c) in self.script_gizmo_lines {
                let col = egui::Color32::from_rgb(
                    (c[0].clamp(0.0, 1.0) * 255.0) as u8,
                    (c[1].clamp(0.0, 1.0) * 255.0) as u8,
                    (c[2].clamp(0.0, 1.0) * 255.0) as u8,
                );
                painter.line_segment([pt(*a), pt(*b)], egui::Stroke::new(2.0, col));
            }
        }

        // Selected particle track's emitter/force gizmo — birth shape (warm), emit
        // direction (cyan-green), and force arrows (magenta), each carrying its color.
        if !game && !self.particle_gizmo.is_empty() {
            let painter = ui
                .ctx()
                .layer_painter(egui::LayerId::new(egui::Order::Background, egui::Id::new("particle_gizmo")))
                .with_clip_rect(rect);
            let ppp = self.ppp;
            let pt = |v: Vec2| egui::pos2(v.x / ppp, v.y / ppp);
            for (a, b, c) in self.particle_gizmo {
                let col = egui::Color32::from_rgb(
                    (c[0].clamp(0.0, 1.0) * 255.0) as u8,
                    (c[1].clamp(0.0, 1.0) * 255.0) as u8,
                    (c[2].clamp(0.0, 1.0) * 255.0) as u8,
                );
                painter.line_segment([pt(*a), pt(*b)], egui::Stroke::new(1.5, col));
            }
        }

        // Mesh collider wireframes (imported maps flagged walkable) — a cyan triangle net.
        if !game && !self.mesh_wire.is_empty() {
            let painter = ui
                .ctx()
                .layer_painter(egui::LayerId::new(egui::Order::Background, egui::Id::new("mesh_wire")))
                .with_clip_rect(rect);
            let ppp = self.ppp;
            let pt = |v: Vec2| egui::pos2(v.x / ppp, v.y / ppp);
            let col = egui::Color32::from_rgba_unmultiplied(120, 220, 220, 120);
            for (a, b) in self.mesh_wire {
                painter.line_segment([pt(*a), pt(*b)], egui::Stroke::new(0.8, col));
            }
        }
    }
}
