//! The Inspector dock tab: the selected node's modular components (Type /
//! Transform / Material / Rigidbody / Collider / Scripts / Animation), the
//! component clipboard, and the material property editors shared with the
//! floating Material Editor window.

use std::path::Path;

use floptle_core::math::{EulerRot, Quat};
use floptle_core::transform::Transform;
use floptle_core::{Light, Material, Matter, Name, Scripts, Shape};

use crate::assets::{
    collect_model_paths, collect_script_names, collect_texture_paths, is_material, is_model,
    is_script, is_texture, AssetPayload,
};
use crate::matter_catalog::{matter_icon, matter_kind_label, type_catalog};
use crate::{anim_ui, EditorTabViewer};

/// A copied component's values, held on the editor clipboard so they can be pasted
/// onto another component of the same kind (Inspector ⎘ copy / 📋 paste).
#[derive(Clone)]
pub(crate) enum ComponentClip {
    Transform(Transform),
    /// The node's "type" component (geometry / camera / light / …).
    Matter(Matter),
    Material(Box<Material>),
    RigidBody(floptle_core::RigidBody),
    Particles(floptle_core::ParticleSystem),
    Audio(floptle_audio::AudioSource),
    /// A single attached script (its kind, enabled flag, and tuned params).
    Script(floptle_core::ScriptInst),
}

impl ComponentClip {
    /// A short human label for the clipboard's current contents.
    pub(crate) fn label(&self) -> String {
        match self {
            ComponentClip::Transform(_) => "Transform".into(),
            ComponentClip::Matter(_) => "Type".into(),
            ComponentClip::Material(_) => "Material".into(),
            ComponentClip::RigidBody(_) => "Rigidbody".into(),
            ComponentClip::Particles(_) => "Particle System".into(),
            ComponentClip::Audio(_) => "Audio Source".into(),
            ComponentClip::Script(s) => format!("Script: {}", s.kind),
        }
    }
}
/// A component section header row: bold title on the left, a right-aligned `…`
/// overflow menu (Copy ⎘ always; Paste 📋 when `can_paste`; Remove 🗑 when
/// `can_remove`). Returns `(copy, paste, remove)` — which item was clicked.
pub(crate) fn component_header(
    ui: &mut egui::Ui,
    title: &str,
    can_paste: bool,
    can_remove: bool,
) -> (bool, bool, bool) {
    let mut copy = false;
    let mut paste = false;
    let mut remove = false;
    // Right-to-left: the … menu is laid out FIRST, so it's pinned to the
    // visible right edge no matter how long the title is — the title takes
    // whatever is left and truncates. (Title-first would push the menu past
    // the panel edge the moment the title outgrows the row.)
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.menu_button("…", |ui| {
                if ui.button("⎘  Copy values").clicked() {
                    copy = true;
                    ui.close();
                }
                if can_paste && ui.button("📋  Paste values").clicked() {
                    paste = true;
                    ui.close();
                }
                if can_remove {
                    ui.separator();
                    if ui.button("🗑  Remove component").clicked() {
                        remove = true;
                        ui.close();
                    }
                }
            })
            .response
            .on_hover_text("component options");
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add(egui::Label::new(egui::RichText::new(title).strong()).truncate());
            });
        });
    });
    (copy, paste, remove)
}
/// [`component_header`] for components with no copyable values (Collider,
/// Networked, Animation Controller): the `…` menu offers only Remove, so no
/// dead "Copy values" item sits there doing nothing. Returns `remove`.
pub(crate) fn component_header_no_copy(ui: &mut egui::Ui, title: &str, can_remove: bool) -> bool {
    let mut remove = false;
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.menu_button("…", |ui| {
                if can_remove {
                    if ui.button("🗑  Remove component").clicked() {
                        remove = true;
                        ui.close();
                    }
                } else {
                    ui.weak("(no options)");
                }
            })
            .response
            .on_hover_text("component options");
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add(egui::Label::new(egui::RichText::new(title).strong()).truncate());
            });
        });
    });
    remove
}

/// The tiling controls for one texture binding: an Off / Tile / Triplanar mode
/// row, then the active mode's fields. Returns true when anything changed.
/// Shared by the base-texture row and each shader texture slot (proposal §8:
/// tiling is per-BINDING; wrap/filter stay per-texture in the Assets panel).
fn tiling_ui(ui: &mut egui::Ui, t: &mut Option<floptle_core::Tiling>) -> bool {
    use floptle_core::Tiling;
    let mut changed = false;
    ui.horizontal(|ui| {
        let mode = match t {
            None => 0,
            Some(Tiling::Uv { .. }) => 1,
            Some(Tiling::Triplanar { .. }) => 2,
        };
        let mut pick = |ui: &mut egui::Ui, m: usize, label: &str, hover: &str| {
            if ui.selectable_label(mode == m, label).on_hover_text(hover).clicked() && mode != m {
                *t = match m {
                    1 => Some(Tiling::uv()),
                    2 => Some(Tiling::triplanar()),
                    _ => None,
                };
                changed = true;
            }
        };
        pick(ui, 0, "off", "plain mesh UVs — exactly as before");
        pick(ui, 1, "tile", "repeat/scroll/rotate across the mesh UVs");
        pick(
            ui,
            2,
            "triplanar",
            "project from the object's three axes — clean tiling on shapes with stretched or no UVs",
        );
    });
    match t {
        None => {}
        Some(Tiling::Uv { count, offset, rotation }) => {
            ui.horizontal(|ui| {
                ui.label("count");
                changed |= ui
                    .add(egui::DragValue::new(&mut count[0]).speed(0.05).range(0.01..=1000.0))
                    .on_hover_text("repeats across the surface (x)")
                    .changed();
                changed |= ui
                    .add(egui::DragValue::new(&mut count[1]).speed(0.05).range(0.01..=1000.0))
                    .on_hover_text("repeats across the surface (y)")
                    .changed();
                ui.label("offset");
                changed |=
                    ui.add(egui::DragValue::new(&mut offset[0]).speed(0.01)).changed();
                changed |=
                    ui.add(egui::DragValue::new(&mut offset[1]).speed(0.01)).changed();
                ui.label("rot°");
                changed |= ui
                    .add(egui::DragValue::new(rotation).speed(0.5))
                    .on_hover_text("rotation around the UV center (degrees)")
                    .changed();
            });
        }
        Some(Tiling::Triplanar { scale, blend }) => {
            ui.horizontal(|ui| {
                ui.label("tile size");
                changed |= ui
                    .add(egui::DragValue::new(scale).speed(0.02).range(0.01..=1000.0))
                    .on_hover_text("one tile spans this many object units")
                    .changed();
                ui.label("blend");
                changed |= ui
                    .add(egui::Slider::new(blend, 0.5..=8.0))
                    .on_hover_text("axis-edge sharpness")
                    .changed();
            });
        }
    }
    changed
}

/// Widget rows for a shader's exposed uniforms (shared by fragment materials,
/// Field Shape sdf shaders and the Skybox's sky shader): edits write into the
/// given `params` map by uniform name (absent names use the shader default).
fn shader_uniform_rows(
    ui: &mut egui::Ui,
    uniforms: &[floptle_shader::Uniform],
    params: &mut std::collections::BTreeMap<String, [f32; 4]>,
) -> bool {
    let mut changed = false;
    for u in uniforms {
        ui.label(&u.name);
        let mut v = params.get(&u.name).copied().unwrap_or(u.default);
        let mut ch = false;
        if u.is_color {
            ch |= ui.color_edit_button_rgba_unmultiplied(&mut v).changed();
        } else {
            match u.ty {
                floptle_shader::Ty::Float => {
                    ch |= match u.range {
                        Some((lo, hi)) => {
                            ui.add(egui::Slider::new(&mut v[0], lo..=hi)).changed()
                        }
                        None => ui.add(egui::DragValue::new(&mut v[0]).speed(0.02)).changed(),
                    };
                }
                ty => {
                    let lanes = ty.lanes() as usize;
                    ui.horizontal(|ui| {
                        for lane in v.iter_mut().take(lanes) {
                            ch |= ui.add(egui::DragValue::new(lane).speed(0.02)).changed();
                        }
                    });
                }
            }
        }
        if ch {
            params.insert(u.name.clone(), v);
            changed = true;
        }
        ui.end_row();
    }
    changed
}

/// Deferred intents from [`material_props_ui`] (applied after the borrow ends).
#[derive(Default)]
pub(crate) struct MatEditResult {
    pub(crate) changed: bool,
    pub(crate) remove: bool,
    pub(crate) save_as: Option<String>,
}
/// In-depth material property editors — shared by the Inspector's Material section
/// and the floating Material Editor window. Edits `m` in place (so undo coalesces
/// via `inspector_changed`); preset apply/save/remove come back as intents.
pub(crate) fn material_props_ui(
    ui: &mut egui::Ui,
    m: &mut Material,
    presets: &[(String, floptle_scene::MaterialDoc)],
    asset_tree: &[crate::assets::AssetEntry],
    name_buf: &mut String,
    flsl: &crate::shaders::FlslCache,
    sdf: &crate::shaders::SdfCache,
) -> MatEditResult {
    let mut r = MatEditResult::default();

    egui::Grid::new("mat_top").num_columns(2).spacing([8.0, 5.0]).show(ui, |ui| {
        ui.label("base color");
        r.changed |= ui.color_edit_button_rgb(&mut m.color).changed();
        ui.end_row();
        ui.label("texture");
        let cur = m
            .texture
            .as_deref()
            .map(|p| Path::new(p).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default())
            .unwrap_or_else(|| "none".into());
        if let Some(pick) = crate::ui_widgets::asset_picker(
            ui,
            egui::Id::new("mat_tex"),
            if cur.is_empty() { "none" } else { &cur },
            Some("none"),
            asset_tree,
            crate::assets::is_texture,
            160.0,
        ) {
            m.texture = pick;
            r.changed = true;
        }
        ui.end_row();
        // Tiling applies to the base texture (the mesh's own or the override).
        ui.label("tiling");
        ui.vertical(|ui| {
            r.changed |= tiling_ui(ui, &mut m.tiling);
        });
        ui.end_row();
        ui.label("emissive");
        ui.horizontal(|ui| {
            r.changed |= ui.color_edit_button_rgb(&mut m.emissive).changed();
            r.changed |= ui
                .add(egui::DragValue::new(&mut m.emissive_strength).speed(0.02).range(0.0..=20.0).prefix("×"))
                .on_hover_text("emissive strength")
                .changed();
        });
        ui.end_row();
        ui.label("unlit");
        r.changed |= ui.checkbox(&mut m.unlit, "fullbright / flat").changed();
        ui.end_row();
    });

    // ---- custom shader (ADR-0007): pick a .flsl; its exposed uniforms and
    // texture slots become the rows below, live-editing the group(3) params.
    egui::Grid::new("mat_shader").num_columns(2).spacing([8.0, 5.0]).show(ui, |ui| {
        ui.label("shader").on_hover_text(
            "a custom .flsl look — \"Built-in\" is the classic material above.\n\
             Make one with Assets → right-click → ◈ New Shader.",
        );
        let cur = m
            .shader
            .as_deref()
            .map(|p| {
                Path::new(p)
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| p.to_string())
            })
            .unwrap_or_else(|| "Built-in".into());
        if let Some(pick) = crate::ui_widgets::asset_picker(
            ui,
            egui::Id::new("mat_shader"),
            &cur,
            Some("Built-in"),
            asset_tree,
            crate::assets::is_shader,
            160.0,
        ) {
            m.shader = pick;
            // A different shader is a different schema — stale overrides would
            // silently misfill the new param block.
            m.shader_params.clear();
            m.shader_textures.clear();
            r.changed = true;
        }
        ui.end_row();
    });
    if let Some(shader_path) = m.shader.clone() {
        if let Some(entry) = flsl.get(&shader_path) {
            if let Some(err) = &entry.error {
                ui.colored_label(
                    egui::Color32::from_rgb(235, 100, 100),
                    egui::RichText::new(format!("⚠ {err}")).small(),
                );
            }
            if let Some((compiled, _)) = &entry.compiled {
                egui::Grid::new("mat_shader_rows").num_columns(2).spacing([8.0, 5.0]).show(
                    ui,
                    |ui| {
                        r.changed |= shader_uniform_rows(ui, &compiled.uniforms, &mut m.shader_params);
                        for (i, slot) in compiled.textures.iter().enumerate() {
                            ui.label(slot);
                            let cur = m
                                .shader_textures
                                .get(slot)
                                .map(|p| {
                                    Path::new(p)
                                        .file_name()
                                        .map(|s| s.to_string_lossy().to_string())
                                        .unwrap_or_else(|| p.clone())
                                })
                                .unwrap_or_else(|| "none".into());
                            if let Some(pick) = crate::ui_widgets::asset_picker(
                                ui,
                                egui::Id::new(("mat_shader_tex", i)),
                                &cur,
                                Some("none"),
                                asset_tree,
                                crate::assets::is_texture,
                                160.0,
                            ) {
                                match pick {
                                    Some(p) => {
                                        m.shader_textures.insert(slot.clone(), p);
                                    }
                                    None => {
                                        m.shader_textures.remove(slot);
                                    }
                                }
                                r.changed = true;
                            }
                            ui.end_row();
                            // The slot's own tiling block (read by sample()
                            // / sampleTriplanar() in the shader).
                            ui.label("");
                            ui.vertical(|ui| {
                                let mut t = m.shader_tiling.get(slot).copied();
                                if tiling_ui(ui, &mut t) {
                                    match t {
                                        Some(t) => {
                                            m.shader_tiling.insert(slot.clone(), t);
                                        }
                                        None => {
                                            m.shader_tiling.remove(slot);
                                        }
                                    }
                                    r.changed = true;
                                }
                            });
                            ui.end_row();
                        }
                    },
                );
            }
        } else if let Some(entry) = sdf.get(&shader_path) {
            // An sdf-stage shader: geometry, not a surface — its knobs still
            // edit live (they ride the raymarch globals).
            ui.small("◈ sdf stage — this shader IS the node's geometry (use on a Field Shape)");
            if let Some(err) = &entry.error {
                ui.colored_label(
                    egui::Color32::from_rgb(235, 100, 100),
                    egui::RichText::new(format!("⚠ {err}")).small(),
                );
            }
            if let Some((ir, _)) = &entry.parsed {
                egui::Grid::new("mat_sdf_rows").num_columns(2).spacing([8.0, 5.0]).show(ui, |ui| {
                    r.changed |= shader_uniform_rows(ui, &ir.uniforms, &mut m.shader_params);
                });
            }
        } else {
            ui.small("compiling…");
        }
    }

    // These only affect the lit path, so grey them out when unlit.
    ui.add_enabled_ui(!m.unlit, |ui| {
        egui::Grid::new("mat_lit").num_columns(2).spacing([8.0, 5.0]).show(ui, |ui| {
            ui.label("specular");
            ui.horizontal(|ui| {
                r.changed |= ui.color_edit_button_rgb(&mut m.specular).changed();
                r.changed |= ui
                    .add(egui::DragValue::new(&mut m.specular_strength).speed(0.02).range(0.0..=8.0).prefix("×"))
                    .on_hover_text("specular strength")
                    .changed();
            });
            ui.end_row();
            ui.label("shininess");
            r.changed |= ui.add(egui::Slider::new(&mut m.shininess, 1.0..=256.0).logarithmic(true)).changed();
            ui.end_row();
            ui.label("rim");
            ui.horizontal(|ui| {
                r.changed |= ui.color_edit_button_rgb(&mut m.rim).changed();
                r.changed |= ui
                    .add(egui::DragValue::new(&mut m.rim_strength).speed(0.02).range(0.0..=8.0).prefix("×"))
                    .on_hover_text("rim / fresnel strength")
                    .changed();
            });
            ui.end_row();
            ui.label("ambient");
            r.changed |= ui.add(egui::Slider::new(&mut m.ambient, 0.0..=4.0)).changed();
            ui.end_row();
            ui.label("opacity");
            r.changed |= ui
                .add(egui::Slider::new(&mut m.alpha, 0.0..=1.0))
                .on_hover_text("1 = opaque; below 1 alpha-blends over the scene (drawn after opaque objects)")
                .changed();
            ui.end_row();
        });
    });

    ui.separator();
    ui.horizontal(|ui| {
        if !presets.is_empty() {
            ui.menu_button("Apply preset", |ui| {
                for (name, doc) in presets {
                    if ui.button(name).clicked() {
                        *m = doc.to_material();
                        r.changed = true;
                        ui.close();
                    }
                }
            });
        }
        ui.add(egui::TextEdit::singleline(name_buf).desired_width(100.0).hint_text("preset name"));
        if ui.button("Save preset").clicked() && !name_buf.trim().is_empty() {
            r.save_as = Some(name_buf.trim().to_string());
        }
    });
    if ui.button("🗑 Remove material").clicked() {
        r.remove = true;
    }
    r
}
impl EditorTabViewer<'_> {
    /// The Inspector for a selected armature bone: shows which mesh it belongs to and
    /// edits its LOCAL transform. Editing auto-keys the bone into the open animator clip
    /// at the playhead — so posing a bone and animating it are one act — and the
    /// Animating-tab preview shows it live. Numeric for now (a bone isn't an ECS entity,
    /// so the move gizmo doesn't target it yet), mirroring the BoneAttach offset editor.
    fn bone_inspector_ui(&mut self, ui: &mut egui::Ui) {
        let Some((mesh, idx)) = *self.bone_selection else { return };
        // Resolve the bone's name + rest pose, dropping the world/registry borrows before
        // we touch the animator.
        let resolved = match self.world.get::<Matter>(mesh) {
            Some(Matter::Mesh { asset_path }) => self
                .mesh_registry
                .get(asset_path)
                .and_then(|m| m.rig.as_ref())
                .and_then(|rig| rig.skeleton.nodes.get(idx))
                .map(|n| (n.name.clone(), n.rest)),
            _ => None,
        };
        let Some((bone_name, rest)) = resolved else {
            *self.bone_selection = None;
            return;
        };
        // Current local pose: the live preview pose if the mesh is animating, else rest.
        let cur = self
            .anim
            .instances
            .get(&mesh)
            .and_then(|inst| inst.ctl.pose().get(idx).copied())
            .unwrap_or(rest);
        let mesh_name = self.world.get::<Name>(mesh).map(|n| n.0.clone()).unwrap_or_default();

        ui.horizontal(|ui| {
            ui.strong(format!("🔗 {bone_name}"));
            if ui.small_button("⮪ back").on_hover_text("back to the node inspector").clicked() {
                *self.bone_selection = None;
            }
        });
        ui.small(format!("armature bone of {mesh_name}"));
        ui.separator();

        let mut trs = cur;
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.label("position");
            changed |= ui.add(egui::DragValue::new(&mut trs.t.x).speed(0.01).prefix("x ")).changed();
            changed |= ui.add(egui::DragValue::new(&mut trs.t.y).speed(0.01).prefix("y ")).changed();
            changed |= ui.add(egui::DragValue::new(&mut trs.t.z).speed(0.01).prefix("z ")).changed();
        });
        let (ey, ex, ez) = trs.r.to_euler(EulerRot::YXZ);
        let mut deg = [ex.to_degrees(), ey.to_degrees(), ez.to_degrees()];
        ui.horizontal(|ui| {
            ui.label("rotation°");
            let mut rc = false;
            rc |= ui.add(egui::DragValue::new(&mut deg[0]).speed(0.5).prefix("x ")).changed();
            rc |= ui.add(egui::DragValue::new(&mut deg[1]).speed(0.5).prefix("y ")).changed();
            rc |= ui.add(egui::DragValue::new(&mut deg[2]).speed(0.5).prefix("z ")).changed();
            if rc {
                trs.r = Quat::from_euler(
                    EulerRot::YXZ,
                    deg[1].to_radians(),
                    deg[0].to_radians(),
                    deg[2].to_radians(),
                );
                changed = true;
            }
        });
        ui.horizontal(|ui| {
            ui.label("scale");
            changed |= ui.add(egui::DragValue::new(&mut trs.s.x).speed(0.01).range(0.001..=100.0).prefix("x ")).changed();
            changed |= ui.add(egui::DragValue::new(&mut trs.s.y).speed(0.01).range(0.001..=100.0).prefix("y ")).changed();
            changed |= ui.add(egui::DragValue::new(&mut trs.s.z).speed(0.01).range(0.001..=100.0).prefix("z ")).changed();
        });

        // Auto-key into the open clip at the playhead — but only when the Animating tab is
        // targeting THIS mesh with a clip open (bone channels are name-bound to this
        // skeleton, so writing into another mesh's clip would be wrong).
        ui.separator();
        let can_key = self.anim_ui.target == Some(mesh) && self.anim_ui.clip_doc.is_some();
        if can_key {
            let ph = self.anim_ui.playhead;
            let dur = self.anim_ui.clip_doc.as_ref().map(|(_, d)| d.duration).unwrap_or(0.0);
            ui.small(format!("⏺ keys at playhead {ph:.2}s / {dur:.2}s"));
            if changed {
                if let Some((_, doc)) = self.anim_ui.clip_doc.as_mut() {
                    crate::anim_ui::write_key(doc, &bone_name, ph, &trs);
                }
                self.anim_ui.clip_dirty = true;
            }
        } else {
            ui.colored_label(
                egui::Color32::from_rgb(235, 200, 90),
                "⚠ pick this mesh + a clip in the Animating tab to keyframe this bone",
            );
        }
    }

    pub(crate) fn inspector_ui(&mut self, ui: &mut egui::Ui) {
        // When the Particles tab is up and a track is selected, the Inspector
        // becomes that track's editor (VFX artists tune tracks here, not in a
        // cramped bottom panel). Deselecting the track — or picking a scene node,
        // which clears the track selection — reverts to the node inspector.
        if self.vfx_track_active() {
            self.vfx_track_inspector_ui(ui);
            return;
        }
        // A selected armature bone (clicked in the Hierarchy) takes over the Inspector:
        // edit its local transform, auto-keyed into the open animator clip. It yields the
        // moment a node or asset is also selected, so no stale-selection clearing needed.
        if self.bone_selection.is_some() && self.selection.is_empty() && self.selected_asset.is_none() {
            self.bone_inspector_ui(ui);
            return;
        }
        // The Inspector shows *only* the current selection (the scene name + save
        // live in the Hierarchy header). An asset selected in the browser shows here.
        if let Some(path) = self.selected_asset.clone() {
            ui.strong("Asset");
            let name_resp = ui.selectable_label(false, &path);
            if is_model(&path) {
                ui.label("glTF model — drag onto the scene to place it.");
                self.asset_preview_ui(ui);
                self.model_asset_anim_ui(ui, &path);
            } else if anim_ui::is_anim_clip(&path) {
                self.clip_asset_ui(ui, &path);
            } else if anim_ui::is_anim_ctl(&path) {
                self.ctl_asset_ui(ui, &path);
            } else if is_material(&path) {
                ui.label("material preset");
                self.asset_preview_ui(ui);
                self.material_asset_ui(ui, &path);
            } else if is_texture(&path) {
                self.asset_preview_ui(ui);
                self.texture_settings_ui(ui, &path);
            } else if is_script(&path) {
                ui.label("script — drag onto a node, double-click, or:");
                if ui.button("🖊  Open in Scripting").clicked() {
                    self.cmd.open_script = Some(path.clone());
                    self.cmd.focus_scripting = true;
                }
                if name_resp.double_clicked() {
                    self.cmd.open_script_pref = Some(path.clone());
                }
            }
            ui.separator();
        }

        let primary = self.selection.last().copied();
        if self.selection.len() > 1 {
            ui.small(format!("{} selected", self.selection.len()));
        }
        let cmd = &mut *self.cmd;
        let world = &mut *self.world;
        let bone_names = self.bone_names;
        match primary {
            Some(e) if world.get::<Light>(e).is_some() => {
                if let Some(l) = world.get_mut::<Light>(e) {
                    ui.label("Lighting node");
                    cmd.inspector_changed |= ui
                        .checkbox(&mut l.stars, "stars mode ☀")
                        .on_hover_text(
                            "the directional light turns OFF and every Celestial Body with \
                             luminosity > 0 becomes a real light source — light radiates \
                             from each star with inverse-square falloff, terminators wrap \
                             planets, far sides go dark, and multiple stars just work \
                             (up to 4 reach the shaders).",
                        )
                        .changed();
                    if l.stars {
                        ui.small("light comes from Celestial Bodies with luminosity > 0");
                    } else {
                        ui.label("direction");
                        ui.horizontal(|ui| {
                            cmd.inspector_changed |= ui.add(egui::DragValue::new(&mut l.direction[0]).speed(0.02).prefix("x ")).changed();
                            cmd.inspector_changed |= ui.add(egui::DragValue::new(&mut l.direction[1]).speed(0.02).prefix("y ")).changed();
                            cmd.inspector_changed |= ui.add(egui::DragValue::new(&mut l.direction[2]).speed(0.02).prefix("z ")).changed();
                        });
                    }
                    ui.horizontal(|ui| {
                        ui.label("light");
                        cmd.inspector_changed |= ui.color_edit_button_rgb(&mut l.color).changed();
                        ui.label("ambient");
                        cmd.inspector_changed |= ui.color_edit_button_rgb(&mut l.ambient).changed();
                    });
                    cmd.inspector_changed |=
                        ui.add(egui::Slider::new(&mut l.intensity, 0.0..=8.0).text("intensity")).changed();

                    ui.separator();
                    cmd.inspector_changed |= ui
                        .checkbox(&mut l.shadows, "sun shadows")
                        .on_hover_text(
                            "march the SDF field toward the sun — analytically soft shadows, \
                             no shadow maps. Terrain and blobs cast on everything; meshes cast \
                             via their collider shapes and receive like everything else.",
                        )
                        .changed();
                    ui.add_enabled_ui(l.shadows, |ui| {
                        cmd.inspector_changed |= ui
                            .add(egui::Slider::new(&mut l.shadow_softness, 0.0..=1.0).text("softness"))
                            .on_hover_text("0 = razor-hard edge (retro), 1 = dreamy-soft penumbra")
                            .changed();
                        cmd.inspector_changed |= ui
                            .add(egui::Slider::new(&mut l.shadow_strength, 0.0..=1.0).text("strength"))
                            .on_hover_text("how dark full shadow gets — ambient light still fills, so 1.0 isn't pitch black")
                            .changed();
                        ui.horizontal(|ui| {
                            ui.label("tint");
                            cmd.inspector_changed |= ui
                                .color_edit_button_rgb(&mut l.shadow_tint)
                                .on_hover_text("shadows darken toward this color — black is neutral; try purple dusk or sepia")
                                .changed();
                            ui.label("quantize");
                            let qlabel = match l.shadow_quantize {
                                0 => "smooth".to_string(),
                                n => format!("{n} bands"),
                            };
                            egui::ComboBox::from_id_salt("shadow_quantize")
                                .selected_text(qlabel)
                                .show_ui(ui, |ui| {
                                    cmd.inspector_changed |=
                                        ui.selectable_value(&mut l.shadow_quantize, 0, "smooth").clicked();
                                    for nb in 2..=4u32 {
                                        cmd.inspector_changed |= ui
                                            .selectable_value(&mut l.shadow_quantize, nb, format!("{nb} bands"))
                                            .clicked();
                                    }
                                });
                        });
                        ui.add_enabled_ui(l.shadow_quantize >= 2, |ui| {
                            cmd.inspector_changed |= ui
                                .checkbox(&mut l.shadow_dither, "dither the penumbra")
                                .on_hover_text("Bayer-pattern the quantized penumbra — the PS1 shadow edge; pairs with retro mode")
                                .changed();
                        });
                        cmd.inspector_changed |= ui
                            .add(
                                egui::Slider::new(&mut l.shadow_distance, 10.0..=1000.0)
                                    .logarithmic(true)
                                    .text("distance"),
                            )
                            .on_hover_text("max distance a shadow ray marches (a perf fence — farther geometry stops casting)")
                            .changed();
                    });
                    // Depth fog — dirt-cheap distance haze (independent of shadows).
                    ui.separator();
                    cmd.inspector_changed |= ui
                        .checkbox(&mut l.fog, "depth fog")
                        .on_hover_text("fade the scene toward a color with distance — cheap atmosphere; the skybox stays crisp")
                        .changed();
                    ui.add_enabled_ui(l.fog, |ui| {
                        ui.horizontal(|ui| {
                            ui.label("color");
                            cmd.inspector_changed |= ui
                                .color_edit_button_rgb(&mut l.fog_color)
                                .on_hover_text("match the horizon / background so no seam shows at the skybox")
                                .changed();
                        });
                        ui.horizontal(|ui| {
                            ui.label("start");
                            cmd.inspector_changed |= ui
                                .add(egui::DragValue::new(&mut l.fog_start).speed(0.5).range(0.0..=10000.0).suffix("m"))
                                .changed();
                            ui.label("end");
                            cmd.inspector_changed |= ui
                                .add(egui::DragValue::new(&mut l.fog_end).speed(0.5).range(0.1..=10000.0).suffix("m"))
                                .changed();
                        });
                        // Dither: hide 8-bit banding on long, slow fog ramps.
                        ui.horizontal(|ui| {
                            cmd.inspector_changed |= ui
                                .checkbox(&mut l.fog_dither, "dither")
                                .on_hover_text("break up color banding across the fog gradient (matches the retro pixel grid)")
                                .changed();
                            ui.add_enabled_ui(l.fog_dither, |ui| {
                                cmd.inspector_changed |= ui
                                    .add(egui::Slider::new(&mut l.fog_dither_strength, 0.0..=1.0).text("amount"))
                                    .changed();
                            });
                        });
                    });
                }
            }
            Some(e) if world.get::<Transform>(e).is_some() => {
                if let Some(n) = world.get_mut::<Name>(e) {
                    ui.horizontal(|ui| {
                        ui.label("name");
                        cmd.inspector_changed |= ui.text_edit_singleline(&mut n.0).changed();
                    });
                }
                // ===== Layer + tags — identity every node carries. =====
                // Layer: the node's collision/query layer (project-defined names,
                // Project Settings → Layers). Tags: free-form chips scripts find
                // with `findTagged` / compare with `node:hasTag`.
                ui.horizontal(|ui| {
                    ui.label("layer");
                    let cur = world
                        .get::<floptle_core::Layer>(e)
                        .map(|l| l.0.clone())
                        .unwrap_or_else(|| floptle_core::layers::DEFAULT_LAYER.to_string());
                    let known = self.layer_names.contains(&cur);
                    let shown = if known { cur.clone() } else { format!("⚠ {cur}") };
                    egui::ComboBox::from_id_salt("node_layer")
                        .selected_text(shown)
                        .show_ui(ui, |ui| {
                            for name in self.layer_names {
                                if ui.selectable_label(*name == cur, name).clicked()
                                    && *name != cur
                                {
                                    cmd.set_layer = Some((e, name.clone()));
                                }
                            }
                        })
                        .response
                        .on_hover_text(
                            "collision/query layer — the Project Settings matrix decides \
                             which layers collide; raycasts can filter by them",
                        );
                    if !known {
                        ui.small("not in Project Settings — acts as Default")
                            .on_hover_text("define it in Project Settings → Layers, or pick another");
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label("tags");
                    let mut remove: Option<String> = None;
                    if let Some(tags) = world.get::<floptle_core::Tags>(e) {
                        for t in &tags.0 {
                            if ui
                                .small_button(format!("{t} ✕"))
                                .on_hover_text("remove this tag")
                                .clicked()
                            {
                                remove = Some(t.clone());
                            }
                        }
                    }
                    let field = egui::TextEdit::singleline(self.tag_edit)
                        .hint_text("add tag…")
                        .desired_width(90.0);
                    let resp = ui.add(field);
                    let commit = (resp.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                        || ui.small_button("➕").on_hover_text("add the tag").clicked();
                    if commit && !self.tag_edit.trim().is_empty() {
                        let tag = self.tag_edit.trim().to_string();
                        self.tag_edit.clear();
                        let tags = match world.get_mut::<floptle_core::Tags>(e) {
                            Some(t) => t,
                            None => {
                                world.insert(e, floptle_core::Tags::default());
                                world.get_mut::<floptle_core::Tags>(e).unwrap()
                            }
                        };
                        if !tags.has(&tag) {
                            tags.0.push(tag);
                            cmd.inspector_changed = true;
                        }
                        resp.request_focus(); // keep typing tags
                    }
                    if let Some(tag) = remove
                        && let Some(tags) = world.get_mut::<floptle_core::Tags>(e)
                    {
                        tags.0.retain(|t| *t != tag);
                        if tags.0.is_empty() {
                            world.remove::<floptle_core::Tags>(e);
                        }
                        cmd.inspector_changed = true;
                    }
                });
                // The component clipboard (read-only); copy/paste route through `cmd`.
                let clip = self.component_clip.as_ref();

                // ===== Type — the node's primary kind (mutually exclusive). =====
                {
                    let (icon, label, is_terrain) = match world.get::<Matter>(e) {
                        Some(m) => (matter_icon(m), matter_kind_label(m), matches!(m, Matter::Terrain { .. })),
                        None => ("◎", "Type", false),
                    };
                    let (copy, paste, _) = component_header(
                        ui,
                        &format!("{icon} {label}"),
                        !is_terrain && matches!(clip, Some(ComponentClip::Matter(_))),
                        false,
                    );
                    if copy && !is_terrain
                        && let Some(m) = world.get::<Matter>(e) {
                            cmd.copy_component = Some(ComponentClip::Matter(m.clone()));
                        }
                    if paste {
                        cmd.paste_component = Some(e);
                    }
                }
                ui.indent("type_props", |ui| {
                    if let Some(m) = world.get_mut::<Matter>(e) {
                        match m {
                            Matter::Primitive { shape, color } => {
                                ui.horizontal(|ui| {
                                    ui.label("shape");
                                    egui::ComboBox::from_id_salt("shape")
                                        .selected_text(format!("{shape:?}"))
                                        .show_ui(ui, |ui| {
                                            cmd.inspector_changed |= ui.selectable_value(shape, Shape::Cube, "Cube").clicked();
                                            cmd.inspector_changed |= ui.selectable_value(shape, Shape::Sphere, "Sphere").clicked();
                                            cmd.inspector_changed |= ui.selectable_value(shape, Shape::Capsule, "Capsule").clicked();
                                            cmd.inspector_changed |= ui.selectable_value(shape, Shape::Plane, "Plane").clicked();
                                        });
                                });
                                ui.horizontal(|ui| {
                                    ui.label("color");
                                    cmd.inspector_changed |= ui.color_edit_button_rgb(color).changed();
                                    ui.small("(base color — add a Material below for emissive, specular, …)");
                                });
                            }
                            Matter::Blob { scale } => {
                                cmd.inspector_changed |= ui
                                    .add(egui::DragValue::new(scale).speed(0.02).prefix("blob size ").range(0.05..=50.0))
                                    .changed();
                            }
                            Matter::FieldShape { radius } => {
                                cmd.inspector_changed |= ui
                                    .add(egui::DragValue::new(radius).speed(0.02).prefix("bounds radius ").range(0.05..=200.0))
                                    .on_hover_text(
                                        "the shape must fit inside this sphere (local units) — \
                                         the march, shadows and culling all key off it",
                                    )
                                    .changed();
                                ui.small(
                                    "an sdf-stage .flsl (Material → Shader) IS this node's geometry — \
                                     raymarched into the scene field. Visual only (no collision yet).",
                                );
                            }
                            Matter::Mesh { asset_path } => {
                                ui.label("imported mesh");
                                // Swap the model freely — pick any model in the project.
                                let tree = self.asset_tree;
                                let file_label = |p: &str| {
                                    Path::new(p)
                                        .file_name()
                                        .map(|s| s.to_string_lossy().to_string())
                                        .unwrap_or_else(|| p.to_string())
                                };
                                ui.horizontal(|ui| {
                                    ui.label("model");
                                    if let Some(Some(p)) = crate::ui_widgets::asset_picker(
                                        ui,
                                        egui::Id::new("mesh-model"),
                                        &file_label(asset_path),
                                        None,
                                        tree,
                                        is_model,
                                        180.0,
                                    )
                                        && *asset_path != p {
                                            *asset_path = p.clone();
                                            cmd.import_model = Some(p.clone());
                                            cmd.inspector_changed = true;
                                        }
                                });
                                ui.small(asset_path.as_str());
                                if ui
                                    .button("⏏ Extract textures")
                                    .on_hover_text("Save this model's embedded textures to assets/textures/ so you can build materials from them")
                                    .clicked()
                                {
                                    cmd.extract_textures = Some(asset_path.clone());
                                }
                            }
                            Matter::Empty => {
                                ui.label("group / empty");
                                ui.small("a folder — organizes child nodes; has a transform but no geometry");
                            }
                            Matter::Terrain { .. } => {
                                ui.label("editable terrain");
                                ui.small("a sculptable SDF field — move it with the transform below");
                                if ui.button("Δ Open Terrain tools").clicked() {
                                    cmd.focus_terrain = true;
                                }
                            }
                            Matter::Camera { fov_y, active, target, cull_mask } => {
                                ui.label("camera");
                                ui.small("a viewpoint — play mode renders from the active camera");
                                // Live preview of what this camera sees.
                                if let Some(tex) = self.cam_preview {
                                    let w = ui.available_width().min(300.0);
                                    let size = egui::vec2(w, w * 9.0 / 16.0);
                                    ui.add(egui::Image::new((tex, size)).corner_radius(4.0));
                                    ui.small("preview — what this camera sees");
                                }
                                ui.horizontal(|ui| {
                                    ui.label("field of view");
                                    let mut deg = fov_y.to_degrees();
                                    if ui.add(egui::Slider::new(&mut deg, 20.0..=120.0).suffix("°")).changed() {
                                        *fov_y = deg.to_radians();
                                        cmd.inspector_changed = true;
                                    }
                                });
                                // A1: render-target name — a live texture any material
                                // or UI image can wear as `rt:<name>`.
                                ui.horizontal(|ui| {
                                    ui.label("target").on_hover_text(
                                        "render this camera into a live texture every frame; \
                                         use it as texture \"rt:<name>\" on a material or UI \
                                         image — cockpit screens, monitors, mirrors",
                                    );
                                    if ui.text_edit_singleline(target).changed() {
                                        cmd.inspector_changed = true;
                                    }
                                });
                                if !target.is_empty() {
                                    ui.small(format!("live texture: rt:{target}"));
                                }
                                // Per-layer cull checkboxes (bit i = project layer i).
                                let label = if *cull_mask == u32::MAX {
                                    "renders: all layers".to_string()
                                } else {
                                    format!(
                                        "renders: {}/{} layers",
                                        cull_mask.count_ones().min(self.layer_names.len() as u32),
                                        self.layer_names.len()
                                    )
                                };
                                ui.menu_button(label, |ui| {
                                    for (i, name) in self.layer_names.iter().enumerate() {
                                        let mut on = (*cull_mask >> i) & 1 == 1;
                                        if ui.checkbox(&mut on, name).changed() {
                                            *cull_mask ^= 1 << i;
                                            cmd.inspector_changed = true;
                                        }
                                    }
                                    if ui.small_button("all").clicked() {
                                        *cull_mask = u32::MAX;
                                        cmd.inspector_changed = true;
                                    }
                                });
                                if *active {
                                    ui.colored_label(egui::Color32::from_rgb(120, 200, 140), "⌖ active camera");
                                } else if ui.button("⌖ Make active camera").clicked() {
                                    cmd.set_active_camera = Some(e);
                                }
                                if ui.button("⎙ Snap to this view").on_hover_text("move the camera to the current editor viewpoint").clicked() {
                                    cmd.camera_from_view = Some(e);
                                }
                            }
                            Matter::PointLight { color, intensity, range } => {
                                ui.label("point light");
                                ui.small("an omni light — position comes from the transform below");
                                ui.horizontal(|ui| {
                                    ui.label("color");
                                    cmd.inspector_changed |= ui.color_edit_button_rgb(color).changed();
                                });
                                cmd.inspector_changed |=
                                    ui.add(egui::Slider::new(intensity, 0.0..=20.0).text("intensity")).changed();
                                cmd.inspector_changed |=
                                    ui.add(egui::Slider::new(range, 0.1..=200.0).text("range")).changed();
                            }
                            Matter::GravityVolume { mode, strength, radius } => {
                                use floptle_core::GravityMode;
                                ui.label("gravity volume");
                                ui.small("level physics gravity — Down (normal) or Radial (planet)");
                                ui.horizontal(|ui| {
                                    let mut radial = *mode == GravityMode::Radial;
                                    if ui.selectable_label(!radial, "⬇ Down").clicked() {
                                        radial = false;
                                    }
                                    if ui.selectable_label(radial, "◎ Radial (planet)").clicked() {
                                        radial = true;
                                    }
                                    let new =
                                        if radial { GravityMode::Radial } else { GravityMode::Down };
                                    if new != *mode {
                                        *mode = new;
                                        cmd.inspector_changed = true;
                                    }
                                });
                                cmd.inspector_changed |=
                                    ui.add(egui::Slider::new(strength, 0.0..=60.0).text("strength")).changed();
                                if *mode == GravityMode::Radial {
                                    cmd.inspector_changed |= ui
                                        .add(egui::Slider::new(radius, 0.5..=500.0).text("well radius"))
                                        .changed();
                                }
                            }
                            Matter::Skybox { color, size, texture, tint, shader, shader_params } => {
                                ui.label("skybox");
                                ui.small("the scene environment, drawn behind everything. Rotate this node (or a script) to spin the sky.");
                                // A Sky-stage .flsl overrides the solid/texture look with a
                                // procedural sky (per-ray-direction color). Clear it to fall
                                // back to the solid/texture controls below.
                                ui.horizontal(|ui| {
                                    ui.label("shader");
                                    let cur = shader.clone().unwrap_or_default();
                                    let slabel = if cur.is_empty() {
                                        "(none — built-in sky)".to_string()
                                    } else {
                                        Path::new(&cur).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or(cur.clone())
                                    };
                                    if let Some(pick) = crate::ui_widgets::asset_picker(
                                        ui,
                                        egui::Id::new("sky-shader"),
                                        &slabel,
                                        None,
                                        self.asset_tree,
                                        crate::assets::is_shader,
                                        180.0,
                                    ) {
                                        // A different sky shader has different knobs — the old
                                        // overrides would misfill by name, so drop them (same
                                        // as the Material path clears params on shader change).
                                        if *shader != pick {
                                            shader_params.clear();
                                        }
                                        *shader = pick;
                                        cmd.inspector_changed = true;
                                    }
                                    if shader.is_some() && ui.button("✖").on_hover_text("remove the sky shader").clicked() {
                                        *shader = None;
                                        shader_params.clear();
                                        cmd.inspector_changed = true;
                                    }
                                });
                                if shader.is_some() {
                                    ui.small("a `stage sky` .flsl computes the sky from `skyDir`.");
                                    // Knob rows from the compiled sky shader's uniform schema —
                                    // same widgets as a Material's shader params. Edits write
                                    // into `shader_params`; the raymarch reads them next frame.
                                    if self.sky_uniforms.is_empty() {
                                        ui.small("(its knobs appear here once it compiles — check the Console if not)");
                                    } else {
                                        egui::Grid::new("sky_shader_rows")
                                            .num_columns(2)
                                            .spacing([8.0, 5.0])
                                            .show(ui, |ui| {
                                                if shader_uniform_rows(ui, self.sky_uniforms, shader_params) {
                                                    cmd.inspector_changed = true;
                                                }
                                            });
                                        if ui
                                            .button("Reset knobs")
                                            .on_hover_text("back to the shader's own defaults")
                                            .clicked()
                                        {
                                            shader_params.clear();
                                            cmd.inspector_changed = true;
                                        }
                                    }
                                }
                                let mut textured = texture.is_some();
                                ui.horizontal(|ui| {
                                    if ui.selectable_label(!textured, "■ Solid color").clicked() && textured {
                                        *texture = None;
                                        cmd.inspector_changed = true;
                                    }
                                    if ui.selectable_label(textured, "▦ Texture").clicked() && !textured {
                                        let mut tl = Vec::new();
                                        collect_texture_paths(self.asset_tree, &mut tl);
                                        *texture = Some(tl.first().cloned().unwrap_or_default());
                                        cmd.inspector_changed = true;
                                    }
                                });
                                textured = texture.is_some();
                                if !textured {
                                    ui.horizontal(|ui| {
                                        ui.label("color");
                                        cmd.inspector_changed |= ui.color_edit_button_rgb(color).changed();
                                    });
                                } else {
                                    let tree = self.asset_tree;
                                    let cur = texture.clone().unwrap_or_default();
                                    let label = |p: &str| {
                                        Path::new(p).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| p.to_string())
                                    };
                                    ui.horizontal(|ui| {
                                        ui.label("texture");
                                        if let Some(Some(p)) = crate::ui_widgets::asset_picker(
                                            ui,
                                            egui::Id::new("sky-tex"),
                                            &if cur.is_empty() { "(pick a texture)".to_string() } else { label(&cur) },
                                            None,
                                            tree,
                                            is_texture,
                                            180.0,
                                        ) {
                                            *texture = Some(p);
                                            cmd.inspector_changed = true;
                                        }
                                    });
                                    ui.small("an equirectangular (2:1) image, wrapped seamlessly around the sky.");
                                    ui.horizontal(|ui| {
                                        ui.label("tint");
                                        cmd.inspector_changed |= ui.color_edit_button_rgb(tint).changed();
                                    });
                                }
                                cmd.inspector_changed |= ui
                                    .add(egui::Slider::new(size, 10.0..=5000.0).logarithmic(true).text("size (radius)"))
                                    .changed();
                            }
                            Matter::PostProcess {
                                enabled,
                                bloom,
                                bloom_threshold,
                                bloom_intensity,
                                vignette,
                                vignette_strength,
                                vignette_radius,
                                ao,
                                ao_strength,
                                ao_radius,
                                posterize_bands,
                                posterize_dither,
                            } => {
                                use floptle_core::AoMode;
                                ui.label("post processing");
                                ui.small("this scene's full-screen effect chain — every scene has its own (the settings travel with the scene, not the project)");
                                cmd.inspector_changed |= ui
                                    .checkbox(enabled, "enabled")
                                    .on_hover_text("master switch for the whole chain")
                                    .changed();
                                ui.add_enabled_ui(*enabled, |ui| {
                                    ui.separator();
                                    ui.label("Ambient occlusion");
                                    ui.horizontal(|ui| {
                                        let mut m = *ao;
                                        if ui.selectable_label(m == AoMode::Off, "Off").clicked() {
                                            m = AoMode::Off;
                                        }
                                        if ui
                                            .selectable_label(m == AoMode::ScreenSpace, "Screen space")
                                            .on_hover_text("SSAO — cheap, from the depth buffer; shades everything on screen (meshes and terrain)")
                                            .clicked()
                                        {
                                            m = AoMode::ScreenSpace;
                                        }
                                        if ui
                                            .selectable_label(m == AoMode::Sdf, "SDF (true)")
                                            .on_hover_text("samples the real distance field — no screen-space artifacts; everything receives it, but only SDF matter (terrain/blobs) occludes — meshes are not in the field")
                                            .clicked()
                                        {
                                            m = AoMode::Sdf;
                                        }
                                        if m != *ao {
                                            *ao = m;
                                            cmd.inspector_changed = true;
                                        }
                                    });
                                    if *ao != AoMode::Off {
                                        cmd.inspector_changed |= ui
                                            .add(egui::Slider::new(ao_strength, 0.0..=1.0).text("strength"))
                                            .changed();
                                        cmd.inspector_changed |= ui
                                            .add(egui::Slider::new(ao_radius, 0.05..=5.0).logarithmic(true).text("radius (m)"))
                                            .changed();
                                    }
                                    ui.separator();
                                    cmd.inspector_changed |= ui.checkbox(bloom, "Bloom").changed();
                                    if *bloom {
                                        cmd.inspector_changed |= ui
                                            .add(egui::Slider::new(bloom_threshold, 0.0..=2.0).text("threshold"))
                                            .changed();
                                        cmd.inspector_changed |= ui
                                            .add(egui::Slider::new(bloom_intensity, 0.0..=2.0).text("intensity"))
                                            .changed();
                                    }
                                    cmd.inspector_changed |= ui.checkbox(vignette, "Vignette").changed();
                                    if *vignette {
                                        cmd.inspector_changed |= ui
                                            .add(egui::Slider::new(vignette_strength, 0.0..=1.0).text("strength"))
                                            .changed();
                                        cmd.inspector_changed |= ui
                                            .add(egui::Slider::new(vignette_radius, 0.3..=1.0).text("radius"))
                                            .changed();
                                    }
                                    // Posterize — crush the final image to a limited palette.
                                    ui.separator();
                                    ui.horizontal(|ui| {
                                        ui.label("Posterize")
                                            .on_hover_text("reduce the final color to a fixed number of levels per channel — a limited-palette / banded retro look");
                                        let plabel = match *posterize_bands {
                                            0 | 1 => "off".to_string(),
                                            n => format!("{n} levels"),
                                        };
                                        egui::ComboBox::from_id_salt("posterize_bands")
                                            .selected_text(plabel)
                                            .show_ui(ui, |ui| {
                                                cmd.inspector_changed |=
                                                    ui.selectable_value(posterize_bands, 0, "off").clicked();
                                                for nb in [2u32, 3, 4, 5, 6, 8, 12, 16] {
                                                    cmd.inspector_changed |= ui
                                                        .selectable_value(posterize_bands, nb, format!("{nb} levels"))
                                                        .clicked();
                                                }
                                            });
                                    });
                                    ui.add_enabled_ui(*posterize_bands >= 2, |ui| {
                                        cmd.inspector_changed |= ui
                                            .checkbox(posterize_dither, "dither the bands")
                                            .on_hover_text("ordered dither so smooth gradients don't hard-step between levels")
                                            .changed();
                                    });
                                });
                            }
                        }
                    }
                });
                // Visibility (geometry nodes) — hide the node's visual without deleting it.
                if matches!(
                    world.get::<Matter>(e),
                    Some(Matter::Mesh { .. } | Matter::Primitive { .. } | Matter::Blob { .. })
                ) {
                    ui.indent("visible_toggle", |ui| {
                        let mut vis =
                            world.get::<floptle_core::Visible>(e).map(|v| v.0).unwrap_or(true);
                        if ui
                            .checkbox(&mut vis, "👁 visible")
                            .on_hover_text("uncheck to hide this node's geometry (scripts: node.visible = true/false)")
                            .changed()
                        {
                            cmd.set_visible = Some((e, vis));
                            cmd.inspector_changed = true;
                        }
                    });
                }

                // ===== Transform (always present) =====
                ui.separator();
                {
                    let (copy, paste, _) = component_header(
                        ui,
                        "⊕ Transform",
                        matches!(clip, Some(ComponentClip::Transform(_))),
                        false,
                    );
                    if copy
                        && let Some(t) = world.get::<Transform>(e) {
                            cmd.copy_component = Some(ComponentClip::Transform(*t));
                        }
                    if paste {
                        cmd.paste_component = Some(e);
                    }
                }
                ui.indent("xform_props", |ui| {
                    if let Some(t) = world.get_mut::<Transform>(e) {
                        ui.label("translation");
                        ui.horizontal(|ui| {
                            cmd.inspector_changed |= ui.add(egui::DragValue::new(&mut t.translation.x).speed(0.05).prefix("x ")).changed();
                            cmd.inspector_changed |= ui.add(egui::DragValue::new(&mut t.translation.y).speed(0.05).prefix("y ")).changed();
                            cmd.inspector_changed |= ui.add(egui::DragValue::new(&mut t.translation.z).speed(0.05).prefix("z ")).changed();
                        });
                        ui.label("rotation (deg)");
                        let (ey, ex, ez) = t.rotation.to_euler(EulerRot::YXZ);
                        let mut deg = [ey.to_degrees(), ex.to_degrees(), ez.to_degrees()];
                        let mut rot_changed = false;
                        ui.horizontal(|ui| {
                            rot_changed |= ui.add(egui::DragValue::new(&mut deg[0]).speed(1.0).prefix("y ")).changed();
                            rot_changed |= ui.add(egui::DragValue::new(&mut deg[1]).speed(1.0).prefix("x ")).changed();
                            rot_changed |= ui.add(egui::DragValue::new(&mut deg[2]).speed(1.0).prefix("z ")).changed();
                        });
                        if rot_changed {
                            t.rotation = Quat::from_euler(
                                EulerRot::YXZ,
                                deg[0].to_radians(),
                                deg[1].to_radians(),
                                deg[2].to_radians(),
                            );
                            cmd.inspector_changed = true;
                        }
                        ui.label("scale");
                        ui.horizontal(|ui| {
                            cmd.inspector_changed |= ui.add(egui::DragValue::new(&mut t.scale.x).speed(0.02).prefix("x ")).changed();
                            cmd.inspector_changed |= ui.add(egui::DragValue::new(&mut t.scale.y).speed(0.02).prefix("y ")).changed();
                            cmd.inspector_changed |= ui.add(egui::DragValue::new(&mut t.scale.z).speed(0.02).prefix("z ")).changed();
                        });
                    }
                });

                // ===== Material (only when the node has one) =====
                if world.get::<Material>(e).is_some() {
                    ui.separator();
                    let (copy, paste, remove) = component_header(
                        ui,
                        "◑ Material",
                        matches!(clip, Some(ComponentClip::Material(_))),
                        true,
                    );
                    if copy
                        && let Some(mat) = world.get::<Material>(e) {
                            cmd.copy_component = Some(ComponentClip::Material(Box::new(mat.clone())));
                        }
                    if paste {
                        cmd.paste_component = Some(e);
                    }
                    if remove {
                        cmd.remove_material = Some(e);
                    }
                    ui.indent("material_props", |ui| {
                        if let Some(mat) = world.get_mut::<Material>(e) {
                            let res = material_props_ui(ui, mat, self.materials, self.asset_tree, self.mat_name_buf, self.flsl_cache, self.sdf_cache);
                            cmd.inspector_changed |= res.changed;
                            if res.remove {
                                cmd.remove_material = Some(e);
                            }
                            if let Some(name) = res.save_as {
                                cmd.save_material =
                                    Some((name, floptle_scene::MaterialDoc::from_material(mat)));
                            }
                            if ui.button("⛶ Open in Material Editor").clicked() {
                                *self.show_material_editor = true;
                            }
                        }
                    });
                }

                // ===== Particle System (only when the node has one) =====
                if world.get::<floptle_core::ParticleSystem>(e).is_some() {
                    ui.separator();
                    let (copy, paste, remove) = component_header(
                        ui,
                        "✨ Particle System",
                        matches!(clip, Some(ComponentClip::Particles(_))),
                        true,
                    );
                    if copy
                        && let Some(ps) = world.get::<floptle_core::ParticleSystem>(e) {
                            cmd.copy_component = Some(ComponentClip::Particles(ps.clone()));
                        }
                    if paste {
                        cmd.paste_component = Some(e);
                    }
                    if remove {
                        cmd.remove_particles = Some(e);
                    }
                    let effect_keys: Vec<String> =
                        self.vfx.effects.iter().map(|(k, _)| k.clone()).collect();
                    ui.indent("particles_props", |ui| {
                        if let Some(ps) = world.get_mut::<floptle_core::ParticleSystem>(e) {
                            egui::ComboBox::from_label("Effect")
                                .selected_text(if ps.asset.is_empty() {
                                    "(none)".to_string()
                                } else {
                                    ps.asset.clone()
                                })
                                .show_ui(ui, |ui| {
                                    for k in &effect_keys {
                                        if ui
                                            .selectable_label(*k == ps.asset, k)
                                            .clicked()
                                        {
                                            ps.asset = k.clone();
                                            cmd.inspector_changed = true;
                                        }
                                    }
                                });
                            cmd.inspector_changed |= ui
                                .checkbox(&mut ps.play_on_start, "Play on start")
                                .on_hover_text(
                                    "Start emitting the moment Play begins \
                                     (off = a script triggers it)",
                                )
                                .changed();
                            let edit_key =
                                (!ps.asset.is_empty()).then(|| ps.asset.clone());
                            if let Some(k) = edit_key
                                && ui.button("✏ Edit effect").clicked()
                            {
                                cmd.open_particle_editor = Some(k);
                            }
                        }
                    });
                }

                // ===== Audio Source (only when the node has one) =====
                if world.get::<floptle_audio::AudioSource>(e).is_some() {
                    ui.separator();
                    let (copy, paste, remove) = component_header(
                        ui,
                        "♪ Audio Source",
                        matches!(clip, Some(ComponentClip::Audio(_))),
                        true,
                    );
                    if copy
                        && let Some(a) = world.get::<floptle_audio::AudioSource>(e) {
                            cmd.copy_component = Some(ComponentClip::Audio(a.clone()));
                        }
                    if paste {
                        cmd.paste_component = Some(e);
                    }
                    if remove {
                        cmd.remove_audio = Some(e);
                    }
                    // Clip candidates: browse the audio files as a foldered tree;
                    // the picked full path is stored as a project-relative key.
                    let tree = self.asset_tree;
                    let root = self.project_root;
                    let track_names: Vec<String> =
                        std::iter::once(floptle_audio::MASTER.to_string())
                            .chain(self.mixer.tracks.iter().map(|t| t.name.clone()))
                            .collect();
                    ui.indent("audio_props", |ui| {
                        if let Some(src) = world.get_mut::<floptle_audio::AudioSource>(e) {
                            ui.horizontal(|ui| {
                                ui.label("Clip");
                                let sel =
                                    if src.clip.is_empty() { "(none)" } else { src.clip.as_str() };
                                if let Some(pick) = crate::ui_widgets::asset_picker(
                                    ui,
                                    egui::Id::new(("audio_clip_pick", e)),
                                    sel,
                                    Some("(none)"),
                                    tree,
                                    crate::assets::is_audio,
                                    200.0,
                                ) {
                                    src.clip = pick
                                        .map(|p| {
                                            crate::assets::asset_rel_path(&p, root).replace('\\', "/")
                                        })
                                        .unwrap_or_default();
                                    cmd.inspector_changed = true;
                                }
                                if !src.clip.is_empty()
                                    && ui
                                        .button("▶")
                                        .on_hover_text("Preview the clip (flat, through Master)")
                                        .clicked()
                                {
                                    cmd.preview_audio = Some(src.clip.clone());
                                }
                            });
                            let p = &mut src.params;
                            cmd.inspector_changed |= ui
                                .add(
                                    egui::Slider::new(&mut p.volume, 0.0..=2.0).text("Volume"),
                                )
                                .changed();
                            cmd.inspector_changed |= ui
                                .add(egui::Slider::new(&mut p.pitch, 0.25..=4.0).text("Pitch").logarithmic(true))
                                .changed();
                            egui::ComboBox::from_label("Spatial")
                                .selected_text(p.mode.name())
                                .show_ui(ui, |ui| {
                                    for m in [
                                        floptle_audio::SpatialMode::Spatial,
                                        floptle_audio::SpatialMode::Distance,
                                        floptle_audio::SpatialMode::Flat,
                                    ] {
                                        if ui.selectable_label(p.mode == m, m.name()).clicked() {
                                            p.mode = m;
                                            cmd.inspector_changed = true;
                                        }
                                    }
                                });
                            match p.mode {
                                floptle_audio::SpatialMode::Flat => {
                                    cmd.inspector_changed |= ui
                                        .add(egui::Slider::new(&mut p.pan, -1.0..=1.0).text("Pan"))
                                        .changed();
                                }
                                _ => {
                                    egui::ComboBox::from_label("Falloff")
                                        .selected_text(p.falloff.name())
                                        .show_ui(ui, |ui| {
                                            for f in [
                                                floptle_audio::Falloff::Inverse,
                                                floptle_audio::Falloff::Linear,
                                                floptle_audio::Falloff::Exponential,
                                            ] {
                                                if ui
                                                    .selectable_label(p.falloff == f, f.name())
                                                    .clicked()
                                                {
                                                    p.falloff = f;
                                                    cmd.inspector_changed = true;
                                                }
                                            }
                                        });
                                    ui.horizontal(|ui| {
                                        ui.label("Distance");
                                        cmd.inspector_changed |= ui
                                            .add(
                                                egui::DragValue::new(&mut p.min_distance)
                                                    .speed(0.1)
                                                    .range(0.01..=10_000.0)
                                                    .prefix("min "),
                                            )
                                            .changed();
                                        cmd.inspector_changed |= ui
                                            .add(
                                                egui::DragValue::new(&mut p.max_distance)
                                                    .speed(0.5)
                                                    .range(0.02..=100_000.0)
                                                    .prefix("max "),
                                            )
                                            .on_hover_text(
                                                "Full volume inside min; silent past max",
                                            )
                                            .changed();
                                    });
                                }
                            }
                            egui::ComboBox::from_label("Mixer track")
                                .selected_text(if p.track.is_empty() {
                                    floptle_audio::MASTER
                                } else {
                                    p.track.as_str()
                                })
                                .show_ui(ui, |ui| {
                                    for t in &track_names {
                                        let cur = if p.track.is_empty() {
                                            floptle_audio::MASTER
                                        } else {
                                            p.track.as_str()
                                        };
                                        if ui.selectable_label(cur == t, t).clicked() {
                                            p.track = if t == floptle_audio::MASTER {
                                                String::new()
                                            } else {
                                                t.clone()
                                            };
                                            cmd.inspector_changed = true;
                                        }
                                    }
                                });
                            egui::ComboBox::from_label("On end")
                                .selected_text(p.end.name())
                                .show_ui(ui, |ui| {
                                    for (b, hint) in [
                                        (floptle_audio::EndBehavior::Stop, "The node stays; replayable from scripts"),
                                        (floptle_audio::EndBehavior::Destroy, "Despawn the node when the sound finishes"),
                                        (floptle_audio::EndBehavior::Loop, "Restart seamlessly forever"),
                                    ] {
                                        if ui
                                            .selectable_label(p.end == b, b.name())
                                            .on_hover_text(hint)
                                            .clicked()
                                        {
                                            p.end = b;
                                            cmd.inspector_changed = true;
                                        }
                                    }
                                });
                            cmd.inspector_changed |= ui
                                .checkbox(&mut src.play_on_start, "Play on start")
                                .on_hover_text(
                                    "Start playing the moment Play begins \
                                     (off = a script triggers it via node:sound():play())",
                                )
                                .changed();
                        }
                    });
                }

                // ===== Rigidbody (only when the node has one) =====
                if world.get::<floptle_core::RigidBody>(e).is_some() {
                    ui.separator();
                    let (copy, paste, remove) = component_header(
                        ui,
                        "♦ Rigidbody",
                        matches!(clip, Some(ComponentClip::RigidBody(_))),
                        true,
                    );
                    if copy
                        && let Some(rb) = world.get::<floptle_core::RigidBody>(e) {
                            cmd.copy_component = Some(ComponentClip::RigidBody(*rb));
                        }
                    if paste {
                        cmd.paste_component = Some(e);
                    }
                    if remove {
                        cmd.remove_rigidbody = Some(e);
                    }
                    ui.indent("rb_props", |ui| {
                        if let Some(rb) = world.get_mut::<floptle_core::RigidBody>(e) {
                            use floptle_core::{BodyKind, BodyMode};
                            // The ONE dropdown that replaces hand-freezing axes +
                            // disabling gravity. Structural (a Static body is a
                            // baked collider, not a body) — rebuild the live sim.
                            ui.horizontal(|ui| {
                                ui.label("mode");
                                let label = match rb.mode {
                                    BodyMode::Dynamic => "Dynamic",
                                    BodyMode::Kinematic => "Kinematic",
                                    BodyMode::Static => "Static",
                                };
                                egui::ComboBox::from_id_salt("rb-mode")
                                    .selected_text(label)
                                    .show_ui(ui, |ui| {
                                        let mut changed = false;
                                        changed |= ui
                                            .selectable_value(&mut rb.mode, BodyMode::Dynamic, "Dynamic")
                                            .on_hover_text("fully simulated: gravity, collisions, gets pushed around")
                                            .changed();
                                        changed |= ui
                                            .selectable_value(&mut rb.mode, BodyMode::Kinematic, "Kinematic")
                                            .on_hover_text("transform-driven: never falls or gets pushed — scripts/animation move it, and dynamic bodies collide WITH it (moving platforms, elevators). Near-zero per-tick cost")
                                            .changed();
                                        changed |= ui
                                            .selectable_value(&mut rb.mode, BodyMode::Static, "Static")
                                            .on_hover_text("baked immovable collider in this shape — no body at all, ZERO per-tick cost (walls, floors, props)")
                                            .changed();
                                        if changed {
                                            cmd.inspector_changed = true;
                                            cmd.rebuild_physics = true;
                                        }
                                    });
                                match rb.mode {
                                    BodyMode::Dynamic => {}
                                    BodyMode::Kinematic => {
                                        ui.small("moves via its transform; pushes dynamic bodies");
                                    }
                                    BodyMode::Static => {
                                        ui.small("baked collider — cheapest way to be solid");
                                    }
                                }
                            });
                            ui.horizontal(|ui| {
                                ui.label("shape");
                                egui::ComboBox::from_id_salt("rb-shape")
                                    .selected_text(match rb.kind {
                                        BodyKind::Sphere => "Sphere",
                                        BodyKind::Capsule => "Capsule",
                                        BodyKind::Box => "Box",
                                    })
                                    .show_ui(ui, |ui| {
                                        cmd.inspector_changed |=
                                            ui.selectable_value(&mut rb.kind, BodyKind::Sphere, "Sphere").changed();
                                        cmd.inspector_changed |=
                                            ui.selectable_value(&mut rb.kind, BodyKind::Capsule, "Capsule").changed();
                                        cmd.inspector_changed |=
                                            ui.selectable_value(&mut rb.kind, BodyKind::Box, "Box").changed();
                                    });
                            });
                            if rb.kind == BodyKind::Box {
                                ui.label("half-extents");
                                ui.horizontal(|ui| {
                                    for (i, ax) in ["x", "y", "z"].iter().enumerate() {
                                        cmd.inspector_changed |= ui
                                            .add(egui::DragValue::new(&mut rb.half_extents[i]).speed(0.02).range(0.02..=50.0).prefix(format!("{ax} ")))
                                            .changed();
                                    }
                                });
                            } else {
                                cmd.inspector_changed |=
                                    ui.add(egui::Slider::new(&mut rb.radius, 0.05..=10.0).text("radius")).changed();
                                if rb.kind == BodyKind::Capsule {
                                    cmd.inspector_changed |=
                                        ui.add(egui::Slider::new(&mut rb.height, 0.2..=20.0).text("height")).changed();
                                }
                            }
                            // Bounce/friction/gravity/locks only matter on a
                            // SIMULATED body — grey them out otherwise so the
                            // mode dropdown reads as the one switch it is.
                            let dynamic = rb.mode == BodyMode::Dynamic;
                            ui.add_enabled_ui(dynamic, |ui| {
                                cmd.inspector_changed |=
                                    ui.add(egui::Slider::new(&mut rb.restitution, 0.0..=1.0).text("bounce")).changed();
                                cmd.inspector_changed |=
                                    ui.add(egui::Slider::new(&mut rb.friction, 0.0..=1.0).text("friction")).changed();
                                cmd.inspector_changed |= ui
                                    .checkbox(&mut rb.gravity, "affected by gravity")
                                    .on_hover_text("off = floats (still collides; a script can still move it)")
                                    .changed();
                                ui.horizontal(|ui| {
                                    ui.label("freeze pos");
                                    for (i, ax) in ["x", "y", "z"].iter().enumerate() {
                                        cmd.inspector_changed |= ui.toggle_value(&mut rb.lock_pos[i], *ax).changed();
                                    }
                                });
                                ui.horizontal(|ui| {
                                    ui.label("freeze rot");
                                    for (i, ax) in ["x", "y", "z"].iter().enumerate() {
                                        cmd.inspector_changed |= ui.toggle_value(&mut rb.lock_rot[i], *ax).changed();
                                    }
                                });
                                cmd.inspector_changed |= ui
                                    .checkbox(&mut rb.align_up, "align to gravity")
                                    .on_hover_text(
                                        "Tilt this node so its up follows −gravity — a \
                                         character on a radial-gravity planet stands on it \
                                         (and its camera/children inherit the tilt). \
                                         Overrides freeze rot.",
                                    )
                                    .changed();
                            });
                        }
                    });
                    // Trigger: the BODY becomes a sensor — it never blocks or gets
                    // blocked (and rays skip it), but overlap fires the trigger
                    // hooks. Moving pickups, sweeping zones, pass-through projectiles.
                    let mut trig = world.get::<floptle_core::Trigger>(e).is_some();
                    if ui
                        .checkbox(&mut trig, "trigger")
                        .on_hover_text(
                            "events only, no blocking: the body passes through everything \
                             and nothing pushes back, but overlap fires onTriggerEnter / \
                             onTriggerStay / onTriggerExit on both nodes' scripts. A \
                             Dynamic trigger still falls — use Kinematic (or gravity off) \
                             for pickups and zones that stay put",
                        )
                        .changed()
                    {
                        cmd.set_trigger = Some((e, trig));
                    }
                    // The body shape doubles as the node's sun-shadow proxy (see the
                    // Lighting node) — casting is the default; the component only
                    // exists to record an opt-out.
                    let mut casts =
                        world.get::<floptle_core::CastShadow>(e).map(|c| c.0).unwrap_or(true);
                    if ui
                        .checkbox(&mut casts, "casts shadows")
                        .on_hover_text("this body shape stands in for the mesh in the sun-shadow march — untick to stop this node casting")
                        .changed()
                    {
                        if casts {
                            world.remove::<floptle_core::CastShadow>(e);
                        } else {
                            world.insert(e, floptle_core::CastShadow(false));
                        }
                        cmd.inspector_changed = true;
                    }
                }

                // ===== Celestial Body (on-rails orbit; only when the node has one) =====
                if world.get::<floptle_core::CelestialBody>(e).is_some() {
                    ui.separator();
                    let (_, _, remove) = component_header(ui, "🪐 Celestial Body", false, true);
                    if remove {
                        cmd.remove_celestial = Some(e);
                    }
                    ui.indent("cb_props", |ui| {
                        if let Some(cb) = world.get_mut::<floptle_core::CelestialBody>(e) {
                            let drag = |ui: &mut egui::Ui, label: &str, v: &mut f64, speed: f64, hover: &str| -> bool {
                                ui.horizontal(|ui| {
                                    ui.label(label);
                                    ui.add(egui::DragValue::new(v).speed(speed))
                                        .on_hover_text(hover)
                                        .changed()
                                })
                                .inner
                            };
                            let mut ch = false;
                            ch |= drag(ui, "µ (GM)", &mut cb.mu, 1000.0, "gravitational parameter — surface gravity = µ / radius²");
                            ch |= drag(ui, "radius", &mut cb.body_radius, 1.0, "physical surface radius (altitude readouts, impostors)");
                            ch |= drag(ui, "SOI", &mut cb.soi, 10.0, "sphere-of-influence radius; 0 = auto (Laplace) from the parent");
                            ch |= drag(ui, "occluder", &mut cb.occluder_radius, 1.0, "occlusion culling: radius of the solid core geometry never pierces — terrain chunks fully behind it skip their draws. Keep BELOW the deepest cave/dig; 0 = off");
                            ui.horizontal(|ui| {
                                ui.label("parent");
                                ch |= ui
                                    .text_edit_singleline(&mut cb.parent)
                                    .on_hover_text("NAME of the parent body's node; empty = system root (stays put)")
                                    .changed();
                            });
                            ui.small("orbit around the parent (radians, semi-major in units):");
                            ch |= drag(ui, "semi-major a", &mut cb.a, 1.0, "orbit size; NEGATIVE = hyperbolic escape");
                            ch |= drag(ui, "eccentricity e", &mut cb.e, 0.005, "0 = circle, <1 ellipse, >1 hyperbola");
                            ch |= drag(ui, "inclination i", &mut cb.i, 0.01, "tilt from the XZ plane (radians)");
                            ch |= drag(ui, "node Ω", &mut cb.lan, 0.01, "longitude of the ascending node (radians)");
                            ch |= drag(ui, "periapsis ω", &mut cb.arg_pe, 0.01, "argument of periapsis (radians)");
                            ch |= drag(ui, "phase M₀", &mut cb.m0, 0.01, "mean anomaly at t = 0 — where on the orbit it starts");
                            ui.small("atmosphere (S8; height 0 = airless):");
                            ui.horizontal(|ui| {
                                ui.label("sky color");
                                ch |= ui
                                    .color_edit_button_rgb(&mut cb.atmo_color)
                                    .on_hover_text("the sky seen from inside the atmosphere")
                                    .changed();
                            });
                            ch |= drag(ui, "atmo height", &mut cb.atmo_height, 1.0, "shell height above the surface; the sky fades to space across it");
                            ui.horizontal(|ui| {
                                ui.label("density");
                                ch |= ui
                                    .add(egui::Slider::new(&mut cb.atmo_density, 0.0..=1.0))
                                    .on_hover_text("how opaque the sky gets at full depth")
                                    .changed();
                            });
                            ui.horizontal(|ui| {
                                ui.label("clouds");
                                ch |= ui
                                    .add(egui::Slider::new(&mut cb.clouds, 0.0..=1.0))
                                    .on_hover_text("cloud coverage in the atmosphere (0 = clear)")
                                    .changed();
                            });
                            ui.small("star (Lighting `stars mode` uses these as the lights):");
                            ui.horizontal(|ui| {
                                ui.label("luminosity");
                                ch |= ui
                                    .add(egui::DragValue::new(&mut cb.luminosity).speed(0.5))
                                    .on_hover_text(
                                        "0 = not a star. Irradiance at distance d = luminosity × 1e6 / d² \
                                         — ~36 fully lights a planet 6000 units away.",
                                    )
                                    .changed();
                                ui.label("color");
                                ch |= ui.color_edit_button_rgb(&mut cb.star_color).changed();
                            });
                            if ch {
                                cmd.inspector_changed = true;
                            }
                        }
                    });
                }

                // ===== Game UI (layer/element; only when the node has one) =====
                {
                    if crate::Editor::ui_inspector(world, e, ui, self.asset_tree, self.texture_settings) {
                        cmd.inspector_changed = true;
                    }
                }

                // ===== Networked (replication; only when the node has one) =====
                // The authored half of the netcode (docs/netcode-design.md §4.2): which
                // props sync and whether the owner-client predicts it. Owner/NetId are
                // session state, assigned at runtime — not edited here.
                if world.get::<floptle_core::Replicated>(e).is_some() {
                    ui.separator();
                    let remove = component_header_no_copy(ui, "🌐 Networked", true);
                    if remove {
                        world.remove::<floptle_core::Replicated>(e);
                        cmd.inspector_changed = true;
                    }
                    ui.indent("net_props", |ui| {
                        if let Some(rep) = world.get_mut::<floptle_core::Replicated>(e) {
                            use floptle_core::ReplicationMode;
                            ui.horizontal(|ui| {
                                ui.label("mode");
                                egui::ComboBox::from_id_salt("net-mode")
                                    .selected_text(match rep.mode {
                                        ReplicationMode::Authority => "Server authority",
                                        ReplicationMode::Predicted => "Predicted (owner)",
                                    })
                                    .show_ui(ui, |ui| {
                                        cmd.inspector_changed |= ui
                                            .selectable_value(
                                                &mut rep.mode,
                                                ReplicationMode::Authority,
                                                "Server authority",
                                            )
                                            .on_hover_text("the server simulates it; clients render interpolated snapshots — the default, cheat-proof mode")
                                            .changed();
                                        cmd.inspector_changed |= ui
                                            .selectable_value(
                                                &mut rep.mode,
                                                ReplicationMode::Predicted,
                                                "Predicted (owner)",
                                            )
                                            .on_hover_text("the owning player's client ALSO simulates it locally, ahead of the server (their own avatar) — the server still has the final word")
                                            .changed();
                                    });
                            });
                            cmd.inspector_changed |= ui
                                .checkbox(&mut rep.transform, "sync transform")
                                .on_hover_text("replicate position/rotation to clients")
                                .changed();
                            cmd.inspector_changed |= ui
                                .checkbox(&mut rep.physics, "sync physics")
                                .on_hover_text("replicate velocity too — better extrapolation, required to predict a rigidbody")
                                .changed();
                            cmd.inspector_changed |= ui
                                .checkbox(&mut rep.animator, "sync animator")
                                .on_hover_text(
                                    "replicate the Animation Controller's playback (which state + \
                                     where in it, per layer) — a few bytes per TRANSITION; every \
                                     machine samples the pose locally. Off = client-sided: each \
                                     client drives this node's animator itself",
                                )
                                .changed();
                            cmd.inspector_changed |= ui
                                .checkbox(&mut rep.interp, "interpolate")
                                .on_hover_text("smooth remote copies between snapshots (off = snap, for teleporty things)")
                                .changed();
                            if rep.interp {
                                let mut d = rep.interp_delay as i32;
                                if ui
                                    .add(egui::Slider::new(&mut d, 0..=30).text("interp delay (ticks)"))
                                    .on_hover_text("how far behind the server remote copies render — 6 ticks ≈ 100 ms. Lower = tighter tracking (stutters under jitter/loss); higher = smoother on bad links")
                                    .changed()
                                {
                                    rep.interp_delay = d as u8;
                                    cmd.inspector_changed = true;
                                }
                            }
                        }
                    });
                    ui.small("only nodes with this component replicate — everything else stays local. Sessions start via Lua: net.host{} / net.join(...)");
                }

                // ===== Collider (static collision; only when the node has one) =====
                // Auto-shaped from the node's geometry (Cube → box, Sphere → sphere,
                // Capsule → capsule, Mesh → its triangles). A legacy MeshCollider counts.
                {
                    let has_collidable = world.get::<floptle_core::Collidable>(e).is_some()
                        || world.get::<floptle_core::MeshCollider>(e).is_some();
                    if has_collidable {
                        let kind = match world.get::<Matter>(e) {
                            Some(Matter::Mesh { .. }) => "triangle mesh",
                            Some(Matter::Primitive { shape, .. }) => match shape {
                                floptle_core::Shape::Cube | floptle_core::Shape::Plane => "box",
                                floptle_core::Shape::Sphere => "sphere",
                                floptle_core::Shape::Capsule => "capsule",
                            },
                            _ => "mesh",
                        };
                        ui.separator();
                        let remove = component_header_no_copy(ui, "▦ Collider", true);
                        ui.small(format!(
                            "static {kind} collider — built from this node's geometry on Play. Walk on it / bump into it; no rigidbody needed. Scale the node to resize it."
                        ));
                        if world.get::<floptle_core::RigidBody>(e).is_some() {
                            ui.small("⚠ This node also has a Rigidbody, so its body owns the physics and this static Collider is ignored — the trigger checkbox lives on the Rigidbody above. To make it a solid obstacle, set the Rigidbody's mode to Static (a baked collider in the body's shape) — or remove the Rigidbody to use this geometry-shaped collider instead.");
                        } else {
                            // The collider doubles as the node's sun-shadow caster:
                            // primitives stand in as analytic proxy shapes, and a
                            // Collidable MESH is baked into a shadow-only occluder
                            // volume (its true silhouette — interiors go dark).
                            let mut casts = world
                                .get::<floptle_core::CastShadow>(e)
                                .map(|c| c.0)
                                .unwrap_or(true);
                            if ui
                                .checkbox(&mut casts, "casts shadows")
                                .on_hover_text("this collider stands in for the node in the sun-shadow march (primitives as proxy shapes, meshes as a baked occluder volume) — untick to stop this node casting")
                                .changed()
                            {
                                if casts {
                                    world.remove::<floptle_core::CastShadow>(e);
                                } else {
                                    world.insert(e, floptle_core::CastShadow(false));
                                }
                                cmd.inspector_changed = true;
                            }
                            // Trigger: bodies pass through, overlap fires the
                            // onTriggerEnter/Stay/Exit hooks — portals, pickup
                            // zones, checkpoints.
                            let mut trig = world.get::<floptle_core::Trigger>(e).is_some();
                            if ui
                                .checkbox(&mut trig, "trigger")
                                .on_hover_text(
                                    "events only, no blocking: bodies (and rays) pass through, \
                                     but overlap fires onTriggerEnter / onTriggerStay / \
                                     onTriggerExit on both nodes' scripts",
                                )
                                .changed()
                            {
                                cmd.set_trigger = Some((e, trig));
                            }
                        }
                        if remove {
                            cmd.set_collidable = Some((e, false));
                            cmd.inspector_changed = true;
                        }
                    }
                }

                // ===== Scripts =====
                ui.separator();
                // Always-available drop target: drag a script here to attach it.
                {
                    let (_, dropped) = ui.dnd_drop_zone::<AssetPayload, ()>(
                        egui::Frame::group(ui.style()),
                        |ui| {
                            ui.set_min_height(18.0);
                            ui.small("⚙  drop a script here to attach (or use ➕ Add Component)");
                        },
                    );
                    if let Some(p) = dropped
                        && is_script(&p.path) {
                            cmd.drop_script_on = Some((p.path.clone(), e));
                        }
                }
                if world.get::<Scripts>(e).map(|s| !s.0.is_empty()).unwrap_or(false) {
                    // Menu first (right-to-left) so it stays pinned on-screen —
                    // see component_header.
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if matches!(clip, Some(ComponentClip::Script(_))) {
                                ui.menu_button("…", |ui| {
                                    if ui.button("📋  Paste script").clicked() {
                                        cmd.paste_component = Some(e);
                                        ui.close();
                                    }
                                })
                                .response
                                .on_hover_text("adds the copied script, or updates a matching one");
                            }
                            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                ui.strong("⚙ Scripts");
                            });
                        });
                    });
                    let mut remove: Option<usize> = None;
                    let mut copy_idx: Option<usize> = None;
                    // Candidates for reference params, filtered by declared kind:
                    // noderef → any named node; scriptref(k) → nodes carrying that
                    // script; componentref(c) → nodes carrying that component.
                    let mut node_names: Vec<String> =
                        world.query::<floptle_core::Name>().map(|(_, n)| n.0.clone()).collect();
                    node_names.sort();
                    node_names.dedup();
                    let mut script_nodes: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
                    for (oe, sc) in world.query::<Scripts>() {
                        if let Some(n) = world.get::<floptle_core::Name>(oe) {
                            for si in &sc.0 {
                                script_nodes.entry(si.kind.clone()).or_default().push(n.0.clone());
                            }
                        }
                    }
                    for v in script_nodes.values_mut() {
                        v.sort();
                        v.dedup();
                    }
                    let mut comp_nodes: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
                    for kind in self.ref_kinds.values() {
                        if let floptle_script::RefKind::Component(c) = kind
                            && !comp_nodes.contains_key(c)
                        {
                            let mut v: Vec<String> = world
                                .query::<floptle_core::Name>()
                                .filter(|(oe, _)| node_has_component(world, *oe, c))
                                .map(|(_, n)| n.0.clone())
                                .collect();
                            v.sort();
                            v.dedup();
                            comp_nodes.insert(c.clone(), v);
                        }
                    }
                    // Entity → name, for dropped hierarchy nodes.
                    let name_of: std::collections::HashMap<floptle_core::Entity, String> = world
                        .query::<floptle_core::Name>()
                        .map(|(oe, n)| (oe, n.0.clone()))
                        .collect();
                    ui.indent("script_list", |ui| {
                        if let Some(scr) = world.get_mut::<Scripts>(e) {
                            for (i, inst) in scr.0.iter_mut().enumerate() {
                                // Menu first (right-to-left) so a long script
                                // name truncates instead of pushing the … menu
                                // off-screen — see component_header.
                                ui.horizontal(|ui| {
                                    cmd.inspector_changed |= ui.checkbox(&mut inst.enabled, "").changed();
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        ui.menu_button("…", |ui| {
                                            if ui.button("⎘  Copy values").clicked() {
                                                copy_idx = Some(i);
                                                ui.close();
                                            }
                                            if ui.button("🖊  Edit script").clicked() {
                                                let p = self
                                                    .project_root
                                                    .join("scripts")
                                                    .join(format!("{}.lua", inst.kind));
                                                cmd.open_script_pref = Some(p.to_string_lossy().to_string());
                                                ui.close();
                                            }
                                            ui.separator();
                                            if ui.button("🗑  Remove").clicked() {
                                                remove = Some(i);
                                                ui.close();
                                            }
                                        });
                                        ui.with_layout(
                                            egui::Layout::left_to_right(egui::Align::Center),
                                            |ui| {
                                                ui.add(
                                                    egui::Label::new(
                                                        egui::RichText::new(&inst.kind).strong(),
                                                    )
                                                    .truncate(),
                                                );
                                            },
                                        );
                                    });
                                });
                                // Editor-action buttons the script declares
                                // (`--@editorButton Label fn`): clicking runs
                                // fn(node) against the OPEN scene — Lua editor
                                // tooling (generators, batch fixups).
                                let actions = crate::script_actions::script_editor_buttons(
                                    self.project_root,
                                    &inst.kind,
                                );
                                if !actions.is_empty() {
                                    ui.horizontal_wrapped(|ui| {
                                        for (label, func) in actions {
                                            if ui
                                                .button(format!("▶ {label}"))
                                                .on_hover_text(format!(
                                                    "runs {func}(node) from {}.lua on this node, \
                                                     editing the open scene",
                                                    inst.kind
                                                ))
                                                .clicked()
                                            {
                                                cmd.run_editor_action =
                                                    Some((e, inst.kind.clone(), func));
                                            }
                                        }
                                    });
                                }
                                for (k, v) in inst.params.iter_mut() {
                                    cmd.inspector_changed |= ui
                                        .add(egui::DragValue::new(v).speed(0.05).prefix(format!("{k}  ")))
                                        .changed();
                                }
                                // String params (`name = "text"` in defaults):
                                // free-text tunables — a portal's destination
                                // scene, an item id. Same two-way rules as the
                                // numbers.
                                for (k, v) in inst.strs.iter_mut() {
                                    ui.horizontal(|ui| {
                                        ui.label(k.as_str());
                                        cmd.inspector_changed |= ui
                                            .add(
                                                egui::TextEdit::singleline(v)
                                                    .desired_width(140.0),
                                            )
                                            .changed();
                                    });
                                }
                                // Reference params (`name = noderef() / scriptref(k) /
                                // componentref(c)` in the script's defaults): wire a
                                // scene node — the script gets a node / script /
                                // component handle directly, no find() scans. Pick
                                // from the (kind-filtered) list, or DRAG a node from
                                // the Hierarchy onto the row.
                                for (ri, (k, target)) in inst.refs.iter_mut().enumerate() {
                                    let kind = self.ref_kinds.get(&(inst.kind.clone(), k.clone()));
                                    let empty: Vec<String> = Vec::new();
                                    let (cands, hint) = match kind {
                                        Some(floptle_script::RefKind::Script(sk)) => (
                                            script_nodes.get(sk).unwrap_or(&empty),
                                            format!("→ the '{sk}' SCRIPT on the wired node (lists nodes carrying it); drag a node from the Hierarchy to wire"),
                                        ),
                                        Some(floptle_script::RefKind::Component(c)) => (
                                            comp_nodes.get(c).unwrap_or(&empty),
                                            format!("→ the {c} COMPONENT on the wired node (lists nodes carrying it); drag a node from the Hierarchy to wire"),
                                        ),
                                        _ => (
                                            &node_names,
                                            "→ a node handle; drag a node from the Hierarchy to wire".to_string(),
                                        ),
                                    };
                                    let row = ui.horizontal(|ui| {
                                        ui.label(format!("{k}  ")).on_hover_text(&hint);
                                        if let Some(pick) = crate::ui_widgets::searchable_picker(
                                            ui,
                                            egui::Id::new(("script_ref", e.index(), i, ri)),
                                            if target.is_empty() { "(pick node)" } else { target },
                                            Some("(none)"),
                                            cands,
                                            150.0,
                                        ) {
                                            *target = pick.unwrap_or_default();
                                            cmd.inspector_changed = true;
                                        }
                                        match kind {
                                            Some(floptle_script::RefKind::Script(sk)) => {
                                                ui.weak(format!("⚙{sk}"));
                                            }
                                            Some(floptle_script::RefKind::Component(c)) => {
                                                ui.weak(format!("◆{c}"));
                                            }
                                            _ => {}
                                        }
                                    })
                                    .response;
                                    // Drag-and-drop wiring: drop a Hierarchy node here.
                                    if let Some(p) = row.dnd_hover_payload::<crate::hierarchy::NodePayload>() {
                                        let ok = name_of
                                            .get(&p.0)
                                            .is_some_and(|n| cands.contains(n));
                                        ui.painter().rect_stroke(
                                            row.rect.expand(2.0),
                                            3.0,
                                            egui::Stroke::new(
                                                1.5,
                                                if ok {
                                                    egui::Color32::from_rgb(120, 220, 120)
                                                } else {
                                                    egui::Color32::from_rgb(220, 120, 120)
                                                },
                                            ),
                                            egui::StrokeKind::Outside,
                                        );
                                    }
                                    if let Some(p) = row.dnd_release_payload::<crate::hierarchy::NodePayload>()
                                        && let Some(n) = name_of.get(&p.0)
                                        && cands.contains(n)
                                    {
                                        *target = n.clone();
                                        cmd.inspector_changed = true;
                                    }
                                }
                                ui.add_space(4.0);
                            }
                            if let Some(i) = copy_idx {
                                cmd.copy_component = Some(ComponentClip::Script(scr.0[i].clone()));
                            }
                            if let Some(i) = remove {
                                scr.0.remove(i);
                                cmd.inspector_changed = true;
                            }
                        }
                    });
                }

                // ===== Animation Controller (when attached) =====
                anim_ui::anim_component_ui(ui, e, world, &*self.anim, self.anim_ui, cmd);

                // ===== 🔗 Bone attachment (node parented to a rigged mesh) =====
                if let Some(floptle_core::Parent(mesh)) = world.get::<floptle_core::Parent>(e).copied()
                    && let Some(bones) = bone_names.get(&mesh)
                {
                    ui.separator();
                    ui.strong("🔗 Bone attachment");
                    ui.small("ride a bone / part of the parent model (a weapon on a hand)");
                    let cur = world.get::<floptle_core::BoneAttach>(e).map(|a| a.bone.clone());
                    egui::ComboBox::from_id_salt("bone_attach_pick")
                        .selected_text(cur.clone().unwrap_or_else(|| "(not attached)".into()))
                        .show_ui(ui, |ui| {
                            if ui.selectable_label(cur.is_none(), "(not attached)").clicked()
                                && cur.is_some()
                            {
                                world.remove::<floptle_core::BoneAttach>(e);
                                cmd.inspector_changed = true;
                            }
                            for (name, _parent) in bones {
                                let sel = cur.as_deref() == Some(name.as_str());
                                if ui.selectable_label(sel, name).clicked() && !sel {
                                    // Attach snapping the node to the bone (offset kept if
                                    // re-picking, else identity — then nudge it below).
                                    let offset = world
                                        .get::<floptle_core::BoneAttach>(e)
                                        .map(|a| a.offset)
                                        .unwrap_or(floptle_core::transform::Transform::IDENTITY);
                                    world.insert(
                                        e,
                                        floptle_core::BoneAttach { target: mesh, bone: name.clone(), offset },
                                    );
                                    cmd.inspector_changed = true;
                                }
                            }
                        });
                    // Offset editor + detach (only when attached) — position the node on
                    // the bone relative to it.
                    if let Some(a) = world.get::<floptle_core::BoneAttach>(e).cloned() {
                        let mut off = a.offset;
                        let mut ch = false;
                        ui.horizontal(|ui| {
                            ui.label("pos");
                            ch |= ui.add(egui::DragValue::new(&mut off.translation.x).speed(0.01).prefix("x ")).changed();
                            ch |= ui.add(egui::DragValue::new(&mut off.translation.y).speed(0.01).prefix("y ")).changed();
                            ch |= ui.add(egui::DragValue::new(&mut off.translation.z).speed(0.01).prefix("z ")).changed();
                        });
                        let (ey, ex, ez) = off.rotation.to_euler(EulerRot::YXZ);
                        let mut deg = [ex.to_degrees(), ey.to_degrees(), ez.to_degrees()];
                        ui.horizontal(|ui| {
                            ui.label("rot°");
                            let mut rc = false;
                            rc |= ui.add(egui::DragValue::new(&mut deg[0]).speed(0.5).prefix("x ")).changed();
                            rc |= ui.add(egui::DragValue::new(&mut deg[1]).speed(0.5).prefix("y ")).changed();
                            rc |= ui.add(egui::DragValue::new(&mut deg[2]).speed(0.5).prefix("z ")).changed();
                            if rc {
                                off.rotation = Quat::from_euler(
                                    EulerRot::YXZ,
                                    deg[1].to_radians(),
                                    deg[0].to_radians(),
                                    deg[2].to_radians(),
                                );
                                ch = true;
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label("scale");
                            let mut s = off.scale.x;
                            if ui.add(egui::DragValue::new(&mut s).speed(0.01).range(0.001..=100.0)).changed() {
                                off.scale = floptle_core::math::Vec3::splat(s);
                                ch = true;
                            }
                            if ui.button("🗑 detach").clicked() {
                                world.remove::<floptle_core::BoneAttach>(e);
                                cmd.inspector_changed = true;
                            }
                        });
                        if ch {
                            if let Some(at) = world.get_mut::<floptle_core::BoneAttach>(e) {
                                at.offset = off;
                            }
                            cmd.inspector_changed = true;
                        }
                    }
                }

                // ===== ➕ Add Component (searchable, icon'd) =====
                ui.separator();
                ui.add_space(2.0);
                let add_btn = ui.button("➕  Add Component");
                let add_popup_id = egui::Popup::default_response_id(&add_btn);
                // True only on the frame the menu transitions closed → open, so we
                // focus the search box exactly once (start typing immediately).
                let add_opening =
                    add_btn.clicked() && !egui::Popup::is_id_open(ui.ctx(), add_popup_id);
                // CloseOnClickOutside (not the menu default CloseOnClick) so clicking
                // the search field doesn't dismiss the menu.
                egui::Popup::menu(&add_btn)
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                    .width(236.0)
                    .show(|ui| {
                    let filter = &mut *self.add_component_filter;
                    let search = ui.add(
                        egui::TextEdit::singleline(filter)
                            .hint_text("🔍 search components…")
                            .desired_width(212.0),
                    );
                    if add_opening {
                        search.request_focus();
                    }
                    let f = filter.trim().to_lowercase();
                    let hit = |s: &str| f.is_empty() || s.to_lowercase().contains(&f);

                    // What the node already has decides what's offered.
                    let cur = world.get::<Matter>(e);
                    let is_terrain = matches!(cur, Some(Matter::Terrain { .. }));
                    let has_mat = world.get::<Material>(e).is_some();
                    let has_rb = world.get::<floptle_core::RigidBody>(e).is_some();
                    let has_net = world.get::<floptle_core::Replicated>(e).is_some();
                    let has_collidable = world.get::<floptle_core::Collidable>(e).is_some()
                        || world.get::<floptle_core::MeshCollider>(e).is_some();
                    let collider_kind = match cur {
                        Some(Matter::Mesh { .. }) => Some("triangle mesh"),
                        Some(Matter::Primitive { shape, .. }) => Some(match shape {
                            floptle_core::Shape::Cube | floptle_core::Shape::Plane => "box",
                            floptle_core::Shape::Sphere => "sphere",
                            floptle_core::Shape::Capsule => "capsule",
                        }),
                        _ => None,
                    };
                    let cur_kind = cur.map(matter_kind_label);

                    // One catalog of (category, label, action) — built from current state.
                    enum Add {
                        Rb,
                        Celestial,
                        Coll,
                        Mat,
                        Net,
                        Preset(String),
                        Script(String),
                        Type(Matter),
                        AnimCtl(String),
                        AnimNew,
                        Particles(String),
                        ParticlesNew,
                        Audio,
                    }
                    let mut items: Vec<(&str, String, Add)> = Vec::new();
                    if !has_rb {
                        items.push(("Physics", "♦  Rigidbody".into(), Add::Rb));
                    }
                    if world.get::<floptle_core::CelestialBody>(e).is_none() {
                        items.push(("Physics", "🪐  Celestial Body (orbit rails)".into(), Add::Celestial));
                    }
                    if !has_net {
                        items.push(("Networking", "🌐  Networked".into(), Add::Net));
                    }
                    if !has_collidable
                        && let Some(k) = collider_kind {
                            items.push(("Physics", format!("▦  Collider ({k})"), Add::Coll));
                        }
                    if !has_mat {
                        items.push(("Rendering", "◑  Material".into(), Add::Mat));
                    }
                    // Animation Controller: attach an existing controller asset, or
                    // create a fresh one (opens the graph editor).
                    if world.get::<floptle_core::AnimController>(e).is_none() {
                        items.push((
                            "Animation",
                            "▶  Animation Controller (new)".into(),
                            Add::AnimNew,
                        ));
                        for (k, _) in self.anim.controllers.iter() {
                            items.push(("Animation", format!("▶  {k}"), Add::AnimCtl(k.clone())));
                        }
                    }
                    if world.get::<floptle_audio::AudioSource>(e).is_none() {
                        items.push(("Effects", "♪  Audio Source".into(), Add::Audio));
                    }
                    // Particle System: attach an existing effect asset, or create a
                    // starter effect (a small looping fountain to shape from).
                    if world.get::<floptle_core::ParticleSystem>(e).is_none() {
                        items.push(("Effects", "✨  Particle System (new)".into(), Add::ParticlesNew));
                        for (k, _) in self.vfx.effects.iter() {
                            items.push(("Effects", format!("✨  {k}"), Add::Particles(k.clone())));
                        }
                    }
                    for (name, _) in self.materials {
                        items.push(("Rendering", format!("◑  {name}  (preset)"), Add::Preset(name.clone())));
                    }
                    // Scripts not already attached.
                    let attached: std::collections::HashSet<String> = world
                        .get::<Scripts>(e)
                        .map(|s| s.0.iter().map(|i| i.kind.clone()).collect())
                        .unwrap_or_default();
                    let mut snames = Vec::new();
                    collect_script_names(self.asset_tree, &mut snames);
                    for n in snames {
                        if !attached.contains(&n) {
                            items.push(("Scripts", format!("⚙  {n}"), Add::Script(n)));
                        }
                    }
                    // Type switch (mutually exclusive). Terrain is special — leave it be.
                    if !is_terrain {
                        for (lbl, mt) in type_catalog() {
                            if cur_kind != Some(matter_kind_label(&mt)) {
                                items.push(("Type — replaces current", lbl.to_string(), Add::Type(mt)));
                            }
                        }
                        // Each importable model is a Mesh type you can become.
                        let mut models = Vec::new();
                        collect_model_paths(self.asset_tree, &mut models);
                        for p in models {
                            let name = Path::new(&p)
                                .file_name()
                                .map(|s| s.to_string_lossy().to_string())
                                .unwrap_or_else(|| p.clone());
                            let is_cur = matches!(cur, Some(Matter::Mesh { asset_path }) if *asset_path == p);
                            if !is_cur {
                                items.push((
                                    "Mesh — replaces type",
                                    format!("✳  {name}"),
                                    Add::Type(Matter::Mesh { asset_path: p }),
                                ));
                            }
                        }
                    }

                    let mut picked = false;
                    egui::ScrollArea::vertical().max_height(340.0).show(ui, |ui| {
                        // Paste the clipboard onto a component the node doesn't have yet.
                        if let Some(c) = clip {
                            let can = match c {
                                ComponentClip::Material(_) => !has_mat,
                                ComponentClip::RigidBody(_) => !has_rb,
                                ComponentClip::Particles(_) => {
                                    world.get::<floptle_core::ParticleSystem>(e).is_none()
                                }
                                ComponentClip::Audio(_) => {
                                    world.get::<floptle_audio::AudioSource>(e).is_none()
                                }
                                ComponentClip::Script(_) => true,
                                ComponentClip::Transform(_) | ComponentClip::Matter(_) => false,
                            };
                            if can {
                                let lbl = format!("📋  Paste {}", c.label());
                                if hit(&lbl) && ui.button(lbl).clicked() {
                                    cmd.paste_component = Some(e);
                                    picked = true;
                                    ui.close();
                                }
                            }
                        }
                        let mut shown = false;
                        for cat in [
                            "Physics",
                            "Networking",
                            "Rendering",
                            "Effects",
                            "Animation",
                            "Scripts",
                            "Type — replaces current",
                            "Mesh — replaces type",
                        ] {
                            if !items.iter().any(|(c, l, _)| *c == cat && hit(l)) {
                                continue;
                            }
                            ui.add_space(4.0);
                            ui.weak(cat);
                            for (c, l, a) in &items {
                                if *c != cat || !hit(l) {
                                    continue;
                                }
                                shown = true;
                                if ui.button(l).clicked() {
                                    match a {
                                        Add::Rb => cmd.add_rigidbody = Some(e),
                                        Add::Celestial => cmd.add_celestial = Some(e),
                                        Add::Net => cmd.add_networked = Some(e),
                                        Add::Coll => cmd.set_collidable = Some((e, true)),
                                        Add::Mat => cmd.add_material = Some(e),
                                        Add::Preset(n) => cmd.apply_preset = Some((e, n.clone())),
                                        Add::Script(n) => cmd.attach_named = Some((n.clone(), e)),
                                        Add::Type(mt) => cmd.set_matter = Some((e, mt.clone())),
                                        Add::AnimCtl(k) => {
                                            cmd.set_anim_controller = Some((e, Some(k.clone())))
                                        }
                                        Add::AnimNew => cmd.new_anim_controller = Some(Some(e)),
                                        Add::Particles(k) => {
                                            cmd.add_particles = Some((e, k.clone()))
                                        }
                                        Add::ParticlesNew => cmd.new_particles = Some(e),
                                        Add::Audio => cmd.add_audio = Some(e),
                                    }
                                    picked = true;
                                    ui.close();
                                }
                            }
                        }
                        if !shown && !f.is_empty() {
                            ui.weak("no matching components");
                        }
                    });
                    // Reset the search for next open once something's been added.
                    if picked {
                        filter.clear();
                    }
                });
            }
            Some(_) => {
                ui.label("(no editable properties)");
            }
            None => {
                if self.selected_asset.is_none() {
                    ui.weak("Nothing selected. Click a node in the viewport or the Hierarchy.");
                }
            }
        }

        // ---- floating Material Editor window (edits the primary selection) ----
        if *self.show_material_editor {
            let mut open = true;
            egui::Window::new("◑ Material Editor")
                .open(&mut open)
                .default_width(300.0)
                .show(ui.ctx(), |ui| match self.selection.last().copied() {
                    Some(e) if world.get::<Matter>(e).is_some() => {
                        let nm = self
                            .entity_names
                            .iter()
                            .find(|(x, _)| *x == e)
                            .map(|(_, n)| n.clone())
                            .unwrap_or_default();
                        ui.label(format!("editing: {nm}"));
                        ui.separator();
                        if let Some(mat) = world.get_mut::<Material>(e) {
                            let res = material_props_ui(ui, mat, self.materials, self.asset_tree, self.mat_name_buf, self.flsl_cache, self.sdf_cache);
                            cmd.inspector_changed |= res.changed;
                            if res.remove {
                                cmd.remove_material = Some(e);
                            }
                            if let Some(name) = res.save_as {
                                cmd.save_material =
                                    Some((name, floptle_scene::MaterialDoc::from_material(mat)));
                            }
                        } else {
                            ui.label("This object uses the default look.");
                            if ui.button("✚ Add material").clicked() {
                                cmd.add_material = Some(e);
                            }
                        }
                    }
                    _ => {
                        ui.label("Select a node to edit its material.");
                    }
                });
            if !open {
                *self.show_material_editor = false;
            }
        }
    }
}

/// Whether a node carries the named component (mirrors the script-side
/// `getcomponent` names) — the candidate filter for `componentref` pickers.
fn node_has_component(
    world: &floptle_core::World,
    e: floptle_core::Entity,
    comp: &str,
) -> bool {
    match comp {
        "RigidBody" => world.get::<floptle_core::RigidBody>(e).is_some(),
        "PointLight" => {
            matches!(world.get::<Matter>(e), Some(Matter::PointLight { .. }))
        }
        "Camera" => matches!(world.get::<Matter>(e), Some(Matter::Camera { .. })),
        "ParticleSystem" => world.get::<floptle_core::ParticleSystem>(e).is_some(),
        "UiElement" => world.get::<floptle_ui::ElementSpec>(e).is_some(),
        "UiSlider" => world
            .get::<floptle_ui::ElementSpec>(e)
            .is_some_and(|s| s.slider.is_some()),
        "UiLayer" => world.get::<floptle_ui::UiLayer>(e).is_some(),
        _ => false,
    }
}
