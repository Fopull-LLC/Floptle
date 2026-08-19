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
    is_script, is_texture, script_name_of, AssetPayload,
};
use crate::matter_catalog::{matter_icon, matter_kind_label, type_catalog};
use crate::multi_edit;
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
    ui.horizontal_wrapped(|ui| {
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
    ui.horizontal_wrapped(|ui| {
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
    ui.horizontal_wrapped(|ui| {
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
            ui.horizontal_wrapped(|ui| {
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
            ui.horizontal_wrapped(|ui| {
                ui.label("tile size");
                changed |= ui
                    .add(egui::DragValue::new(scale).speed(0.02).range(0.01..=1000.0))
                    .on_hover_text("one tile spans this many object units")
                    .changed();
                ui.label("blend");
                changed |= crate::responsive::slider(ui, egui::Slider::new(blend, 0.5..=8.0))
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
pub(crate) fn shader_uniform_rows(
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
                            crate::responsive::slider(ui, egui::Slider::new(&mut v[0], lo..=hi)).changed()
                        }
                        None => ui.add(egui::DragValue::new(&mut v[0]).speed(0.02)).changed(),
                    };
                }
                ty => {
                    let lanes = ty.lanes() as usize;
                    ui.horizontal_wrapped(|ui| {
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

/// What [`script_tunables_ui`] needs from the surrounding Inspector to draw a
/// script's rows: its parsed metadata plus the candidate lists reference params
/// pick from.
pub(crate) struct ScriptRowCtx<'a> {
    pub(crate) meta: &'a crate::script_meta::ScriptMeta,
    pub(crate) ref_kinds: &'a std::collections::HashMap<(String, String), floptle_script::RefKind>,
    pub(crate) node_names: &'a Vec<String>,
    pub(crate) script_nodes: &'a std::collections::HashMap<String, Vec<String>>,
    pub(crate) comp_nodes: &'a std::collections::HashMap<String, Vec<String>>,
    pub(crate) name_of: &'a std::collections::HashMap<floptle_core::Entity, String>,
    /// Salts every widget id in this block (node index, script slot).
    pub(crate) salt: (u32, usize),
}

/// A section header inside a script's tunables (`--@header Movement`) — the same
/// TITLE ──── rule the Map tab uses, so panels read alike.
fn param_header(ui: &mut egui::Ui, title: &str) {
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        ui.label(
            egui::RichText::new(title)
                .small()
                .strong()
                .color(ui.visuals().strong_text_color()),
        );
        let rect = ui.available_rect_before_wrap();
        if rect.width() > 8.0 {
            let y = rect.center().y;
            ui.painter().line_segment(
                [egui::pos2(rect.left() + 4.0, y), egui::pos2(rect.right(), y)],
                egui::Stroke::new(1.0, ui.visuals().weak_text_color().gamma_multiply(0.5)),
            );
        }
    });
}

/// Attach a param's `--@desc` (or the plain comment above it) as the row's
/// tooltip. Without one the row is bare, exactly as before.
fn with_desc(resp: egui::Response, meta: Option<&crate::script_meta::ParamMeta>) -> egui::Response {
    match meta.and_then(|m| m.desc.as_deref()) {
        Some(d) => resp.on_hover_text(d),
        None => resp,
    }
}

/// One attached script's tunables: the editor-action buttons it declares, then
/// every `defaults` entry **in declaration order**, grouped under its
/// `--@header`s and drawn as the widget its annotations ask for — slider,
/// dropdown, checkbox, colour swatch, text box, node picker. Returns whether
/// anything changed.
///
/// Ordering is the reason this exists as one walk rather than three loops: the
/// old code drew numbers, then strings, then refs, each alphabetised, so a header
/// could never sit above the rows it names.
fn script_tunables_ui(
    ui: &mut egui::Ui,
    inst: &mut floptle_core::ScriptInst,
    cx: ScriptRowCtx<'_>,
    run_action: &mut Option<(floptle_core::Entity, String, String)>,
    e: floptle_core::Entity,
) -> bool {
    let mut changed = false;

    if !cx.meta.buttons.is_empty() {
        ui.horizontal_wrapped(|ui| {
            for (label, func) in &cx.meta.buttons {
                if ui
                    .button(format!("▶ {label}"))
                    .on_hover_text(format!(
                        "runs {func}(node) from {}.lua on this node, editing the open scene",
                        inst.kind
                    ))
                    .clicked()
                {
                    *run_action = Some((e, inst.kind.clone(), func.clone()));
                }
            }
        });
    }

    // Declaration order first (the metadata's order), then anything stored on the
    // instance that the script no longer declares — never silently dropped, so a
    // renamed default doesn't hide a value that's still in the scene file.
    let mut order: Vec<String> = cx.meta.params.iter().map(|p| p.name.clone()).collect();
    for k in inst
        .params
        .iter()
        .map(|(k, _)| k)
        .chain(inst.strs.iter().map(|(k, _)| k))
        .chain(inst.refs.iter().map(|(k, _)| k))
    {
        if !order.contains(k) {
            order.push(k.clone());
        }
    }

    // A row whose stored value is OVERRIDING the script's default, dropped
    // after the walk so the row itself can hold its `&mut` while it draws.
    let mut reset: Option<(bool, usize)> = None; // (is a number, index)
    for name in order {
        let pm = cx.meta.param(&name);
        if pm.is_some_and(|m| m.hidden) {
            continue;
        }
        if let Some(h) = pm.and_then(|m| m.header.as_deref()) {
            param_header(ui, h);
        }
        let declared = pm.and_then(|m| m.default.clone());
        if let Some(idx) = inst.params.iter().position(|(k, _)| *k == name) {
            let over = pins(declared.as_deref(), &inst.params[idx].1.to_string());
            ui.horizontal_wrapped(|ui| {
                changed |= num_param_row(ui, &mut inst.params[idx], pm, cx.salt);
                if pinned_badge(ui, over.as_deref()) {
                    reset = Some((true, idx));
                }
            });
        } else if let Some(idx) = inst.strs.iter().position(|(k, _)| *k == name) {
            let over = pins(declared.as_deref(), &format!("\"{}\"", inst.strs[idx].1));
            ui.horizontal_wrapped(|ui| {
                changed |= str_param_row(ui, &mut inst.strs[idx], pm, cx.salt);
                if pinned_badge(ui, over.as_deref()) {
                    reset = Some((false, idx));
                }
            });
        } else if let Some(idx) = inst.refs.iter().position(|(k, _)| *k == name) {
            changed |= ref_param_row(ui, inst, idx, &cx);
        }
    }
    if let Some((num, idx)) = reset {
        if num {
            inst.params.remove(idx);
        } else {
            inst.strs.remove(idx);
        }
        changed = true;
    }
    changed
}

/// The script's declared value, when the scene is holding a DIFFERENT one —
/// `None` when they agree, or when the script declares nothing comparable.
///
/// Compared as text on purpose: what a person wants to see is the literal they
/// wrote in the file, and `4` and `4.0` are the same number written twice.
fn pins(declared: Option<&str>, stored: &str) -> Option<String> {
    let d = declared?.trim();
    let same = match (d.parse::<f64>(), stored.trim().parse::<f64>()) {
        (Ok(a), Ok(b)) => (a - b).abs() <= f64::EPSILON.max(a.abs() * 1e-6),
        _ => d == stored.trim(),
    };
    (!same).then(|| d.to_string())
}

/// The "this scene is pinning the script's number" badge. Returns whether the
/// reset was clicked.
///
/// This is the half of `floptle/0068` the Console cannot catch: the name is
/// legitimate, the value is legitimate, and the only wrong thing about it is
/// its AGE. From the outside it is indistinguishable from a script whose
/// numbers do nothing — you edit one, press Play, and nothing happens.
fn pinned_badge(ui: &mut egui::Ui, declared: Option<&str>) -> bool {
    let Some(d) = declared else { return false };
    ui.add(egui::Label::new(
        egui::RichText::new("●").small().color(ui.visuals().warn_fg_color),
    ))
    .on_hover_text(format!(
        "this scene overrides the script, which declares {d}.\n\
         Editing the script's number will not change this node until you reset it."
    ));
    ui.small_button("↺")
        .on_hover_text(format!("drop the scene's value and use the script's ({d})"))
        .clicked()
}

/// A numeric tunable: checkbox (`--@bool` / a `true`/`false` default), dropdown
/// (`--@options`, value = index), slider (`--@slider`), else a drag value —
/// bounded by `--@range`, stepped by `--@step`, suffixed by `--@units`.
fn num_param_row(
    ui: &mut egui::Ui,
    (k, v): &mut (String, f32),
    pm: Option<&crate::script_meta::ParamMeta>,
    salt: (u32, usize),
) -> bool {
    let mut changed = false;
    let (lo, hi) = pm.map(|m| m.bounds()).unwrap_or((f32::MIN, f32::MAX));
    let step = pm.and_then(|m| m.step);
    let units = pm.and_then(|m| m.units.clone()).unwrap_or_default();
    ui.horizontal_wrapped(|ui| {
        if pm.is_some_and(|m| m.boolean) {
            let mut on = *v != 0.0;
            let r = with_desc(crate::responsive::check(ui, &mut on, k.as_str()), pm);
            if r.changed() {
                *v = f32::from(on);
                changed = true;
            }
            return;
        }
        if let Some(opts) = pm.map(|m| &m.options).filter(|o| !o.is_empty()) {
            with_desc(ui.label(k.as_str()), pm);
            let cur = (*v).clamp(0.0, (opts.len() - 1) as f32).round() as usize;
            egui::ComboBox::from_id_salt(("param_opt", salt, k.as_str()))
                .width(crate::responsive::fit_here(ui, 220.0))
                .wrap_mode(egui::TextWrapMode::Truncate)
                .selected_text(opts[cur].as_str())
                .show_ui(ui, |ui| {
                    for (i, label) in opts.iter().enumerate() {
                        if ui.selectable_label(i == cur, label).clicked() {
                            *v = i as f32;
                            changed = true;
                        }
                    }
                });
            return;
        }
        if pm.is_some_and(|m| m.slider) {
            with_desc(ui.label(k.as_str()), pm);
            let mut s = egui::Slider::new(v, lo..=hi).show_value(true);
            if let Some(st) = step {
                s = s.step_by(st as f64);
            }
            if !units.is_empty() {
                s = s.suffix(format!(" {units}"));
            }
            changed |= ui.add(s).changed();
            return;
        }
        let mut d = egui::DragValue::new(v)
            .speed(step.unwrap_or(0.05))
            .prefix(format!("{k}  "));
        if pm.and_then(|m| m.range).is_some() {
            d = d.range(lo..=hi);
        }
        if !units.is_empty() {
            d = d.suffix(format!(" {units}"));
        }
        changed |= with_desc(ui.add(d), pm).changed();
    });
    changed
}

/// A string tunable: colour swatch (`--@color`), dropdown (`--@options`, value =
/// the label), multi-line box (`--@multiline`), else a single-line field.
fn str_param_row(
    ui: &mut egui::Ui,
    (k, v): &mut (String, String),
    pm: Option<&crate::script_meta::ParamMeta>,
    salt: (u32, usize),
) -> bool {
    let mut changed = false;
    ui.horizontal_wrapped(|ui| {
        with_desc(ui.label(k.as_str()), pm);
        if pm.is_some_and(|m| m.color) {
            // `#rrggbb` in the script, a swatch in the Inspector.
            let mut rgb = hex_rgb(v);
            if ui.color_edit_button_srgb(&mut rgb).changed() {
                *v = format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2]);
                changed = true;
            }
            ui.weak(v.as_str());
            return;
        }
        if let Some(opts) = pm.map(|m| &m.options).filter(|o| !o.is_empty()) {
            egui::ComboBox::from_id_salt(("param_str_opt", salt, k.as_str()))
                .width(crate::responsive::fit_here(ui, 220.0))
                .wrap_mode(egui::TextWrapMode::Truncate)
                .selected_text(v.as_str())
                .show_ui(ui, |ui| {
                    for label in opts {
                        if ui.selectable_label(label == v, label).clicked() {
                            *v = label.clone();
                            changed = true;
                        }
                    }
                });
            return;
        }
        let edit = if pm.is_some_and(|m| m.multiline) {
            egui::TextEdit::multiline(v).desired_width(180.0).desired_rows(3)
        } else {
            egui::TextEdit::singleline(v).desired_width(140.0)
        };
        changed |= ui.add(edit).changed();
    });
    changed
}

/// `#rrggbb` → bytes (anything unparseable reads as white, so a typo shows as a
/// swatch you can fix rather than a black hole).
fn hex_rgb(s: &str) -> [u8; 3] {
    let h = s.trim().trim_start_matches('#');
    if h.len() < 6 {
        return [255, 255, 255];
    }
    let byte = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).unwrap_or(255);
    [byte(0), byte(2), byte(4)]
}

/// A reference param (`noderef()` / `scriptref(k)` / `componentref(c)`): a
/// kind-filtered node picker that also accepts a node dragged from the Hierarchy.
fn ref_param_row(
    ui: &mut egui::Ui,
    inst: &mut floptle_core::ScriptInst,
    idx: usize,
    cx: &ScriptRowCtx<'_>,
) -> bool {
    let mut changed = false;
    let kind_key = (inst.kind.clone(), inst.refs[idx].0.clone());
    let kind = cx.ref_kinds.get(&kind_key);
    let empty: Vec<String> = Vec::new();
    let (cands, hint) = match kind {
        Some(floptle_script::RefKind::Script(sk)) => (
            cx.script_nodes.get(sk).unwrap_or(&empty),
            format!("→ the '{sk}' SCRIPT on the wired node (lists nodes carrying it); drag a node from the Hierarchy to wire"),
        ),
        Some(floptle_script::RefKind::Component(c)) => (
            cx.comp_nodes.get(c).unwrap_or(&empty),
            format!("→ the {c} COMPONENT on the wired node (lists nodes carrying it); drag a node from the Hierarchy to wire"),
        ),
        _ => (
            cx.node_names,
            "→ a node handle; drag a node from the Hierarchy to wire".to_string(),
        ),
    };
    // A declared description replaces the generic hint's lead line.
    let desc = cx
        .meta
        .param(&inst.refs[idx].0)
        .and_then(|m| m.desc.clone())
        .map(|d| format!("{d}\n{hint}"))
        .unwrap_or(hint);
    let (k, target) = &mut inst.refs[idx];
    let row = ui
        .horizontal(|ui| {
            ui.label(format!("{k}  ")).on_hover_text(&desc);
            if let Some(pick) = crate::ui_widgets::searchable_picker(
                ui,
                egui::Id::new(("script_ref", cx.salt, idx)),
                if target.is_empty() { "(pick node)" } else { target },
                Some("(none)"),
                cands,
                150.0,
            ) {
                *target = pick.unwrap_or_default();
                changed = true;
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
        let ok = cx.name_of.get(&p.0).is_some_and(|n| cands.contains(n));
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
        && let Some(n) = cx.name_of.get(&p.0)
        && cands.contains(n)
    {
        *target = n.clone();
        changed = true;
    }
    changed
}

/// Deferred intents from [`material_props_ui`] (applied after the borrow ends).
#[derive(Default)]
pub(crate) struct MatEditResult {
    pub(crate) changed: bool,
    pub(crate) remove: bool,
    pub(crate) save_as: Option<String>,
    /// The ◈ button was pressed: open this `.flsl` in the Shaders graph. An
    /// intent rather than a direct call for the same reason the others are —
    /// this runs with the material borrowed.
    pub(crate) open_shader: Option<String>,
}
/// In-depth material property editors — shared by the Inspector's Material section
/// and the floating Material Editor window. Edits `m` in place (so undo coalesces
/// via `inspector_changed`); preset apply/save/remove come back as intents.
#[allow(clippy::too_many_arguments)] // one widget, one call shape — a param struct would just rename the args
pub(crate) fn material_props_ui(
    ui: &mut egui::Ui,
    m: &mut Material,
    presets: &[(String, floptle_scene::MaterialDoc)],
    asset_tree: &[crate::assets::AssetEntry],
    project_root: &Path,
    name_buf: &mut String,
    flsl: &crate::shaders::FlslCache,
    sdf: &crate::shaders::SdfCache,
    texture_settings: &std::collections::HashMap<String, crate::assets::TexSetting>,
) -> MatEditResult {
    let mut r = MatEditResult::default();
    // Every picker in here is identified RELATIVE to the Ui it was drawn in.
    // Absolute ids (`Id::new("mat_tex")`) made two material editors on screen at
    // once — the Inspector's and the Map tab's per-slot one, which live in
    // different dock panels and so are both visible — share one popup: opening
    // either one drew two popups under the same id, and each counted the click
    // that opened it as a click OUTSIDE the other, so the dropdown shut on the
    // frame it opened and the texture could never be picked. One salt per call
    // site is what makes them independent.
    let salt = ui.id();

    // The base texture's spritesheet grid comes from the TEXTURE's asset settings
    // (slice the .png once, every material using it inherits the same cells), so
    // re-slicing an asset re-slices its materials. A cell that no longer exists
    // falls back into range instead of drawing off the end of the sheet.
    let sheet_of = |m: &Material| {
        crate::assets::tex_setting(
            texture_settings,
            project_root,
            m.texture.as_deref().unwrap_or_default(),
        )
        .sheet()
    };
    let (sc, sr) = sheet_of(m);
    if (m.sheet_cols, m.sheet_rows) != (sc, sr) {
        (m.sheet_cols, m.sheet_rows) = (sc, sr);
        m.cell = m.cell.min((sc * sr).saturating_sub(1));
        r.changed = true;
    }

    crate::responsive::grid(ui, "mat_top", |ui| {
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
            salt.with("mat_tex"),
            project_root,
            if cur.is_empty() { "none" } else { &cur },
            Some("none"),
            asset_tree,
            crate::assets::is_texture,
            160.0,
        ) {
            // Inherit the texture's spritesheet grid (set once in its asset
            // settings) so a picked sheet slices without any extra steps — the
            // same hand-off a UI image gets.
            let (sc, sr) = crate::assets::tex_setting(
                texture_settings,
                project_root,
                pick.as_deref().unwrap_or_default(),
            )
            .sheet();
            (m.sheet_cols, m.sheet_rows, m.cell) = (sc, sr, 0);
            m.texture = pick;
            r.changed = true;
        }
        ui.end_row();
        // Tiling applies to the base texture (the mesh's own or the override).
        // A sheet takes the tiling lanes over (one cell, no repeats), so the
        // controls say so rather than silently doing nothing.
        ui.label("tiling");
        ui.vertical(|ui| {
            if m.is_sheet() {
                ui.label(
                    egui::RichText::new("— a sheet draws one cell (tiling off)")
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
            } else {
                r.changed |= tiling_ui(ui, &mut m.tiling);
            }
        });
        ui.end_row();
        ui.label("emissive");
        ui.horizontal_wrapped(|ui| {
            r.changed |= ui.color_edit_button_rgb(&mut m.emissive).changed();
            r.changed |= ui
                .add(egui::DragValue::new(&mut m.emissive_strength).speed(0.02).range(0.0..=20.0).prefix("×"))
                .on_hover_text("emissive strength")
                .changed();
        });
        ui.end_row();
        ui.label("unlit");
        r.changed |= crate::responsive::check(ui, &mut m.unlit, "fullbright / flat").changed();
        ui.end_row();
        ui.label("fog");
        r.changed |= crate::responsive::check(ui, &mut m.fog, "affected by scene fog")
            .on_hover_text(
                "Off draws this surface at its own colour however far away it is — \
                 both the distance ramp and the volumetric layer leave it alone.\n\n\
                 What it is for: the things that are not really in the world at that \
                 distance. A first-person weapon sits a metre from the eye and a \
                 hundred metres from the level's origin; a sky shell or a backdrop \
                 card is painted at its own depth and greying it out fogs the horizon \
                 twice; a marker has to stay readable through the weather that is the \
                 point of the scene.\n\nA planet's atmosphere is a separate effect \
                 with its own controls and still applies.",
            )
            .changed();
        ui.end_row();
    });

    // ---- spritesheet: which cell of the sliced texture this surface draws.
    // Outside the grid, on its own full-width row — a 21-wide sheet's cell grid
    // needs the whole panel, not a grid cell. (`m.texture` may have changed in the
    // rows above, so the grid is re-read here.)
    let (sc, sr) = sheet_of(m);
    if sc * sr > 1 {
        r.changed |= crate::ui_widgets::sheet_cell_picker(
            ui,
            salt.with("mat_cells"),
            m.texture.as_deref().unwrap_or_default(),
            sc,
            sr,
            &mut m.cell,
        );
    }

    // ---- custom shader (ADR-0007): pick a .flsl; its exposed uniforms and
    // texture slots become the rows below, live-editing the group(3) params.

    crate::responsive::grid(ui, "mat_shader", |ui| {
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
            salt.with("mat_shader"),
            project_root,
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
        // Straight to the graph. Everywhere a shader can be PICKED it can now be
        // OPENED, because the alternative is finding it again in the Assets
        // panel every time you want to change a line of it.
        if let Some(path) = m.shader.clone()
            && ui
                .button("◈")
                .on_hover_text("edit this shader in the ◈ Shaders graph")
                .clicked()
        {
            r.open_shader = Some(path);
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
                crate::responsive::grid(ui, "mat_shader_rows", |ui| {
                        r.changed |= shader_uniform_rows(ui, &compiled.uniforms, &mut m.shader_params);
                        for (i, slot) in compiled.textures.iter().enumerate() {
                            ui.label(slot);
                            let file_of = |p: &str| {
                                Path::new(p)
                                    .file_name()
                                    .map(|s| s.to_string_lossy().to_string())
                                    .unwrap_or_else(|| p.to_string())
                            };
                            // Empty here means the shader's own default binds —
                            // name it, so "none" never lies about what renders.
                            let dflt = compiled
                                .texture_defaults
                                .get(i)
                                .and_then(|d| d.as_deref());
                            let empty = match dflt {
                                Some(p) => format!("{} (shader)", file_of(p)),
                                None => "none".into(),
                            };
                            let cur = m
                                .shader_textures
                                .get(slot)
                                .map(|p| file_of(p))
                                .unwrap_or_else(|| empty.clone());
                            if let Some(pick) = crate::ui_widgets::asset_picker(
                                ui,
                                salt.with(("mat_shader_tex", i)),
                                project_root,
                                &cur,
                                Some(empty.as_str()),
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
                crate::responsive::grid(ui, "mat_sdf_rows", |ui| {
                    r.changed |= shader_uniform_rows(ui, &ir.uniforms, &mut m.shader_params);
                });
            }
        } else {
            ui.small("compiling…");
        }
    }

    // One surface-map slot: a texture picker over an `Option<String>`, showing
    // the file name and offering "none". Returns whether it changed.
    fn map_slot_picker(
        ui: &mut egui::Ui,
        salt: egui::Id,
        project_root: &Path,
        asset_tree: &[crate::assets::AssetEntry],
        slot: &mut Option<String>,
    ) -> bool {
        let cur = slot
            .as_deref()
            .map(|p| Path::new(p).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default())
            .unwrap_or_else(|| "none".into());
        let picked = crate::ui_widgets::asset_picker(
            ui,
            salt,
            project_root,
            if cur.is_empty() { "none" } else { &cur },
            Some("none"),
            asset_tree,
            crate::assets::is_texture,
            160.0,
        );
        match picked {
            Some(p) => {
                *slot = p;
                true
            }
            None => false,
        }
    }

    // ---- the SURFACE MAPS. The answer to "where do I put a normal map".
    //
    // Above the lighting model on purpose: a normal map and an occlusion map
    // describe the surface itself and apply under either model, so they must not
    // read as belonging to one of them.
    ui.add_enabled_ui(!m.unlit, |ui| {
        egui::CollapsingHeader::new(crate::responsive::header_text(ui, "Surface maps"))
            .id_salt(salt.with("mat_maps"))
            .default_open(crate::responsive::start_open(m.has_maps()))
            .show(ui, |ui| {
                            crate::responsive::grid(ui, "mat_maps_rows", |ui| {
                    let map_row =
                        |ui: &mut egui::Ui, id: &str, label: &str, help: &str, slot: &mut Option<String>| {
                            ui.label(label).on_hover_text(help);
                            let cur = slot
                                .as_deref()
                                .map(|p| {
                                    Path::new(p)
                                        .file_name()
                                        .map(|s| s.to_string_lossy().to_string())
                                        .unwrap_or_default()
                                })
                                .unwrap_or_else(|| "none".into());
                            if let Some(pick) = crate::ui_widgets::asset_picker(
                                ui,
                                salt.with(id),
                                project_root,
                                if cur.is_empty() { "none" } else { &cur },
                                Some("none"),
                                asset_tree,
                                crate::assets::is_texture,
                                160.0,
                            ) {
                                *slot = pick;
                                return true;
                            }
                            false
                        };
                    r.changed |= map_row(
                        ui,
                        "mat_normal",
                        "normal",
                        "A tangent-space normal map. Fakes bumps, bricks, panel lines and \
                         stitching without geometry.\n\nNo tangent attribute needed — the \
                         frame is derived per pixel, so this works on terrain, primitives, \
                         Model-tool meshes and skinned characters alike.",
                        &mut m.normal_map,
                    );
                    ui.end_row();
                    if m.normal_map.is_some() {
                        ui.label("  strength");
                        r.changed |= crate::responsive::slider(ui, egui::Slider::new(&mut m.normal_strength, -2.0..=2.0))
                            .on_hover_text(
                                "1 = as authored, 0 = flat. NEGATIVE flips the green channel — \
                                 the one-click fix when every bump reads as a dent (a map \
                                 authored in the other handedness).",
                            )
                            .changed();
                        ui.end_row();
                    }
                    r.changed |= map_row(
                        ui,
                        "mat_ao",
                        "occlusion",
                        "Baked ambient occlusion, read from the RED channel. Darkens ambient \
                         and indirect light only — never the key light — so it deepens \
                         crevices instead of greying the whole surface.",
                        &mut m.ao_map,
                    );
                    ui.end_row();
                    if m.ao_map.is_some() {
                        ui.label("  strength");
                        r.changed |= crate::responsive::slider(ui, egui::Slider::new(&mut m.occlusion_strength, 0.0..=1.0))
                            .changed();
                        ui.end_row();
                    }
                });
            });
    });

    // ---- the lighting model, and the knobs that belong to whichever one is on.
    ui.add_enabled_ui(!m.unlit, |ui| {
        crate::responsive::grid(ui, "mat_model", |ui| {
            ui.label("lighting").on_hover_text(
                "Classic — a highlight you set by hand (colour, exponent, strength).\n\
                 Physical — a highlight that falls out of roughness and metallic.\n\n\
                 Neither is better. Classic suits a stylised look, Physical suits a \
                 realistic one, and both take the same surface maps.",
            );
            let mut phys = matches!(m.shading, floptle_core::Shading::Physical);
            egui::ComboBox::from_id_salt(salt.with("mat_shading"))
                .width(crate::responsive::fit_here(ui, 220.0))
                .wrap_mode(egui::TextWrapMode::Truncate)
                .selected_text(if phys { "Physical (metal-rough)" } else { "Classic (Blinn-Phong)" })
                .show_ui(ui, |ui| {
                    r.changed |= ui.selectable_value(&mut phys, false, "Classic (Blinn-Phong)").changed();
                    r.changed |= ui.selectable_value(&mut phys, true, "Physical (metal-rough)").changed();
                });
            m.shading =
                if phys { floptle_core::Shading::Physical } else { floptle_core::Shading::Classic };
            ui.end_row();
        });
    });

    if matches!(m.shading, floptle_core::Shading::Physical) {
        ui.add_enabled_ui(!m.unlit, |ui| {
            crate::responsive::grid(ui, "mat_pbr", |ui| {
                ui.label("roughness");
                r.changed |= crate::responsive::slider(ui, egui::Slider::new(&mut m.roughness, 0.0..=1.0))
                    .on_hover_text("0 = mirror, 1 = chalk. Multiplied by the roughness map.")
                    .changed();
                ui.end_row();
                ui.label("  map").on_hover_text(
                    "Roughness from the GREEN channel — so a glTF-style packed \
                     occlusion/roughness/metallic image drops straight in (and the \
                     occlusion slot above reads RED from the same file).",
                );
                r.changed |= map_slot_picker(
                    ui,
                    salt.with("mat_rough"),
                    project_root,
                    asset_tree,
                    &mut m.roughness_map,
                );
                ui.end_row();
                ui.label("metallic");
                r.changed |= crate::responsive::slider(ui, egui::Slider::new(&mut m.metallic, 0.0..=1.0))
                    .on_hover_text(
                        "0 = dielectric (plastic, wood, stone: a white highlight over a \
                         coloured surface). 1 = metal (no diffuse at all; the highlight \
                         takes the base colour).\n\nValues in between are a transition, \
                         not a material — real surfaces sit at one end or the other.",
                    )
                    .changed();
                ui.end_row();
                ui.label("  map").on_hover_text(
                    "Metallic from the BLUE channel — the third channel of the same \
                     packed image roughness and occlusion read.",
                );
                r.changed |= map_slot_picker(
                    ui,
                    salt.with("mat_metal"),
                    project_root,
                    asset_tree,
                    &mut m.metallic_map,
                );
                ui.end_row();
                ui.label("reflections").on_hover_text(
                    "How much of the SKY this surface reflects. 1 is the real \
                     amount and the default.\n\nA mirror is metallic 1 with \
                     roughness 0 — it shows the sky sharply. Raise the roughness \
                     and the same reflection blurs, which is the difference \
                     between chrome and brushed steel.\n\nTurn this down to take \
                     the sheen off something reading too glassy; past 1 is a \
                     deliberate cheat that flatters a hero prop.",
                );
                r.changed |=
                    crate::responsive::slider(ui, egui::Slider::new(&mut m.reflectivity, 0.0..=2.0)).changed();
                ui.end_row();

                // ---- glass -------------------------------------------------
                // Beside reflections rather than in a section of its own,
                // because they are two halves of one question — what a surface
                // does with the light that reaches it — and a crystal ball needs
                // both turned up.
                ui.label("see-through").on_hover_text(
                    "GLASS: how much light passes THROUGH this surface instead of \
                     stopping at it. 0 is solid, 1 is clear glass.\n\nDifferent from \
                     opacity: opacity fades the surface away, and takes its highlight \
                     and its reflection with it. This keeps the surface — its \
                     reflection, its bright grazing edge — and lets the scene behind \
                     come through it, bent.\n\nThe base colour tints what comes \
                     through, so green glass makes what is behind it green.",
                );
                r.changed |=
                    crate::responsive::slider(ui, egui::Slider::new(&mut m.transmission, 0.0..=1.0)).changed();
                ui.end_row();
                ui.label("  bend").on_hover_text(
                    "Index of refraction — how sharply light bends on the way in.\n\n                     1.0 does not bend at all (the scene shows through undistorted), \
                     1.33 water, 1.5 window glass, 1.8 heavy crystal, 2.4 diamond.\n\n                     This is the whole difference between a flat pane and a lens: a \
                     solid ball at 1.5 or above turns what is behind it upside down.",
                );
                r.changed |= ui
                    .add_enabled(
                        m.transmission > 0.0,
                        egui::Slider::new(&mut m.ior, 1.0..=2.5),
                    )
                    .changed();
                ui.end_row();
                ui.label("  thickness").on_hover_text(
                    "How far light travels inside the material, in metres. Set it to \
                     roughly the size of the object — a windowpane is thin, a paperweight \
                     is not.\n\nHow far the distortion actually throws what is behind \
                     ALSO depends on how far away that is: glass against a wall barely \
                     shifts it, the same glass held up against a distant one throws it \
                     right across. That part is the scene's doing, not this slider's.",
                );
                r.changed |= ui
                    .add_enabled(
                        m.transmission > 0.0,
                        egui::Slider::new(&mut m.thickness, 0.0..=5.0),
                    )
                    .changed();
                ui.end_row();
                if m.transmission > 0.0 {
                    ui.label("");
                    ui.small(
                        "roughness frosts it — the same slider that blurs a reflection \
                         blurs what you see through",
                    );
                    ui.end_row();
                }
            });
        });
    }

    // ---- the deliberate PS1/N64 artefacts. Its own section, collapsed unless
    // something is on, because these are a look you opt into — not a quality
    // setting anyone should stumble across while tuning a material.
    ui.add_enabled_ui(true, |ui| {
        egui::CollapsingHeader::new(crate::responsive::header_text(ui, "Retro artefacts"))
            .id_salt(salt.with("mat_retro"))
            .default_open(crate::responsive::start_open(m.retro.any() || m.retro.exempt))
            .show(ui, |ui| {
                crate::responsive::grid(ui, "mat_retro_rows", |ui| {
                    ui.label("vertex jitter").on_hover_text(
                        "Snap vertices to a screen grid, the way hardware with no \
                         fractional vertex coordinates did. Geometry near the camera \
                         wobbles between cells as it moves.\n\n0 = off. Lower = coarser \
                         (80 is very chunky, 320 is a hint).\n\nThe wobble is MOTION: \
                         the snap happens every frame, but a still camera on a still \
                         object lands in the same cell every time and holds perfectly \
                         still. Move something to see it.\n\n0 here also means \"follow \
                         the project\" — see Project Settings ⏵ Rendering ⏵ Era \
                         artefacts.",
                    );
                    r.changed |= crate::responsive::slider(ui, egui::Slider::new(&mut m.retro.jitter, 0.0..=512.0).step_by(1.0))
                        .changed();
                    ui.end_row();
                    ui.label("affine UVs");
                    r.changed |= crate::responsive::check(ui, &mut m.retro.affine_uv, "skip perspective correction")
                        .on_hover_text(
                            "The era's warping, swimming textures on large near-camera \
                             polygons. Most visible on floors and long walls.",
                        )
                        .changed();
                    ui.end_row();
                    ui.label("vertex lighting");
                    r.changed |= crate::responsive::check(ui, &mut m.retro.vertex_lit, "light per vertex (Gouraud)")
                        .on_hover_text(
                            "Faceted highlights that slide across a face as it turns.\n\n\
                             A vertex-lit surface receives no shadows, no SDF occlusion \
                             and no normal map — hardware that shaded per vertex had none \
                             of those.",
                        )
                        .changed();
                    ui.end_row();
                    ui.label("dither alpha");
                    r.changed |= crate::responsive::check(ui, &mut m.retro.dither_alpha, "screen-door transparency")
                        .on_hover_text(
                            "Draw partial opacity as a 4×4 dither of solid pixels instead \
                             of blending. Stays in the opaque pass, so it never needs \
                             sorting and never shows the sky through the wall behind it.",
                        )
                        .changed();
                    ui.end_row();
                    ui.label("project artefacts");
                    r.changed |= crate::responsive::check(ui, &mut m.retro.exempt, "opt out")
                        .on_hover_text(
                            "Take NONE of the project-wide artefacts (Project Settings ⏵ \
                             Rendering). This surface then shows exactly what is set \
                             above and nothing else.\n\nWhat it is for: the one thing \
                             that has to hold still in a world that wobbles — a \
                             first-person weapon, a screen-facing card, a sky shell \
                             whose seams the snap would tear open.",
                        )
                        .changed();
                    ui.end_row();
                });
            });
    });

    // These only affect the lit path, so grey them out when unlit.
    ui.add_enabled_ui(!m.unlit && matches!(m.shading, floptle_core::Shading::Classic), |ui| {
        crate::responsive::grid(ui, "mat_lit", |ui| {
            ui.label("specular");
            ui.horizontal_wrapped(|ui| {
                r.changed |= ui.color_edit_button_rgb(&mut m.specular).changed();
                r.changed |= ui
                    .add(egui::DragValue::new(&mut m.specular_strength).speed(0.02).range(0.0..=8.0).prefix("×"))
                    .on_hover_text("specular strength")
                    .changed();
            });
            ui.end_row();
            ui.label("shininess");
            r.changed |= crate::responsive::slider(ui, egui::Slider::new(&mut m.shininess, 1.0..=256.0).logarithmic(true)).changed();
            ui.end_row();
        });
    });

    // Rim, ambient and opacity are NOT part of either lighting model — a rim
    // glow is art direction and opacity is opacity — so they stay live whichever
    // model is selected.
    ui.add_enabled_ui(!m.unlit, |ui| {
        crate::responsive::grid(ui, "mat_common", |ui| {
            ui.label("rim");
            ui.horizontal_wrapped(|ui| {
                r.changed |= ui.color_edit_button_rgb(&mut m.rim).changed();
                r.changed |= ui
                    .add(egui::DragValue::new(&mut m.rim_strength).speed(0.02).range(0.0..=8.0).prefix("×"))
                    .on_hover_text("rim / fresnel strength")
                    .changed();
            });
            ui.end_row();
            ui.label("ambient");
            r.changed |= crate::responsive::slider(ui, egui::Slider::new(&mut m.ambient, 0.0..=4.0)).changed();
            ui.end_row();
            ui.label("opacity");
            r.changed |= crate::responsive::slider(ui, egui::Slider::new(&mut m.alpha, 0.0..=1.0))
                .on_hover_text("1 = opaque; below 1 alpha-blends over the scene (drawn after opaque objects)")
                .changed();
            ui.end_row();
        });
    });

    ui.separator();
    ui.horizontal_wrapped(|ui| {
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
                .and_then(|rig| {
                    let n = rig.skeleton.nodes.get(idx)?;
                    let is_object = rig.node_is_object.get(idx).copied().unwrap_or(true);
                    Some((n.name.clone(), n.rest, n.pivot, is_object))
                }),
            _ => None,
        };
        let Some((bone_name, rest, pivot, is_object)) = resolved else {
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

        let (icon, kind) = if is_object { ("◈", "object") } else { ("🔗", "bone") };
        ui.horizontal_wrapped(|ui| {
            ui.strong(format!("{icon} {bone_name}"));
            if ui.small_button("⮪ back").on_hover_text("back to the node inspector").clicked() {
                *self.bone_selection = None;
                *self.pivot_edit = false;
            }
        });
        ui.small(format!("{kind} of {mesh_name}"));
        ui.separator();

        let mut trs = cur;
        let mut changed = false;
        ui.horizontal_wrapped(|ui| {
            ui.label("position");
            changed |= ui.add(egui::DragValue::new(&mut trs.t.x).speed(0.01).prefix("x ")).changed();
            changed |= ui.add(egui::DragValue::new(&mut trs.t.y).speed(0.01).prefix("y ")).changed();
            changed |= ui.add(egui::DragValue::new(&mut trs.t.z).speed(0.01).prefix("z ")).changed();
        });
        let (ey, ex, ez) = trs.r.to_euler(EulerRot::YXZ);
        let mut deg = [ex.to_degrees(), ey.to_degrees(), ez.to_degrees()];
        ui.horizontal_wrapped(|ui| {
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
        ui.horizontal_wrapped(|ui| {
            ui.label("scale");
            changed |= ui.add(egui::DragValue::new(&mut trs.s.x).speed(0.01).range(0.001..=100.0).prefix("x ")).changed();
            changed |= ui.add(egui::DragValue::new(&mut trs.s.y).speed(0.01).range(0.001..=100.0).prefix("y ")).changed();
            changed |= ui.add(egui::DragValue::new(&mut trs.s.z).speed(0.01).range(0.001..=100.0).prefix("z ")).changed();
        });

        // ---- rotation pivot (the "joint" this object turns around) ----
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            ui.strong("⌖ Pivot");
            let mut editing = *self.pivot_edit;
            if ui
                .selectable_label(editing, "⌖ drag in view")
                .on_hover_text(
                    "move the rotation joint by dragging the gizmo in the Scene view \
                     instead of posing — turn off to pose around the new pivot",
                )
                .clicked()
            {
                editing = !editing;
                *self.pivot_edit = editing;
            }
        });
        ui.small("where this object rotates/scales around — defaults to its geometry center");
        let mut p = pivot;
        let mut pchanged = false;
        ui.horizontal_wrapped(|ui| {
            ui.label("pivot");
            pchanged |= ui.add(egui::DragValue::new(&mut p.x).speed(0.01).prefix("x ")).changed();
            pchanged |= ui.add(egui::DragValue::new(&mut p.y).speed(0.01).prefix("y ")).changed();
            pchanged |= ui.add(egui::DragValue::new(&mut p.z).speed(0.01).prefix("z ")).changed();
        });
        if pchanged {
            self.cmd.set_object_pivot = Some((mesh, bone_name.clone(), [p.x, p.y, p.z]));
        }

        // ---- ◑ per-object material override (objects only — bones have no faces) ----
        if is_object {
            ui.separator();
            let has_override = self
                .world
                .get::<floptle_core::ObjectMaterials>(mesh)
                .is_some_and(|om| om.0.contains_key(&bone_name));
            if has_override {
                let mut clear = false;
                ui.horizontal_wrapped(|ui| {
                    ui.strong("◑ Material (this object)");
                    clear = ui
                        .small_button("🗑 clear")
                        .on_hover_text("remove this object's override — back to the model's own look")
                        .clicked();
                });
                ui.small("overrides the model's imported look for JUST this object");
                let mut save_as = None;
                if let Some(mat) = self
                    .world
                    .get_mut::<floptle_core::ObjectMaterials>(mesh)
                    .and_then(|om| om.0.get_mut(&bone_name))
                {
                    let res = material_props_ui(ui, mat, self.materials, self.asset_tree, self.project_root, self.mat_name_buf, self.flsl_cache, self.sdf_cache, self.texture_settings);
                    self.cmd.inspector_changed |= res.changed;
                    self.cmd.open_shader_graph = res.open_shader.or(self.cmd.open_shader_graph.take());
                    clear |= res.remove;
                    if let Some(name) = res.save_as {
                        save_as =
                            Some((name, floptle_scene::MaterialDoc::from_material(mat)));
                    }
                }
                self.cmd.save_material = save_as.or(self.cmd.save_material.take());
                if clear {
                    if let Some(om) = self.world.get_mut::<floptle_core::ObjectMaterials>(mesh) {
                        om.0.remove(&bone_name);
                        if om.0.is_empty() {
                            self.world.remove::<floptle_core::ObjectMaterials>(mesh);
                        }
                    }
                    self.cmd.inspector_changed = true;
                }
            } else if ui
                .button("◑ Override material for this object")
                .on_hover_text(
                    "give JUST this sub-object its own material (color/texture/shader) \
                     while the rest of the model keeps its imported look",
                )
                .clicked()
            {
                let mut om = self
                    .world
                    .get::<floptle_core::ObjectMaterials>(mesh)
                    .cloned()
                    .unwrap_or_default();
                om.0.insert(bone_name.clone(), floptle_core::Material::default());
                self.world.insert(mesh, om);
                self.cmd.inspector_changed = true;
            }
        }

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
                // One undo step per edit gesture (no-op once already dirty).
                crate::anim_ui::snapshot_clip(self.anim_ui);
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

    /// "◑ Materials" for a selected model ASSET: every embedded material — its
    /// tint, whether it has a texture, and which sub-objects draw with it — plus
    /// the per-model embedded-texture filter (persisted in the `.rig.ron`
    /// sidecar). The registry only knows models something has loaded; a model
    /// never placed in a scene shows a hint instead.
    fn model_asset_materials_ui(&mut self, ui: &mut egui::Ui, path: &str) {
        ui.separator();
        ui.strong("◑ Materials");
        let Some(asset) = self.mesh_registry.get(path) else {
            ui.small("drag the model into a scene once to inspect its materials");
            return;
        };
        if asset.part_meta.is_empty() {
            ui.small("no material metadata (re-import: touch the file or restart)");
            return;
        }
        // Group parts by material name, keeping the objects each covers.
        let mut by_mat: std::collections::BTreeMap<&str, Vec<usize>> = Default::default();
        for (i, m) in asset.part_meta.iter().enumerate() {
            by_mat.entry(m.material.as_str()).or_default().push(i);
        }
        for (mat, parts) in &by_mat {
            let meta = &asset.part_meta[parts[0]];
            ui.horizontal_wrapped(|ui| {
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                ui.painter().rect_filled(
                    rect,
                    2.0,
                    egui::Color32::from_rgb(
                        (meta.base_color[0] * 255.0) as u8,
                        (meta.base_color[1] * 255.0) as u8,
                        (meta.base_color[2] * 255.0) as u8,
                    ),
                );
                ui.label(*mat);
                if meta.textured {
                    ui.small("🖼 textured");
                }
            });
            // Which sub-objects use it (also the override keys).
            let objs: Vec<&str> = parts.iter().filter_map(|&i| asset.override_key(i)).collect();
            if objs.len() > 1 || objs.first().is_some_and(|o| *o != *mat) {
                ui.small(format!("   on: {}", objs.join(", ")));
            }
        }
        ui.small(
            "to change one of these: place the model, select the node, and use \
             \"◑ Model materials\" in its inspector — or expand the model in the \
             Hierarchy and override the object directly",
        );
        // Embedded-texture filtering (the whole model's baked-in textures).
        if asset.part_meta.iter().any(|m| m.textured) {
            let cur = asset.tex_filter;
            let label = |f: Option<crate::assets::FilterMode>| match f {
                None | Some(crate::assets::FilterMode::Pixelated) => "crisp (pixelated)",
                Some(crate::assets::FilterMode::Smooth) => "smooth",
                Some(crate::assets::FilterMode::SmoothMipmaps) => "smooth + mipmaps",
            };
            ui.horizontal_wrapped(|ui| {
                ui.label("embedded texture filter");
                egui::ComboBox::from_id_salt(("model_tex_filter", path))
                    .width(crate::responsive::fit_here(ui, 220.0))
                .wrap_mode(egui::TextWrapMode::Truncate)
                    .selected_text(label(cur))
                    .show_ui(ui, |ui| {
                        for opt in [
                            None,
                            Some(crate::assets::FilterMode::Smooth),
                            Some(crate::assets::FilterMode::SmoothMipmaps),
                        ] {
                            if ui.selectable_label(cur == opt, label(opt)).clicked() && cur != opt
                            {
                                self.cmd.set_model_filter = Some((path.to_string(), opt));
                            }
                        }
                    });
            });
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
                self.model_asset_materials_ui(ui, &path);
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
            } else if crate::assets::is_map_sidecar(&path) {
                self.map_asset_ui(ui, &path);
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
            ui.small(format!("{} selected — an edit here applies to all of them", self.selection.len()))
                .on_hover_text(
                    "The panel edits the last node you picked. Whatever you change on it — and \
                     only what you change — is handed to the rest of the selection when you let \
                     go, so each node keeps everything you didn't touch.\n\nNodes of a different \
                     kind take whatever they have in common (a material, a transform, a script's \
                     tunables) and ignore the rest.",
                );
        }
        // Everything this panel is about to write goes onto the PRIMARY. Take the
        // "before" here so the change can be found by comparison afterwards —
        // an immediate-mode panel leaves no other record of what it touched.
        let multi = multi_edit::Snapshot::take(self.world, self.selection);
        let cmd = &mut *self.cmd;
        let world = &mut *self.world;
        // Read before `self` is split up below (`floptle/0110`).
        let playing = self.playing;
        let bone_names = self.bone_names;
        // Snapshot the selected object/bone before `world` reborrows `self` — the
        // Objects & Rig lists highlight it and route clicks through `cmd.select_bone`.
        let cur_bone = *self.bone_selection;
        match primary {
            Some(e) if world.get::<Light>(e).is_some() => {
                // What ELSE is lighting this scene — counted before the `Light`
                // borrow, because the answer needs the whole world.
                //
                // "I set intensity to 0 and I can still see" is a fair thing to
                // expect and a fair thing to be confused by: `intensity` scales
                // the KEY light only, and four other things put photons on the
                // screen. None of them is discoverable from this panel, so the
                // panel now names them.
                let point_lights = world
                    .query::<Matter>()
                    .filter(|(pe, m)| {
                        matches!(m, Matter::PointLight { intensity, .. } if *intensity > 0.0)
                            && !floptle_core::is_disabled(world, *pe)
                    })
                    .count();
                let unlit_mats =
                    world.query::<Material>().filter(|(_, m)| m.unlit).count();
                let emissive_mats = world
                    .query::<Material>()
                    .filter(|(_, m)| m.emissive != [0.0; 3] && m.emissive_strength > 0.0)
                    .count();
                if let Some(l) = world.get_mut::<Light>(e) {
                    ui.label("Lighting node");
                    cmd.inspector_changed |= crate::responsive::check(ui, &mut l.stars, "stars mode ☀")
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
                        ui.horizontal_wrapped(|ui| {
                            cmd.inspector_changed |= ui.add(egui::DragValue::new(&mut l.direction[0]).speed(0.02).prefix("x ")).changed();
                            cmd.inspector_changed |= ui.add(egui::DragValue::new(&mut l.direction[1]).speed(0.02).prefix("y ")).changed();
                            cmd.inspector_changed |= ui.add(egui::DragValue::new(&mut l.direction[2]).speed(0.02).prefix("z ")).changed();
                        });
                    }
                    ui.horizontal_wrapped(|ui| {
                        ui.label("light");
                        cmd.inspector_changed |= ui.color_edit_button_rgb(&mut l.color).changed();
                        ui.label("ambient");
                        cmd.inspector_changed |= ui.color_edit_button_rgb(&mut l.ambient).changed();
                    });
                    // The 2D half, next to its 3D twin so the pair is obvious.
                    // Without a row here the only way to find out that a flat
                    // scene's base brightness is a field on the Lighting node
                    // was to be told.
                    ui.horizontal_wrapped(|ui| {
                        ui.label("2D base light");
                        cmd.inspector_changed |=
                            ui.color_edit_button_rgb(&mut l.ambient_2d).changed();
                        ui.small(if l.ambient_2d == [1.0, 1.0, 1.0] {
                            "full — 2D lights only add"
                        } else {
                            "turned down — 2D lights carve into it"
                        });
                    })
                    .response
                    .on_hover_text(
                        "What every tilemap and sprite batch is lit by before any 2D light \
                         reaches it. White means placing a light can only make things \
                         brighter. Turn it down for a dark room a torch cuts a circle out \
                         of.\n\nThis is the 2D one; `ambient` above is the 3D fill under \
                         the key light.",
                    );
                    cmd.inspector_changed |=
                        crate::responsive::slider(ui, egui::Slider::new(&mut l.intensity, 0.0..=8.0).text("intensity"))
                            .on_hover_text(
                                "brightness of the KEY (directional) light only. It is not a \
                                 master dimmer — ambient, 2D base light, point lights, emissive \
                                 and unlit materials are all separate.",
                            )
                            .changed();

                    // Turned the key light off and the scene is still lit. Say
                    // what by, and offer the one thing the person doing this
                    // almost always wants: actual darkness.
                    if l.intensity <= 0.0 && !l.stars {
                        let ambient_on = l.ambient != [0.0; 3];
                        let base_2d_on = l.ambient_2d != [0.0; 3];
                        let mut sources: Vec<String> = Vec::new();
                        if ambient_on {
                            sources.push("3D ambient".into());
                        }
                        if base_2d_on {
                            sources.push("the 2D base light".into());
                        }
                        if point_lights > 0 {
                            sources.push(format!(
                                "{point_lights} point light{}",
                                if point_lights == 1 { "" } else { "s" }
                            ));
                        }
                        if emissive_mats > 0 {
                            sources.push(format!("{emissive_mats} emissive material(s)"));
                        }
                        if unlit_mats > 0 {
                            sources.push(format!(
                                "{unlit_mats} UNLIT material(s) — unlit ignores light entirely"
                            ));
                        }
                        ui.add_space(2.0);
                        if sources.is_empty() {
                            ui.small("key light off, nothing else lights this scene — it is black.");
                        } else {
                            ui.colored_label(
                                egui::Color32::from_rgb(255, 200, 80),
                                "the key light is off and the scene is still lit",
                            );
                            ui.small(format!("still lighting it: {}.", sources.join(", ")));
                            if (ambient_on || base_2d_on)
                                && ui
                                    .button("🌑  Black it out")
                                    .on_hover_text(
                                        "zero the 3D ambient and the 2D base light — the two \
                                         fills this panel owns. Point lights, emissive and unlit \
                                         materials are per-node and stay as they are.",
                                    )
                                    .clicked()
                            {
                                l.ambient = [0.0; 3];
                                l.ambient_2d = [0.0; 3];
                                cmd.inspector_changed = true;
                            }
                        }
                    }

                    ui.separator();
                    cmd.inspector_changed |= crate::responsive::check(ui, &mut l.shadows, "sun shadows")
                        .on_hover_text(
                            "march the SDF field toward the sun — analytically soft shadows, \
                             no shadow maps. Terrain and blobs cast on everything; meshes cast \
                             via their collider shapes and receive like everything else.",
                        )
                        .changed();
                    ui.add_enabled_ui(l.shadows, |ui| {
                        cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(&mut l.shadow_softness, 0.0..=1.0).text("softness"))
                            .on_hover_text("0 = razor-hard edge (retro), 1 = dreamy-soft penumbra")
                            .changed();
                        cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(&mut l.shadow_strength, 0.0..=1.0).text("strength"))
                            .on_hover_text("how dark full shadow gets — ambient light still fills, so 1.0 isn't pitch black")
                            .changed();
                        ui.horizontal_wrapped(|ui| {
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
                                .width(crate::responsive::fit_here(ui, 220.0))
                .wrap_mode(egui::TextWrapMode::Truncate)
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
                            cmd.inspector_changed |= crate::responsive::check(ui, &mut l.shadow_dither, "dither the penumbra")
                                .on_hover_text("Bayer-pattern the quantized penumbra — the PS1 shadow edge; pairs with retro mode")
                                .changed();
                        });
                        cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(&mut l.shadow_distance, 10.0..=1000.0)
                                    .logarithmic(true)
                                    .text("distance"),
                            )
                            .on_hover_text("max distance a shadow ray marches (a perf fence — farther geometry stops casting)")
                            .changed();
                        // Contact shadows: the short-range half, from the depth
                        // buffer rather than from the field.
                        ui.separator();
                        cmd.inspector_changed |= crate::responsive::check(ui, &mut l.contact_shadows, "contact shadows")
                            .on_hover_text(
                                "the small dark line where things touch. A moving mesh casts through its \
                                 COLLIDER, so a character's shadow is a capsule's — this shadows from the \
                                 real silhouette of whatever is on screen instead. Short range: it is the \
                                 shadow under a foot, in a seam, behind a bolt.",
                            )
                            .changed();
                        ui.add_enabled_ui(l.contact_shadows, |ui| {
                            cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(&mut l.contact_length, 0.02..=3.0).text("reach").suffix("m"))
                                .on_hover_text("how far it traces. Too far and distant geometry starts smearing shadows over things in front of it")
                                .changed();
                            cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(&mut l.contact_strength, 0.0..=1.0).text("strength"))
                                .changed();
                            let mut steps = l.contact_steps as i32;
                            if crate::responsive::slider(ui, egui::Slider::new(&mut steps, 4..=32).text("steps"))
                                .on_hover_text("samples along the trace — raise it if the shadow looks striped")
                                .changed()
                            {
                                l.contact_steps = steps as u32;
                                cmd.inspector_changed = true;
                            }
                            ui.small("only shadows what is ON SCREEN — nothing off the edge of the frame casts one");
                        });
                        // Reflections of the SCENE. Sits with the shadow knobs
                        // rather than with fog because it is the same kind of
                        // thing: a scene-wide switch that costs a march, reads
                        // the depth buffer, and only sees what is on screen.
                        ui.separator();
                        cmd.inspector_changed |= crate::responsive::check(ui, &mut l.reflections, "reflections (screen space)")
                            .on_hover_text(
                                "reflective surfaces show the SCENE, not only the sky — a floor shows the \
                                 room standing on it. Every physical material with some reflectivity picks \
                                 this up at once; how much and how sharply is each material's roughness \
                                 and reflections. Only what is ON SCREEN can be reflected: anything behind \
                                 the camera or hidden behind something nearer falls back to the sky.",
                            )
                            .changed();
                        ui.add_enabled_ui(l.reflections, |ui| {
                            cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(&mut l.reflection_distance, 1.0..=200.0)
                                        .logarithmic(true)
                                        .text("reach")
                                        .suffix("m"),
                                )
                                .on_hover_text("how far a reflected ray travels before giving up. A puddle showing a building across the street needs more of this than a floor showing the table on it")
                                .changed();
                            let mut steps = l.reflection_steps as i32;
                            if crate::responsive::slider(ui, egui::Slider::new(&mut steps, 8..=64).text("steps"))
                                .on_hover_text("samples along that ray — raise it with the reach, or reflections start missing thin things")
                                .changed()
                            {
                                l.reflection_steps = steps as u32;
                                cmd.inspector_changed = true;
                            }
                            cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(&mut l.reflection_thickness, 0.02..=5.0)
                                        .logarithmic(true)
                                        .text("thickness")
                                        .suffix("m"),
                                )
                                .on_hover_text("how solid things are assumed to be. Too little and reflections come out speckled with holes; too much and railings and leaves smear over what is really behind them")
                                .changed();
                            cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(&mut l.reflection_clamp, 0.0..=64.0)
                                        .text("brightness cap"),
                                )
                                .on_hover_text(
                                    "the most one reflected bounce may carry. Two mirrors facing \
                                     each other re-reflect each other every frame and a polished \
                                     metal loses almost nothing per pass, so without a ceiling the \
                                     pair climbs into a white blob. Ordinary highlights sit well \
                                     under this. 0 removes the ceiling.",
                                )
                                .changed();
                            ui.small("reflects the PREVIOUS frame, so a reflection is one frame behind — invisible except on a mirror under a whipping camera");
                        });
                        ui.small("off screen, a reflection falls back to the SKY — place a ◍ Reflection Probe to give a room something else to show");
                        // Glass, in the same place as reflections and for the
                        // same reason: it is what a surface shows of the scene
                        // when the light goes THROUGH it rather than off it, and
                        // it is a scene-wide cost rather than a material one.
                        ui.separator();
                        let mut layers = l.refraction_layers as i32;
                        if crate::responsive::slider(ui, egui::Slider::new(
                                    &mut layers,
                                    1..=floptle_core::Light::MAX_REFRACTION_LAYERS as i32,
                                )
                                .text("glass layers"),
                            )
                            .on_hover_text(
                                "how many depths of see-through surface can be looked through at \
                                 once. At 1 only the nearest pane shows what is behind it, so a \
                                 fish tank has to be one box; raising it lets a window have a \
                                 bottle standing behind it. Each layer costs one more pass, and \
                                 only when something see-through is in view",
                            )
                            .changed()
                        {
                            l.refraction_layers = layers as u32;
                            cmd.inspector_changed = true;
                        }
                    });
                    // Fog — distance haze (depth ramp) or real marched media (volumetric).
                    ui.separator();
                    cmd.inspector_changed |= crate::responsive::check(ui, &mut l.fog, "fog")
                        .on_hover_text("fade the scene into a color — cheap depth ramp or a marched volumetric layer; the skybox stays crisp")
                        .changed();
                    ui.add_enabled_ui(l.fog, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label("mode");
                            egui::ComboBox::from_id_salt("fog_mode")
                                .width(crate::responsive::fit_here(ui, 220.0))
                .wrap_mode(egui::TextWrapMode::Truncate)
                                .selected_text(if l.fog_volumetric { "volumetric" } else { "depth" })
                                .show_ui(ui, |ui| {
                                    if ui.selectable_label(!l.fog_volumetric, "depth").clicked() && l.fog_volumetric {
                                        l.fog_volumetric = false;
                                        cmd.inspector_changed = true;
                                    }
                                    if ui
                                        .selectable_label(l.fog_volumetric, "volumetric")
                                        .on_hover_text("a height-bounded layer of drifting mist marched per pixel — hills poke out of ground fog, patches roll by")
                                        .clicked()
                                        && !l.fog_volumetric
                                    {
                                        l.fog_volumetric = true;
                                        cmd.inspector_changed = true;
                                    }
                                });
                        });
                        ui.horizontal_wrapped(|ui| {
                            ui.label("color");
                            cmd.inspector_changed |= ui
                                .color_edit_button_rgb(&mut l.fog_color)
                                .on_hover_text("match the horizon / background so no seam shows at the skybox")
                                .changed();
                        });
                        if l.fog_volumetric {
                            ui.horizontal_wrapped(|ui| {
                                ui.label("density");
                                cmd.inspector_changed |= ui
                                    .add(egui::DragValue::new(&mut l.fog_density).speed(0.001).range(0.0..=2.0))
                                    .on_hover_text("media thickness per world unit — how fast things vanish into it")
                                    .changed();
                            });
                            ui.horizontal_wrapped(|ui| {
                                ui.label("layer top");
                                cmd.inspector_changed |= ui
                                    .add(egui::DragValue::new(&mut l.fog_height).speed(0.1).suffix("m"))
                                    .on_hover_text("world height the fog fills up to")
                                    .changed();
                                ui.label("softness");
                                cmd.inspector_changed |= ui
                                    .add(egui::DragValue::new(&mut l.fog_falloff).speed(0.1).range(0.01..=1000.0).suffix("m"))
                                    .on_hover_text("how gradually the layer thins out above its top")
                                    .changed();
                            });
                            ui.horizontal_wrapped(|ui| {
                                ui.label("noise");
                                cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(&mut l.fog_noise, 0.0..=1.0))
                                    .on_hover_text("break the media into drifting patches (0 = uniform)")
                                    .changed();
                                ui.label("scale");
                                cmd.inspector_changed |= ui
                                    .add(egui::DragValue::new(&mut l.fog_noise_scale).speed(0.5).range(0.5..=1000.0).suffix("m"))
                                    .on_hover_text("wisp size in world units")
                                    .changed();
                            });
                            ui.horizontal_wrapped(|ui| {
                                ui.label("max distance");
                                cmd.inspector_changed |= ui
                                    .add(egui::DragValue::new(&mut l.fog_end).speed(1.0).range(1.0..=10000.0).suffix("m"))
                                    .on_hover_text("how far a ray that hits nothing keeps marching fog — a perf fence for sky pixels (an upward ray already stops where the layer ends)")
                                    .changed();
                            });
                            // Light injection: the media lit by the scene rather
                            // than painted a flat colour.
                            ui.separator();
                            cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(&mut l.fog_light, 0.0..=3.0).text("lit by the scene"))
                                .on_hover_text(
                                    "0 = the flat fog colour; 1 = the media lit by the sun, the point lights and the baked bounce; \
                                     past 1 exaggerates. The fog colour becomes what the media is MADE of rather than what it looks like.",
                                )
                                .changed();
                            ui.add_enabled_ui(l.fog_light > 0.0, |ui| {
                                cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(&mut l.fog_anisotropy, -0.9..=0.9).text("forward scatter"))
                                    .on_hover_text(
                                        "which way the media throws light. Positive blooms toward the sun (look into it and the air glows); \
                                         0 is an even haze; negative bounces it back at you. A mote of fog has no facing — this is what \
                                         does the job a surface normal does everywhere else.",
                                    )
                                    .changed();
                                cmd.inspector_changed |= crate::responsive::check(ui, &mut l.fog_shafts, "shafts (shadows in the fog)")
                                    .on_hover_text(
                                        "march the sun shadow at every fog step, so shadowed air stays dark and beams appear through \
                                         windows and branches. This is the entire cost of lit fog — turn it off and the media is lit \
                                         but never occluded.",
                                    )
                                    .changed();
                            });
                            ui.horizontal_wrapped(|ui| {
                                ui.label("quality");
                                let mut steps = l.fog_steps as i32;
                                if crate::responsive::slider(ui, egui::Slider::new(&mut steps, 4..=64).text("steps"))
                                    .on_hover_text("samples along each pixel's ray — raise it until the fog stops looking stepped, then stop")
                                    .changed()
                                {
                                    l.fog_steps = steps as u32;
                                    cmd.inspector_changed = true;
                                }
                            });
                            if l.fog_shafts && l.fog_light > 0.0 && !l.shadows {
                                ui.colored_label(
                                    egui::Color32::from_rgb(220, 170, 90),
                                    "shafts need shadows on (above) — the fog is lit but nothing occludes it",
                                );
                            }
                        } else {
                            ui.horizontal_wrapped(|ui| {
                                ui.label("start");
                                cmd.inspector_changed |= ui
                                    .add(egui::DragValue::new(&mut l.fog_start).speed(0.5).range(0.0..=10000.0).suffix("m"))
                                    .changed();
                                ui.label("end");
                                cmd.inspector_changed |= ui
                                    .add(egui::DragValue::new(&mut l.fog_end).speed(0.5).range(0.1..=10000.0).suffix("m"))
                                    .changed();
                            });
                        }
                        // Dither: hide 8-bit banding on long, slow fog ramps.
                        ui.horizontal_wrapped(|ui| {
                            cmd.inspector_changed |= crate::responsive::check(ui, &mut l.fog_dither, "dither")
                                .on_hover_text("break up color banding across the fog gradient (matches the retro pixel grid)")
                                .changed();
                            ui.add_enabled_ui(l.fog_dither, |ui| {
                                cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(&mut l.fog_dither_strength, 0.0..=1.0).text("amount"))
                                    .changed();
                            });
                        });
                    });
                }
            }
            Some(e) if world.get::<Transform>(e).is_some() => {
                // The on/off switch, where every other editor puts it: on the
                // name row, one click, always visible. It was reachable only
                // from a right-click menu, which is fine for something you do
                // once and wrong for something you do while trying things out —
                // and a node you cannot SEE the state of is a node you forget is
                // off. The checkbox reads the node's OWN flag; if an ancestor is
                // what switched it off, the line under it says so, because
                // ticking this one would then change nothing visible.
                let off_self = world.get::<floptle_core::Disabled>(e).is_some();
                let off_inherited = !off_self && floptle_core::is_disabled(world, e);
                ui.horizontal_wrapped(|ui| {
                    let mut on = !off_self;
                    if crate::responsive::check(ui, &mut on, "")
                        .on_hover_text(
                            "enabled — a switched-off node doesn't draw, doesn't collide, its \
                             scripts don't run, it can't be the active camera, and find() skips \
                             it. Everything under it goes with it.",
                        )
                        .changed()
                    {
                        // The whole selection, so switching six things off is one
                        // gesture — and the TARGET state is decided here, once,
                        // rather than each node flipping its own way.
                        let targets: Vec<floptle_core::Entity> = if self.selection.contains(&e) {
                            self.selection.clone()
                        } else {
                            vec![e]
                        };
                        cmd.set_enabled = Some((targets, on));
                    }
                    ui.label("name");
                    if let Some(n) = world.get_mut::<Name>(e) {
                        cmd.inspector_changed |= ui.text_edit_singleline(&mut n.0).changed();
                    }
                });
                if off_inherited {
                    ui.small(
                        egui::RichText::new(
                            "⚠ switched off by a parent — turning this one on changes nothing \
                             until the parent is on",
                        )
                        .color(egui::Color32::from_rgb(255, 200, 80)),
                    );
                }
                // ===== Layer + tags — identity every node carries. =====
                // Layer: the node's collision/query layer (project-defined names,
                // Project Settings → Layers). Tags: free-form chips scripts find
                // with `findTagged` / compare with `node:hasTag`.
                ui.horizontal_wrapped(|ui| {
                    // "collision layer", not "layer". A node has TWO things
                    // called a layer — this one, which answers "does this hit
                    // that", and the sorting layer below, which answers "which
                    // draws in front" — and they are deliberately independent: a
                    // background collides with nothing and still sorts, a player
                    // collides with everything and sorts separately. Two controls
                    // both labelled "layer" is how that independence gets read as
                    // a duplicate, and then as a bug.
                    ui.label("collision layer")
                        .on_hover_text(
                            "what this collides with and what a raycast can hit. NOT the \
                             sorting layer below — that one is about drawing, and the two \
                             are set independently on purpose.",
                        );
                    let cur = world
                        .get::<floptle_core::Layer>(e)
                        .map(|l| l.0.clone())
                        .unwrap_or_else(|| floptle_core::layers::DEFAULT_LAYER.to_string());
                    let known = self.layer_names.contains(&cur);
                    let shown = if known { cur.clone() } else { format!("⚠ {cur}") };
                    egui::ComboBox::from_id_salt("node_layer")
                        .width(crate::responsive::fit_here(ui, 220.0))
                .wrap_mode(egui::TextWrapMode::Truncate)
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
                // What draws in front of what, for a flat scene.
                //
                // Offered on anything FLAT, whether or not the project has named
                // a second sorting layer. Gating it on a second layer hid the
                // whole of Y-sorting from every new project — and Y-sorting is
                // the one thing here that needs no layers at all: a top-down
                // game with a single layer is the ordinary case, and it was the
                // case that could not reach the control. A 3D scene still sees
                // none of this.
                let flat = matches!(
                    world.get::<Matter>(e),
                    Some(Matter::Tilemap { .. })
                        | Some(Matter::SpriteBatch { .. })
                        | Some(Matter::Sprite { .. })
                );
                let sorts = flat
                    || self.sorting_names.len() > 1
                    || world.get::<floptle_core::Sorting>(e).is_some();
                if sorts {
                    ui.horizontal_wrapped(|ui| {
                        ui.label("sorting layer")
                            .on_hover_text(
                                "which stack this draws in — later layers draw in front of \
                                 earlier ones. Nothing to do with the collision layer above.",
                            );
                        let cur = world
                            .get::<floptle_core::Sorting>(e)
                            .cloned()
                            .unwrap_or_default();
                        let name = if cur.layer.trim().is_empty() {
                            floptle_core::DEFAULT_SORTING_LAYER.to_string()
                        } else {
                            cur.layer.clone()
                        };
                        let known = self.sorting_names.contains(&name);
                        egui::ComboBox::from_id_salt("node_sorting")
                            .width(crate::responsive::fit_here(ui, 220.0))
                .wrap_mode(egui::TextWrapMode::Truncate)
                            .selected_text(if known { name.clone() } else { format!("⚠ {name}") })
                            .show_ui(ui, |ui| {
                                for n in self.sorting_names {
                                    if ui.selectable_label(*n == name, n).clicked() && *n != name {
                                        cmd.set_sorting = Some((e, n.clone(), cur.order));
                                    }
                                }
                            })
                            .response
                            .on_hover_text(
                                "which sorting layer this draws in — later layers draw in \
                                 front. Project Settings names them.",
                            );
                        // How the place WITHIN the layer is decided. Offered
                        // beside the layer rather than hidden behind it, because
                        // "by Y" is the answer for a whole genre and a developer
                        // who does not know it exists will write it in Lua.
                        let mut mode = cur.mode;
                        for m in floptle_core::SortMode::ALL {
                            let hover = match m {
                                floptle_core::SortMode::Order => {
                                    "you say the position, with the number beside this"
                                }
                                floptle_core::SortMode::Y => {
                                    "lower on the screen draws in front — a character below \
                                     a table is in front of it and one above is behind, with \
                                     nobody authoring a number. The full sort is sorting \
                                     layer, then order, then Y: this only decides between \
                                     nodes that are level on both of the others."
                                }
                            };
                            if ui
                                .selectable_label(mode == m, m.label())
                                .on_hover_text(hover)
                                .clicked()
                                && mode != m
                            {
                                mode = m;
                                cmd.set_sort_mode = Some((e, m));
                            }
                        }
                        // `order` stays live under BOTH modes. Y is a tiebreak
                        // inside an order, not a replacement for it, and hiding
                        // the field would teach the wrong model — the one where
                        // turning Y-sorting on throws away the layering you
                        // already authored.
                        let mut order = cur.order;
                        if ui
                            .add(egui::DragValue::new(&mut order).speed(1).prefix("order "))
                            .on_hover_text(if mode == floptle_core::SortMode::Y {
                                "within the layer: higher draws in front. Y only decides \
                                 between nodes on the SAME order, so a shadow on order -1 \
                                 stays under a Y-sorted crowd."
                            } else {
                                "within the layer: higher draws in front"
                            })
                            .changed()
                        {
                            cmd.set_sorting = Some((e, name.clone(), order));
                        }
                        if mode == floptle_core::SortMode::Y {
                            crate::responsive::para(
                                ui,
                                egui::RichText::new("ties on this order go to whichever is lower")
                                    .weak()
                                    .small(),
                            );
                        }
                        if !known {
                            ui.small("not in Project Settings — draws in front")
                                .on_hover_text(
                                    "A layer that no longer exists sorts LAST, so the node is \
                                     visible and obviously wrong rather than hidden behind \
                                     the background.",
                                );
                        }
                    });
                }
                // Parallax, beside sorting because they are the two things a
                // flat scene says about a layer as a whole. Offered on the same
                // condition, for the same reason.
                if sorts || world.get::<floptle_core::Parallax>(e).is_some() {
                    ui.horizontal_wrapped(|ui| {
                        ui.label("parallax");
                        let cur = world.get::<floptle_core::Parallax>(e).copied().unwrap_or_default();
                        let mut next = cur;
                        let mut changed = false;
                        for (i, axis) in ["x ", "y "].iter().enumerate() {
                            changed |= ui
                                .add(
                                    egui::DragValue::new(&mut next.factor[i])
                                        .speed(0.01)
                                        .range(0.0..=4.0)
                                        .prefix(*axis),
                                )
                                .on_hover_text(
                                    "how much of the camera's movement this layer keeps. \
                                     1 moves with the world (no parallax), 0 is pinned to \
                                     the camera as if infinitely far away, 0.3 is distant \
                                     hills. Nothing actually moves — it is an offset on the \
                                     drawn transform, so the collider stays put.",
                                )
                                .changed();
                        }
                        if changed {
                            cmd.set_parallax = Some((e, next));
                        }
                        if !cur.is_identity() {
                            crate::responsive::para(
                                ui,
                                egui::RichText::new("drawn offset only — nothing moves")
                                    .weak()
                                    .small(),
                            );
                        }
                    });
                }
                lighting_2d_row(ui, world, e, self.sorting_names, cmd);
                camera_2d_section(ui, world, e, cmd);
                ui.horizontal_wrapped(|ui| {
                    ui.label("tags");
                    let mut remove: Option<String> = None;
                    if let Some(tags) = world.get::<floptle_core::Tags>(e) {
                        for t in &tags.0 {
                            if ui
                                .small_button(format!("{t} ✖"))
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
                    // The Sprite editor needs its node's Material, and the
                    // Matter borrow below is mutable — so this is read first.
                    let sprite_facts = {
                        let mat = world.get::<Material>(e);
                        (mat.map(|m| m.sheet()), mat.is_some_and(|m| m.texture.is_some()))
                    };
                    if let Some(m) = world.get_mut::<Matter>(e) {
                        match m {
                            Matter::Primitive { shape, color } => {
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("shape");
                                    egui::ComboBox::from_id_salt("shape")
                                        .width(crate::responsive::fit_here(ui, 220.0))
                .wrap_mode(egui::TextWrapMode::Truncate)
                                        .selected_text(format!("{shape:?}"))
                                        .show_ui(ui, |ui| {
                                            cmd.inspector_changed |= ui.selectable_value(shape, Shape::Cube, "Cube").clicked();
                                            cmd.inspector_changed |= ui.selectable_value(shape, Shape::Sphere, "Sphere").clicked();
                                            cmd.inspector_changed |= ui.selectable_value(shape, Shape::Capsule, "Capsule").clicked();
                                            cmd.inspector_changed |= ui.selectable_value(shape, Shape::Plane, "Plane").clicked();
                                        });
                                });
                                ui.horizontal_wrapped(|ui| {
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
                            // 2D (`floptle/0058`). The grid is edited from
                            // Lua — a room is re-dressed per floor — so the
                            // Inspector states the shape and the one thing that
                            // is easy to get wrong: the sheet is the MATERIAL's.
                            Matter::Tilemap { cols, rows, tile, data, tileset } => {
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("grid");
                                    cmd.inspector_changed |= ui
                                        .add(egui::DragValue::new(cols).speed(1.0).prefix("cols ").range(0..=1024))
                                        .changed();
                                    cmd.inspector_changed |= ui
                                        .add(egui::DragValue::new(rows).speed(1.0).prefix("rows ").range(0..=1024))
                                        .changed();
                                });
                                cmd.inspector_changed |= ui
                                    .add(egui::DragValue::new(tile).speed(0.01).prefix("tile ").range(0.001..=64.0))
                                    .on_hover_text("world size of one tile's edge")
                                    .changed();
                                let want = (*cols as usize) * (*rows as usize);
                                let placed = data
                                    .iter()
                                    .filter(|&&p| p != floptle_core::EMPTY_TILE)
                                    .count();
                                ui.small(format!("{placed} of {want} squares placed"));
                                if data.len() != want && ui.button("resize to fit").clicked() {
                                    data.resize(want, floptle_core::EMPTY_TILE);
                                    cmd.inspector_changed = true;
                                }
                                // The tileset — what says whether these tiles collide,
                                // what they are tagged, and how they autotile. Read-only
                                // here on purpose: attaching one is a ◫ Tiles operation
                                // (it needs the sheet's dimensions to make sense of), and
                                // a free-text path field is a way to typo a level solid.
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("tileset");
                                    if tileset.is_empty() {
                                        // Named as the two FEATURES somebody would
                                        // go looking for, and coloured, because
                                        // this was reported twice as the engine
                                        // not having either of them.
                                        ui.colored_label(
                                            egui::Color32::from_rgb(255, 200, 80),
                                            "none",
                                        );
                                    } else {
                                        ui.small(
                                            floptle_tiles::tileset_name(tileset).unwrap_or(tileset),
                                        );
                                    }
                                    if ui.small_button("◫ Tiles").clicked() {
                                        cmd.focus_tiles = true;
                                    }
                                });
                                if tileset.is_empty() {
                                    ui.small(
                                        "Without one, these tiles collide with nothing and \
                                         cannot autotile — both are per-tile facts and live \
                                         in a tileset. Make one in ◫ Tiles; it takes its \
                                         sheet from this node's Material.",
                                    );
                                }
                                ui.small(
                                    "the sheet comes from this node's Material (texture + \
                                     sheet cols/rows). Paint it in the ◫ Tiles tab, or fill \
                                     it from a script: node:setTilemap{...} then tm:set(x, y, cell).",
                                );
                            }
                            Matter::SpriteBatch { size } => {
                                cmd.inspector_changed |= ui
                                    .add(egui::DragValue::new(size).speed(0.01).prefix("sprite size ").range(0.001..=64.0))
                                    .on_hover_text("world edge of one sprite, before its own scale")
                                    .changed();
                                ui.small(
                                    "sprites are written per frame from a script — \
                                     node:sprites() then b:clear() and b:draw(...). Each one \
                                     carries its own cell AND tint, which a shared Material \
                                     cannot give it.",
                                );
                            }
                            Matter::Sprite { ppu, size, cell, flip_x, flip_y, pivot } => {
                                // `ppu` measures the TEXTURE, so with no texture
                                // there is nothing to measure and the sprite
                                // falls back to `size` — a field this mode hides.
                                // Silently, that is a headline control that does
                                // nothing on a node somebody just created.
                                // (Read before the Matter borrow — see `sprite_facts`.)
                                let (sheet, has_tex) = sprite_facts;
                                if !has_tex {
                                    crate::responsive::para(
                                        ui,
                                        egui::RichText::new(
                                            "no texture yet — give this node a Material with one,                                              and the sheet's cols/rows in its import settings",
                                        )
                                        .weak()
                                        .small(),
                                    );
                                }
                                // Size, two ways, and only one of them live at a
                                // time — a pixels-per-unit sprite takes its size
                                // from the image, so leaving the world-size field
                                // editable beside it would offer a number that
                                // does nothing.
                                let mut by_px = *ppu > 0.0;
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("size from");
                                    for (on, label, hover) in [
                                        (true, "pixels", "the image decides: a 32x32 cell at 32 pixels per unit is one unit across. The number a pixel artist already has."),
                                        (false, "world units", "you decide: an edge length, whatever the image is."),
                                    ] {
                                        if ui
                                            .selectable_label(by_px == on, label)
                                            .on_hover_text(hover)
                                            .clicked()
                                            && by_px != on
                                        {
                                            by_px = on;
                                            // Leaving a remembered `ppu` behind
                                            // would make "world units" silently
                                            // revert next time it was touched.
                                            *ppu = if on { 32.0 } else { 0.0 };
                                            cmd.inspector_changed = true;
                                        }
                                    }
                                });
                                if by_px {
                                    cmd.inspector_changed |= ui
                                        .add(
                                            egui::DragValue::new(ppu)
                                                .speed(1.0)
                                                .range(1.0..=1024.0)
                                                .prefix("pixels per unit "),
                                        )
                                        .on_hover_text(
                                            "measured against ONE CELL of the sheet, not the \
                                             whole image — so slicing a sheet finer does not \
                                             resize every sprite on it",
                                        )
                                        .changed();
                                } else {
                                    cmd.inspector_changed |= ui
                                        .add(
                                            egui::DragValue::new(size)
                                                .speed(0.01)
                                                .range(0.001..=1024.0)
                                                .prefix("size "),
                                        )
                                        .on_hover_text("world edge length")
                                        .changed();
                                }
                                let cells = sheet.map(|(c, r)| c.max(1) * r.max(1)).unwrap_or(1);
                                cmd.inspector_changed |= ui
                                    .add(
                                        egui::DragValue::new(cell)
                                            .speed(1)
                                            .range(0..=cells.saturating_sub(1)),
                                    )
                                    .on_hover_text(if cells > 1 {
                                        "which cell of the Material's sheet, row-major from the \
                                         top-left"
                                    } else {
                                        "this Material's texture is not sliced into a sheet, so \
                                         there is only one cell — set cols/rows in the texture's \
                                         import settings"
                                    })
                                    .changed();
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("flip");
                                    cmd.inspector_changed |=
                                        crate::responsive::check(ui, flip_x, "x").changed();
                                    cmd.inspector_changed |=
                                        crate::responsive::check(ui, flip_y, "y").changed();
                                    crate::responsive::para(
                                        ui,
                                        egui::RichText::new("mirrors the picture, not the node")
                                            .weak()
                                            .small(),
                                    );
                                });
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("pivot");
                                    for (i, axis) in ["x ", "y "].iter().enumerate() {
                                        cmd.inspector_changed |= ui
                                            .add(
                                                egui::DragValue::new(&mut pivot[i])
                                                    .speed(0.01)
                                                    .range(-2.0..=3.0)
                                                    .prefix(*axis),
                                            )
                                            .on_hover_text(
                                                "where the node's origin sits in the sprite, \
                                                 0..1 from the bottom-left; 0.5, 0.5 is the \
                                                 centre. Outside 0..1 is allowed and puts the \
                                                 origin off the picture entirely, which is \
                                                 occasionally what a hand-drawn rig wants.",
                                            )
                                            .changed();
                                    }
                                    if ui
                                        .small_button("feet")
                                        .on_hover_text(
                                            "0.5, 0 — the origin at the bottom of the sprite. \
                                             What a Y-sorted character wants: sorting reads \
                                             the node's Y, and a centred origin sorts by a \
                                             point floating at the character's waist.",
                                        )
                                        .clicked()
                                    {
                                        *pivot = [0.5, 0.0];
                                        cmd.inspector_changed = true;
                                    }
                                });
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
                            Matter::MapMesh { id } => {
                                ui.label(format!("map mesh #{id}"));
                                ui.small(
                                    "editable blockout geometry — use the ▦ Model tool (key 8) \
                                     to edit faces/edges/verts, extrude, and assign per-face \
                                     materials; the Map tab has the shape ops",
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
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("model");
                                    if let Some(Some(p)) = crate::ui_widgets::asset_picker(
                                        ui,
                                        egui::Id::new("mesh-model"),
                                        self.project_root,
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
                            Matter::Camera {
                                fov_y,
                                active,
                                target,
                                cull_mask,
                                target_w,
                                target_h,
                                target_hz,
                                ortho,
                                ortho_height,
                            } => {
                                ui.label("camera");
                                ui.small("a viewpoint — play mode renders from the active camera");
                                // Live preview of what this camera sees.
                                if let Some(tex) = self.cam_preview {
                                    let w = ui.available_width().min(300.0);
                                    let size = egui::vec2(w, w * 9.0 / 16.0);
                                    ui.add(egui::Image::new((tex, size)).corner_radius(4.0));
                                    ui.small("preview — what this camera sees");
                                }
                                // Perspective or orthographic. The two knobs are
                                // exclusive and only the live one is shown —
                                // greying out the other would still invite
                                // dragging a number that does nothing.
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("projection").on_hover_text(
                                        "orthographic draws everything at the same scale at \
                                         every distance — what a 2D, isometric or strategy \
                                         camera wants. Perspective is the 3D default.",
                                    );
                                    for (label, want) in
                                        [("perspective", false), ("orthographic", true)]
                                    {
                                        if ui.selectable_label(*ortho == want, label).clicked()
                                            && *ortho != want
                                        {
                                            *ortho = want;
                                            cmd.inspector_changed = true;
                                        }
                                    }
                                });
                                if *ortho {
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label("height").on_hover_text(
                                            "how many world units the view covers top to \
                                             bottom. With 1-unit tiles this is how many tiles \
                                             tall the shot is; the width follows the aspect.",
                                        );
                                        let mut h = *ortho_height;
                                        if ui
                                            .add(
                                                egui::DragValue::new(&mut h)
                                                    .speed(0.1)
                                                    .range(0.1..=1000.0)
                                                    .suffix(" units"),
                                            )
                                            .changed()
                                        {
                                            *ortho_height = Matter::clamp_ortho_height(h);
                                            cmd.inspector_changed = true;
                                        }
                                    });
                                } else {
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label("field of view");
                                        let mut deg = fov_y.to_degrees();
                                        if crate::responsive::slider(ui, egui::Slider::new(&mut deg, 20.0..=120.0).suffix("°")).changed() {
                                            *fov_y = deg.to_radians();
                                            cmd.inspector_changed = true;
                                        }
                                    });
                                }
                                // A1: render-target name — a live texture any material
                                // or UI image can wear as `rt:<name>`.
                                ui.horizontal_wrapped(|ui| {
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
                                    // Size + refresh rate: a minimap is not worth a
                                    // full-rate 480×270 (floptle/0078).
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label("size").on_hover_text(
                                            "the target texture's pixel size — smaller is \
                                             cheaper, and a screen a few metres away does \
                                             not need many",
                                        );
                                        let mut w = *target_w as i32;
                                        let mut h = *target_h as i32;
                                        let lo = Matter::TARGET_MIN as i32;
                                        let hi = Matter::TARGET_MAX as i32;
                                        let cw = ui.add(egui::DragValue::new(&mut w).range(lo..=hi));
                                        ui.label("×");
                                        let ch = ui.add(egui::DragValue::new(&mut h).range(lo..=hi));
                                        if cw.changed() || ch.changed() {
                                            *target_w = w as u32;
                                            *target_h = h as u32;
                                            cmd.inspector_changed = true;
                                        }
                                    });
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label("refresh").on_hover_text(
                                            "how often the target redraws, in Hz. 0 = every \
                                             frame. A 10 Hz minimap costs a sixth of a 60 Hz one.",
                                        );
                                        let mut hz = *target_hz;
                                        if ui
                                            .add(
                                                egui::DragValue::new(&mut hz)
                                                    .range(0.0..=240.0)
                                                    .speed(0.5),
                                            )
                                            .changed()
                                        {
                                            *target_hz = hz.max(0.0);
                                            cmd.inspector_changed = true;
                                        }
                                        ui.small(if *target_hz <= 0.0 {
                                            "every frame".to_string()
                                        } else {
                                            format!("{:.0} Hz", *target_hz)
                                        });
                                    });
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
                                        if crate::responsive::check(ui, &mut on, name).changed() {
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
                            Matter::PointLight {
                                color,
                                intensity,
                                range,
                                shape,
                                shadows,
                                spot_angle,
                                spot_softness,
                            } => {
                                use floptle_core::LightShape as LS;
                                let aimed = floptle_core::is_spot(*spot_angle);
                                ui.label(if aimed { "spot light" } else { "light" });
                                ui.small("position and facing come from the transform below");
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("color");
                                    cmd.inspector_changed |= ui.color_edit_button_rgb(color).changed();
                                });
                                cmd.inspector_changed |=
                                    crate::responsive::slider(ui, egui::Slider::new(intensity, 0.0..=20.0).text("intensity")).changed();
                                cmd.inspector_changed |=
                                    crate::responsive::slider(ui, egui::Slider::new(range, 0.1..=200.0).text("range")).changed();
                                cmd.inspector_changed |= crate::responsive::check(ui, shadows, "casts shadows")
                                    .on_hover_text(
                                        "stop this lamp at the walls between it and what it lights, instead \
                                         of shining through them. Per lamp, because it costs a march per lit \
                                         pixel and most lights in a level have nothing to be blocked by. \
                                         Shadows from what is ON SCREEN: a wall casts while it is in frame \
                                         and stops when you look away from it. Quality and darkness are on \
                                         the Lighting node.",
                                    )
                                    .changed();
                                // AIMING it. Above the emitter section because
                                // it is the bigger question — "does this lamp
                                // light the room or one thing in it" changes
                                // what every control under it means.
                                ui.separator();
                                let mut on = aimed;
                                if crate::responsive::check(ui, &mut on, "aim it (spot)")
                                    .on_hover_text(
                                        "cone down the node's forward, the same axis a camera looks \
                                         down — rotate the node to aim it. Off means the lamp lights \
                                         everything around it, which is what it has always done.",
                                    )
                                    .changed()
                                {
                                    // Turning it off parks the angle at omni and
                                    // KEEPS the softness, so switching a spot off
                                    // and on again gives back the same cone
                                    // rather than the default one.
                                    *spot_angle =
                                        if on { 45.0 } else { floptle_core::OMNI_ANGLE };
                                    cmd.inspector_changed = true;
                                }
                                if on {
                                    cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(
                                                spot_angle,
                                                floptle_core::MIN_SPOT_ANGLE
                                                    ..=floptle_core::OMNI_ANGLE - 0.5,
                                            )
                                            .text("cone")
                                            .suffix("°"),
                                        )
                                        .on_hover_text(
                                            "the FULL angle, the number on a real fixture — 45° is a \
                                             45° cone, not a 90° one",
                                        )
                                        .changed();
                                    cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(spot_softness, 0.0..=1.0).text("edge"))
                                        .on_hover_text(
                                            "how much of the cone is falloff. 0 is a hard circle; 1 \
                                             fades from the middle out. A fraction of the cone, so \
                                             widening the beam keeps the edge you gave it.",
                                        )
                                        .changed();
                                }

                                // The EMITTER. Switching shape keeps whatever
                                // size the old one had where the two agree, so
                                // trying rect against disk is one click and not
                                // a re-measure.
                                ui.separator();
                                let old = *shape;
                                let size = old.extent().max(0.25);
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("emits from");
                                    let mut pick = |ui: &mut egui::Ui, label: &str, on: bool, make: LS| {
                                        if ui.selectable_label(on, label).clicked() && !on {
                                            *shape = make;
                                            cmd.inspector_changed = true;
                                        }
                                    };
                                    pick(ui, "point", matches!(old, LS::Point), LS::Point);
                                    pick(ui, "sphere", matches!(old, LS::Sphere { .. }), LS::Sphere { radius: size });
                                    pick(
                                        ui,
                                        "rect",
                                        matches!(old, LS::Rect { .. }),
                                        LS::Rect { width: size * 2.0, height: size * 2.0, two_sided: false },
                                    );
                                    pick(ui, "disk", matches!(old, LS::Disk { .. }), LS::Disk { radius: size, two_sided: false });
                                    pick(ui, "tube", matches!(old, LS::Tube { .. }), LS::Tube { length: size * 4.0, radius: size * 0.25 });
                                });
                                let drag = |ui: &mut egui::Ui, label: &str, v: &mut f32| -> bool {
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label(label);
                                        ui.add(egui::DragValue::new(v).speed(0.05).range(0.001..=200.0).suffix("m"))
                                            .changed()
                                    })
                                    .inner
                                };
                                match shape {
                                    LS::Point => {
                                        ui.small(
                                            "a dimensionless point — a hard highlight and a hard shadow edge, \
                                             which is right for a bare bulb and wrong for a window",
                                        );
                                    }
                                    LS::Sphere { radius } => {
                                        cmd.inspector_changed |= drag(ui, "radius", radius);
                                        ui.small("a bulb with size: the highlight becomes a disc and the terminator softens");
                                    }
                                    LS::Rect { width, height, two_sided } => {
                                        cmd.inspector_changed |= drag(ui, "width", width);
                                        cmd.inspector_changed |= drag(ui, "height", height);
                                        cmd.inspector_changed |= crate::responsive::check(ui, two_sided, "lights both ways")
                                            .on_hover_text("off = a window, on = a floating panel that glows from both faces")
                                            .changed();
                                        ui.small("faces the node's forward — rotate the node to aim it");
                                    }
                                    LS::Disk { radius, two_sided } => {
                                        cmd.inspector_changed |= drag(ui, "radius", radius);
                                        cmd.inspector_changed |= crate::responsive::check(ui, two_sided, "lights both ways").changed();
                                        ui.small("faces the node's forward — rotate the node to aim it");
                                    }
                                    LS::Tube { length, radius } => {
                                        cmd.inspector_changed |= drag(ui, "length", length);
                                        cmd.inspector_changed |= drag(ui, "thickness", radius);
                                        ui.small("lies along the node's local X, and streaks its highlight along itself");
                                    }
                                }
                            }
                            Matter::GravityVolume { mode, strength, radius } => {
                                use floptle_core::GravityMode;
                                ui.label("gravity volume");
                                ui.small("level physics gravity — Down (normal) or Radial (planet)");
                                ui.horizontal_wrapped(|ui| {
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
                                    crate::responsive::slider(ui, egui::Slider::new(strength, 0.0..=60.0).text("strength")).changed();
                                if *mode == GravityMode::Radial {
                                    cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(radius, 0.5..=500.0).text("well radius"))
                                        .changed();
                                }
                            }
                            Matter::WaterVolume {
                                kind,
                                radius,
                                half_extents,
                                density,
                                drag,
                                angular_drag,
                                frozen,
                                tint,
                                visibility,
                            } => {
                                use floptle_core::WaterKind;
                                ui.label("water volume");
                                ui.small(
                                    "buoyancy, drag and an underwater look. A Sea is a sphere \
                                     about this node (a planet's ocean); a Pool is an oriented \
                                     box — rotate the node and the surface tilts with it.",
                                );
                                ui.horizontal_wrapped(|ui| {
                                    for k in WaterKind::ALL {
                                        if ui.selectable_label(*kind == k, k.label()).clicked()
                                            && *kind != k
                                        {
                                            *kind = k;
                                            cmd.inspector_changed = true;
                                        }
                                    }
                                });
                                match kind {
                                    WaterKind::Sea => {
                                        cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(radius, 1.0..=100_000.0)
                                                    .logarithmic(true)
                                                    .text("sea radius"),
                                            )
                                            .changed();
                                    }
                                    WaterKind::Pool => {
                                        for (i, label) in ["half X", "half Y (depth)", "half Z"]
                                            .iter()
                                            .enumerate()
                                        {
                                            cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(
                                                        &mut half_extents[i],
                                                        0.1..=1000.0,
                                                    )
                                                    .logarithmic(true)
                                                    .text(*label),
                                                )
                                                .changed();
                                        }
                                    }
                                }
                                cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(density, 1.0..=5000.0)
                                            .text("density kg/m³"),
                                    )
                                    .on_hover_text(
                                        "1000 = fresh water. What decides whether a given hull \
                                         floats is this against the hull's own density, so a \
                                         denser sea carries heavier craft.",
                                    )
                                    .changed();
                                cmd.inspector_changed |=
                                    crate::responsive::slider(ui, egui::Slider::new(drag, 0.0..=10.0).text("drag")).changed();
                                cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(angular_drag, 0.0..=10.0).text("spin drag"))
                                    .changed();
                                ui.separator();
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("underwater tint");
                                    cmd.inspector_changed |=
                                        ui.color_edit_button_rgb(tint).changed();
                                });
                                cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(visibility, 1.0..=500.0)
                                            .logarithmic(true)
                                            .text("visibility m"),
                                    )
                                    .on_hover_text(
                                        "How far you can see from inside. Replaces the scene's \
                                         own fog while the camera is under, so meshes, terrain \
                                         and particles go murky together.",
                                    )
                                    .changed();
                                ui.separator();
                                cmd.inspector_changed |= crate::responsive::check(ui, frozen, "frozen")
                                    .on_hover_text(
                                        "A frozen sea is not a fluid: no buoyancy, no drag, no \
                                         underwater state. Add a Collidable surface and it \
                                         becomes walkable ground. A script can thaw it.",
                                    )
                                    .changed();
                            }
                            Matter::Skybox { color, size, texture, tint, shader, shader_params } => {
                                ui.label("skybox");
                                ui.small("the scene environment, drawn behind everything. Rotate this node (or a script) to spin the sky.");
                                // A Sky-stage .flsl overrides the solid/texture look with a
                                // procedural sky (per-ray-direction color). Clear it to fall
                                // back to the solid/texture controls below.
                                ui.horizontal_wrapped(|ui| {
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
                                        self.project_root,
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
                                    if let Some(path) = shader.clone()
                                        && ui
                                            .button("◈")
                                            .on_hover_text("edit this shader in the ◈ Shaders graph")
                                            .clicked()
                                    {
                                        cmd.open_shader_graph = Some(path);
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
                                        crate::responsive::grid(ui, "sky_shader_rows", |ui| {
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
                                ui.horizontal_wrapped(|ui| {
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
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label("color");
                                        cmd.inspector_changed |= ui.color_edit_button_rgb(color).changed();
                                    });
                                } else {
                                    let tree = self.asset_tree;
                                    let cur = texture.clone().unwrap_or_default();
                                    let label = |p: &str| {
                                        Path::new(p).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| p.to_string())
                                    };
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label("texture");
                                        if let Some(Some(p)) = crate::ui_widgets::asset_picker(
                                            ui,
                                            egui::Id::new("sky-tex"),
                                            self.project_root,
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
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label("tint");
                                        cmd.inspector_changed |= ui.color_edit_button_rgb(tint).changed();
                                    });
                                }
                                cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(size, 10.0..=5000.0).logarithmic(true).text("size (radius)"))
                                    .changed();
                            }
                            Matter::NavMesh {
                                id: _,
                                half_extents,
                                auto_bounds,
                                layers,
                                agent_radius,
                                agent_height,
                                max_slope,
                                step_height,
                                cell_size,
                                enabled,
                                auto_rebake,
                            } => {
                                let nav = self.nav.clone();
                                if crate::responsive::check(ui, enabled, "characters can path on this").changed() {
                                    cmd.inspector_changed = true;
                                }
                                ui.small(
                                    "Where characters can walk. Bakes what they would collide \
                                     with — narrow it by layer, or drop one object with the \
                                     Navmesh Exclude switch on it.",
                                );
                                ui.separator();

                                // ---- what gets baked ----------------------------
                                let label = if layers.is_empty() {
                                    "layers: everything".to_string()
                                } else {
                                    format!("layers: {}", layers.join(", "))
                                };
                                ui.menu_button(label, |ui| {
                                    for name in self.layer_names.iter() {
                                        let mut on = layers.iter().any(|l| l == name);
                                        if crate::responsive::check(ui, &mut on, name).changed() {
                                            if on {
                                                layers.push(name.clone());
                                            } else {
                                                layers.retain(|l| l != name);
                                            }
                                            cmd.inspector_changed = true;
                                        }
                                    }
                                    if ui.small_button("everything").clicked() {
                                        layers.clear();
                                        cmd.inspector_changed = true;
                                    }
                                })
                                .response
                                .on_hover_text(
                                    "Which layers count as level geometry. Nothing ticked means \
                                     every layer.",
                                );
                                ui.small(format!(
                                    "{} object{} would be baked",
                                    nav.sources,
                                    if nav.sources == 1 { "" } else { "s" }
                                ));
                                if nav.sources == 0 {
                                    ui.small(
                                        "— nothing matches. A navmesh bakes what a character \
                                         would collide with, so level geometry needs the \
                                         collidable switch on it.",
                                    );
                                }

                                // ---- the box ------------------------------------
                                ui.separator();
                                if crate::responsive::check(ui, auto_bounds, "fit the box to what it finds")
                                    .on_hover_text(
                                        "Work the volume out from the geometry instead of \
                                         sizing it by hand. A box that is too small clips the \
                                         level, and nothing about the result says which.",
                                    )
                                    .changed()
                                {
                                    cmd.inspector_changed = true;
                                }
                                if !*auto_bounds {
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label("size");
                                        for (i, axis) in ["x", "y", "z"].iter().enumerate() {
                                            let mut full = half_extents[i] * 2.0;
                                            let r = ui.add(
                                                egui::DragValue::new(&mut full)
                                                    .speed(0.25)
                                                    .range(0.5..=100000.0)
                                                    .prefix(format!("{axis} ")),
                                            );
                                            if r.changed() {
                                                half_extents[i] = full * 0.5;
                                                cmd.inspector_changed = true;
                                            }
                                        }
                                    })
                                    .response
                                    .on_hover_text(
                                        "The volume's full size in world units, before the \
                                         node's scale. Move the node to move the box.",
                                    );
                                }

                                // ---- the character ------------------------------
                                ui.separator();
                                ui.small("the character this is for");
                                let mut touched = false;
                                touched |= crate::responsive::slider(ui, egui::Slider::new(agent_radius, 0.0..=5.0)
                                            .text("radius"),
                                    )
                                    .on_hover_text(
                                        "How wide it is. Ground closer than this to a wall or a \
                                         drop is not walkable, so a path can be walked by \
                                         something with a body rather than by a point.",
                                    )
                                    .changed();
                                touched |= crate::responsive::slider(ui, egui::Slider::new(agent_height, 0.1..=10.0)
                                            .text("height"),
                                    )
                                    .on_hover_text(
                                        "How tall it is. Ground with less headroom than this is \
                                         not walkable.",
                                    )
                                    .changed();
                                touched |= crate::responsive::slider(ui, egui::Slider::new(max_slope, 0.0..=89.0)
                                            .suffix("°")
                                            .text("max slope"),
                                    )
                                    .on_hover_text("The steepest floor it will walk up.")
                                    .changed();
                                touched |= crate::responsive::slider(ui, egui::Slider::new(step_height, 0.0..=5.0)
                                            .text("step height"),
                                    )
                                    .on_hover_text(
                                        "The tallest lip it steps over rather than walks around. \
                                         This is what makes a staircase one place and a ledge \
                                         two.",
                                    )
                                    .changed();
                                touched |= crate::responsive::slider(ui, egui::Slider::new(cell_size, 0.02..=2.0)
                                            .logarithmic(true)
                                            .text("cell size"),
                                    )
                                    .on_hover_text(
                                        "How finely the level is sampled. The one performance \
                                         knob: halving it quadruples the bake.",
                                    )
                                    .changed();
                                if touched {
                                    cmd.inspector_changed = true;
                                }
                                // The one setting that quietly does something other
                                // than what it says, named with the number to use.
                                if let Some(advice) = nav.advice.as_deref() {
                                    ui.add_space(2.0);
                                    ui.small(egui::RichText::new(advice).color(
                                        egui::Color32::from_rgb(230, 180, 90),
                                    ));
                                }

                                // ---- the bake -----------------------------------
                                ui.separator();
                                ui.horizontal_wrapped(|ui| {
                                    if ui
                                        .button("⬚  Bake")
                                        .on_hover_text(
                                            "Work out where this character can walk. Saved next \
                                             to the scene as a .fnav.",
                                        )
                                        .clicked()
                                    {
                                        cmd.nav_bake = true;
                                    }
                                    if nav.polys > 0
                                        && ui
                                            .button("🗑  Clear")
                                            .on_hover_text("Throw the bake away.")
                                            .clicked()
                                    {
                                        cmd.nav_clear = true;
                                    }
                                });
                                if crate::responsive::check(ui, auto_rebake, "bake again when the level changes")
                                    .on_hover_text(
                                        "Off is right for a finished level: the bake is a file \
                                         saved beside the scene and loaded with it, so it never \
                                         needs doing twice.\n\nOn, the volume watches what it \
                                         would bake, waits for it to stop moving, and bakes on \
                                         another thread — so the editor keeps its frame rate, \
                                         and a game that puts buildings down while it runs gets \
                                         a navmesh that knows about them.",
                                    )
                                    .changed()
                                {
                                    cmd.inspector_changed = true;
                                }
                                if nav.baking {
                                    ui.small("baking…");
                                }
                                if nav.polys == 0 {
                                    ui.small("no bake yet — nothing can path here.");
                                    ui.small(
                                        "A bake is saved beside the scene and loaded with it, so \
                                         this is a one-off — not something to do again each time \
                                         you open the project.",
                                    );
                                } else {
                                    ui.small(format!(
                                        "baked: {} polygons over {:.0} m², from {} triangles in \
                                         {:.2}s",
                                        nav.polys, nav.area, nav.triangles, nav.seconds
                                    ));
                                    // Where it lives. A bake is a file, it is
                                    // loaded with the scene, and saying so is
                                    // the difference between trusting that and
                                    // pressing Bake every time out of habit.
                                    match nav.file.as_deref() {
                                        Some(f) => {
                                            ui.small(format!("saved in {f} — it loads with the scene"));
                                        }
                                        None => {
                                            ui.small(
                                                egui::RichText::new(
                                                    "not saved to disk — this bake will be gone \
                                                     when the scene is closed",
                                                )
                                                .color(egui::Color32::from_rgb(230, 180, 90)),
                                            );
                                        }
                                    }
                                    // More than one island is worth seeing rather than
                                    // finding out about when a character will not go
                                    // somewhere: it is usually a door nobody fits through.
                                    if nav.regions > 1 {
                                        ui.small(format!(
                                            "{} separate areas — a character cannot walk \
                                             between them.",
                                            nav.regions
                                        ));
                                    }
                                    if nav.stale {
                                        ui.small(
                                            egui::RichText::new(
                                                "the settings have changed since this was baked",
                                            )
                                            .color(egui::Color32::from_rgb(230, 180, 90)),
                                        );
                                    }
                                    // The box was smaller than the level. Said
                                    // here as well as in the Console, because
                                    // this is the panel somebody opens when a
                                    // character will not walk somewhere, and a
                                    // bake of one corner of the map looks
                                    // exactly like a bake of the map.
                                    if let Some(missed) = nav.coverage.as_deref() {
                                        ui.add_space(2.0);
                                        ui.small(
                                            egui::RichText::new(missed)
                                                .color(egui::Color32::from_rgb(230, 180, 90)),
                                        );
                                    }
                                }
                            }
                            Matter::NavLink {
                                id: _,
                                to,
                                bidirectional,
                                cost,
                                area,
                                duration,
                                enabled,
                            } => {
                                if crate::responsive::check(ui, enabled, "this way is open").changed() {
                                    cmd.inspector_changed = true;
                                }
                                ui.small(
                                    "A way across that is not walking: a ladder, a jump down, a \
                                     vault, a door. This node is one end; the offset below is \
                                     the other. Both ends have to land on the navmesh.",
                                );
                                ui.separator();
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("far end");
                                    for (i, axis) in ["x", "y", "z"].iter().enumerate() {
                                        cmd.inspector_changed |= ui
                                            .add(
                                                egui::DragValue::new(&mut to[i])
                                                    .speed(0.1)
                                                    .prefix(format!("{axis} ")),
                                            )
                                            .changed();
                                    }
                                })
                                .response
                                .on_hover_text(
                                    "Where it comes out, measured in this node's own space — so \
                                     a link inside a prefab turns and scales with the prefab.",
                                );
                                if crate::responsive::check(ui, bidirectional, "can be crossed both ways")
                                    .on_hover_text(
                                        "A ladder can. A jump down cannot, and making one \
                                         two-way is a character walking up a cliff.",
                                    )
                                    .changed()
                                {
                                    cmd.inspector_changed = true;
                                }
                                cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(cost, 0.0..=100.0)
                                            .logarithmic(true)
                                            .text("cost"),
                                    )
                                    .on_hover_text(
                                        "What crossing costs the router, in metres of ordinary \
                                         walking. Raise it to make this a last resort; lower it \
                                         to make it a shortcut.",
                                    )
                                    .changed();
                                cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(duration, 0.0..=10.0)
                                            .suffix(" s")
                                            .text("crossing takes"),
                                    )
                                    .on_hover_text(
                                        "How long an agent spends on it. 0 means at walking \
                                         speed, which is right for a vault and wrong for a lift.",
                                    )
                                    .changed();
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("area");
                                    cmd.inspector_changed |=
                                        ui.text_edit_singleline(area).changed();
                                })
                                .response
                                .on_hover_text(
                                    "Optional. Name a nav area and one exclusion can rule out \
                                     every link like this — every jump in the level, say — \
                                     rather than one per link.",
                                );
                                ui.separator();
                                ui.small(
                                    "In a script: agent.link is this link's name while it is \
                                     being crossed, and agent.linkProgress runs 0 to 1 — which \
                                     is what a climb animation is driven by. nav.link(name, \
                                     false) shuts it.",
                                );
                            }
                            Matter::NavArea { half_extents, area, cost, blocks, enabled } => {
                                if crate::responsive::check(ui, enabled, "this volume counts").changed() {
                                    cmd.inspector_changed = true;
                                }
                                ui.small(
                                    "Changes what the ground inside it means — either it costs \
                                     more to cross, or it is not walkable at all.",
                                );
                                ui.separator();
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("size");
                                    for (i, axis) in ["x", "y", "z"].iter().enumerate() {
                                        let mut full = half_extents[i] * 2.0;
                                        if ui
                                            .add(
                                                egui::DragValue::new(&mut full)
                                                    .speed(0.25)
                                                    .range(0.1..=100000.0)
                                                    .prefix(format!("{axis} ")),
                                            )
                                            .changed()
                                        {
                                            half_extents[i] = full * 0.5;
                                            cmd.inspector_changed = true;
                                        }
                                    }
                                });
                                if crate::responsive::check(ui, blocks, "carve this out of the navmesh")
                                    .on_hover_text(
                                        "Nothing walks here, whatever it thinks of the ground. \
                                         The answer to \"keep out of this room\" that does not \
                                         involve an invisible wall nobody remembers building.",
                                    )
                                    .changed()
                                {
                                    cmd.inspector_changed = true;
                                }
                                if !*blocks {
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label("area");
                                        cmd.inspector_changed |=
                                            ui.text_edit_singleline(area).changed();
                                    })
                                    .response
                                    .on_hover_text(
                                        "What this ground is called — water, mud, road, danger. \
                                         The name is what scripts ask for, so two volumes with \
                                         the same name are the same kind of ground.",
                                    );
                                    cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(cost, 0.0..=50.0)
                                                .logarithmic(true)
                                                .text("costs"),
                                        )
                                        .on_hover_text(
                                            "How expensive a metre of it is next to ordinary \
                                             ground. Above 1 is walked round when there is a way \
                                             round; below 1 is sought out, which is how a road \
                                             works.",
                                        )
                                        .changed();
                                    ui.small(
                                        "One character can disagree: nav.agent(node, { filter = \
                                         { avoid = {\"water\"}, cost = { mud = 0.5 } } }).",
                                    );
                                }
                                ui.separator();
                                ui.small(
                                    "Bake the navmesh again after moving this — a volume is \
                                     read when the bake runs, not while the game is playing.",
                                );
                            }
                            Matter::LightProbes {
                                half_extents,
                                spacing,
                                enabled,
                                intensity,
                                bounces,
                                quality,
                                leak,
                                normal_bias,
                                exclude_layers,
                            } => {
                                let gi = self.gi;
                                if crate::responsive::check(ui, enabled, "light this scene").changed() {
                                    cmd.inspector_changed = true;
                                    cmd.gi_changed = true;
                                }
                                ui.small(
                                    "Baked bounce light. Inside this box the scene's flat ambient \
                                     is replaced by what the surfaces around it actually reflect.",
                                );
                                ui.separator();

                                // ---- the box ------------------------------------
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("size");
                                    for (i, axis) in ["x", "y", "z"].iter().enumerate() {
                                        let mut full = half_extents[i] * 2.0;
                                        let r = ui.add(
                                            egui::DragValue::new(&mut full)
                                                .speed(0.25)
                                                .range(0.5..=4000.0)
                                                .prefix(format!("{axis} ")),
                                        );
                                        if r.changed() {
                                            half_extents[i] = full * 0.5;
                                            cmd.inspector_changed = true;
                                            cmd.gi_changed = true;
                                        }
                                    }
                                })
                                .response
                                .on_hover_text(
                                    "The volume's full size in world units, before the node's \
                                     scale. Move the node to move the box.",
                                );
                                if crate::responsive::slider(ui, egui::Slider::new(spacing, 0.25..=16.0)
                                            .logarithmic(true)
                                            .text("probe spacing"),
                                    )
                                    .on_hover_text(
                                        "World units between probes. This is the resolution of \
                                         the bounce: it cannot represent a shadow sharper than \
                                         one cell.",
                                    )
                                    .changed()
                                {
                                    cmd.inspector_changed = true;
                                    cmd.gi_changed = true;
                                }
                                let planned = gi.planned_count();
                                ui.small(format!(
                                    "{}×{}×{} = {planned} probes  ·  {} renders per bounce",
                                    gi.planned[0],
                                    gi.planned[1],
                                    gi.planned[2],
                                    planned * 6
                                ));

                                // ---- the bake -----------------------------------
                                ui.separator();
                                if gi.baking {
                                    ui.add(
                                        egui::ProgressBar::new(gi.progress).text(format!(
                                            "baking — bounce {}/{}  ·  {:.0}s",
                                            gi.bounce, gi.bounces, gi.seconds
                                        )),
                                    );
                                    if ui.button("✖  Cancel").clicked() {
                                        cmd.gi_cancel = true;
                                    }
                                } else {
                                    ui.horizontal_wrapped(|ui| {
                                        if ui
                                            .button("☀  Bake")
                                            .on_hover_text(
                                                "Render the scene from every probe and keep the \
                                                 light. Saved next to the scene as a .fgi.",
                                            )
                                            .clicked()
                                        {
                                            cmd.gi_bake = true;
                                        }
                                        if gi.baked_probes > 0
                                            && ui
                                                .button("🗑  Clear")
                                                .on_hover_text("Throw the bake away.")
                                                .clicked()
                                        {
                                            cmd.gi_clear = true;
                                        }
                                    });
                                    if gi.baked_probes == 0 {
                                        ui.small("no bake yet — this volume lights nothing.");
                                    } else {
                                        ui.small(format!(
                                            "baked: {} probes, {} bounce{}",
                                            gi.baked_probes,
                                            gi.baked_bounces,
                                            if gi.baked_bounces == 1 { "" } else { "s" }
                                        ));
                                    }
                                    // Said plainly rather than by going dark: a
                                    // volume you just resized is still lit by the
                                    // old data, and that is a choice, not a bug.
                                    if gi.stale {
                                        ui.colored_label(
                                            egui::Color32::from_rgb(220, 180, 90),
                                            "⚠ the box changed since this was baked — \
                                             still using the old light",
                                        );
                                    }
                                }

                                // ---- how it is baked ----------------------------
                                ui.separator();
                                let mut b = *bounces;
                                if crate::responsive::slider(ui, egui::Slider::new(&mut b, 1..=4).text("bounces"))
                                    .on_hover_text(
                                        "1 is light coming off surfaces once — the difference \
                                         between a black corner and a lit one. Each extra bounce \
                                         re-renders every probe.",
                                    )
                                    .changed()
                                {
                                    *bounces = b;
                                    cmd.inspector_changed = true;
                                }
                                let mut q = *quality;
                                if crate::responsive::slider(ui, egui::Slider::new(&mut q, 8..=64)
                                            .step_by(8.0)
                                            .text("bake detail"),
                                    )
                                    .on_hover_text(
                                        "Pixels per cube face. Higher resolves small bright \
                                         things — a lamp, a window — and does not change how \
                                         bright the result is.",
                                    )
                                    .changed()
                                {
                                    *quality = q;
                                    cmd.inspector_changed = true;
                                }
                                let names: Vec<String> = self.layer_names.to_vec();
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("skip layers");
                                    egui::ComboBox::from_id_salt("gi_skip")
                                        .width(crate::responsive::fit_here(ui, 220.0))
                .wrap_mode(egui::TextWrapMode::Truncate)
                                        .selected_text(if exclude_layers.is_empty() {
                                            "none".to_string()
                                        } else {
                                            exclude_layers.join(", ")
                                        })
                                        .show_ui(ui, |ui| {
                                            for n in &names {
                                                let mut on = exclude_layers.contains(n);
                                                if crate::responsive::check(ui, &mut on, n).changed() {
                                                    if on {
                                                        exclude_layers.push(n.clone());
                                                    } else {
                                                        exclude_layers.retain(|x| x != n);
                                                    }
                                                    cmd.inspector_changed = true;
                                                }
                                            }
                                        });
                                })
                                .response
                                .on_hover_text(
                                    "Anything that moves — a character, a door, a lift — should \
                                     not be baked into the light it happens to be standing in.",
                                );

                                // ---- how it is applied --------------------------
                                ui.separator();
                                if crate::responsive::slider(ui, egui::Slider::new(intensity, 0.0..=4.0).text("intensity"))
                                    .on_hover_text(
                                        "1 is the light as measured. Past that is a look, not a \
                                         mistake. Changing this does not need a re-bake.",
                                    )
                                    .changed()
                                {
                                    cmd.inspector_changed = true;
                                    cmd.gi_changed = true;
                                }
                                if crate::responsive::slider(ui, egui::Slider::new(leak, 0.0..=3.0).text("leak rejection"))
                                    .on_hover_text(
                                        "Throws away probes buried in geometry, so the lit room \
                                         next door stops glowing through the wall. Costs some \
                                         bounce in tight spaces. 0 = off.",
                                    )
                                    .changed()
                                {
                                    cmd.inspector_changed = true;
                                    cmd.gi_changed = true;
                                }
                                if crate::responsive::slider(ui, egui::Slider::new(normal_bias, 0.0..=2.0)
                                            .text("surface offset"),
                                    )
                                    .on_hover_text(
                                        "How far a surface steps along its own normal before \
                                         looking the light up, in cells. Too little leaks at \
                                         corners; too much drags light around them.",
                                    )
                                    .changed()
                                {
                                    cmd.inspector_changed = true;
                                    cmd.gi_changed = true;
                                }

                                // ---- looking at it ------------------------------
                                ui.separator();
                                let mut show_only = gi.show_only;
                                if crate::responsive::check(ui, &mut show_only, "show only the bounce")
                                    .on_hover_text(
                                        "Every direct light off, so what is left on screen is \
                                         exactly what was baked. The fastest way to tell a dark \
                                         bake from a dark scene.",
                                    )
                                    .changed()
                                {
                                    cmd.gi_show_only = Some(show_only);
                                }
                                let mut show_probes = gi.show_probes;
                                if crate::responsive::check(ui, &mut show_probes, "show the probes")
                                    .on_hover_text(
                                        "Draw each probe in the colour it baked. A grid that is \
                                         too coarse, or a row of probes buried in the floor, is \
                                         invisible in the final picture and obvious here.",
                                    )
                                    .changed()
                                {
                                    cmd.gi_show_probes = Some(show_probes);
                                }
                            }
                            Matter::ReflectionProbe { half_extents, enabled, intensity, fade } => {
                                if crate::responsive::check(ui, enabled, "reflect this room").changed() {
                                    cmd.inspector_changed = true;
                                }
                                ui.small(
                                    "What reflective surfaces inside this box show when what \
                                     they are reflecting is not on screen. Without one they show \
                                     the sky — daylight, indoors, through the ceiling.",
                                );
                                ui.separator();

                                // ---- the box ------------------------------------
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("size");
                                    for (i, axis) in ["x", "y", "z"].iter().enumerate() {
                                        let mut full = half_extents[i] * 2.0;
                                        let r = ui.add(
                                            egui::DragValue::new(&mut full)
                                                .speed(0.25)
                                                .range(0.5..=4000.0)
                                                .prefix(format!("{axis} ")),
                                        );
                                        if r.changed() {
                                            half_extents[i] = full * 0.5;
                                            cmd.inspector_changed = true;
                                        }
                                    }
                                })
                                .response
                                .on_hover_text(
                                    "The room, in world units. This decides which surfaces the \
                                     probe covers AND where the reflection lands: sized to the \
                                     walls, a reflected wall sits on the wall instead of \
                                     sliding as the camera moves.",
                                );
                                if crate::responsive::slider(ui, egui::Slider::new(fade, 0.0..=20.0).text("edge fade"))
                                    .on_hover_text(
                                        "How far outside the box the room's reflection gives way \
                                         to the sky, in world units. A doorway wants a metre or \
                                         two so walking out does not switch in one step.",
                                    )
                                    .changed()
                                {
                                    cmd.inspector_changed = true;
                                }
                                if crate::responsive::slider(ui, egui::Slider::new(intensity, 0.0..=4.0).text("strength"))
                                    .on_hover_text(
                                        "How much of the capture to apply. 1 is what was \
                                         measured; this is the artistic knob for a room that \
                                         reads too busy or too dim in the reflections.",
                                    )
                                    .changed()
                                {
                                    cmd.inspector_changed = true;
                                }

                                // ---- the capture --------------------------------
                                ui.separator();
                                ui.small(
                                    "Captured when the scene loads and whenever the probe is \
                                     moved or resized. Nothing is saved to disk, so a capture \
                                     cannot go stale in a file.",
                                );
                                if ui
                                    .button("recapture")
                                    .on_hover_text(
                                        "Take it again now — after relighting the room, or \
                                         moving the furniture in it.",
                                    )
                                    .clicked()
                                {
                                    cmd.recapture_probes = true;
                                }
                            }
                            Matter::PostProcess {
                                tonemap,
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
                                posterize_chroma,
                                exposure,
                                contrast,
                                saturation,
                                temperature,
                                tint,
                                lift,
                                grade_gamma,
                                gain,
                                aberration,
                                distortion,
                                sharpen,
                                denoise,
                                grain,
                                grain_size,
                                dof_focus,
                                dof_range,
                                dof_near_range,
                                dof_max_blur,
                                dof_blades,
                                dof_blade_rotation,
                                dof_highlight,
                                dof_quality,
                                motion_blur,
                                motion_samples,
                                dof_show_focus,
                                dof_focus_node,
                                screen_shaders,
                            } => {
                                use floptle_core::AoMode;
                                ui.label("post processing");
                                ui.small("this scene's full-screen effect chain — every scene has its own (the settings travel with the scene, not the project)");
                                cmd.inspector_changed |= crate::responsive::check(ui, enabled, "enabled")
                                    .on_hover_text("master switch for the whole chain")
                                    .changed();
                                ui.add_enabled_ui(*enabled, |ui| {
                                    ui.separator();
                                    ui.label("Ambient occlusion");
                                    ui.horizontal_wrapped(|ui| {
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
                                        cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(ao_strength, 0.0..=1.0).text("strength"))
                                            .changed();
                                        cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(ao_radius, 0.05..=5.0).logarithmic(true).text("radius (m)"))
                                            .changed();
                                    }
                                    ui.separator();
                                    cmd.inspector_changed |= crate::responsive::check(ui, bloom, "Bloom").changed();
                                    if *bloom {
                                        cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(bloom_threshold, 0.0..=2.0).text("threshold"))
                                            .changed();
                                        cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(bloom_intensity, 0.0..=2.0).text("intensity"))
                                            .changed();
                                    }
                                    cmd.inspector_changed |= crate::responsive::check(ui, vignette, "Vignette").changed();
                                    if *vignette {
                                        cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(vignette_strength, 0.0..=1.0).text("strength"))
                                            .changed();
                                        cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(vignette_radius, 0.3..=1.0).text("radius"))
                                            .changed();
                                    }
                                    // Posterize — crush the ART to a limited palette. It runs
                                    // before the 2D light rather than at the end of the frame
                                    // (`floptle/0127`), which is why the tooltip says palette.
                                    ui.separator();
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label("Posterize")
                                            .on_hover_text(
                                                "reduce your ART to a fixed number of levels per channel — a \
                                                 limited-palette / banded retro look. It quantises the palette \
                                                 only: 2D lights, the vignette, bloom and ambient occlusion are \
                                                 applied on top and stay smooth.",
                                            );
                                        let plabel = match *posterize_bands {
                                            0 | 1 => "off".to_string(),
                                            n => format!("{n} levels"),
                                        };
                                        egui::ComboBox::from_id_salt("posterize_bands")
                                            .width(crate::responsive::fit_here(ui, 220.0))
                .wrap_mode(egui::TextWrapMode::Truncate)
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
                                        cmd.inspector_changed |= crate::responsive::check(ui, posterize_dither, "dither the bands")
                                            .on_hover_text(
                                                "ordered dither, so a gradient in your ART stipples between two \
                                                 levels instead of hard-stepping — a painted sky, a soft-edged \
                                                 sprite. It has no effect on lighting.",
                                            )
                                            .changed();
                                        cmd.inspector_changed |= crate::responsive::check(ui, posterize_chroma, "step brightness, keep colour")
                                            .on_hover_text(
                                                "off — the default — steps each colour channel on its own, which is a real \
                                                 look and what every project built before now is made of. It is often not \
                                                 what warm ART wants: a sunset or a torch-lit wall crosses each channel's \
                                                 boundary at a different value, so it steps through colours nobody chose. \
                                                 On, the step happens once to brightness and the colour rides along — a grey \
                                                 pixel comes out identical either way.",
                                            )
                                            .changed();
                                    });
                                });

                                // ---- the look chain -------------------------
                                //
                                // One collapsing section per effect, each with
                                // its own reset, because a grade you cannot get
                                // back to neutral is a grade you stop touching.
                                // Every heading says what OFF is, so "is this
                                // doing anything" is answerable at a glance.
                                let acc = egui::Color32::from_rgb(255, 200, 80);

                                // Tonemap first, and on its own, because it is
                                // not one effect among the others: it is how the
                                // scene's light reaches the display at all. The
                                // grade below it is working in the range this
                                // choice defines.
                                ui.separator();
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("tonemap").on_hover_text(
                                        "The scene is lit in real, unbounded light — a lamp can \
                                         be ten times brighter than white. A screen stops at \
                                         white. This chooses how to get from one to the other.\n\n\
                                         Doing nothing is a choice too: each colour channel \
                                         clips on its own, so a very bright colour slides toward \
                                         white through whatever hue clips last. That is why \
                                         blown highlights can go strange colours.",
                                    );
                                    let names = [
                                        ("clip", "clip — clamp each channel (what 2D and pixel art want)"),
                                        ("Reinhard", "Reinhard — never clips, everything bright washes to grey"),
                                        ("ACES", "ACES — filmic: crushed shadows, long warm highlight roll-off"),
                                        ("AgX", "AgX — bright colours whiten the way film does, instead of \
                                                 hitting a flat ceiling of their own hue"),
                                    ];
                                    let cur = (*tonemap as usize).min(3);
                                    egui::ComboBox::from_id_salt("pp_tonemap")
                                        .width(crate::responsive::fit_here(ui, 220.0))
                .wrap_mode(egui::TextWrapMode::Truncate)
                                        .selected_text(names[cur].0)
                                        .width(160.0)
                                        .show_ui(ui, |ui| {
                                            for (i, (short, long)) in names.iter().enumerate() {
                                                if ui
                                                    .selectable_label(cur == i, *short)
                                                    .on_hover_text(*long)
                                                    .clicked()
                                                {
                                                    *tonemap = i as u32;
                                                    cmd.inspector_changed = true;
                                                }
                                            }
                                        });
                                });
                                if *tonemap == 0 {
                                    ui.small(
                                        egui::RichText::new(
                                            "anything brighter than white is clipped — try AgX \
                                             if bright lights look like flat blocks of colour",
                                        )
                                        .small()
                                        .color(ui.visuals().weak_text_color()),
                                    );
                                }

                                // ---- the scene's own screen shaders ---------
                                //
                                // Placed after the tonemap and before the grade
                                // because that is where they RUN, and a panel
                                // that lists effects in an order the frame does
                                // not follow is a panel that teaches the wrong
                                // thing.
                                ui.separator();
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("screen shaders");
                                    ui.small(
                                        egui::RichText::new(format!(
                                            "{} pass{}",
                                            screen_shaders.len(),
                                            if screen_shaders.len() == 1 { "" } else { "es" }
                                        ))
                                        .color(ui.visuals().weak_text_color()),
                                    );
                                });
                                ui.small(
                                    "full-screen passes you wrote — a `stage post` .flsl gets the \
                                     finished frame plus its depth and normals, and returns a new \
                                     colour. They run in this order, over the picture, before the \
                                     grade and the lens below.",
                                );
                                {
                                    let mut remove: Option<usize> = None;
                                    let mut swap: Option<(usize, usize)> = None;
                                    let n = screen_shaders.len();
                                    for (i, pass) in screen_shaders.iter_mut().enumerate() {
                                        let name = Path::new(&pass.shader)
                                            .file_name()
                                            .map(|s| s.to_string_lossy().to_string())
                                            .unwrap_or_else(|| pass.shader.clone());
                                        let entry = self.post_flsl_cache.get(&pass.shader);
                                        let err = entry.and_then(|e| e.error.as_deref());
                                        crate::responsive::group(ui, |ui| {
                                            ui.horizontal_wrapped(|ui| {
                                                cmd.inspector_changed |= crate::responsive::check(ui, &mut pass.enabled, "")
                                                    .on_hover_text(
                                                        "off keeps the pass and its settings \
                                                         without running it",
                                                    )
                                                    .changed();
                                                ui.label(egui::RichText::new(&name).strong());
                                                ui.with_layout(
                                                    egui::Layout::right_to_left(egui::Align::Center),
                                                    |ui| {
                                                        if ui
                                                            .button("✖")
                                                            .on_hover_text("remove this pass")
                                                            .clicked()
                                                        {
                                                            remove = Some(i);
                                                        }
                                                        // Straight to the graph, the
                                                        // same door a Material's
                                                        // shader row opens — a pass
                                                        // you can add and tune but
                                                        // not open is a pass you
                                                        // hunt for in the Assets
                                                        // panel every time.
                                                        if ui
                                                            .button("◈")
                                                            .on_hover_text("edit this shader in the ◈ Shaders graph")
                                                            .clicked()
                                                        {
                                                            cmd.open_shader_graph =
                                                                Some(pass.shader.clone());
                                                        }
                                                        if ui
                                                            .add_enabled(
                                                                i + 1 < n,
                                                                egui::Button::new("▼"),
                                                            )
                                                            .on_hover_text("run later")
                                                            .clicked()
                                                        {
                                                            swap = Some((i, i + 1));
                                                        }
                                                        if ui
                                                            .add_enabled(i > 0, egui::Button::new("▲"))
                                                            .on_hover_text("run earlier")
                                                            .clicked()
                                                        {
                                                            swap = Some((i, i - 1));
                                                        }
                                                    },
                                                );
                                            });
                                            match (err, entry.and_then(|e| e.compiled.as_ref())) {
                                                (Some(msg), _) => {
                                                    ui.small(
                                                        egui::RichText::new(format!("◈ {msg}"))
                                                            .color(egui::Color32::from_rgb(
                                                                255, 120, 110,
                                                            )),
                                                    );
                                                }
                                                (None, None) => {
                                                    ui.small("(compiling — its knobs appear here)");
                                                }
                                                (None, Some(_)) => {}
                                            }
                                            // Knobs from the compiled shader's own schema. Shown
                                            // even when the newest edit failed, because they are
                                            // still driving the last good pipeline.
                                            if let Some((compiled, _)) =
                                                entry.and_then(|e| e.compiled.as_ref())
                                                && !compiled.uniforms.is_empty()
                                            {
                                                crate::responsive::grid(ui, ("pp_shader_rows", i), |ui| {
                                                        if shader_uniform_rows(
                                                            ui,
                                                            &compiled.uniforms,
                                                            &mut pass.params,
                                                        ) {
                                                            cmd.inspector_changed = true;
                                                        }
                                                    });
                                                if !pass.params.is_empty()
                                                    && ui
                                                        .button("Reset knobs")
                                                        .on_hover_text(
                                                            "back to the shader's own defaults",
                                                        )
                                                        .clicked()
                                                {
                                                    pass.params.clear();
                                                    cmd.inspector_changed = true;
                                                }
                                            }
                                        });
                                    }
                                    if let Some((a, b)) = swap {
                                        screen_shaders.swap(a, b);
                                        cmd.inspector_changed = true;
                                    }
                                    if let Some(i) = remove {
                                        screen_shaders.remove(i);
                                        cmd.inspector_changed = true;
                                    }
                                    ui.horizontal_wrapped(|ui| {
                                        if let Some(pick) = crate::ui_widgets::asset_picker(
                                            ui,
                                            egui::Id::new("pp-add-screen-shader"),
                                            self.project_root,
                                            "+ Add screen shader",
                                            None,
                                            self.asset_tree,
                                            crate::assets::is_shader,
                                            200.0,
                                        ) && let Some(path) = pick
                                        {
                                            screen_shaders
                                                .push(floptle_core::ScreenShader::new(path));
                                            cmd.inspector_changed = true;
                                        }
                                        ui.small(
                                            egui::RichText::new(
                                                "try shaders/examples/inkOutline.flsl",
                                            )
                                            .color(ui.visuals().weak_text_color()),
                                        );
                                    });
                                }

                                ui.separator();
                                ui.label("colour grade");
                                {
                                    let neutral = *exposure == 0.0
                                        && *contrast == 1.0
                                        && *saturation == 1.0
                                        && *temperature == 0.0
                                        && *tint == 0.0
                                        && *lift == 0.0
                                        && *grade_gamma == 1.0
                                        && *gain == 1.0;
                                    ui.horizontal_wrapped(|ui| {
                                        ui.small(if neutral {
                                            "neutral — no pass runs"
                                        } else {
                                            "grading"
                                        });
                                        if !neutral && ui.small_button("reset").clicked() {
                                            *exposure = 0.0;
                                            *contrast = 1.0;
                                            *saturation = 1.0;
                                            *temperature = 0.0;
                                            *tint = 0.0;
                                            *lift = 0.0;
                                            *grade_gamma = 1.0;
                                            *gain = 1.0;
                                            cmd.inspector_changed = true;
                                        }
                                    });
                                    cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(exposure, -4.0..=4.0).text("exposure"))
                                        .on_hover_text(
                                            "in STOPS: +1 is twice the light. The unit a camera and a \
                                             renderer already share — it keeps meaning the same thing \
                                             when the scene's brightness changes.",
                                        )
                                        .changed();
                                    cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(contrast, 0.0..=3.0).text("contrast"))
                                        .on_hover_text("pivots on 18% grey, so adding contrast doesn't also darken everything")
                                        .changed();
                                    cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(saturation, 0.0..=3.0).text("saturation"))
                                        .on_hover_text("0 = greyscale, 1 = untouched. Brightness is preserved.")
                                        .changed();
                                    cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(temperature, -1.0..=1.0).text("temperature"))
                                        .on_hover_text("cool (−) ↔ warm (+)")
                                        .changed();
                                    cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(tint, -1.0..=1.0).text("tint"))
                                        .on_hover_text(
                                            "green (−) ↔ magenta (+) — the axis temperature can't reach, \
                                             and the one that fixes a scene that has gone subtly sickly",
                                        )
                                        .changed();
                                    ui.small("shadows / midtones / highlights");
                                    cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(lift, -0.5..=0.5).text("lift"))
                                        .on_hover_text("raise or crush the black floor — a lifted black is the film look")
                                        .changed();
                                    cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(grade_gamma, 0.2..=3.0).text("gamma"))
                                        .on_hover_text("bend the midtones without moving black or white")
                                        .changed();
                                    cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(gain, 0.0..=3.0).text("gain"))
                                        .on_hover_text("scale the highlights")
                                        .changed();
                                }

                                ui.separator();
                                ui.label("lens");
                                cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(aberration, 0.0..=2.0).text("chromatic aberration"))
                                    .on_hover_text(
                                        "red and blue drift apart toward the edges, the way real glass \
                                         disperses. 0 = off.",
                                    )
                                    .changed();
                                cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(distortion, -0.5..=0.5).text("distortion"))
                                    .on_hover_text(
                                        "positive barrels (fisheye), negative pincushions. The corners go \
                                         BLACK rather than smearing the edge pixel outward — a bent frame \
                                         genuinely has no picture out there.",
                                    )
                                    .changed();
                                if *aberration == 0.0 && *distortion == 0.0 {
                                    ui.small("both at 0 — no lens pass runs");
                                }

                                ui.separator();
                                ui.label("detail");
                                cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(sharpen, 0.0..=2.0).text("sharpen"))
                                    .on_hover_text(
                                        "unsharp mask, clamped to the local neighbourhood so edges get \
                                         crisper without growing a bright halo. 0 = off.",
                                    )
                                    .changed();
                                cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(denoise, 0.0..=1.0).text("denoise"))
                                    .on_hover_text(
                                        "bilateral: averages within a flat region and refuses to average \
                                         across an edge, which is the difference between removing noise \
                                         and removing detail. Runs FIRST in the chain, on the raw frame. \
                                         0 = off.",
                                    )
                                    .changed();
                                if *sharpen > 0.0 && *denoise > 0.0 {
                                    ui.small("denoise runs first, then sharpen — the useful order");
                                }

                                ui.separator();
                                ui.label("film grain");
                                cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(grain, 0.0..=1.0).text("amount"))
                                    .on_hover_text(
                                        "multiplicative and strongest in the MIDTONES, the way emulsion \
                                         responds — additive grain lifts every shadow into grey mud, \
                                         which is the tell of a cheap filter. Applied last, so nothing \
                                         downstream turns it into crawling static. 0 = off.",
                                    )
                                    .changed();
                                ui.add_enabled_ui(*grain > 0.0, |ui| {
                                    cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(grain_size, 1.0..=8.0).text("size"))
                                        .on_hover_text(
                                            "grain cell in pixels. 1 is per-pixel — which under a retro \
                                             upscale is invisible, then suddenly a flat shimmer. 2–4 is \
                                             what reads as film.",
                                        )
                                        .changed();
                                });

                                ui.separator();
                                ui.label("depth of field");
                                cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(dof_focus, 0.0..=200.0)
                                            .logarithmic(true)
                                            .text("focus distance"),
                                    )
                                    .on_hover_text("world units from the camera that are sharp. 0 = off.")
                                    .changed();
                                // Focus on a NODE instead of a number: the focus
                                // distance becomes the camera's distance to it,
                                // every frame. This is what a rack focus is made
                                // of, and by hand it means a script measuring a
                                // distance the engine already knows.
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("follow");
                                    let cur = dof_focus_node.clone();
                                    let label = if cur.is_empty() {
                                        "(a fixed distance)".to_string()
                                    } else {
                                        cur.clone()
                                    };
                                    egui::ComboBox::from_id_salt("pp_dof_follow")
                                        .width(crate::responsive::fit_here(ui, 220.0))
                .wrap_mode(egui::TextWrapMode::Truncate)
                                        .selected_text(label)
                                        .width(170.0)
                                        .show_ui(ui, |ui| {
                                            if ui
                                                .selectable_label(cur.is_empty(), "(a fixed distance)")
                                                .clicked()
                                                && !cur.is_empty()
                                            {
                                                dof_focus_node.clear();
                                                cmd.inspector_changed = true;
                                            }
                                            for (_, name) in self.entity_names {
                                                if ui.selectable_label(cur == *name, name).clicked()
                                                    && cur != *name
                                                {
                                                    *dof_focus_node = name.clone();
                                                    cmd.inspector_changed = true;
                                                }
                                            }
                                        });
                                })
                                .response
                                .on_hover_text(
                                    "keep this node in focus — the focus distance becomes the \
                                     camera's distance to it, measured every frame and per \
                                     viewport, so the Scene view shows its own focus while you \
                                     fly around. A name that matches nothing falls back to the \
                                     slider above rather than to zero.",
                                );
                                if !dof_focus_node.is_empty()
                                    && !self.entity_names.iter().any(|(_, n)| n == dof_focus_node)
                                {
                                    ui.colored_label(
                                        acc,
                                        format!(
                                            "⚠ no node named \"{dof_focus_node}\" in this scene — \
                                             using the focus distance above"
                                        ),
                                    );
                                }
                                ui.add_enabled_ui(*dof_focus > 0.0 || !dof_focus_node.is_empty(), |ui| {
                                    cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(dof_range, 0.1..=100.0).logarithmic(true).text("far range"))
                                        .on_hover_text("how far BEYOND the focus distance stays sharp")
                                        .changed();
                                    let mut near = *dof_near_range;
                                    let auto = near <= 0.0;
                                    if auto {
                                        near = *dof_range * 0.5;
                                    }
                                    let r = crate::responsive::slider(ui, egui::Slider::new(&mut near, 0.05..=100.0).logarithmic(true).text("near range"))
                                        .on_hover_text(
                                            "how far IN FRONT of it stays sharp. A lens goes soft \
                                             on the near side much sooner than on the far side, \
                                             which is why these are two numbers: a portrait wants \
                                             the foreground gone and the background readable.",
                                        );
                                    if r.changed() {
                                        *dof_near_range = near;
                                        cmd.inspector_changed = true;
                                    }
                                    if auto {
                                        ui.small(
                                            egui::RichText::new("near range is following the far one (half of it)")
                                                .color(ui.visuals().weak_text_color()),
                                        );
                                    } else if ui
                                        .small_button("link to far range")
                                        .on_hover_text("back to half the far range")
                                        .clicked()
                                    {
                                        *dof_near_range = 0.0;
                                        cmd.inspector_changed = true;
                                    }
                                    cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(dof_max_blur, 0.0..=16.0).text("max blur"))
                                        .on_hover_text("the widest the out-of-focus blur gets, in pixels. 0 = off.")
                                        .changed();

                                    ui.add_space(3.0);
                                    ui.small("the iris");
                                    let mut blades = *dof_blades as f32;
                                    let r = crate::responsive::slider(ui, egui::Slider::new(&mut blades, 0.0..=10.0)
                                                .step_by(1.0)
                                                .text("blades"),
                                        )
                                        .on_hover_text(
                                            "0 is a round iris. 3 and up gives the polygonal bokeh \
                                             of a real lens — six is the classic hexagon.",
                                        );
                                    if r.changed() {
                                        *dof_blades = blades.max(0.0) as u32;
                                        cmd.inspector_changed = true;
                                    }
                                    if *dof_blades >= 3 {
                                        cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(dof_blade_rotation, 0.0..=180.0)
                                                    .text("blade angle°"),
                                            )
                                            .on_hover_text("turn the polygon")
                                            .changed();
                                    }
                                    cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(dof_highlight, 0.0..=8.0).text("highlight bokeh"))
                                        .on_hover_text(
                                            "how much brighter-than-white pixels dominate the \
                                             blur. 0 averages them away into grey; turn it up and \
                                             a specular glint spreads into a visible disc. It \
                                             reads the scene's real light, so it needs something \
                                             genuinely brighter than white to work on.",
                                        )
                                        .changed();
                                    let mut q = if *dof_quality == 0 { 16.0 } else { *dof_quality as f32 };
                                    let r = crate::responsive::slider(ui, egui::Slider::new(&mut q, 4.0..=64.0).step_by(1.0).text("samples"))
                                        .on_hover_text(
                                            "taps in the blur. More is smoother bokeh and costs \
                                             linearly more; fewer is the chunky look, on purpose.",
                                        );
                                    if r.changed() {
                                        *dof_quality = q.round().clamp(4.0, 64.0) as u32;
                                        cmd.inspector_changed = true;
                                    }
                                    cmd.inspector_changed |= crate::responsive::check(ui, dof_show_focus, "show the focus band")
                                        .on_hover_text(
                                            "a tuning view: cool where the near side is going \
                                             soft, warm where the far side is, the picture itself \
                                             where it is sharp. Which half of the band a pixel is \
                                             on is the one thing you cannot read off a blurred \
                                             frame.",
                                        )
                                        .changed();
                                });
                                if *dof_show_focus {
                                    ui.colored_label(acc, "◐ showing the focus band — turn it off before you look at the art");
                                }
                                if (*dof_focus > 0.0 || !dof_focus_node.is_empty())
                                    && *dof_max_blur <= 0.0
                                {
                                    ui.colored_label(acc, "⚠ max blur is 0 — nothing will look out of focus");
                                }

                                // ---- motion blur --------------------------------
                                ui.separator();
                                ui.strong("≈ Motion blur");
                                ui.small("shows in the Game view");
                                cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(motion_blur, 0.0..=1.0).text("shutter"))
                                    .on_hover_text(
                                        "How much of the frame's camera motion is smeared. 0 is off. \
                                         0.5 is the 180° shutter a film camera has and is the one \
                                         that reads as footage; 1 leaves the shutter open for the \
                                         whole frame and is a stylistic choice.\n\nIt blurs CAMERA \
                                         motion — a pan, a whip, a dolly, a roll. Something crossing \
                                         a locked-off shot stays sharp.\n\nThe Scene view is left \
                                         alone deliberately: you have to be able to place things \
                                         while the camera is moving.",
                                    )
                                    .changed();
                                if *motion_blur > 0.0 {
                                    let mut taps =
                                        if *motion_samples == 0 { 12.0 } else { *motion_samples as f32 };
                                    if crate::responsive::slider(ui, egui::Slider::new(&mut taps, 4.0..=32.0).text("samples"))
                                        .on_hover_text(
                                            "Taps along the streak. Too few and a fast pan bands \
                                             into separate copies of the picture.",
                                        )
                                        .changed()
                                    {
                                        *motion_samples = taps.round().clamp(4.0, 32.0) as u32;
                                        cmd.inspector_changed = true;
                                    }
                                }
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
                        if crate::responsive::check(ui, &mut vis, "👁 visible")
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
                // `floptle/0110`: Stop reverts the world, so a transform typed
                // here while playing is thrown away — `push_history` no-ops
                // during Play, which also means it is not undoable and never
                // marks the scene unsaved. Nothing used to say so.
                //
                // The way out already existed and was simply never pointed at:
                // the header's … menu copies the transform, and the component
                // clipboard survives Stop — so "nudge it while watching, then
                // keep the value" is Copy values → Stop → Paste values. Saying
                // that here is worth more than a warning would be, because it
                // ends with the user keeping their work.
                if playing {
                    ui.colored_label(
                        egui::Color32::from_rgb(255, 150, 140),
                        "▶ discarded on Stop — … Copy values, Stop, then Paste to keep it",
                    )
                    .on_hover_text(
                        "Play reverts the whole world when you Stop. The … menu on \
                         this header copies the Transform to the component clipboard, \
                         which survives Stop — paste it onto the same node afterwards \
                         and it is a real, undoable, saveable edit.",
                    );
                }
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
                        ui.horizontal_wrapped(|ui| {
                            cmd.inspector_changed |= ui.add(egui::DragValue::new(&mut t.translation.x).speed(0.05).prefix("x ")).changed();
                            cmd.inspector_changed |= ui.add(egui::DragValue::new(&mut t.translation.y).speed(0.05).prefix("y ")).changed();
                            cmd.inspector_changed |= ui.add(egui::DragValue::new(&mut t.translation.z).speed(0.05).prefix("z ")).changed();
                        });
                        ui.label("rotation (deg)");
                        let (ey, ex, ez) = t.rotation.to_euler(EulerRot::YXZ);
                        let mut deg = [ey.to_degrees(), ex.to_degrees(), ez.to_degrees()];
                        let mut rot_changed = false;
                        ui.horizontal_wrapped(|ui| {
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
                        ui.horizontal_wrapped(|ui| {
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
                            let res = material_props_ui(ui, mat, self.materials, self.asset_tree, self.project_root, self.mat_name_buf, self.flsl_cache, self.sdf_cache, self.texture_settings);
                            cmd.inspector_changed |= res.changed;
                            cmd.open_shader_graph = res.open_shader.or(cmd.open_shader_graph.take());
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

                // ===== The model's OWN materials =====
                //
                // An imported model arrives with a material per part, and until
                // now the only way to see them was to select the model in the
                // Assets panel (read-only) and the only way to edit one was to
                // expand the model in the Hierarchy and find the right
                // sub-object. So "give this model a normal map" or "make this
                // model jitter" had no obvious door, and the obvious-looking one
                // — adding a Material to the node — used to flatten every part
                // to a single colour.
                //
                // Both are answered here: the whole list, on the node, editable,
                // with the model-wide button beside it.
                if let Some(Matter::Mesh { asset_path }) = world.get::<Matter>(e).cloned()
                    && let Some(parts) = self.mesh_registry.get(&asset_path).map(|a| {
                        // Collected up front: the rows below need `world` mutably,
                        // and the asset is borrowed out of the registry.
                        a.part_meta
                            .iter()
                            .enumerate()
                            .filter_map(|(i, m)| {
                                a.override_key(i).map(|k| {
                                    (k.to_string(), m.material.clone(), m.base_color, m.textured)
                                })
                            })
                            .collect::<Vec<_>>()
                    })
                    && !parts.is_empty()
                {
                    ui.separator();
                    ui.strong("◑ Model materials");
                    ui.small(
                        "what this model was imported with. Overriding one changes it for \
                         this node only — the model file is never touched.",
                    );
                    if world.get::<Material>(e).is_none() {
                        ui.horizontal_wrapped(|ui| {
                            if ui
                                .button("◑ Add Material (whole model)")
                                .on_hover_text(
                                    "one material over every part at once — the fast way to \
                                     give a whole model a normal map, a roughness, or the \
                                     retro artefacts.\n\nIts colour MULTIPLIES each part's \
                                     own, so a fresh one changes nothing until you dial \
                                     something in.",
                                )
                                .clicked()
                            {
                                cmd.add_material = Some(e);
                            }
                        });
                    }
                    // One row per sub-object, because that is what an override is
                    // keyed by. A flattened prop's object name IS its material
                    // name, so the two read the same there.
                    let mut dedup: std::collections::BTreeSet<String> = Default::default();
                    for (key, mat_name, base_color, textured) in parts {
                        if !dedup.insert(key.clone()) {
                            continue;
                        }
                        let overridden = world
                            .get::<floptle_core::ObjectMaterials>(e)
                            .is_some_and(|om| om.0.contains_key(&key));
                        let mut clear = false;
                        let mut make = false;
                        ui.horizontal_wrapped(|ui| {
                            let (rect, _) = ui
                                .allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                            ui.painter().rect_filled(
                                rect,
                                2.0,
                                egui::Color32::from_rgb(
                                    (base_color[0] * 255.0) as u8,
                                    (base_color[1] * 255.0) as u8,
                                    (base_color[2] * 255.0) as u8,
                                ),
                            );
                            ui.label(&key);
                            if mat_name != key {
                                ui.weak(format!("· {mat_name}"));
                            }
                            if textured {
                                ui.small("🖼");
                            }
                            if overridden {
                                clear = ui
                                    .small_button("🗑")
                                    .on_hover_text("back to the model's own look")
                                    .clicked();
                            } else {
                                make = ui
                                    .small_button("override")
                                    .on_hover_text("give this part its own material")
                                    .clicked();
                            }
                        });
                        if make {
                            let mut om = world
                                .get::<floptle_core::ObjectMaterials>(e)
                                .cloned()
                                .unwrap_or_default();
                            // Seeded with the part's imported colour, so
                            // overriding is visibly a starting point and not a
                            // reset to white.
                            om.0.insert(key.clone(), floptle_core::Material::tinted(base_color));
                            world.insert(e, om);
                            cmd.inspector_changed = true;
                        }
                        if overridden {
                            egui::CollapsingHeader::new(crate::responsive::header_text(ui, "edit"))
                                .id_salt(("model_mat", e, &key))
                                .default_open(crate::responsive::start_open(false))
                                .show(ui, |ui| {
                                    let mut save_as = None;
                                    if let Some(mat) = world
                                        .get_mut::<floptle_core::ObjectMaterials>(e)
                                        .and_then(|om| om.0.get_mut(&key))
                                    {
                                        let res = material_props_ui(
                                            ui,
                                            mat,
                                            self.materials,
                                            self.asset_tree,
                                            self.project_root,
                                            self.mat_name_buf,
                                            self.flsl_cache,
                                            self.sdf_cache,
                                            self.texture_settings,
                                        );
                                        cmd.inspector_changed |= res.changed;
                                        cmd.open_shader_graph =
                                            res.open_shader.or(cmd.open_shader_graph.take());
                                        clear |= res.remove;
                                        if let Some(name) = res.save_as {
                                            save_as = Some((
                                                name,
                                                floptle_scene::MaterialDoc::from_material(mat),
                                            ));
                                        }
                                    }
                                    if save_as.is_some() {
                                        cmd.save_material = save_as;
                                    }
                                });
                        }
                        if clear
                            && let Some(om) = world.get_mut::<floptle_core::ObjectMaterials>(e)
                        {
                            om.0.remove(&key);
                            if om.0.is_empty() {
                                world.remove::<floptle_core::ObjectMaterials>(e);
                            }
                            cmd.inspector_changed = true;
                        }
                    }
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
                            cmd.inspector_changed |= crate::responsive::check(ui, &mut ps.play_on_start, "Play on start")
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
                            .chain(self.project.mixer.tracks.iter().map(|t| t.name.clone()))
                            .collect();
                    ui.indent("audio_props", |ui| {
                        if let Some(src) = world.get_mut::<floptle_audio::AudioSource>(e) {
                            ui.horizontal_wrapped(|ui| {
                                ui.label("Clip");
                                let sel =
                                    if src.clip.is_empty() { "(none)" } else { src.clip.as_str() };
                                if let Some(pick) = crate::ui_widgets::asset_picker(
                                    ui,
                                    egui::Id::new(("audio_clip_pick", e)),
                                    self.project_root,
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
                            cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(&mut p.volume, 0.0..=2.0).text("Volume"),
                                )
                                .changed();
                            cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(&mut p.pitch, 0.25..=4.0).text("Pitch").logarithmic(true))
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
                                    cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(&mut p.pan, -1.0..=1.0).text("Pan"))
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
                                    ui.horizontal_wrapped(|ui| {
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
                            cmd.inspector_changed |= crate::responsive::check(ui, &mut src.play_on_start, "Play on start")
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
                            ui.horizontal_wrapped(|ui| {
                                ui.label("mode");
                                let label = match rb.mode {
                                    BodyMode::Dynamic => "Dynamic",
                                    BodyMode::Kinematic => "Kinematic",
                                    BodyMode::Static => "Static",
                                };
                                egui::ComboBox::from_id_salt("rb-mode")
                                    .width(crate::responsive::fit_here(ui, 220.0))
                .wrap_mode(egui::TextWrapMode::Truncate)
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
                            ui.horizontal_wrapped(|ui| {
                                ui.label("shape");
                                egui::ComboBox::from_id_salt("rb-shape")
                                    .width(crate::responsive::fit_here(ui, 220.0))
                .wrap_mode(egui::TextWrapMode::Truncate)
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
                                ui.horizontal_wrapped(|ui| {
                                    for (i, ax) in ["x", "y", "z"].iter().enumerate() {
                                        cmd.inspector_changed |= ui
                                            .add(egui::DragValue::new(&mut rb.half_extents[i]).speed(0.02).range(0.02..=50.0).prefix(format!("{ax} ")))
                                            .changed();
                                    }
                                });
                            } else {
                                cmd.inspector_changed |=
                                    crate::responsive::slider(ui, egui::Slider::new(&mut rb.radius, 0.05..=10.0).text("radius")).changed();
                                if rb.kind == BodyKind::Capsule {
                                    cmd.inspector_changed |=
                                        crate::responsive::slider(ui, egui::Slider::new(&mut rb.height, 0.2..=20.0).text("height")).changed();
                                }
                            }
                            // Bounce/friction/gravity/locks only matter on a
                            // SIMULATED body — grey them out otherwise so the
                            // mode dropdown reads as the one switch it is.
                            let dynamic = rb.mode == BodyMode::Dynamic;
                            ui.add_enabled_ui(dynamic, |ui| {
                                let asm = crate::responsive::check(ui, &mut rb.assembly, "assembly (compound of children)")
                                    .on_hover_text(
                                        "This node roots ONE 6-DOF rigid body built from every \
                                         descendant node that has a RigidBody: each becomes an \
                                         oriented shape at its offset, weighted by its mass (this \
                                         node's own shape fields are ignored). Multi-part \
                                         vehicles, decoupling rockets, breakable structures — \
                                         drive it from Lua with assembly.forceAt / .split.",
                                    )
                                    .changed();
                                cmd.inspector_changed |= asm;
                                cmd.rebuild_physics |= asm;
                                if rb.assembly {
                                    ui.small("children with RigidBody = this vessel's parts");
                                }
                                cmd.inspector_changed |= ui
                                    .add(
                                        egui::DragValue::new(&mut rb.mass)
                                            .speed(0.05)
                                            .range(0.001..=100000.0)
                                            .prefix("mass "),
                                    )
                                    .on_hover_text(
                                        "This shape's mass share inside an assembly compound \
                                         (composed mass / center of mass / inertia). Plain \
                                         bodies ignore it.",
                                    )
                                    .changed();
                                cmd.inspector_changed |=
                                    crate::responsive::slider(ui, egui::Slider::new(&mut rb.restitution, 0.0..=1.0).text("bounce")).changed();
                                cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(&mut rb.friction, 0.0..=2.0).text("friction"))
                                    .on_hover_text(
                                        "Grip, as a coefficient. A ramp holds this body while \
                                         tan(its angle) ≤ friction — so 0 is ice, 0.3 lets go \
                                         at about 17°, 1 holds exactly 45°, and a surface \
                                         grippier than that goes above 1 (rubber on rubber is \
                                         around 1.5).\n\nIt opposes motion rather than \
                                         capping it: a shoved crate slides and then stops.",
                                    )
                                    .changed();
                                cmd.inspector_changed |= crate::responsive::slider(ui, egui::Slider::new(&mut rb.slope_limit, 0.0..=90.0)
                                            .text("slope limit °"),
                                    )
                                    .on_hover_text(
                                        "The steepest surface this body can stand on. Past it \
                                         the body is not grounded, the surface reads as \
                                         node.wallNormal instead of node.groundNormal, and it \
                                         stops holding the body up — so a character slides off \
                                         a cliff face however grippy its boots are.",
                                    )
                                    .changed();
                                cmd.inspector_changed |= crate::responsive::check(ui, &mut rb.gravity, "affected by gravity")
                                    .on_hover_text("off = floats (still collides; a script can still move it)")
                                    .changed();
                                // 2D first, and above the axis toggles, because
                                // it is the answer to the question the axis
                                // toggles make you ask. Working out that a 2D
                                // object means "freeze pos z, freeze rot x and
                                // y" is a thing you should have to do once, in
                                // the engine, not once per node.
                                cmd.inspector_changed |= crate::responsive::check(ui, &mut rb.two_d, "2D — keep it in the XY plane")
                                    .on_hover_text(
                                        "One switch for a 2D game: the body keeps its depth, \
                                         never drifts out of the layer, and still spins the \
                                         one way a flat object spins. It collides with the \
                                         same world a 3D body does — a tilemap's colliders, \
                                         a slope you drew, anything Collidable. Adds to the \
                                         freezes below rather than replacing them.",
                                    )
                                    .changed();
                                let two_d = rb.two_d;
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("freeze pos");
                                    for (i, ax) in ["x", "y", "z"].iter().enumerate() {
                                        // Z is held by 2D: show it on and say so
                                        // rather than letting the box look
                                        // unticked while the solver freezes it.
                                        let forced = two_d && i == 2;
                                        let mut v = rb.lock_pos[i] || forced;
                                        let r = ui.add_enabled(!forced, egui::Button::new(*ax).selected(v));
                                        if r.clicked() && !forced {
                                            v = !v;
                                            rb.lock_pos[i] = v;
                                            cmd.inspector_changed = true;
                                        }
                                        if forced {
                                            r.on_hover_text("held by 2D");
                                        }
                                    }
                                });
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("freeze rot");
                                    for (i, ax) in ["x", "y", "z"].iter().enumerate() {
                                        let forced = two_d && i < 2;
                                        let mut v = rb.lock_rot[i] || forced;
                                        let r = ui.add_enabled(!forced, egui::Button::new(*ax).selected(v));
                                        if r.clicked() && !forced {
                                            v = !v;
                                            rb.lock_rot[i] = v;
                                            cmd.inspector_changed = true;
                                        }
                                        if forced {
                                            r.on_hover_text("held by 2D");
                                        }
                                    }
                                });
                                cmd.inspector_changed |= crate::responsive::check(ui, &mut rb.align_up, "align to gravity")
                                    .on_hover_text(
                                        "Tilt this node so its up follows −gravity — a \
                                         character on a radial-gravity planet stands on it \
                                         (and its camera/children inherit the tilt). \
                                         Overrides freeze rot.",
                                    )
                                    .changed();
                                cmd.inspector_changed |= crate::responsive::check(ui, &mut rb.pushbox_only, "pushbox only")
                                    .on_hover_text(
                                        "The solver never resolves this body's contacts — it \
                                         integrates its velocity and nothing else: no gravity, \
                                         no depenetration, no ground detection. It stays fully \
                                         visible to raycasts and overlap queries, so it's a box \
                                         you can HIT, not a box physics moves.\n\n\
                                         This is the supported profile for ROLLBACK netcode. \
                                         The contact solver is the part least likely to agree \
                                         bit-for-bit between two machines, and a fighting game \
                                         replaces it with integer frame data anyway. Your script \
                                         owns gravity, the floor and pushout — and should move \
                                         the body through node.tickX/tickY/tickZ, not node.x.",
                                    )
                                    .changed();
                            });
                        }
                    });
                    // Trigger: the BODY becomes a sensor — it never blocks or gets
                    // blocked (and rays skip it), but overlap fires the trigger
                    // hooks. Moving pickups, sweeping zones, pass-through projectiles.
                    let mut trig = world.get::<floptle_core::Trigger>(e).is_some();
                    if crate::responsive::check(ui, &mut trig, "trigger")
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
                    if crate::responsive::check(ui, &mut casts, "casts shadows")
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
                    let (_, _, remove) = component_header(ui, "☉ Celestial Body", false, true);
                    if remove {
                        cmd.remove_celestial = Some(e);
                    }
                    ui.indent("cb_props", |ui| {
                        if let Some(cb) = world.get_mut::<floptle_core::CelestialBody>(e) {
                            let drag = |ui: &mut egui::Ui, label: &str, v: &mut f64, speed: f64, hover: &str| -> bool {
                                ui.horizontal_wrapped(|ui| {
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
                            ui.horizontal_wrapped(|ui| {
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
                            ui.horizontal_wrapped(|ui| {
                                ui.label("sky color");
                                ch |= ui
                                    .color_edit_button_rgb(&mut cb.atmo_color)
                                    .on_hover_text("the sky seen from inside the atmosphere")
                                    .changed();
                            });
                            ch |= drag(ui, "atmo height", &mut cb.atmo_height, 1.0, "shell height above the surface; the sky fades to space across it");
                            ui.horizontal_wrapped(|ui| {
                                ui.label("density");
                                ch |= crate::responsive::slider(ui, egui::Slider::new(&mut cb.atmo_density, 0.0..=1.0))
                                    .on_hover_text("how opaque the sky gets at full depth")
                                    .changed();
                            });
                            ui.horizontal_wrapped(|ui| {
                                ui.label("clouds");
                                ch |= crate::responsive::slider(ui, egui::Slider::new(&mut cb.clouds, 0.0..=1.0))
                                    .on_hover_text("cloud coverage in the atmosphere (0 = clear)")
                                    .changed();
                            });
                            ui.small("star (Lighting `stars mode` uses these as the lights):");
                            ui.horizontal_wrapped(|ui| {
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
                    if crate::Editor::ui_inspector(world, e, ui, self.asset_tree, self.project_root, self.texture_settings, self.ui_flsl_cache, self.ui_styles) {
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
                            ui.horizontal_wrapped(|ui| {
                                ui.label("mode");
                                egui::ComboBox::from_id_salt("net-mode")
                                    .width(crate::responsive::fit_here(ui, 220.0))
                .wrap_mode(egui::TextWrapMode::Truncate)
                                    .selected_text(match rep.mode {
                                        ReplicationMode::Authority => "Server authority",
                                        ReplicationMode::Predicted => "Predicted (owner)",
                                        ReplicationMode::Rollback => "Rollback (all peers)",
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
                                        cmd.inspector_changed |= ui
                                            .selectable_value(
                                                &mut rep.mode,
                                                ReplicationMode::Rollback,
                                                "Rollback (all peers)",
                                            )
                                            .on_hover_text("EVERY peer simulates this node every tick from the shared input set and re-simulates on a mispredict — for a fighting game, where the opponent's exact state matters on every frame. Its scripts need snapshot()/restore().")
                                            .changed();
                                    });
                            });
                            cmd.inspector_changed |= crate::responsive::check(ui, &mut rep.transform, "sync transform")
                                .on_hover_text("replicate position/rotation to clients")
                                .changed();
                            cmd.inspector_changed |= crate::responsive::check(ui, &mut rep.physics, "sync physics")
                                .on_hover_text("replicate velocity too — better extrapolation, required to predict a rigidbody")
                                .changed();
                            cmd.inspector_changed |= crate::responsive::check(ui, &mut rep.animator, "sync animator")
                                .on_hover_text(
                                    "replicate the Animation Controller's playback (which state + \
                                     where in it, per layer) — a few bytes per TRANSITION; every \
                                     machine samples the pose locally. Off = client-sided: each \
                                     client drives this node's animator itself",
                                )
                                .changed();
                            cmd.inspector_changed |= crate::responsive::check(ui, &mut rep.interp, "interpolate")
                                .on_hover_text("smooth remote copies between snapshots (off = snap, for teleporty things)")
                                .changed();
                            if rep.interp {
                                let mut d = rep.interp_delay as i32;
                                if crate::responsive::slider(ui, egui::Slider::new(&mut d, 0..=30).text("interp delay (ticks)"))
                                    .on_hover_text("how far behind the server remote copies render — 6 ticks ≈ 100 ms. Lower = tighter tracking (stutters under jitter/loss); higher = smoother on bad links")
                                    .changed()
                                {
                                    rep.interp_delay = d as u8;
                                    cmd.inspector_changed = true;
                                }
                            }
                            cmd.inspector_changed |= crate::responsive::check(ui, &mut rep.always_relevant, "always relevant")
                                .on_hover_text(
                                    "never interest-culled: replicated to every client wherever \
                                     they are. For the few things every player must agree on \
                                     from anywhere — the match clock, the objective, the boss. \
                                     Does nothing unless the host turned interest management on \
                                     with net.host{ interest = <metres> }",
                                )
                                .changed();
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
                    // A tilemap you PAINTED SOLID and cannot stand on. The
                    // warning existed, and it was printed to the Console at Play
                    // — which is the one moment you are looking at the game and
                    // not at the editor, and by then "I fall through the floor"
                    // already reads as a physics bug. Say it here, next to the
                    // collider section it is about, with the fix on it.
                    //
                    // Still not automatic: a solid tileset implying `Collidable`
                    // would silently switch collision on in every project that
                    // ever painted one, including the parallax backgrounds.
                    if !has_collidable
                        && world.get::<floptle_core::RigidBody>(e).is_none()
                        && let Some(Matter::Tilemap { data, tileset, .. }) = world.get::<Matter>(e)
                        && !tileset.is_empty()
                        && let Some(set) = self.tiles.get(tileset)
                        && floptle_tiles::solid_count(data, set) > 0
                    {
                        let n = floptle_tiles::solid_count(data, set);
                        ui.separator();
                        ui.colored_label(
                            egui::Color32::from_rgb(255, 200, 80),
                            format!("⚠ {n} solid squares, but nothing collides with this layer"),
                        );
                        ui.small(
                            "This tilemap's tileset marks squares solid, and the layer has no \
                             collider — so bodies fall straight through the floor you painted. \
                             Tilemaps are not collidable by default because most projects have \
                             background layers painted from the same sheet.",
                        );
                        if ui
                            .button("▦  Make this layer solid")
                            .on_hover_text("adds a Collidable component — the squares your tileset calls solid become real geometry on Play")
                            .clicked()
                        {
                            cmd.set_collidable = Some((e, true));
                        }
                    }
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
                            if crate::responsive::check(ui, &mut casts, "casts shadows")
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
                            if crate::responsive::check(ui, &mut trig, "trigger")
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

                // ===== Navmesh Exclude =====
                // A marker with nothing to configure, so the whole component is
                // its own explanation and a remove button.
                if world.get::<floptle_core::NavMeshExclude>(e).is_some() {
                    ui.separator();
                    if component_header_no_copy(ui, "⬚ Navmesh Exclude", true) {
                        cmd.set_nav_exclude = Some((e, false));
                        cmd.inspector_changed = true;
                    }
                    ui.small(
                        "kept out of every navmesh bake. Characters will not path over this \
                         node, whatever it collides with.",
                    );
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
                    ui.horizontal_wrapped(|ui| {
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
                                ui.horizontal_wrapped(|ui| {
                                    cmd.inspector_changed |= crate::responsive::check(ui, &mut inst.enabled, "").changed();
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
                                // Everything this script declares — editor
                                // buttons, then its tunables in DECLARATION
                                // order, grouped under `--@header` sections and
                                // drawn as the widget each one's annotations ask
                                // for. See `script_meta`.
                                cmd.inspector_changed |= script_tunables_ui(
                                    ui,
                                    inst,
                                    ScriptRowCtx {
                                        meta: self.script_meta.get(self.project_root, &inst.kind),
                                        ref_kinds: self.ref_kinds,
                                        node_names: &node_names,
                                        script_nodes: &script_nodes,
                                        comp_nodes: &comp_nodes,
                                        name_of: &name_of,
                                        salt: (e.index(), i),
                                    },
                                    &mut cmd.run_editor_action,
                                    e,
                                );
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

                // ===== ◈ Objects & Rig (this model's sub-objects + bones) =====
                // Shown on the model node itself: two lists (Objects = mesh sub-objects,
                // Bones = rig joints) whose entries select the same pose-able skeleton
                // node the Hierarchy tree does, plus the per-object re-parent dropdown
                // and the Mirror / flow-rig tools.
                if let Some(nodes) = bone_names.get(&e) {
                    ui.separator();
                    ui.strong("◈ Objects & Rig");
                    ui.small("every object and bone in this model — click to select, then pose or keyframe it");

                    let sel_idx = cur_bone.filter(|(m, _)| *m == e).map(|(_, i)| i);
                    let objects: Vec<usize> =
                        (0..nodes.len()).filter(|&i| nodes[i].is_object).collect();
                    let bones_only: Vec<usize> =
                        (0..nodes.len()).filter(|&i| !nodes[i].is_object).collect();

                    // Descendants of `child` (so the "parent under" dropdown never offers
                    // a cycle): walk parents up from every node and mark those under child.
                    let is_descendant_of = |node: usize, ancestor: usize| -> bool {
                        let mut cur = nodes[node].parent;
                        let mut guard = 0;
                        while let Some(p) = cur {
                            if p == ancestor {
                                return true;
                            }
                            cur = nodes[p].parent;
                            guard += 1;
                            if guard > 256 {
                                break;
                            }
                        }
                        false
                    };

                    let mut list_group = |ui: &mut egui::Ui, title: String, idxs: &[usize], allow_reparent: bool| {
                        egui::CollapsingHeader::new(title)
                            .id_salt(("objrig", e, allow_reparent))
                            .default_open(true)
                            .show(ui, |ui| {
                                for &i in idxs {
                                    let sel = sel_idx == Some(i);
                                    let icon = if nodes[i].is_object { "◈" } else { "🔗" };
                                    ui.horizontal_wrapped(|ui| {
                                        if ui
                                            .selectable_label(sel, format!("{icon} {}", nodes[i].name))
                                            .clicked()
                                        {
                                            cmd.select_bone = Some((e, i));
                                        }
                                        if allow_reparent {
                                            // "under ▸ <parent>" — reparent this object within
                                            // the model (persisted to the .rig.ron sidecar).
                                            let cur_parent = nodes[i].parent.map(|p| nodes[p].name.clone());
                                            let cur_label = cur_parent.clone().unwrap_or_else(|| "(root)".into());
                                            egui::ComboBox::from_id_salt(("reparent", e, i))
                                                .width(crate::responsive::fit_here(ui, 220.0))
                .wrap_mode(egui::TextWrapMode::Truncate)
                                                .selected_text(format!("under {cur_label}"))
                                                .width(140.0)
                                                .show_ui(ui, |ui| {
                                                    if ui.selectable_label(cur_parent.is_none(), "(root)").clicked()
                                                        && cur_parent.is_some()
                                                    {
                                                        cmd.set_object_parent = Some((e, nodes[i].name.clone(), None));
                                                    }
                                                    for j in 0..nodes.len() {
                                                        if j == i || is_descendant_of(j, i) {
                                                            continue; // no self / cycles
                                                        }
                                                        let picked = cur_parent.as_deref() == Some(nodes[j].name.as_str());
                                                        let jicon = if nodes[j].is_object { "◈" } else { "🔗" };
                                                        if ui
                                                            .selectable_label(picked, format!("{jicon} {}", nodes[j].name))
                                                            .clicked()
                                                            && !picked
                                                        {
                                                            cmd.set_object_parent = Some((
                                                                e,
                                                                nodes[i].name.clone(),
                                                                Some(nodes[j].name.clone()),
                                                            ));
                                                        }
                                                    }
                                                });
                                        }
                                    });
                                }
                            });
                    };

                    if !objects.is_empty() {
                        list_group(ui, format!("◈ Objects ({})", objects.len()), &objects, true);
                    }
                    if !bones_only.is_empty() {
                        // Bones re-parent too: a flow-rig chain root under "Head"
                        // makes skinned hair ride the head (skinned verts follow
                        // JOINTS, so parenting the hair object alone isn't enough).
                        list_group(ui, format!("🔗 Bones ({})", bones_only.len()), &bones_only, true);
                    }

                    // ---- tools ----
                    ui.add_space(4.0);
                    if ui
                        .button("⇋ Apply Mirror → new .glb")
                        .on_hover_text(
                            "Complete a Blender model whose Mirror modifier wasn't applied: \
                             synthesize the missing half, split off-center limbs into an L/R \
                             pair, weld centerline halves. Writes a new .mirrored.glb beside \
                             the source (non-destructive).",
                        )
                        .clicked()
                    {
                        cmd.mirror_model = Some(e);
                    }
                    let sel_obj_name = sel_idx
                        .filter(|&i| nodes[i].is_object)
                        .map(|i| nodes[i].name.clone());
                    ui.add_enabled_ui(sel_obj_name.is_some(), |ui| {
                        if ui
                            .button("🔗 Rig selected object to flow")
                            .on_hover_text(
                                "Generate a soft bone-chain down the selected object and \
                                 auto-weight it (hair, cloth, antennae). Writes a new rigged \
                                 .glb beside the source; pose/keyframe the chain to make it \
                                 bend and flow.",
                            )
                            .clicked()
                            && let Some(name) = sel_obj_name.clone()
                        {
                            cmd.add_hair_rig = Some((e, name));
                        }
                    });
                }

                // ===== 🔗 Bone attachment (any descendant of a rigged mesh) =====
                // An equipped mesh is commonly put below an Empty/socket below the
                // character first. Walk its ancestors instead of requiring the rigged
                // Mesh to be its immediate Parent; choosing a bone will normalize the
                // link to that mesh while preserving the child's world pose.
                let rig_parent = {
                    let mut at = e;
                    let mut found = None;
                    for _ in 0..64 {
                        let Some(floptle_core::Parent(p)) = world.get::<floptle_core::Parent>(at).copied() else {
                            break;
                        };
                        if bone_names.contains_key(&p) {
                            found = Some(p);
                            break;
                        }
                        at = p;
                    }
                    found
                };
                if let Some(mesh) = rig_parent
                    && let Some(bones) = bone_names.get(&mesh)
                {
                    ui.separator();
                    ui.strong("🔗 Bone attachment");
                    ui.small("ride an object or bone of this parent model (a weapon on a hand)");
                    let cur = world.get::<floptle_core::BoneAttach>(e).map(|a| a.bone.clone());
                    egui::ComboBox::from_id_salt("bone_attach_pick")
                        .width(crate::responsive::fit_here(ui, 220.0))
                .wrap_mode(egui::TextWrapMode::Truncate)
                        .selected_text(cur.clone().unwrap_or_else(|| "(not attached)".into()))
                        .show_ui(ui, |ui| {
                            if ui.selectable_label(cur.is_none(), "(not attached)").clicked()
                                && cur.is_some()
                            {
                                world.remove::<floptle_core::BoneAttach>(e);
                                cmd.inspector_changed = true;
                            }
                            for node in bones {
                                let sel = cur.as_deref() == Some(node.name.as_str());
                                let icon = if node.is_object { "◈" } else { "🔗" };
                                if ui.selectable_label(sel, format!("{icon} {}", node.name)).clicked() && !sel {
                                    // Deferred because attaching reparents to the mesh and
                                    // derives a bone-local offset from the current world pose.
                                    // That keeps a nested weapon/socket exactly where it was.
                                    cmd.attach_to_bone = Some((e, mesh, node.name.clone()));
                                    cmd.inspector_changed = true;
                                }
                            }
                        });
                    // Offset editor + detach (only when attached) — position the node on
                    // the bone relative to it.
                    if let Some(a) = world.get::<floptle_core::BoneAttach>(e).cloned() {
                        let mut off = a.offset;
                        let mut ch = false;
                        ui.horizontal_wrapped(|ui| {
                            ui.label("pos");
                            ch |= ui.add(egui::DragValue::new(&mut off.translation.x).speed(0.01).prefix("x ")).changed();
                            ch |= ui.add(egui::DragValue::new(&mut off.translation.y).speed(0.01).prefix("y ")).changed();
                            ch |= ui.add(egui::DragValue::new(&mut off.translation.z).speed(0.01).prefix("z ")).changed();
                        });
                        let (ey, ex, ez) = off.rotation.to_euler(EulerRot::YXZ);
                        let mut deg = [ex.to_degrees(), ey.to_degrees(), ez.to_degrees()];
                        ui.horizontal_wrapped(|ui| {
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
                        ui.horizontal_wrapped(|ui| {
                            ui.label("scale");
                            let mut s = off.scale.x;
                            // Negative allowed (a mirrored attachment); only the
                            // degenerate |s| < 0.001 band is nudged out.
                            if ui.add(egui::DragValue::new(&mut s).speed(0.01).range(-100.0..=100.0)).changed() {
                                if s.abs() < 0.001 {
                                    s = 0.001f32.copysign(if s == 0.0 { 1.0 } else { s });
                                }
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
                        NavExclude,
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
                        items.push(("Physics", "☉  Celestial Body (orbit rails)".into(), Add::Celestial));
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
                    if world.get::<floptle_core::NavMeshExclude>(e).is_none() {
                        items.push((
                            "Physics",
                            "⬚  Navmesh Exclude".into(),
                            Add::NavExclude,
                        ));
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
                    let mut script_paths = Vec::new();
                    collect_script_names(self.asset_tree, &mut script_paths);
                    for path in script_paths {
                        let stem = script_name_of(&path);
                        if !attached.contains(&stem) {
                            items.push(("Scripts", format!("⚙  {stem}"), Add::Script(path)));
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
                                        Add::NavExclude => cmd.set_nav_exclude = Some((e, true)),
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
                            let res = material_props_ui(ui, mat, self.materials, self.asset_tree, self.project_root, self.mat_name_buf, self.flsl_cache, self.sdf_cache, self.texture_settings);
                            cmd.inspector_changed |= res.changed;
                            cmd.open_shader_graph = res.open_shader.or(cmd.open_shader_graph.take());
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

        // ---- hand the edit to the rest of the selection ---------------------
        // Runs every frame a multi-selection is up, including mid-drag, so a
        // slider moves all of them live rather than snapping the others into
        // place when the mouse comes up. Costs one comparison per selected node
        // per component when nothing changed.
        if let Some(snap) = multi {
            snap.apply(world, self.selection);
        }
    }
}

/// What the 2D lighting inference is allowed to look at, read off the live
/// scene. See [`floptle_core::infers_2d`] for why it is only these three.
fn lit_2d_facts(world: &floptle_core::World, e: floptle_core::Entity) -> floptle_core::Lit2DFacts {
    let matter = world.get::<Matter>(e);
    floptle_core::Lit2DFacts {
        // The scene's key light is a `Light` component; a placeable one is a
        // `PointLight` node. Both emit, so both get the flag.
        emits: matches!(matter, Some(Matter::PointLight { .. }))
            || world.get::<floptle_core::Light>(e).is_some(),
        flat_matter: matches!(
            matter,
            Some(Matter::Tilemap { .. }) | Some(Matter::SpriteBatch { .. })
        ),
        flat_camera: world.query::<Matter>().any(|(ce, m)| {
            matches!(m, Matter::Camera { active: true, ortho: true, .. })
                && !floptle_core::is_disabled(world, ce)
        }),
    }
}

/// The 2D lighting row: the three-valued flag, what `Auto` decided and why, the
/// layers a light reaches, and whether the node blocks light.
///
/// Shown only for a node 2D lighting can mean something to — a light, a flat
/// kind of matter, or anything already carrying the flag. A `Lit2D` dropdown on
/// every mesh in a 3D scene would be four controls of pure noise.
fn lighting_2d_row(
    ui: &mut egui::Ui,
    world: &floptle_core::World,
    e: floptle_core::Entity,
    sorting_names: &[String],
    cmd: &mut crate::EditorCmd,
) {
    let facts = lit_2d_facts(world, e);
    let cur = world.get::<floptle_core::Lighting2D>(e).cloned().unwrap_or_default();
    let stated = world.get::<floptle_core::Lighting2D>(e).is_some()
        || world.get::<floptle_core::Shadow2D>(e).is_some();
    if !facts.emits && !facts.flat_matter && !stated {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.label("2D light");
        egui::ComboBox::from_id_salt("node_lit_2d")
            .width(crate::responsive::fit_here(ui, 220.0))
                .wrap_mode(egui::TextWrapMode::Truncate)
            .selected_text(cur.mode.name())
            .show_ui(ui, |ui| {
                for m in floptle_core::Lit2D::ALL {
                    if ui.selectable_label(cur.mode == m, m.name()).clicked() && m != cur.mode {
                        cmd.set_lighting_2d =
                            Some((e, floptle_core::Lighting2D { mode: m, ..cur.clone() }));
                    }
                }
            })
            .response
            .on_hover_text(
                "whether this is lit by the 2D system. auto decides from the scene; \
                 2d and 3d are never re-decided.",
            );
        // What auto DECIDED, not just that it is deciding. An inference you
        // cannot see is one you cannot trust, and this whole design rests on
        // trusting it.
        let (is2d, why) = floptle_core::resolve_2d(cur.mode, facts);
        if cur.mode == floptle_core::Lit2D::Auto {
            ui.small(format!("→ {} — {why}", if is2d { "2D" } else { "3D" }));
        }
    });
    // Which layers a light reaches. Lights only: the field means nothing on a
    // receiver, and a control that changes nothing is a lie about what the
    // rules are.
    if facts.emits {
        ui.horizontal_wrapped(|ui| {
            ui.label("lights layers").on_hover_text(
                "which SORTING layers this light reaches — the same layers as the \
                 sorting layer row above, and nothing to do with the collision layer. \
                 None ticked means every layer.",
            );
            let mut next = cur.layers.clone();
            let mut changed = false;
            for n in sorting_names {
                // Empty = every layer, so an untouched light shows every box
                // ticked — which is what it does.
                let mut on = cur.layers.is_empty() || cur.layers.contains(n);
                if crate::responsive::check(ui, &mut on, n).changed() {
                    changed = true;
                    if cur.layers.is_empty() {
                        // Turning one off is the first real choice: start from
                        // "all of them" so the tick that was cleared is the only
                        // one missing, rather than the only one present.
                        next = sorting_names.to_vec();
                    }
                    if on {
                        if !next.contains(n) {
                            next.push(n.clone());
                        }
                    } else {
                        next.retain(|x| x != n);
                    }
                }
            }
            if changed {
                // Back to every layer is back to the default, which stores
                // nothing — so a light that reaches everything says so by
                // saying nothing.
                if next.len() == sorting_names.len() {
                    next.clear();
                }
                cmd.set_lighting_2d =
                    Some((e, floptle_core::Lighting2D { layers: next, ..cur.clone() }));
            }
        });
        if cur.layers.is_empty() {
            ui.small("every layer").on_hover_text(
                "a light that named no layers reaches all of them — untick one to \
                 keep it off, e.g. a background that should stay flat",
            );
        }
        // The shape of the falloff (`floptle/0126`). An art control: a hard pool
        // with a defined edge, or a soft glow that reaches. It was ALSO sold as
        // the way to dodge posterize banding, and that is withdrawn — the light
        // is never quantised now (`floptle/0127`).
        let range = match world.get::<Matter>(e) {
            Some(Matter::PointLight { range, .. }) => *range,
            _ => 10.0,
        };
        let mut next = cur.clone();
        let mut changed = false;
        ui.horizontal_wrapped(|ui| {
            ui.label("full out to");
            let mut inner = cur.inner;
            // Capped by the light's own range, because past it the whole disc
            // is flat and the range slider stops meaning anything.
            if crate::responsive::slider(ui, egui::Slider::new(&mut inner, 0.0..=range.max(0.01)).suffix(" m"))
                .on_hover_text(
                    "full brightness out to here, and only then falling away to nothing at \
                     the light's range. 0 starts the ramp at the light, which is what every \
                     light did before. Push it near the range for a bright disc that ends, \
                     rather than a glow that fades the whole way.",
                )
                .changed()
            {
                next.inner = inner;
                changed = true;
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.label("falloff");
            let mut f = cur.falloff;
            if crate::responsive::slider(ui, egui::Slider::new(&mut f, 0.25..=6.0).logarithmic(true))
                .on_hover_text(
                    "the exponent of that ramp. 2 is the curve every light has always had; \
                     below 1 holds the brightness out and drops it late, above 2 dives away \
                     from the core.",
                )
                .changed()
            {
                next.falloff = f;
                changed = true;
            }
            if cur.falloff != 2.0 && ui.small_button("reset").clicked() {
                next.falloff = 2.0;
                changed = true;
            }
        });
        let mut sh = cur.shadows;
        if crate::responsive::check(ui, &mut sh, "casts stop this light")
            .on_hover_text(
                "off makes this light pass through everything, whatever the scene's \
                 nodes say about blocking it — for a glow that is not meant to be a \
                 light source, like a muzzle flash or a UI pulse",
            )
            .changed()
        {
            next.shadows = sh;
            changed = true;
        }
        if changed {
            cmd.set_lighting_2d = Some((e, next));
        }
    } else {
        let cast = world.get::<floptle_core::Shadow2D>(e).map(|s| s.0).unwrap_or_default();
        ui.horizontal_wrapped(|ui| {
            ui.label("blocks light");
            egui::ComboBox::from_id_salt("node_shadow_2d")
                .width(crate::responsive::fit_here(ui, 220.0))
                .wrap_mode(egui::TextWrapMode::Truncate)
                .selected_text(cast.name())
                .show_ui(ui, |ui| {
                    for c in floptle_core::Cast2D::ALL {
                        if ui.selectable_label(cast == c, c.name()).clicked() && c != cast {
                            cmd.set_shadow_2d = Some((e, c));
                        }
                    }
                })
                .response
                .on_hover_text(
                    "auto: a tilemap casts from the collision it already has, so a \
                     level's collision IS its light occlusion. on makes anything cast.",
                );
            let collidable = world.get::<floptle_core::Collidable>(e).is_some();
            let (casts, why) =
                floptle_core::resolve_shadow_2d(cast, facts.flat_matter, collidable);
            if cast == floptle_core::Cast2D::Auto {
                ui.small(format!("→ {} — {why}", if casts { "casts" } else { "no" }));
            }
        });
    }
}

/// **How an orthographic camera follows.** Shown only on one.
///
/// Under a perspective camera these numbers mean something else — a follow that
/// has to think about distance and pitch, a dead zone that is an angle — and
/// offering them there is how somebody learns the wrong model from a panel. So
/// the section is simply absent, and says why if a 2D camera ends up on a
/// perspective node anyway (which a project can do, by switching the projection
/// after setting one up).
fn camera_2d_section(
    ui: &mut egui::Ui,
    world: &floptle_core::World,
    e: floptle_core::Entity,
    cmd: &mut crate::EditorCmd,
) {
    let Some(Matter::Camera { ortho, .. }) = world.get::<Matter>(e) else { return };
    let cur = world.get::<floptle_core::camera2d::Camera2D>(e);
    if !*ortho {
        if cur.is_some() {
            crate::responsive::para(
                ui,
                egui::RichText::new(
                    "this camera has 2D follow settings and is not orthographic — they do nothing until you switch the projection back",
                )
                .weak()
                .small(),
            );
        }
        return;
    }
    let on = cur.is_some();
    let mut next = cur.cloned().unwrap_or_default();
    let mut changed = false;
    let mut want = on;
    if crate::responsive::check(ui, &mut want, "2D camera")
        .on_hover_text(
            "follow a node, with a dead zone, smoothing and world limits — the rule every 2D project writes out in Lua. Off is exactly the camera you placed.",
        )
        .changed()
    {
        cmd.set_camera_2d = Some((e, want.then(|| next.clone())));
        return;
    }
    if !on {
        return;
    }
    crate::responsive::group(ui, |ui| {
        crate::responsive::grid(ui, "cam2d", |ui| {
            ui.label("follow").on_hover_text(
                "the NAME of the node to chase. Empty still gives you the limits and the shake — a fixed camera that cannot show outside the level.",
            );
            changed |= ui.text_edit_singleline(&mut next.follow).changed();
            ui.end_row();

            ui.label("smoothing").on_hover_text(
                "seconds to close the gap. After this long the camera has covered about two thirds of the distance — the same at 30 fps and at 144. 0 snaps.",
            );
            changed |= ui
                .add(
                    egui::DragValue::new(&mut next.smoothing)
                        .speed(0.01)
                        .range(0.0..=5.0)
                        .suffix(" s"),
                )
                .changed();
            ui.end_row();

            ui.label("dead zone").on_hover_text(
                "how far the target may move before the camera moves at all, in world units either side. Without one, every footstep moves the camera and the world reads as wobbling.",
            );
            ui.horizontal_wrapped(|ui| {
                for (i, axis) in ["x ", "y "].iter().enumerate() {
                    changed |= ui
                        .add(
                            egui::DragValue::new(&mut next.dead_zone[i])
                                .speed(0.05)
                                .range(0.0..=100.0)
                                .prefix(*axis),
                        )
                        .changed();
                }
            });
            ui.end_row();
        });
        changed |= crate::responsive::check(ui, &mut next.limits_on, "keep inside a rectangle")
            .on_hover_text("the camera never leaves this box, so it never shows outside the level")
            .changed();
        if next.limits_on {
            crate::responsive::grid(ui, "cam2d_lim", |ui| {
                for (label, v) in
                    [("min", &mut next.limit_min), ("max", &mut next.limit_max)]
                {
                    ui.label(label);
                    ui.horizontal_wrapped(|ui| {
                        for (i, axis) in ["x ", "y "].iter().enumerate() {
                            changed |= ui
                                .add(egui::DragValue::new(&mut v[i]).speed(0.1).prefix(*axis))
                                .changed();
                        }
                    });
                    ui.end_row();
                }
            });
            if next.limit_min[0] > next.limit_max[0] || next.limit_min[1] > next.limit_max[1] {
                crate::responsive::para(
                    ui,
                    egui::RichText::new("min is past max — the camera will park in the middle")
                        .weak()
                        .small(),
                );
            }
        }
        crate::responsive::para(
            ui,
            egui::RichText::new("shake it from a script: node:shake(amount, seconds)")
                .weak()
                .small(),
        );
    });
    if changed {
        cmd.set_camera_2d = Some((e, Some(next)));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Collect every string egui painted this frame (the settings-UI idiom): a
    /// headless pass that renders zero widgets would otherwise "pass".
    fn painted_text(output: &egui::FullOutput) -> String {
        fn walk(shape: &egui::epaint::Shape, out: &mut String) {
            match shape {
                egui::epaint::Shape::Text(t) => {
                    out.push_str(t.galley.text());
                    out.push('\n');
                }
                egui::epaint::Shape::Vec(v) => {
                    for s in v {
                        walk(s, out);
                    }
                }
                _ => {}
            }
        }
        let mut out = String::new();
        for cs in &output.shapes {
            walk(&cs.shape, &mut out);
        }
        out
    }

    /// Draw the Material section and hand back what it painted plus how it left
    /// the material — two frames, because egui needs one to lay out.
    fn run_material_ui(
        m: &mut Material,
        settings: &std::collections::HashMap<String, crate::assets::TexSetting>,
    ) -> String {
        let ctx = crate::icons::test_context();
        let mut name_buf = String::new();
        let (flsl, sdf) = (crate::shaders::FlslCache::new(), crate::shaders::SdfCache::new());
        let mut painted = String::new();
        for _ in 0..2 {
            let out = ctx.run_ui(crate::icons::test_input(), |ui| {
                let _ = material_props_ui(
                    ui,
                    m,
                    &[],
                    &[],
                    Path::new("/project"),
                    &mut name_buf,
                    &flsl,
                    &sdf,
                    settings,
                );
            });
            painted = painted_text(&out);
        }
        painted
    }

    /// The annotated tunables panel: rows in DECLARATION order (not alphabetical),
    /// a `--@header` above the rows it names, `--@hidden` gone, and each widget the
    /// one its annotations asked for. The old panel drew numbers-then-strings, each
    /// alphabetised, so a header could never sit above its own rows.
    #[test]
    fn script_tunables_render_in_declaration_order_with_their_widgets() {
        let meta = crate::script_meta::parse(
            "defaults = {\n\
             \x20 --@header Movement\n\
             \x20 -- How fast you walk.\n\
             \x20 --@range 0 20 --@units m/s\n\
             \x20 walk = 4.5,\n\
             \x20 --@slider 0 1\n\
             \x20 blend = 0.35,\n\
             \x20 --@options Off|On|Auto\n\
             \x20 sas = 0,\n\
             \x20 invert = false,\n\
             \x20 --@header Audio\n\
             \x20 clip = \"footstep\",\n\
             \x20 --@hidden\n\
             \x20 debugScale = 1.0,\n\
             }\n",
        );
        // Values as the editor would have seeded them (deliberately NOT in
        // declaration order, and alphabetically wrong, to prove the order comes
        // from the source).
        let mut inst = floptle_core::ScriptInst {
            kind: "walker".into(),
            enabled: true,
            params: vec![
                ("sas".into(), 1.0),
                ("blend".into(), 0.35),
                ("walk".into(), 4.5),
                ("invert".into(), 0.0),
                ("debugScale".into(), 1.0),
            ],
            refs: Vec::new(),
            strs: vec![("clip".into(), "footstep".into())],
        };

        let ctx = crate::icons::test_context();
        let mut painted = String::new();
        let mut run_action = None;
        for _ in 0..2 {
            let out = ctx.run_ui(crate::icons::test_input(), |ui| {
                let cx = ScriptRowCtx {
                    meta: &meta,
                    ref_kinds: &std::collections::HashMap::new(),
                    node_names: &Vec::new(),
                    script_nodes: &std::collections::HashMap::new(),
                    comp_nodes: &std::collections::HashMap::new(),
                    name_of: &std::collections::HashMap::new(),
                    salt: (0, 0),
                };
                let e = floptle_core::World::new().spawn();
                script_tunables_ui(ui, &mut inst, cx, &mut run_action, e);
            });
            painted = painted_text(&out);
        }

        // Headers and rows are all on screen…
        for want in ["Movement", "walk", "blend", "sas", "invert", "Audio", "clip"] {
            assert!(painted.contains(want), "{want:?} missing from the panel:\n{painted}");
        }
        // …the hidden one is not…
        assert!(!painted.contains("debugScale"), "--@hidden must not draw:\n{painted}");
        // …the dropdown shows its LABEL rather than the raw number…
        assert!(painted.contains("On"), "--@options should render as labels:\n{painted}");
        // …the units suffix rides the number…
        assert!(painted.contains("m/s"), "--@units missing:\n{painted}");
        // …and the order is the script's, not the alphabet's.
        let pos = |s: &str| painted.find(s).unwrap_or(usize::MAX);
        assert!(pos("Movement") < pos("walk"), "a header draws above its rows");
        assert!(pos("walk") < pos("blend") && pos("blend") < pos("sas"), "declaration order");
        assert!(pos("sas") < pos("Audio") && pos("Audio") < pos("clip"), "sections stay grouped");
    }

    /// The hand-off the whole feature stands on: slice the TEXTURE once, and a
    /// material using it inherits the grid, shows the cell picker, and hides the
    /// tiling rows (a sheet draws one cell, so tiling would sample its
    /// neighbours). Without the grid, the section is exactly what it always was.
    #[test]
    fn the_material_section_inherits_a_textures_sheet_grid() {
        let sliced = std::collections::HashMap::from([(
            "textures/face.png".to_string(),
            crate::assets::TexSetting { sheet_cols: 4, sheet_rows: 4, ..Default::default() },
        )]);
        let mut m =
            Material { texture: Some("textures/face.png".into()), cell: 9, ..Material::default() };
        let painted = run_material_ui(&mut m, &sliced);
        assert_eq!((m.sheet_cols, m.sheet_rows), (4, 4), "the material must inherit the grid");
        assert_eq!(m.cell, 9, "a valid cell survives the sync");
        assert!(painted.contains("sprite cell (4×4 sheet)"), "no cell picker drawn:\n{painted}");
        assert!(painted.contains("a sheet draws one cell"), "tiling rows not replaced:\n{painted}");

        // Re-slicing the texture smaller must pull an out-of-range cell back in.
        let smaller = std::collections::HashMap::from([(
            "textures/face.png".to_string(),
            crate::assets::TexSetting { sheet_cols: 2, sheet_rows: 2, ..Default::default() },
        )]);
        let painted = run_material_ui(&mut m, &smaller);
        assert_eq!((m.sheet_cols, m.sheet_rows, m.cell), (2, 2, 3), "cell must clamp into the grid");
        assert!(painted.contains("sprite cell (2×2 sheet)"), "{painted}");

        // An unsliced texture: no sheet anywhere, and the tiling rows are back.
        let mut plain =
            Material { texture: Some("textures/face.png".into()), ..Material::default() };
        let painted = run_material_ui(&mut plain, &std::collections::HashMap::new());
        assert!(!plain.is_sheet());
        assert!(!painted.contains("sprite cell"), "a plain texture must not offer cells:\n{painted}");
        assert!(painted.contains("tiling"), "the tiling rows must come back:\n{painted}");
    }

    // ---- 2D lighting -------------------------------------------------------

    /// Draw the 2D lighting row for one node and hand back what it painted,
    /// plus whatever it asked the editor to change.
    fn run_lighting_2d(
        world: &floptle_core::World,
        e: floptle_core::Entity,
        layers: &[String],
    ) -> (String, crate::EditorCmd) {
        let ctx = crate::icons::test_context();
        let mut cmd = crate::EditorCmd::default();
        let mut painted = String::new();
        for _ in 0..2 {
            cmd = crate::EditorCmd::default();
            let out = ctx.run_ui(crate::icons::test_input(), |ui| {
                lighting_2d_row(ui, world, e, layers, &mut cmd);
            });
            painted = painted_text(&out);
        }
        (painted, cmd)
    }

    fn scene_with(matter: Matter, ortho_camera: bool) -> (floptle_core::World, floptle_core::Entity) {
        let mut world = floptle_core::World::default();
        let cam = world.spawn();
        world.insert(
            cam,
            Matter::Camera {
                fov_y: 60.0,
                active: true,
                target: String::new(),
                cull_mask: !0,
                target_w: 0,
                target_h: 0,
                target_hz: 0.0,
                ortho: ortho_camera,
                ortho_height: 10.0,
            },
        );
        let e = world.spawn();
        world.insert(e, matter);
        (world, e)
    }

    fn tilemap() -> Matter {
        Matter::Tilemap {
            cols: 2,
            rows: 2,
            tile: 1.0,
            data: vec![floptle_core::EMPTY_TILE; 4],
            tileset: String::new(),
        }
    }

    /// `Auto` must say what it DECIDED, not merely that it is deciding. The
    /// whole 2D-vs-3D design rests on the inference being inspectable: an
    /// inference you cannot see is one you cannot trust.
    #[test]
    fn auto_shows_what_it_inferred_and_why() {
        let layers = vec!["Default".to_string(), "Background".to_string()];

        let (world, e) = scene_with(Matter::PointLight { color: [1.0; 3], intensity: 1.0, range: 5.0, shape: Default::default() , shadows: false, spot_angle: floptle_core::OMNI_ANGLE, spot_softness: 0.25}, true);
        let (painted, _) = run_lighting_2d(&world, e, &layers);
        assert!(painted.contains("2D light"), "no row at all:\n{painted}");
        assert!(painted.contains("auto"), "the flag is not shown:\n{painted}");
        assert!(painted.contains("→ 2D"), "auto did not say what it decided:\n{painted}");
        assert!(painted.contains("orthographic"), "…nor why:\n{painted}");

        // The same light in a 3D scene decides the other way, and says so.
        let (world, e) = scene_with(Matter::PointLight { color: [1.0; 3], intensity: 1.0, range: 5.0, shape: Default::default() , shadows: false, spot_angle: floptle_core::OMNI_ANGLE, spot_softness: 0.25}, false);
        let (painted, _) = run_lighting_2d(&world, e, &layers);
        assert!(painted.contains("→ 3D"), "a light in a perspective scene must read 3D:\n{painted}");
    }

    /// A 3D scene must not grow four controls of noise on every mesh.
    #[test]
    fn an_ordinary_mesh_gets_no_2d_lighting_row_at_all() {
        let (world, e) = scene_with(Matter::Primitive { shape: floptle_core::Shape::Cube, color: [1.0; 3] }, false);
        let (painted, _) = run_lighting_2d(&world, e, &["Default".to_string()]);
        assert!(painted.trim().is_empty(), "a plain cube drew a 2D lighting row:\n{painted}");
    }

    /// The layer list is a LIGHT's control. On a receiver it would mean nothing,
    /// and a control that changes nothing is a lie about what the rules are.
    #[test]
    fn only_a_light_is_asked_which_layers_it_reaches() {
        let layers = vec!["Default".to_string(), "Background".to_string()];
        let (world, e) = scene_with(tilemap(), true);
        let (painted, _) = run_lighting_2d(&world, e, &layers);
        assert!(painted.contains("2D light"), "a tilemap IS a 2D receiver:\n{painted}");
        assert!(!painted.contains("lights layers"), "a receiver was offered a light's control:\n{painted}");
        assert!(painted.contains("blocks light"), "…and was not asked whether it occludes:\n{painted}");
        // Not collidable yet, and auto says exactly that rather than "no".
        assert!(painted.contains("nothing to cast from"), "the reason is missing:\n{painted}");

        // Switch its collision on and it casts, with no second authoring step —
        // a level's collision IS its light occlusion.
        let mut world = world;
        world.insert(e, floptle_core::Collidable);
        let (painted, _) = run_lighting_2d(&world, e, &layers);
        assert!(painted.contains("→ casts"), "a solid tilemap must cast:\n{painted}");
        assert!(painted.contains("casts where it is solid"), "{painted}");
    }

    /// Unticking one layer must leave a light reaching all the OTHERS, not only
    /// the one that was already ticked. An untouched light shows every box on
    /// because it reaches everything; the first click has to preserve that.
    #[test]
    fn unticking_a_layer_keeps_the_rest() {
        let layers = vec!["Default".to_string(), "Terrain".to_string(), "Background".to_string()];
        let (world, e) = scene_with(Matter::PointLight { color: [1.0; 3], intensity: 1.0, range: 5.0, shape: Default::default() , shadows: false, spot_angle: floptle_core::OMNI_ANGLE, spot_softness: 0.25}, true);
        let (painted, _) = run_lighting_2d(&world, e, &layers);
        assert!(painted.contains("every layer"), "an untouched light must say it reaches all:\n{painted}");

        // Simulate the click the panel would make on "Background".
        let cur = floptle_core::Lighting2D::default();
        let mut next = layers.clone();
        assert!(cur.layers.is_empty());
        next.retain(|x| x != "Background");
        let after = floptle_core::Lighting2D { layers: next, ..cur.clone() };
        assert!(after.reaches("Default"));
        assert!(after.reaches("Terrain"));
        assert!(!after.reaches("Background"));
    }

    /// **The 2D camera section must survive a thin dock too.** It is a page of
    /// paired X/Y numbers with a per-axis on/off, which is the shape that runs
    /// out of room first, and it shipped with no width guard at all.
    #[test]
    fn the_2d_camera_section_fits_however_thin_the_dock_gets() {
        let mut world = floptle_core::World::new();
        let e = world.spawn();
        world.insert(e, floptle_core::Transform::default());
        world.insert(
            e,
            Matter::Camera {
                fov_y: 1.0,
                active: true,
                target: String::new(),
                cull_mask: !0,
                target_w: 0,
                target_h: 0,
                target_hz: 0.0,
                ortho: true,
                ortho_height: 10.0,
            },
        );
        // Every optional part on: a follow target, a dead zone and limits. With
        // them off the section is three rows, which is not the section.
        world.insert(
            e,
            floptle_core::camera2d::Camera2D {
                follow: "PlayerCharacter".into(),
                smoothing: 0.12,
                dead_zone: [1.5, 0.75],
                limits_on: true,
                limit_min: [-100.0, -20.0],
                limit_max: [200.0, 40.0],
                ..Default::default()
            },
        );
        let mut cmd = crate::EditorCmd::default();
        crate::responsive::tests::assert_fits("the 2D camera section", |ui| {
            camera_2d_section(ui, &world, e, &mut cmd);
        });
    }

    /// **The material editor must survive a thin dock.** It is the widest
    /// shared component in the editor — sliders, dropdowns, checkboxes and
    /// texture pickers — and it is drawn in two places at once: the Inspector,
    /// and the ▦ Model tab's per-slot override. One of them being narrow is the
    /// ordinary case, so this is asserted on the component rather than on
    /// either panel.
    ///
    /// Driven with every optional section open, because a section that is not
    /// on screen cannot overflow and a guard that only sees the closed state
    /// proves nothing about the open one.
    ///
    /// **And with the texture settings a sheet needs.** This passed an EMPTY
    /// settings map, so `sheet_of` answered 1×1 and the spritesheet cell picker
    /// — the widest geometry in the component, a full-width grid of one button
    /// per cell — was never constructed by the guard at all. A guard that is
    /// green because its fixture is empty is worse than none: it reports on a
    /// panel nobody is looking at.
    #[test]
    fn the_material_editor_fits_however_thin_the_dock_gets() {
        let mut m = floptle_core::Material {
            texture: Some("textures/tiles.png".into()),
            normal_map: Some("textures/tiles_n.png".into()),
            transmission: 0.5,
            metallic: 1.0,
            ..Default::default()
        };
        let mut name_buf = String::new();
        let flsl = crate::shaders::FlslCache::default();
        let sdf = crate::shaders::SdfCache::default();
        // A real sheet, so the cell picker is drawn. Wide rather than square:
        // the grid's own width is what has to fit, and a 12-column sheet is an
        // ordinary character strip.
        let tex = std::collections::HashMap::from([(
            "textures/tiles.png".to_string(),
            crate::assets::TexSetting {
                sheet_cols: 12,
                sheet_rows: 3,
                ..Default::default()
            },
        )]);
        let root = std::path::PathBuf::from(".");
        crate::responsive::tests::assert_fits("the material editor", |ui| {
            material_props_ui(ui, &mut m, &[], &[], &root, &mut name_buf, &flsl, &sdf, &tex);
        });
    }
}
