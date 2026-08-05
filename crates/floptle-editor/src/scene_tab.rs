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
            // Canvas bounds first: the layer's full design viewport in the world.
            if !self.ui_canvas.is_empty() {
                let painter = ui.painter_at(rect);
                for quad in self.ui_canvas.iter() {
                    let p: Vec<egui::Pos2> = quad
                        .iter()
                        .map(|c| rect.min + egui::vec2(c[0], c[1]))
                        .collect();
                    let col = egui::Color32::from_rgba_unmultiplied(160, 160, 255, 130);
                    for i in 0..4 {
                        painter.line_segment(
                            [p[i], p[(i + 1) % 4]],
                            egui::Stroke::new(1.5, col),
                        );
                    }
                }
            }
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
                    // Claim the pointer for egui: the raw viewport press must not
                    // pick (it can't see 2D elements ⏵ would clear the selection).
                    if resp.hovered() || resp.dragged() {
                        self.cmd.ui_hot = true;
                    }
                    // A click — or the START of a drag on an unselected element —
                    // selects it. So you can grab ANY element and move it in one
                    // gesture (no click-to-select first), with any tool including
                    // Rect. drag_started fires before the first drag delta.
                    if (resp.clicked() || (resp.drag_started() && !selected))
                        && let Some(e) = ent
                    {
                        self.selection.clear();
                        self.selection.push(e);
                    }
                    // Drag the body to move (Free pos / Pin offset, design units).
                    // Re-checks selection so a just-grabbed element moves this same
                    // frame; the Rect tool's resize handles sit on top of this.
                    if resp.dragged() && ent.is_some_and(|e| self.selection.contains(&e)) {
                        let d = resp.drag_delta() / *scale;
                        match self.cmd.ui_move.iter_mut().find(|(i, _)| *i == *idx) {
                            Some((_, acc)) => {
                                acc[0] += d.x;
                                acc[1] += d.y;
                            }
                            None => self.cmd.ui_move.push((*idx, [d.x, d.y])),
                        }
                    }
                    // Rect tool: 8 grab handles on the selected element — drag a
                    // side/corner to resize toward it; the opposite edge stays
                    // put (offsets are compensated per placement mode).
                    if selected && self.tool == Tool::Rect {
                        let hs = 5.0; // handle half-size, pts
                        let handles: [(egui::Pos2, i8, i8); 8] = [
                            (er.left_top(), -1, -1),
                            (er.center_top(), 0, -1),
                            (er.right_top(), 1, -1),
                            (er.left_center(), -1, 0),
                            (er.right_center(), 1, 0),
                            (er.left_bottom(), -1, 1),
                            (er.center_bottom(), 0, 1),
                            (er.right_bottom(), 1, 1),
                        ];
                        for (k, (pos, hx, hy)) in handles.iter().enumerate() {
                            let hr =
                                egui::Rect::from_center_size(*pos, egui::vec2(hs * 2.0, hs * 2.0));
                            let hresp = ui.interact(
                                hr,
                                egui::Id::new(("ui_rs", *idx, k)),
                                egui::Sense::drag(),
                            );
                            painter.rect_filled(
                                hr.shrink(if hresp.hovered() || hresp.dragged() { 0.0 } else { 2.0 }),
                                1.0,
                                egui::Color32::from_rgb(255, 180, 60),
                            );
                            if hresp.hovered() || hresp.dragged() {
                                self.cmd.ui_hot = true;
                            }
                            if hresp.dragged() {
                                let d = hresp.drag_delta() / *scale;
                                // Size delta grows toward the dragged side.
                                let ds = [d.x * *hx as f32, d.y * *hy as f32];
                                let from_min = [*hx < 0, *hy < 0];
                                let cur = [r[2] / *scale, r[3] / *scale];
                                match &mut self.cmd.ui_resize {
                                    Some((i, acc, fm, _)) if *i == *idx && *fm == from_min => {
                                        acc[0] += ds[0];
                                        acc[1] += ds[1];
                                    }
                                    slot => *slot = Some((*idx, ds, from_min, cur)),
                                }
                            }
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

        // PLAY banner (`floptle/0110`). Persistent, not a toast: the whole
        // failure is that a Play-time edit LOOKS like it worked — the gizmo
        // moves, the Inspector shows the new number — and Stop throws it away
        // with nothing ever said. `push_history` no-ops while playing, so the
        // edit is not undoable and never marks the scene unsaved either.
        //
        // Deliberately NOT a refusal. Nudging a camera while watching a cutscene
        // run is how you find the framing; taking that away would cost more than
        // the trap does. What was missing is the readback, and the Inspector's
        // "copy to the stopped scene" button beside it is what turns a value
        // found during Play into one you keep.
        //
        // Top-CENTRE, and in both views: the Game tab is where somebody watching
        // their game is actually looking, and the top-left corner of the Scene
        // tab is already the tool palette.
        if self.playing {
            egui::Area::new(egui::Id::new(if game { "play_banner_game" } else { "play_banner" }))
                .fixed_pos(egui::pos2(rect.center().x - 132.0, rect.top() + 8.0))
                .order(egui::Order::Middle)
                .show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style())
                        .fill(egui::Color32::from_rgb(60, 34, 34))
                        .show(ui, |ui| {
                            ui.colored_label(
                                egui::Color32::from_rgb(255, 150, 140),
                                "▶ PLAY — edits here are discarded on Stop",
                            )
                            .on_hover_text(
                                "Stop reverts the whole world to the saved scene. Moving, \
                                 re-parenting or re-arranging while playing is not undoable \
                                 and does not mark the scene unsaved.\n\nFound a value you \
                                 want to keep? The Inspector's transform rows have a copy \
                                 button while playing.",
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
                            // Ordered by Tool::ALL — i.e. by keybind, so what you see
                            // left-to-right is what 1..7 select.
                            for t in Tool::ALL {
                                let hit = ui.selectable_label(self.tool == t, t.label());
                                if hit.on_hover_text(format!("{} ({})", t.label(), t.digit())).clicked() {
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

        // ▦ Map tool HUD. The Map PANEL is the control surface; this is a
        // status strip — what the next click and the next drag will do, on one
        // line, so it states the tool's mode without duplicating the panel or
        // covering the scene. `⏷` opens the same chips for anyone who would
        // rather switch from here.
        if !game && self.tool == Tool::MapEdit && !self.map_playing {
            let accent = egui::Color32::from_rgb(120, 220, 255);
            egui::Area::new(egui::Id::new("map_hud"))
                .fixed_pos(rect.left_top() + egui::vec2(8.0, 46.0))
                .order(egui::Order::Middle)
                .show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                        let k = |c: crate::map_keys::MapCmd| self.map_keys.label(c);
                        // The sub-object mode gets THREE chips, not one cycling
                        // label: the mode you want is one click, and the two you
                        // aren't in are visible instead of being somewhere in a
                        // rotation. This is the control you touch most, and it
                        // lives over the viewport so it never hides behind
                        // another dock tab.
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 2.0;
                            for mode in crate::map_edit::MapSubMode::ALL {
                                if ui
                                    .add_sized(
                                        [66.0, 20.0],
                                        egui::Button::selectable(
                                            self.map_mode == mode,
                                            egui::RichText::new(format!(
                                                "{} {}",
                                                mode.glyph(),
                                                mode.label()
                                            ))
                                            .small(),
                                        ),
                                    )
                                    .on_hover_text(format!(
                                        "select {} — {} (or {} to cycle). Your selection \
                                         converts rather than being dropped.",
                                        mode.plural(),
                                        k(mode.cmd()),
                                        k(crate::map_keys::MapCmd::ModeCycle),
                                    ))
                                    .clicked()
                                {
                                    self.cmd.set_map_mode = Some(mode);
                                }
                            }
                            ui.separator();
                            // Select-everything, one click from the viewport —
                            // it was a key nobody could guess and a button in a
                            // panel that might not even be open.
                            let sel_btn = |ui: &mut egui::Ui, text: &str, hover: String| {
                                ui.add(
                                    egui::Button::new(egui::RichText::new(text).small())
                                        .min_size(egui::vec2(0.0, 20.0)),
                                )
                                .on_hover_text(hover)
                                .clicked()
                            };
                            if sel_btn(
                                ui,
                                "All",
                                format!(
                                    "select every {} in this mesh  ({})",
                                    self.map_mode.label(),
                                    k(crate::map_keys::MapCmd::SelectAll)
                                ),
                            ) {
                                self.cmd.map_op = Some(crate::map_edit::MapOp::SelectAll);
                            }
                            if sel_btn(
                                ui,
                                "None",
                                format!("clear it  ({})", k(crate::map_keys::MapCmd::SelectNone)),
                            ) {
                                self.cmd.map_op = Some(crate::map_edit::MapOp::SelectNone);
                            }
                            if sel_btn(
                                ui,
                                "Invert",
                                format!(
                                    "swap selected for unselected  ({})",
                                    k(crate::map_keys::MapCmd::SelectInvert)
                                ),
                            ) {
                                self.cmd.map_op = Some(crate::map_edit::MapOp::SelectInvert);
                            }
                            ui.separator();
                            if ui
                                .add_sized(
                                    [58.0, 20.0],
                                    egui::Button::selectable(
                                        self.map_knife_on,
                                        egui::RichText::new("✂ Knife").small(),
                                    ),
                                )
                                .on_hover_text(format!(
                                    "cut a face from one edge or corner to another  ({})",
                                    k(crate::map_keys::MapCmd::Knife)
                                ))
                                .clicked()
                            {
                                self.cmd.set_map_knife = Some(!self.map_knife_on);
                            }
                        });
                        // Second line: gizmo · handles · what's armed.
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("▦").small().weak());
                            let cycle = |ui: &mut egui::Ui, text: &str, hover: String| {
                                ui.add(egui::Button::new(egui::RichText::new(text).small()).frame(false))
                                    .on_hover_text(hover)
                                    .clicked()
                            };
                            if cycle(
                                ui,
                                self.map_xform.label(),
                                format!("what the gizmo does — click or {} to cycle", k(crate::map_keys::MapCmd::GizmoCycle)),
                            ) {
                                *self.map_xform = match *self.map_xform {
                                    crate::map_edit::MapXform::Move => crate::map_edit::MapXform::Rotate,
                                    crate::map_edit::MapXform::Rotate => crate::map_edit::MapXform::Scale,
                                    crate::map_edit::MapXform::Scale => crate::map_edit::MapXform::Move,
                                };
                            }
                            ui.label(egui::RichText::new("·").weak().small());
                            if cycle(
                                ui,
                                self.map_orient.label(),
                                format!("handle orientation — click or {} to cycle", k(crate::map_keys::MapCmd::OrientCycle)),
                            ) {
                                *self.map_orient = match *self.map_orient {
                                    crate::map_edit::MapOrient::Normal => crate::map_edit::MapOrient::Local,
                                    crate::map_edit::MapOrient::Local => crate::map_edit::MapOrient::Global,
                                    crate::map_edit::MapOrient::Global => crate::map_edit::MapOrient::Normal,
                                };
                            }
                            if let Some(shape) = self.map_arm {
                                ui.label(egui::RichText::new("·").weak().small());
                                ui.colored_label(
                                    accent,
                                    egui::RichText::new(format!(
                                        "drawing {}",
                                        shape.label().trim_start_matches("Map ").to_lowercase()
                                    ))
                                    .small(),
                                );
                            }
                            ui.label(egui::RichText::new("·").weak().small());
                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new(if *self.map_hud_open { "⏶" } else { "⏷" })
                                            .small(),
                                    )
                                    .frame(false),
                                )
                                .on_hover_text(if *self.map_hud_open {
                                    "hide the shape picker"
                                } else {
                                    "show the shape picker"
                                })
                                .clicked()
                            {
                                *self.map_hud_open = !*self.map_hud_open;
                            }
                        });
                        if *self.map_hud_open {
                            ui.horizontal(|ui| {
                                for shape in crate::map_edit::MapShape::ALL {
                                    let armed = self.map_arm == Some(shape);
                                    let label = shape.label().trim_start_matches("Map ");
                                    if ui
                                        .add_sized(
                                            [62.0, 20.0],
                                            egui::Button::selectable(
                                                armed,
                                                format!("{label} {}", self.map_keys.label(shape.cmd())),
                                            ),
                                        )
                                        .on_hover_text("drag out the base, then the height (Esc cancels)")
                                        .clicked()
                                    {
                                        self.cmd.set_map_arm =
                                            Some(if armed { None } else { Some(shape) });
                                    }
                                }
                            });
                        }
                        // The context line: only what applies right now.
                        let hint = match self.map_arm {
                            Some(shape) => {
                                let mut h = format!(
                                    "drag the base, then the height  ·  {} {} turn  ·  {} flip",
                                    k(crate::map_keys::MapCmd::TurnLeft),
                                    k(crate::map_keys::MapCmd::TurnRight),
                                    k(crate::map_keys::MapCmd::TurnAround),
                                );
                                if shape.detail(*self.map_opts).is_some() {
                                    h.push_str(&format!(
                                        "  ·  {} {} resolution",
                                        k(crate::map_keys::MapCmd::ResolutionDown),
                                        k(crate::map_keys::MapCmd::ResolutionUp)
                                    ));
                                }
                                h.push_str("  ·  Esc cancel");
                                h
                            }
                            None if self.map_knife_on => {
                                "✂ click an edge or a corner, then another on the same face  \
                                 ·  keeps cutting from there  ·  Esc ends the cut"
                                    .to_string()
                            }
                            None => format!(
                                "click to select  ·  drag anywhere to box-select  ·  Shift adds, \
                                 Ctrl removes  ·  {} extrude  ·  {} inset",
                                k(crate::map_keys::MapCmd::Extrude),
                                k(crate::map_keys::MapCmd::Inset),
                            ),
                        };
                        ui.label(egui::RichText::new(hint).weak().small());
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
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 2.0;
                            ui.toggle_value(self.show_gizmos, "◎ Gizmos")
                                .on_hover_text("show viewport gizmos/overlays (H)");
                            // Plane lock — square the view to XY/ZY/XZ and keep
                            // it there. Building a 2D game in a 3D editor
                            // otherwise means re-achieving "flat" after every
                            // drag; locked, mouse-look does nothing and the
                            // movement keys slide you around the plane.
                            let lock = *self.view_lock;
                            ui.menu_button(
                                if lock.is_locked() {
                                    format!("▦ {}", lock.label())
                                } else {
                                    "▦ View".to_string()
                                },
                                |ui| {
                                    for l in floptle_render::ViewLock::ALL {
                                        if ui
                                            .selectable_label(lock == l, l.label())
                                            .clicked()
                                        {
                                            *self.view_lock = l;
                                            // Squaring the view to a plane is
                                            // almost always the 2D intent, so
                                            // bring the projection with it — a
                                            // locked PERSPECTIVE view still
                                            // draws a layer 2 units back at a
                                            // different scale, which is the
                                            // thing that stops two tilemaps
                                            // lining up. Free goes back to 3D.
                                            *self.view_ortho = l
                                                .is_locked()
                                                .then(|| self.view_ortho.unwrap_or(12.0));
                                            ui.close();
                                        }
                                    }
                                    ui.separator();
                                    // …and it is still a separate switch, because
                                    // an orthographic FREE view is a real thing to
                                    // want (a technical shot, an isometric look).
                                    let mut ortho = self.view_ortho.is_some();
                                    if ui
                                        .checkbox(&mut ortho, "Orthographic")
                                        .on_hover_text(
                                            "draw everything at the same scale at every \
                                             distance. The wheel zooms the view height \
                                             instead of moving the camera.",
                                        )
                                        .changed()
                                    {
                                        *self.view_ortho = ortho.then_some(12.0);
                                    }
                                    if let Some(h) = self.view_ortho.as_mut() {
                                        ui.horizontal(|ui| {
                                            ui.label("height");
                                            ui.add(
                                                egui::DragValue::new(h)
                                                    .speed(0.2)
                                                    .range(0.02..=100_000.0)
                                                    .suffix(" units"),
                                            );
                                        });
                                    }
                                    ui.separator();
                                    ui.small(
                                        "A locked view ignores mouse-look. Middle-drag pans, \
                                         the wheel zooms, and W/A/S/D slide around the plane \
                                         (Space/Ctrl step along the view axis).",
                                    );
                                },
                            )
                            .response
                            .on_hover_text(
                                "lock the Scene view square to a plane — for 2D and for \
                                 blockout work",
                            );
                            ui.separator();
                            ui.menu_button("⏷", |ui| {
                                ui.add_enabled_ui(*self.show_gizmos, |ui| {
                                    let f = &mut *self.gizmo_filter;
                                    ui.checkbox(&mut f.cameras, "Cameras");
                                    ui.checkbox(&mut f.lights, "Lights & gravity");
                                    ui.checkbox(&mut f.physics, "Rigidbodies & contacts");
                                    ui.checkbox(&mut f.colliders, "Collider wireframes");
                                    ui.checkbox(&mut f.particles, "Particle emitters");
                                    ui.checkbox(&mut f.script, "Script gizmos (Lua)");
                                    ui.indent("script_gizmo_game", |ui| {
                                        ui.add_enabled_ui(f.script, |ui| {
                                            ui.checkbox(self.game_gizmos, "…also in Game view")
                                                .on_hover_text(
                                                    "draw Lua gizmo.* shapes over the Game \
                                                     view too — hit/hurtboxes while you \
                                                     actually play. Off keeps the Game view \
                                                     showing exactly what a player sees.",
                                                );
                                        });
                                    });
                                    ui.separator();
                                    if ui.button("All on").clicked() {
                                        *f = crate::GizmoFilter::default();
                                    }
                                });
                            })
                            .response
                            .on_hover_text("filter which gizmo types draw");
                        });
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
            paint_gizmo(&painter, g, self.gizmo_tool, self.grabbed, self.ppp);
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

        // Vertex-paint telegraph: a ring on the surface under the cursor, so a dab is
        // never a surprise. Magenta, to read as clearly NOT the terrain brush.
        if let Some(viz) = self.paint_viz.filter(|_| !game) {
            let painter = ui
                .ctx()
                .layer_painter(egui::LayerId::new(egui::Order::Background, egui::Id::new("vertex_brush")))
                .with_clip_rect(rect);
            let ppp = self.ppp;
            let pt = |v: Vec2| egui::pos2(v.x / ppp, v.y / ppp);
            if viz.ring.len() >= 2 {
                let mut pts: Vec<egui::Pos2> = viz.ring.iter().map(|v| pt(*v)).collect();
                pts.push(pts[0]); // close the loop
                painter.line(pts, egui::Stroke::new(2.0, egui::Color32::from_rgb(255, 120, 220)));
            }
        }

        // ◫ Tiles overlay: the grid, the collision the tileset gives it, where a
        // click will land, and the selection. Drawn under the Map overlay because
        // the two tools are never both active.
        if let Some(viz) = self.tile_viz.as_ref().filter(|_| !game) {
            let painter = ui
                .ctx()
                .layer_painter(egui::LayerId::new(
                    egui::Order::Background,
                    egui::Id::new("tile_edit"),
                ))
                .with_clip_rect(rect);
            let ppp = self.ppp;
            let pt = |v: Vec2| egui::pos2(v.x / ppp, v.y / ppp);
            let closed = |painter: &egui::Painter, ring: &[Vec2], stroke: egui::Stroke| {
                let mut pts: Vec<egui::Pos2> = ring.iter().map(|v| pt(*v)).collect();
                if let Some(&first) = pts.first() {
                    pts.push(first);
                }
                painter.line(pts, stroke);
            };
            let faint = egui::Color32::from_rgba_unmultiplied(150, 165, 190, 46);
            for &(a, b) in &viz.grid {
                painter.line_segment([pt(a), pt(b)], egui::Stroke::new(1.0, faint));
            }
            if !viz.bounds.is_empty() {
                closed(
                    &painter,
                    &viz.bounds,
                    egui::Stroke::new(1.6, egui::Color32::from_rgba_unmultiplied(150, 175, 210, 150)),
                );
            }
            // Collision in red, matching the palette's overlay — the same fact
            // shown the same colour in both places it appears.
            for ring in &viz.collision {
                closed(
                    &painter,
                    ring,
                    egui::Stroke::new(1.6, egui::Color32::from_rgba_unmultiplied(255, 110, 110, 190)),
                );
            }
            if let Some(ring) = &viz.selection {
                closed(&painter, ring, egui::Stroke::new(2.0, egui::Color32::from_rgb(120, 220, 255)));
            }
            if let Some(ring) = &viz.band {
                closed(&painter, ring, egui::Stroke::new(2.0, egui::Color32::from_rgb(255, 200, 80)));
            }
            if let Some(ring) = &viz.cursor {
                closed(&painter, ring, egui::Stroke::new(2.0, egui::Color32::WHITE));
            }
        }

        // Map tool overlay: the target mesh's wireframe with selection/hover
        // highlights, plus the box-select rectangle.
        if let Some(viz) = self.map_viz.as_ref().filter(|_| !game) {
            let painter = ui
                .ctx()
                .layer_painter(egui::LayerId::new(egui::Order::Background, egui::Id::new("map_edit")))
                .with_clip_rect(rect);
            let ppp = self.ppp;
            let pt = |v: Vec2| egui::pos2(v.x / ppp, v.y / ppp);
            let wire = egui::Color32::from_rgba_unmultiplied(160, 170, 185, 140);
            let sel_col = egui::Color32::from_rgb(255, 200, 80);
            // Depth cues: near geometry draws bright and solid, far geometry
            // fades and thins, and anything the mesh's own front surface hides
            // draws faint. Without them every edge of a box is identical
            // whichever side of it you are looking at, and you cannot tell what
            // a click is about to grab. Selection ignores the fade — a selected
            // edge has to stay findable wherever it is.
            for e in &viz.edges {
                let stroke = if e.selected {
                    egui::Stroke::new(if e.behind { 1.8 } else { 2.5 }, sel_col)
                } else {
                    // 1.5px in front and near, 0.7px behind and far.
                    let width = (1.5 - e.depth * 0.5) * if e.behind { 0.6 } else { 1.0 };
                    let alpha = (200.0 - e.depth * 90.0) * if e.behind { 0.32 } else { 1.0 };
                    egui::Stroke::new(width, wire.gamma_multiply(alpha / 255.0))
                };
                painter.line_segment([pt(e.a), pt(e.b)], stroke);
            }
            for ring in &viz.sel_faces {
                let mut pts: Vec<egui::Pos2> = ring.iter().map(|v| pt(*v)).collect();
                if let Some(&first) = pts.first() {
                    pts.push(first);
                }
                painter.line(pts, egui::Stroke::new(2.5, sel_col));
            }
            for &(a, b) in &viz.hover {
                painter.line_segment([pt(a), pt(b)], egui::Stroke::new(2.0, egui::Color32::WHITE));
            }
            if viz.show_verts {
                for v in &viz.verts {
                    // A vertex round the back draws as a RING, not a dot — the
                    // fastest read there is for "that one is behind the
                    // surface", and it survives being the same colour.
                    let (r, c) = if v.selected {
                        (4.0 - v.depth * 0.8, sel_col)
                    } else {
                        let alpha = (210.0 - v.depth * 100.0) * if v.behind { 0.45 } else { 1.0 };
                        (2.8 - v.depth * 0.9, wire.gamma_multiply(alpha / 255.0))
                    };
                    if v.behind {
                        painter.circle_stroke(pt(v.p), r, egui::Stroke::new(1.0, c));
                    } else {
                        painter.circle_filled(pt(v.p), r, c);
                    }
                }
            }
            // Draw-tool preview: the candidate shape's wireframe, its footprint
            // on the build plane, the height axis, and the live size readout.
            let ghost = egui::Color32::from_rgb(120, 220, 255);
            for &(a, b) in &viz.preview {
                painter.line_segment([pt(a), pt(b)], egui::Stroke::new(1.5, ghost));
            }
            if viz.base_ring.len() >= 3 {
                let mut pts: Vec<egui::Pos2> = viz.base_ring.iter().map(|v| pt(*v)).collect();
                pts.push(pts[0]);
                painter.add(egui::Shape::convex_polygon(
                    pts.clone(),
                    egui::Color32::from_rgba_unmultiplied(120, 220, 255, 22),
                    egui::Stroke::NONE,
                ));
                painter.line(pts, egui::Stroke::new(2.5, ghost));
            }
            if let Some((a, b)) = viz.height_axis {
                painter.line_segment([pt(a), pt(b)], egui::Stroke::new(2.0, sel_col));
            }
            // Climb direction: an arrow from the low end to the high end.
            if let Some((lo, hi)) = viz.arrow {
                let (a, b) = (pt(lo), pt(hi));
                let col = egui::Color32::from_rgb(120, 255, 170);
                painter.line_segment([a, b], egui::Stroke::new(2.5, col));
                crate::gizmo::arrow_head(&painter, a, b, col);
                // Label the high end so the arrow can't be read backwards.
                let font = egui::FontId::proportional(11.0);
                let galley = painter.layout_no_wrap("up".into(), font, col);
                let at = b + (b - a).normalized() * 10.0 - galley.size() * 0.5;
                painter.rect_filled(
                    egui::Rect::from_min_size(at, galley.size()).expand(2.0),
                    2.0,
                    egui::Color32::from_black_alpha(170),
                );
                painter.galley(at, galley, col);
            }
            if let Some((at, text)) = viz.label.as_ref() {
                let p = pt(*at) + egui::vec2(14.0, 14.0);
                let font = egui::FontId::monospace(12.0);
                let galley = painter.layout_no_wrap(text.clone(), font, egui::Color32::WHITE);
                painter.rect_filled(
                    egui::Rect::from_min_size(p, galley.size()).expand(3.0),
                    3.0,
                    egui::Color32::from_black_alpha(190),
                );
                painter.galley(p, galley, egui::Color32::WHITE);
            }
            if let Some((a, b)) = viz.rect {
                let r = egui::Rect::from_two_pos(pt(a), pt(b));
                painter.rect_filled(r, 0.0, egui::Color32::from_rgba_unmultiplied(255, 200, 80, 18));
                painter.rect_stroke(r, 0.0, egui::Stroke::new(1.0, sel_col), egui::StrokeKind::Inside);
            }
            // ✂ Knife: the pending cut and where it would land. The end point is
            // drawn as a RING on an existing corner and a dot mid-edge, so you
            // can see before you click whether the cut reuses a corner or makes
            // a new one.
            // A cut that WOULD be refused draws grey and says why, right at the
            // cursor — the answer arrives while you are still aiming instead of
            // after a click that appeared to do nothing.
            let refused = viz.knife_why.is_some();
            let cut_col = if refused {
                egui::Color32::from_rgb(150, 150, 160)
            } else {
                egui::Color32::from_rgb(255, 120, 120)
            };
            if let Some((p, on_corner)) = viz.knife_to {
                if let Some(from) = viz.knife_from {
                    painter.line_segment([pt(from), pt(p)], egui::Stroke::new(2.0, cut_col));
                }
                if on_corner {
                    painter.circle_stroke(pt(p), 6.0, egui::Stroke::new(2.0, cut_col));
                } else {
                    painter.circle_filled(pt(p), 4.0, cut_col);
                }
                if let Some(why) = viz.knife_why.as_ref() {
                    let at = pt(p) + egui::vec2(12.0, 12.0);
                    let font = egui::FontId::proportional(11.0);
                    let galley = painter.layout_no_wrap(why.clone(), font, cut_col);
                    painter.rect_filled(
                        egui::Rect::from_min_size(at, galley.size()).expand(3.0),
                        3.0,
                        egui::Color32::from_black_alpha(200),
                    );
                    painter.galley(at, galley, cut_col);
                }
            }
            if let Some(from) = viz.knife_from {
                painter.circle_filled(pt(from), 4.0, cut_col);
                painter.circle_stroke(pt(from), 7.0, egui::Stroke::new(1.5, cut_col));
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

        // Script debug gizmos (`gizmo.*`). The Game view stays clean by default — it's
        // what the player would see — but "Also in Game view" opts in, because checking
        // whether a hitbox reaches is something you do with the controller in your hands.
        let script_lines: &[(Vec2, Vec2, [f32; 3])] =
            if game { self.game_gizmo_lines } else { self.script_gizmo_lines };
        if !script_lines.is_empty() {
            let painter = ui
                .ctx()
                .layer_painter(egui::LayerId::new(egui::Order::Background, egui::Id::new("script_gizmos")))
                .with_clip_rect(rect);
            let ppp = self.ppp;
            let pt = |v: Vec2| egui::pos2(v.x / ppp, v.y / ppp);
            for (a, b, c) in script_lines {
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
