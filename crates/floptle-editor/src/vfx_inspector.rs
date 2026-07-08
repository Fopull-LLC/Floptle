//! The particle **track** inspector — shown in the Inspector tab whenever the
//! Particles tab is up and a track is selected, so VFX artists edit a track's
//! full look/behaviour in the roomy Inspector instead of a cramped bottom panel.
//! Reverts to the normal node inspector when the track is deselected (or a scene
//! node is picked). Every field is a constant or a drawn curve (see
//! [`crate::curve_edit`]); automation lanes shape birth values over effect time.

use floptle_scene::{
    VfxBlendDoc, VfxEmitDoc, VfxFlipModeDoc, VfxFlipbookDoc, VfxForceDoc, VfxInterpDoc,
    VfxOrientDoc, VfxRenderDoc, VfxShapeDoc, VfxSpaceDoc, VfxValueDoc,
};

use crate::EditorTabViewer;
use crate::assets::{collect_model_paths, collect_texture_paths};
use crate::curve_edit::value_or_curve;
use crate::vfx_ui::{LaneRef, lane_curve_mut, lane_fixed_range, lane_ref_label};

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
        let dur = doc.lifetime.max(0.01);
        let mut dirty = false;

        ui.horizontal(|ui| {
            ui.strong("✨ Track");
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
            // Collapsible sections so a track's controls stay scannable — collapse the
            // ones you're not touching. Look/Emission/Life open by default.
            let section = |ui: &mut egui::Ui, id: &str, title: &str, open: bool, body: &mut dyn FnMut(&mut egui::Ui)| {
                egui::CollapsingHeader::new(egui::RichText::new(title).strong())
                    .id_salt(id)
                    .default_open(open)
                    .show(ui, |ui| body(ui));
            };
            section(ui, "vfx_look", "🎨  Look", true, &mut |ui| {
                look_section(ui, track, &tex_list, &model_list, &mut dirty)
            });
            section(ui, "vfx_emit", "✳  Emission", true, &mut |ui| {
                emission_section(ui, ti, track, &mut dirty)
            });
            section(ui, "vfx_life", "📈  Over each particle's life", true, &mut |ui| {
                particle_section(ui, track, st, &mut dirty)
            });
            section(ui, "vfx_forces", "💨  Forces", false, &mut |ui| {
                forces_section(ui, track, &mut dirty)
            });
            ui.add_space(2.0);
            selected_point_section(ui, ti, track, st, dur, &mut dirty);
        });

        st.doc = Some(doc);
        if dirty {
            st.mark_dirty();
        }
    }
}

/// The selected clip's editor: placement (start + length, where length IS the particle
/// lifetime), lifetime jitter, and its emission mode — a continuous stream (rate) or a
/// burst-train (count ± jitter, repeated `pulses` times every `interval` ± jitter).
fn clip_burst_detail(
    ui: &mut egui::Ui,
    st: &crate::vfx_ui::VfxUiState,
    doc: &mut floptle_scene::VfxEffectDoc,
    dirty: &mut bool,
) {
    use crate::vfx_ui::VfxSel;
    let Some(VfxSel::Clip(ti, ci)) = st.sel else { return };
    let Some(c) = doc.tracks.get_mut(ti).and_then(|t| t.clips.get_mut(ci)) else { return };
    // A legacy clip may have no emit yet (normally filled on load) — default to a stream.
    if c.emit.is_none() {
        c.emit = Some(VfxEmitDoc::Rate { rate: 10.0 });
    }
    let is_burst = matches!(c.emit, Some(VfxEmitDoc::Burst { .. }));

    ui.horizontal(|ui| {
        ui.small(if is_burst { "✳ burst clip" } else { "▪ stream clip" });
        egui::ComboBox::from_id_salt("vfx_emit_mode")
            .selected_text(if is_burst { "burst" } else { "stream" })
            .show_ui(ui, |ui| {
                if ui.selectable_label(!is_burst, "stream (rate)").clicked() && is_burst {
                    c.emit = Some(VfxEmitDoc::Rate { rate: 10.0 });
                    *dirty = true;
                }
                if ui.selectable_label(is_burst, "burst (pulses)").clicked() && !is_burst {
                    c.emit = Some(VfxEmitDoc::Burst {
                        count: 12,
                        count_jitter: 0.0,
                        pulses: 1,
                        interval: 0.1,
                        interval_jitter: 0.0,
                    });
                    *dirty = true;
                }
            });
    });

    // Placement: start + length. Length is authoritative for lifetime.
    ui.horizontal(|ui| {
        ui.label("start");
        *dirty |= ui.add(egui::DragValue::new(&mut c.start).speed(0.01).range(0.0..=100_000.0).suffix("s")).changed();
        ui.label("life");
        let mut life = (c.end - c.start).max(1e-3);
        if ui
            .add(egui::DragValue::new(&mut life).speed(0.01).range(0.001..=100_000.0).suffix("s"))
            .on_hover_text("the clip's LENGTH on the timeline — how long its particles live")
            .changed()
        {
            c.end = c.start + life.max(1e-3);
            *dirty = true;
        }
        ui.label("± life");
        *dirty |= ui
            .add(egui::DragValue::new(&mut c.lifetime_jitter).speed(0.01).range(0.0..=1.0))
            .on_hover_text("± fraction of random variance on each particle's lifetime")
            .changed();
    });
    if c.end < c.start + 1e-3 {
        c.end = c.start + 1e-3;
    }

    // Emission-mode parameters.
    match c.emit.as_mut() {
        Some(VfxEmitDoc::Rate { rate }) => {
            ui.horizontal(|ui| {
                ui.label("rate");
                *dirty |= ui
                    .add(egui::DragValue::new(rate).speed(0.5).range(0.0..=100_000.0).suffix("/s"))
                    .on_hover_text("particles per second across the whole clip")
                    .changed();
            });
        }
        Some(VfxEmitDoc::Burst { count, count_jitter, pulses, interval, interval_jitter }) => {
            ui.horizontal(|ui| {
                ui.label("count");
                *dirty |= ui.add(egui::DragValue::new(count).speed(0.2).range(0..=1_000_000)).changed();
                ui.label("± count");
                *dirty |= ui
                    .add(egui::DragValue::new(count_jitter).speed(0.01).range(0.0..=1.0))
                    .on_hover_text("± fraction of random variance on each pulse's count")
                    .changed();
            });
            ui.horizontal(|ui| {
                ui.label("pulses");
                *dirty |= ui
                    .add(egui::DragValue::new(pulses).speed(0.1).range(1..=100_000))
                    .on_hover_text("how many bursts fire, from the clip start")
                    .changed();
                ui.label("every");
                *dirty |= ui
                    .add(egui::DragValue::new(interval).speed(0.01).range(0.0..=1000.0).suffix("s"))
                    .on_hover_text("delay between pulses")
                    .changed();
                ui.label("± delay");
                *dirty |= ui
                    .add(egui::DragValue::new(interval_jitter).speed(0.01).range(0.0..=1.0))
                    .on_hover_text("± fraction of random variance on each gap")
                    .changed();
            });
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
    // Render mode: billboard (textured quad) vs instanced mesh.
    let is_mesh = matches!(track.render, VfxRenderDoc::Mesh { .. });
    egui::ComboBox::from_id_salt("vfx_rendermode")
        .selected_text(if is_mesh { "3D mesh" } else { "billboard" })
        .show_ui(ui, |ui| {
            if ui.selectable_label(!is_mesh, "billboard (flat quad)").clicked() && is_mesh {
                track.render = VfxRenderDoc::Billboard { texture: None };
                *dirty = true;
            }
            if ui.selectable_label(is_mesh, "3D mesh (instanced, lit)").clicked() && !is_mesh {
                // Default to a built-in sphere so a fresh mesh track renders immediately
                // instead of showing nothing until you pick an asset.
                track.render = VfxRenderDoc::Mesh { asset_path: "builtin://sphere".to_string() };
                *dirty = true;
            }
        });
    match &mut track.render {
        VfxRenderDoc::Billboard { texture } => {
            ui.horizontal(|ui| {
                ui.label("texture");
                if let Some(pick) = crate::ui_widgets::searchable_picker(
                    ui,
                    egui::Id::new("vfx_tex"),
                    &short(texture.as_deref().unwrap_or("(plain quad)")),
                    Some("(plain quad)"),
                    tex_list,
                    160.0,
                ) {
                    *texture = pick;
                    *dirty = true;
                }
            });
            // Blend only matters for billboards (mesh particles composite through
            // the raster transparent pass by alpha).
            egui::ComboBox::from_id_salt("vfx_blend")
                .selected_text(match track.blend {
                    VfxBlendDoc::Alpha => "blend: alpha",
                    VfxBlendDoc::Additive => "blend: additive (glow)",
                    VfxBlendDoc::Premultiplied => "blend: premultiplied",
                    VfxBlendDoc::Screen => "blend: screen (lighten)",
                    VfxBlendDoc::Multiply => "blend: multiply (darken)",
                })
                .show_ui(ui, |ui| {
                    for (v, l) in [
                        (VfxBlendDoc::Alpha, "alpha"),
                        (VfxBlendDoc::Additive, "additive (glow)"),
                        (VfxBlendDoc::Premultiplied, "premultiplied"),
                        (VfxBlendDoc::Screen, "screen (lighten)"),
                        (VfxBlendDoc::Multiply, "multiply (darken)"),
                    ] {
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
                let selected = if asset_path.is_empty() {
                    "(pick a model)".to_string()
                } else if let Some(l) = crate::vfx::builtin_particle_mesh_label(asset_path) {
                    l.to_string()
                } else {
                    short(asset_path)
                };
                egui::ComboBox::from_id_salt("vfx_mesh")
                    .width(160.0)
                    .selected_text(selected)
                    .show_ui(ui, |ui| {
                        // Built-in primitives first, then a separator, then user assets.
                        for (key, label) in crate::vfx::BUILTIN_PARTICLE_MESHES {
                            if ui.selectable_label(asset_path == key, *label).clicked() {
                                *asset_path = (*key).to_string();
                                *dirty = true;
                            }
                        }
                        if !model_list.is_empty() {
                            ui.separator();
                        }
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
    // Billboard alignment + flipbook: both apply only to billboard tracks.
    if !is_mesh {
        orient_editor(ui, track, dirty);
        flipbook_editor(ui, track, dirty);
    }
    // Lighting / shadow opt-ins (off by default — proposal §5). They only affect
    // MESH particles — the billboard pass draws unlit textured quads — so grey them
    // out for billboards rather than offering a dead knob.
    ui.add_enabled_ui(is_mesh, |ui| {
        ui.horizontal(|ui| {
            *dirty |= ui.checkbox(&mut track.lit, "lit").on_hover_text("full scene lighting per particle (mesh particles only)").changed();
            *dirty |= ui.checkbox(&mut track.cast_shadows, "casts shadow").on_hover_text("the track's cloud darkens the ground — aggregate proxy (mesh particles only)").changed();
        });
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

/// A human label + one-line hint for each billboard alignment mode.
fn orient_label(o: VfxOrientDoc) -> (&'static str, &'static str) {
    match o {
        VfxOrientDoc::FaceCamera => ("face camera", "classic billboard — always turns its flat side to you"),
        VfxOrientDoc::Velocity => ("velocity (stretched)", "stretched along the particle's motion — sparks, rain, speed lines"),
        VfxOrientDoc::Vertical => ("upright (axis-locked)", "stands up on the world Y axis, yawing to you — flames, grass"),
        VfxOrientDoc::Horizontal => ("flat on ground", "lies flat in the ground plane — decals, shockwaves, ripples"),
        VfxOrientDoc::WorldFixed => ("world-fixed (birth)", "keeps the pose it was fired with — debris, cards"),
    }
}

/// Billboard alignment picker + aspect + (for velocity) stretch. This is what makes
/// a quad NOT face the camera.
fn orient_editor(ui: &mut egui::Ui, track: &mut floptle_scene::VfxTrackDoc, dirty: &mut bool) {
    const ALL: [VfxOrientDoc; 5] = [
        VfxOrientDoc::FaceCamera,
        VfxOrientDoc::Velocity,
        VfxOrientDoc::Vertical,
        VfxOrientDoc::Horizontal,
        VfxOrientDoc::WorldFixed,
    ];
    ui.horizontal(|ui| {
        ui.label("align").on_hover_text("how the flat quad is oriented in the world");
        egui::ComboBox::from_id_salt("vfx_orient")
            .width(168.0)
            .selected_text(orient_label(track.orient).0)
            .show_ui(ui, |ui| {
                for o in ALL {
                    let (lbl, hint) = orient_label(o);
                    if ui.selectable_label(track.orient == o, lbl).on_hover_text(hint).clicked()
                        && track.orient != o
                    {
                        track.orient = o;
                        *dirty = true;
                    }
                }
            });
    });
    ui.horizontal(|ui| {
        ui.label("aspect");
        *dirty |= ui
            .add(egui::DragValue::new(&mut track.aspect).speed(0.02).range(0.05..=20.0))
            .on_hover_text("width ÷ height. 1 = square, >1 = wide, <1 = tall")
            .changed();
        if track.orient == VfxOrientDoc::Velocity {
            ui.label("stretch");
            *dirty |= ui
                .add(egui::DragValue::new(&mut track.stretch).speed(0.05).range(0.1..=40.0))
                .on_hover_text("how far the quad stretches along its motion")
                .changed();
        }
    });
}

/// Sprite-sheet flipbook controls (billboard tracks): a cols×rows grid animated over
/// the particle's life or at a fixed fps.
fn flipbook_editor(ui: &mut egui::Ui, track: &mut floptle_scene::VfxTrackDoc, dirty: &mut bool) {
    let mut on = track.flipbook.is_some();
    if ui
        .checkbox(&mut on, "flipbook")
        .on_hover_text("animate a sprite-sheet texture (cols × rows of frames)")
        .changed()
    {
        track.flipbook = on.then_some(VfxFlipbookDoc {
            cols: 4,
            rows: 4,
            mode: VfxFlipModeDoc::OverLife,
            fps: 12.0,
        });
        *dirty = true;
    }
    let Some(f) = &mut track.flipbook else { return };
    ui.horizontal(|ui| {
        ui.label("grid");
        *dirty |= ui.add(egui::DragValue::new(&mut f.cols).range(1..=64).prefix("cols ")).changed();
        *dirty |= ui.add(egui::DragValue::new(&mut f.rows).range(1..=64).prefix("rows ")).changed();
    });
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("vfx_flipmode")
            .selected_text(match f.mode {
                VfxFlipModeDoc::OverLife => "over life",
                VfxFlipModeDoc::LoopFps => "loop @ fps",
            })
            .show_ui(ui, |ui| {
                for (v, l) in
                    [(VfxFlipModeDoc::OverLife, "over life"), (VfxFlipModeDoc::LoopFps, "loop @ fps")]
                {
                    if ui.selectable_label(f.mode == v, l).clicked() && f.mode != v {
                        f.mode = v;
                        *dirty = true;
                    }
                }
            });
        if f.mode == VfxFlipModeDoc::LoopFps {
            ui.label("fps");
            *dirty |= ui
                .add(egui::DragValue::new(&mut f.fps).speed(0.5).range(0.1..=120.0))
                .changed();
        }
    });
}

fn emission_section(ui: &mut egui::Ui, ti: usize, track: &mut floptle_scene::VfxTrackDoc, dirty: &mut bool) {
    // Rate / lifetime / burst counts are per CLIP now (a clip is one emission, its length
    // is the particle lifetime). Select a clip on the timeline to edit them; the emit
    // SHAPE below is shared by every clip on the track.
    ui.small("⏱ Emission is per clip — click a clip on the timeline to set its rate/burst, length (= lifetime) & jitter.");
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
    ui.small("hover a value, tap 📈 to animate it over the particle's life");
    let (exp, sk, vr) = (&mut st.expanded_prop, &mut st.sel_key, &mut st.curve_vrange);
    *dirty |= value_or_curve(ui, "velocity", &mut track.velocity, exp, sk, vr);
    *dirty |= value_or_curve(ui, "size", &mut track.size, exp, sk, vr);
    *dirty |= value_or_curve(ui, "rotation", &mut track.rotation, exp, sk, vr);
    *dirty |= value_or_curve(ui, "angular vel", &mut track.angular_velocity, exp, sk, vr);
    ui.small("rotation/angular are Euler radians (x=pitch, y=yaw, z=roll). Meshes use all three; billboards use roll (z) only — and 'velocity'/'upright' alignment ignore roll.");
    *dirty |= value_or_curve(ui, "color", &mut track.color, exp, sk, vr);
    ui.horizontal(|ui| {
        ui.label("gravity ×");
        *dirty |= ui
            .add(egui::DragValue::new(&mut track.gravity).speed(0.02).range(-4.0..=8.0))
            .on_hover_text(
                "scales the scene's gravity for this track. 0 = weightless, 1 = full, \
                 negative = floats up (buoyancy)",
            )
            .changed();
        ui.label("drag");
        *dirty |= ui
            .add(egui::DragValue::new(&mut track.drag).speed(0.01).range(0.0..=50.0).suffix("/s"))
            .on_hover_text("velocity damping per second (air resistance)")
            .changed();
    });
}

/// Force fields on a track — the "make it feel alive" layer. Add wind / attractor /
/// vortex / turbulence and tune each; centres + directions are in the track's sim
/// space (they follow the emitter, and stay floating-origin-safe).
fn forces_section(ui: &mut egui::Ui, track: &mut floptle_scene::VfxTrackDoc, dirty: &mut bool) {
    use VfxForceDoc as F;
    let mut add: Option<F> = None;
    ui.menu_button("＋ add force", |ui| {
        for (lbl, f) in [
            ("💨 wind (directional)", F::Directional { dir: [0.0, 1.0, 0.0], strength: 2.0 }),
            ("🎯 attractor (point)", F::Point { center: [0.0, 0.0, 0.0], strength: 3.0 }),
            ("🌀 vortex", F::Vortex { center: [0.0; 3], axis: [0.0, 1.0, 0.0], strength: 3.0 }),
            ("〰 turbulence", F::Turbulence { frequency: 0.5, strength: 2.0 }),
        ] {
            if ui.button(lbl).clicked() {
                add = Some(f);
                ui.close();
            }
        }
    });
    if let Some(f) = add {
        track.forces.push(f);
        *dirty = true;
    }
    if track.forces.is_empty() {
        ui.small("none — add wind, an attractor, a vortex, or turbulence");
        return;
    }
    // Small inline drag helpers (a scalar and an xyz vector).
    let dv = |ui: &mut egui::Ui, v: &mut f32, dirty: &mut bool| {
        *dirty |= ui.add(egui::DragValue::new(v).speed(0.05)).changed();
    };
    let vec3 = |ui: &mut egui::Ui, a: &mut [f32; 3], dirty: &mut bool| {
        for (i, p) in ["x", "y", "z"].iter().enumerate() {
            *dirty |= ui.add(egui::DragValue::new(&mut a[i]).speed(0.05).prefix(*p)).changed();
        }
    };
    let mut remove = None;
    for (i, f) in track.forces.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            match f {
                F::Directional { dir, strength } => {
                    ui.small("💨 dir");
                    vec3(ui, dir, dirty);
                    ui.small("×");
                    dv(ui, strength, dirty);
                }
                F::Point { center, strength } => {
                    ui.small("🎯 at");
                    vec3(ui, center, dirty);
                    ui.small("pull");
                    dv(ui, strength, dirty);
                }
                F::Vortex { center, axis, strength } => {
                    ui.small("🌀 at");
                    vec3(ui, center, dirty);
                    ui.small("axis");
                    vec3(ui, axis, dirty);
                    ui.small("×");
                    dv(ui, strength, dirty);
                }
                F::Turbulence { frequency, strength } => {
                    ui.small("〰 freq");
                    dv(ui, frequency, dirty);
                    ui.small("×");
                    dv(ui, strength, dirty);
                }
            }
            if ui.small_button("🗑").on_hover_text("remove force").clicked() {
                remove = Some(i);
            }
        });
    }
    if let Some(i) = remove {
        track.forces.remove(i);
        *dirty = true;
    }
}

/// The Inspector's lane area. Lanes are *shaped* on the timeline (DAW-style); here
/// we point the artist there and, when a breakpoint is selected, give a precise
/// editor for its exact time + value/colour/xyz (drag is approximate; this nails it).
fn selected_point_section(
    ui: &mut egui::Ui,
    ti: usize,
    track: &mut floptle_scene::VfxTrackDoc,
    st: &mut crate::vfx_ui::VfxUiState,
    dur: f32,
    dirty: &mut bool,
) {
    ui.horizontal(|ui| {
        ui.strong("Lanes");
        ui.small("(expand a track ⏷ on the timeline to draw its curves)");
    });
    let Some((ati, lref, ki)) = st.auto_sel else {
        ui.small("Click a breakpoint on a timeline lane to fine-tune it here.");
        return;
    };
    if ati != ti {
        return; // the selected point is on a different track
    }
    let is_time = matches!(lref, LaneRef::Auto(_));
    let dmax = if is_time { dur } else { 1.0 };
    let fixed = lane_fixed_range(track, lref);
    let label = lane_ref_label(track, lref);
    let Some(curve) = lane_curve_mut(track, lref) else {
        st.auto_sel = None;
        return;
    };
    // Neighbour times bound the selected key so editing it can't reorder the curve.
    let n = curve.keys.len();
    let tmin = if ki > 0 { curve.keys[ki - 1].t } else { 0.0 };
    let tmax = if ki + 1 < n { curve.keys[ki + 1].t } else { dmax };
    let (tmin, tmax) = (tmin.min(tmax), tmin.max(tmax));
    let Some(k) = curve.keys.get_mut(ki) else {
        st.auto_sel = None;
        return;
    };
    ui.horizontal(|ui| {
        ui.small(format!("♦ {label}"));
        ui.label("t");
        let suffix = if is_time { "s" } else { "" };
        *dirty |= ui
            .add(egui::DragValue::new(&mut k.t).speed(0.01).range(tmin..=tmax).suffix(suffix))
            .changed();
    });
    ui.horizontal(|ui| {
        ui.small("value");
        match &mut k.v {
            VfxValueDoc::F32(x) => {
                let dv = egui::DragValue::new(x).speed(0.01);
                let dv = if let Some((lo, hi)) = fixed { dv.range(lo..=hi) } else { dv };
                *dirty |= ui.add(dv).changed();
            }
            VfxValueDoc::Vec3(xyz) => {
                for (i, p) in ["x", "y", "z"].iter().enumerate() {
                    *dirty |= ui.add(egui::DragValue::new(&mut xyz[i]).speed(0.01).prefix(*p)).changed();
                }
            }
            VfxValueDoc::Rgba(c) => {
                *dirty |= ui.color_edit_button_rgba_unmultiplied(c).changed();
            }
        }
    });
    // Interp applies to scalar (point) lanes; colour/vector lanes use time-only stops.
    if matches!(k.v, VfxValueDoc::F32(_)) {
        ui.horizontal(|ui| {
            ui.small("interp");
            for (iv, lbl) in [
                (VfxInterpDoc::Constant, "hold"),
                (VfxInterpDoc::Linear, "linear"),
                (VfxInterpDoc::Bezier, "smooth"),
            ] {
                if ui.selectable_label(k.interp == iv, lbl).clicked() && k.interp != iv {
                    k.interp = iv;
                    *dirty = true;
                }
            }
        });
    }
}

/// Shorten a long asset path to the last two components for a picker label.
fn short(p: &str) -> String {
    let parts: Vec<&str> = p.rsplit(['/', '\\']).take(2).collect();
    parts.into_iter().rev().collect::<Vec<_>>().join("/")
}
