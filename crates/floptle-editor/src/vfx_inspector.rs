//! The particle **track** inspector — shown in the Inspector tab whenever the
//! Particles tab is up and a track is selected, so VFX artists edit a track's
//! full look/behaviour in the roomy Inspector instead of a cramped bottom panel.
//! Reverts to the normal node inspector when the track is deselected (or a scene
//! node is picked). Every field is a constant or a drawn curve (see
//! [`crate::curve_edit`]); automation lanes shape birth values over effect time.

use floptle_scene::{
    VfxBlendDoc, VfxCurveDoc, VfxExtrapolateDoc, VfxKeyDoc, VfxInterpDoc, VfxLaneDoc,
    VfxLaneTargetDoc, VfxRenderDoc, VfxShapeDoc, VfxSpaceDoc, VfxValueDoc,
};

use crate::EditorTabViewer;
use crate::assets::{collect_model_paths, collect_texture_paths};
use crate::curve_edit::{CurveKind, curve_editor, sparkline, value_or_curve};

impl EditorTabViewer<'_> {
    /// True when the Inspector should show the selected particle track instead of
    /// the node inspector.
    pub(crate) fn vfx_track_active(&self) -> bool {
        self.particles_active
            && self.vfx_ui.open_key.is_some()
            && self.vfx_ui.sel_track.is_some()
    }

    pub(crate) fn vfx_track_inspector_ui(&mut self, ui: &mut egui::Ui) {
        // Asset lists for the pickers (borrow asset_tree before the doc).
        let mut tex_list = Vec::new();
        collect_texture_paths(self.asset_tree, &mut tex_list);
        let mut model_list = Vec::new();
        collect_model_paths(self.asset_tree, &mut model_list);

        let st = &mut *self.vfx_ui;
        let Some(mut doc) = st.doc.take() else {
            return;
        };
        let Some(ti) = st.sel_track else {
            st.doc = Some(doc);
            return;
        };
        if ti >= doc.tracks.len() {
            st.sel_track = None;
            st.doc = Some(doc);
            return;
        }
        let effect_name = doc.name.clone();
        let n_tracks = doc.tracks.len();
        let mut dirty = false;

        ui.horizontal(|ui| {
            ui.strong("❋ Track");
            ui.weak(format!("· {effect_name}"));
        });

        // Selected clip / burst numeric detail (the thing you just grabbed).
        clip_burst_detail(ui, st, &mut doc, &mut dirty);

        // Name + enable + reorder + delete.
        let mut reorder: i32 = 0;
        let mut delete = false;
        ui.horizontal(|ui| {
            if let Some(t) = doc.tracks.get_mut(ti) {
                dirty |= ui.add(egui::TextEdit::singleline(&mut t.name).desired_width(130.0)).changed();
                dirty |= ui.checkbox(&mut t.enabled, "on").changed();
            }
            if ti > 0 && ui.small_button("⬆").on_hover_text("move up").clicked() {
                reorder = -1;
            }
            if ti + 1 < n_tracks && ui.small_button("⬇").on_hover_text("move down").clicked() {
                reorder = 1;
            }
            if ui.small_button("🗑").on_hover_text("delete track").clicked() {
                delete = true;
            }
        });
        if reorder != 0 {
            let nj = (ti as i32 + reorder) as usize;
            doc.tracks.swap(ti, nj);
            st.sel_track = Some(nj);
            st.doc = Some(doc);
            st.mark_dirty();
            return;
        }
        if delete {
            doc.tracks.remove(ti);
            st.sel_track = None;
            st.sel = None;
            st.doc = Some(doc);
            st.mark_dirty();
            return;
        }

        let track = &mut doc.tracks[ti];
        egui::ScrollArea::vertical().id_salt("vfx_track_insp").show(ui, |ui| {
            look_section(ui, track, &tex_list, &model_list, &mut dirty);
            ui.separator();
            emission_section(ui, ti, track, &mut dirty);
            ui.separator();
            particle_section(ui, track, st, &mut dirty);
            ui.separator();
            automation_section(ui, track, st, &mut dirty);
        });

        st.doc = Some(doc);
        if dirty {
            st.mark_dirty();
        }
    }
}

fn clip_burst_detail(
    ui: &mut egui::Ui,
    st: &crate::vfx_ui::VfxUiState,
    doc: &mut floptle_scene::VfxEffectDoc,
    dirty: &mut bool,
) {
    use crate::vfx_ui::VfxSel;
    match st.sel {
        Some(VfxSel::Clip(ti, ci)) => {
            if let Some(c) = doc.tracks.get_mut(ti).and_then(|t| t.clips.get_mut(ci)) {
                ui.horizontal(|ui| {
                    ui.small("▬ clip");
                    ui.label("start");
                    *dirty |= ui.add(egui::DragValue::new(&mut c.start).speed(0.01).suffix("s")).changed();
                    ui.label("end");
                    *dirty |= ui.add(egui::DragValue::new(&mut c.end).speed(0.01).suffix("s")).changed();
                });
                if c.end < c.start + 0.02 {
                    c.end = c.start + 0.02;
                }
            }
        }
        Some(VfxSel::Burst(ti, bi)) => {
            if let Some(b) = doc.tracks.get_mut(ti).and_then(|t| t.bursts.get_mut(bi)) {
                ui.horizontal(|ui| {
                    ui.small("✦ burst");
                    ui.label("t");
                    *dirty |= ui.add(egui::DragValue::new(&mut b.t).speed(0.01).suffix("s")).changed();
                    ui.label("count");
                    *dirty |= ui.add(egui::DragValue::new(&mut b.count).speed(0.2).range(1..=100_000)).changed();
                });
            }
        }
        None => {}
    }
}

fn look_section(
    ui: &mut egui::Ui,
    track: &mut floptle_scene::VfxTrackDoc,
    tex_list: &[String],
    model_list: &[String],
    dirty: &mut bool,
) {
    ui.strong("Look");
    // Render mode: billboard (textured quad) vs instanced mesh.
    let is_mesh = matches!(track.render, VfxRenderDoc::Mesh { .. });
    egui::ComboBox::from_id_salt("vfx_rendermode")
        .selected_text(if is_mesh { "3D mesh" } else { "billboard" })
        .show_ui(ui, |ui| {
            if ui.selectable_label(!is_mesh, "billboard (camera-facing quad)").clicked() && is_mesh {
                track.render = VfxRenderDoc::Billboard { texture: None };
                *dirty = true;
            }
            if ui.selectable_label(is_mesh, "3D mesh (instanced, lit)").clicked() && !is_mesh {
                track.render = VfxRenderDoc::Mesh { asset_path: String::new() };
                *dirty = true;
            }
        });
    match &mut track.render {
        VfxRenderDoc::Billboard { texture } => {
            ui.horizontal(|ui| {
                ui.label("texture");
                egui::ComboBox::from_id_salt("vfx_tex")
                    .width(160.0)
                    .selected_text(short(texture.as_deref().unwrap_or("(plain quad)")))
                    .show_ui(ui, |ui| {
                        if ui.selectable_label(texture.is_none(), "(plain quad)").clicked() {
                            *texture = None;
                            *dirty = true;
                        }
                        for p in tex_list {
                            if ui.selectable_label(texture.as_deref() == Some(p), short(p)).clicked() {
                                *texture = Some(p.clone());
                                *dirty = true;
                            }
                        }
                    });
            });
            // Blend only matters for billboards (mesh particles composite through
            // the raster transparent pass by alpha).
            egui::ComboBox::from_id_salt("vfx_blend")
                .selected_text(match track.blend {
                    VfxBlendDoc::Alpha => "blend: alpha",
                    VfxBlendDoc::Additive => "blend: additive (glow)",
                })
                .show_ui(ui, |ui| {
                    for (v, l) in [(VfxBlendDoc::Alpha, "alpha"), (VfxBlendDoc::Additive, "additive (glow)")] {
                        if ui.selectable_label(track.blend == v, l).clicked() && track.blend != v {
                            track.blend = v;
                            *dirty = true;
                        }
                    }
                });
        }
        VfxRenderDoc::Mesh { asset_path } => {
            ui.horizontal(|ui| {
                ui.label("model");
                egui::ComboBox::from_id_salt("vfx_mesh")
                    .width(160.0)
                    .selected_text(if asset_path.is_empty() { "(pick a model)".into() } else { short(asset_path) })
                    .show_ui(ui, |ui| {
                        for p in model_list {
                            if ui.selectable_label(asset_path == p, short(p)).clicked() {
                                *asset_path = p.clone();
                                *dirty = true;
                            }
                        }
                    });
            });
            ui.small("mesh particles are lit + sun-shadowed like scene meshes");
        }
    }
    // Lighting / shadow opt-ins (off by default — proposal §5).
    ui.horizontal(|ui| {
        *dirty |= ui.checkbox(&mut track.lit, "lit").on_hover_text("full scene lighting per particle").changed();
        *dirty |= ui.checkbox(&mut track.cast_shadows, "casts shadow").on_hover_text("the track's cloud darkens the ground (aggregate proxy)").changed();
    });
    egui::ComboBox::from_id_salt("vfx_space")
        .selected_text(match track.space {
            VfxSpaceDoc::Local => "space: local (follows node)",
            VfxSpaceDoc::World => "space: world (trails)",
        })
        .show_ui(ui, |ui| {
            for (v, l) in [(VfxSpaceDoc::Local, "local (follows node)"), (VfxSpaceDoc::World, "world (trails)")] {
                if ui.selectable_label(track.space == v, l).clicked() && track.space != v {
                    track.space = v;
                    *dirty = true;
                }
            }
        });
}

fn emission_section(ui: &mut egui::Ui, ti: usize, track: &mut floptle_scene::VfxTrackDoc, dirty: &mut bool) {
    ui.strong("Emission");
    ui.horizontal(|ui| {
        ui.label("rate");
        *dirty |= ui
            .add(egui::DragValue::new(&mut track.rate).speed(0.5).range(0.0..=100_000.0).suffix("/s"))
            .on_hover_text("particles per second while the playhead is inside a clip")
            .changed();
        ui.label("life");
        *dirty |= ui
            .add(egui::DragValue::new(&mut track.particle_lifetime).speed(0.01).range(0.01..=600.0).suffix("s"))
            .changed();
    });
    ui.horizontal(|ui| {
        ui.label("life jitter");
        *dirty |= ui.add(egui::Slider::new(&mut track.lifetime_jitter, 0.0..=1.0)).changed();
    });
    shape_editor(ui, ti, track, dirty);
}

fn shape_editor(ui: &mut egui::Ui, ti: usize, track: &mut floptle_scene::VfxTrackDoc, dirty: &mut bool) {
    let label = match track.shape {
        VfxShapeDoc::Point => "point",
        VfxShapeDoc::Cone { .. } => "cone",
        VfxShapeDoc::Sphere { .. } => "sphere",
        VfxShapeDoc::Edge { .. } => "edge (slash arc)",
        VfxShapeDoc::Ring { .. } => "ring",
    };
    ui.horizontal(|ui| {
        ui.label("shape");
        egui::ComboBox::from_id_salt(("vfx_shape", ti)).selected_text(label).show_ui(ui, |ui| {
            let opts: [(&str, VfxShapeDoc); 5] = [
                ("point", VfxShapeDoc::Point),
                ("cone", VfxShapeDoc::Cone { angle: 25.0, radius: 0.1 }),
                ("sphere", VfxShapeDoc::Sphere { radius: 0.5, shell: false }),
                ("edge (slash arc)", VfxShapeDoc::Edge { length: 1.0 }),
                ("ring", VfxShapeDoc::Ring { radius: 0.5 }),
            ];
            for (l, v) in opts {
                let same = std::mem::discriminant(&track.shape) == std::mem::discriminant(&v);
                if ui.selectable_label(same, l).clicked() && !same {
                    track.shape = v;
                    *dirty = true;
                }
            }
        });
    });
    match &mut track.shape {
        VfxShapeDoc::Point => {}
        VfxShapeDoc::Cone { angle, radius } => {
            ui.horizontal(|ui| {
                ui.label("angle");
                *dirty |= ui.add(egui::DragValue::new(angle).speed(0.5).range(0.0..=180.0).suffix("°")).changed();
                ui.label("radius");
                *dirty |= ui.add(egui::DragValue::new(radius).speed(0.01).range(0.0..=100.0)).changed();
            });
        }
        VfxShapeDoc::Sphere { radius, shell } => {
            ui.horizontal(|ui| {
                ui.label("radius");
                *dirty |= ui.add(egui::DragValue::new(radius).speed(0.01).range(0.0..=100.0)).changed();
                *dirty |= ui.checkbox(shell, "shell only").changed();
            });
        }
        VfxShapeDoc::Edge { length } => {
            ui.horizontal(|ui| {
                ui.label("length");
                *dirty |= ui.add(egui::DragValue::new(length).speed(0.01).range(0.0..=1000.0)).changed();
            });
        }
        VfxShapeDoc::Ring { radius } => {
            ui.horizontal(|ui| {
                ui.label("radius");
                *dirty |= ui.add(egui::DragValue::new(radius).speed(0.01).range(0.0..=1000.0)).changed();
            });
        }
    }
}

fn particle_section(
    ui: &mut egui::Ui,
    track: &mut floptle_scene::VfxTrackDoc,
    st: &mut crate::vfx_ui::VfxUiState,
    dirty: &mut bool,
) {
    ui.strong("Over each particle's life");
    ui.small("hover the value, tap ∿ to animate it into a curve");
    let (exp, sk, vr) = (&mut st.expanded_prop, &mut st.sel_key, &mut st.curve_vrange);
    *dirty |= value_or_curve(ui, "velocity", &mut track.velocity, exp, sk, vr);
    *dirty |= value_or_curve(ui, "size", &mut track.size, exp, sk, vr);
    *dirty |= value_or_curve(ui, "rotation", &mut track.rotation, exp, sk, vr);
    *dirty |= value_or_curve(ui, "color", &mut track.color, exp, sk, vr);
    ui.horizontal(|ui| {
        ui.label("gravity");
        *dirty |= ui.add(egui::Slider::new(&mut track.gravity, 0.0..=2.0)).changed();
        ui.label("drag");
        *dirty |= ui.add(egui::DragValue::new(&mut track.drag).speed(0.01).range(0.0..=50.0)).changed();
    });
}

fn automation_section(
    ui: &mut egui::Ui,
    track: &mut floptle_scene::VfxTrackDoc,
    st: &mut crate::vfx_ui::VfxUiState,
    dirty: &mut bool,
) {
    ui.horizontal(|ui| {
        ui.strong("Automation");
        ui.small("(over the effect's timeline)");
    });
    let mut remove: Option<usize> = None;
    for (li, lane) in track.automation.iter_mut().enumerate() {
        let label = format!("lane:{li}");
        ui.horizontal(|ui| {
            ui.label(lane_label(lane.target));
            let rect = ui.allocate_exact_size(egui::vec2(70.0, 16.0), egui::Sense::click());
            sparkline(ui, &lane.curve, CurveKind::Scalar, rect.0);
            if rect.1.on_hover_text("click to edit").clicked() {
                st.expanded_prop = if st.expanded_prop.as_deref() == Some(&label) {
                    None
                } else {
                    st.sel_key = None;
                    Some(label.clone())
                };
            }
            if ui.small_button("🗑").clicked() {
                remove = Some(li);
            }
        });
        if st.expanded_prop.as_deref() == Some(&label) {
            *dirty |= curve_editor(ui, &mut lane.curve, &mut st.sel_key, &mut st.curve_vrange);
        }
    }
    if let Some(li) = remove {
        track.automation.remove(li);
        *dirty = true;
    }
    // Add a lane for a target not already present.
    let present: Vec<VfxLaneTargetDoc> = track.automation.iter().map(|l| l.target).collect();
    let avail: Vec<VfxLaneTargetDoc> = ALL_TARGETS.iter().copied().filter(|t| !present.contains(t)).collect();
    if !avail.is_empty() {
        egui::ComboBox::from_id_salt("vfx_add_lane")
            .selected_text("＋ add automation")
            .show_ui(ui, |ui| {
                for t in avail {
                    if ui.selectable_label(false, lane_label(t)).clicked() {
                        track.automation.push(VfxLaneDoc { target: t, curve: flat_lane_curve() });
                        *dirty = true;
                    }
                }
            });
    }
}

const ALL_TARGETS: [VfxLaneTargetDoc; 6] = [
    VfxLaneTargetDoc::Rate,
    VfxLaneTargetDoc::Count,
    VfxLaneTargetDoc::Speed,
    VfxLaneTargetDoc::Size,
    VfxLaneTargetDoc::Tint,
    VfxLaneTargetDoc::ShapeScale,
];

fn lane_label(t: VfxLaneTargetDoc) -> &'static str {
    match t {
        VfxLaneTargetDoc::Rate => "× rate",
        VfxLaneTargetDoc::Count => "× burst count",
        VfxLaneTargetDoc::Speed => "× speed",
        VfxLaneTargetDoc::Size => "× size",
        VfxLaneTargetDoc::Tint => "× tint",
        VfxLaneTargetDoc::ShapeScale => "× shape scale",
    }
}

/// A flat multiplier-1 curve — an automation lane starts as a no-op you shape.
fn flat_lane_curve() -> VfxCurveDoc {
    VfxCurveDoc {
        keys: vec![
            VfxKeyDoc { t: 0.0, v: VfxValueDoc::F32(1.0), interp: VfxInterpDoc::Linear, in_tan: 0.0, out_tan: 0.0 },
            VfxKeyDoc { t: 1.0, v: VfxValueDoc::F32(1.0), interp: VfxInterpDoc::Linear, in_tan: 0.0, out_tan: 0.0 },
        ],
        extrapolate: VfxExtrapolateDoc::Clamp,
    }
}

/// Shorten a long asset path to the last two components for a picker label.
fn short(p: &str) -> String {
    let parts: Vec<&str> = p.rsplit(['/', '\\']).take(2).collect();
    parts.into_iter().rev().collect::<Vec<_>>().join("/")
}
