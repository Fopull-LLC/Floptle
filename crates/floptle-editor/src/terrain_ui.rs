//! The Terrain dock tab: sculpt/paint brush settings, the texture palette,
//! and the "New terrain" dialog config.

use std::path::Path;

use crate::assets::collect_texture_paths;
use crate::EditorTabViewer;

/// Terrain sculpt/paint brush settings.
#[derive(Clone, Copy)]
pub(crate) struct TerrainBrush {
    pub(crate) mode: floptle_field::Brush,
    pub(crate) radius: f32,
    pub(crate) strength: f32,
    /// The weight ramp from core to rim. Terrain used to hardcode a linear falloff —
    /// this is what lets a stroke be a hard stamp instead of a soft smear.
    pub(crate) profile: floptle_field::BrushProfile,
    /// Dab spacing as a fraction of the radius. Was hardcoded at 0.34; low values give
    /// a continuous smear, high values give distinct stamps.
    pub(crate) spacing: f32,
    pub(crate) color: [f32; 3],
    /// Paint target: -1 = flat color, else a terrain texture palette slot.
    pub(crate) tex_slot: i32,
    /// "Fill bounds" tool: lay flat ground up to `fill_top`, from `fill_floor` below,
    /// kept `fill_inset` in from the X/Z walls. (Edge-sculpt no longer auto-extends the
    /// ground, so this is the deliberate way to make flat areas.)
    pub(crate) fill_top: f32,
    pub(crate) fill_floor: f32,
    pub(crate) fill_inset: f32,
}

/// Rough sparse-field cost a "New terrain" config will produce — `(surface chunks,
/// resident MB)`, shown live in the dialog. Terrain 2.0: memory scales with the slab's
/// SURFACE (the narrow band), not its volume, and there is no size cap to warn about.
pub(crate) fn new_terrain_preview(size_xz: f32, thickness: f32, voxel: f32) -> (u64, f64) {
    let v = voxel.clamp(0.25, 16.0);
    let chunk_units = floptle_field::CHUNK as f32 * v;
    let n_xz = (size_xz.max(0.1) / chunk_units).ceil() as u64;
    // The band around the top surface + the slab rim: ~1 chunk layer over the
    // footprint, plus a rim proportional to the perimeter.
    let _ = thickness; // volume is (nearly) free in a sparse field
    let chunks = (n_xz * n_xz + 4 * n_xz).max(1);
    let mb = chunks as f64 * (32.0 * 32.0 * 32.0 * 8.0) / 1.0e6;
    (chunks, mb)
}

/// The brush-shape controls, shared verbatim by the Terrain and Paint tabs — the two
/// brushes should not drift into different vocabularies for the same idea.
pub(crate) fn brush_profile_ui(
    ui: &mut egui::Ui,
    profile: &mut floptle_field::BrushProfile,
    spacing: &mut f32,
) {
    use floptle_field::Falloff;
    ui.horizontal(|ui| {
        ui.label("edge");
        // Presets first: most strokes want one of these, and "Hard" is the answer to
        // "why is everything blurry".
        if ui.button("Hard").on_hover_text("no gradient — flat, stamped edges").clicked() {
            *profile = floptle_field::BrushProfile::hard();
        }
        if ui.button("Soft").on_hover_text("airbrush — falloff across the whole radius").clicked() {
            *profile = floptle_field::BrushProfile { hardness: 0.0, falloff: Falloff::Smooth };
        }
        if ui.button("Default").clicked() {
            *profile = floptle_field::BrushProfile::default();
        }
    });
    ui.add(
        egui::Slider::new(&mut profile.hardness, 0.0..=1.0)
            .text("hardness")
            .custom_formatter(|v, _| {
                if v >= 0.999 {
                    "hard edge".into()
                } else {
                    format!("{:.0}%", v * 100.0)
                }
            }),
    )
    .on_hover_text("how much of the radius gets FULL strength before the falloff starts");
    ui.add_enabled_ui(profile.hardness < 0.999, |ui| {
        ui.horizontal(|ui| {
            ui.label("falloff");
            ui.selectable_value(&mut profile.falloff, Falloff::Smooth, "Smooth");
            ui.selectable_value(&mut profile.falloff, Falloff::Linear, "Linear");
            ui.selectable_value(&mut profile.falloff, Falloff::Sharp, "Sharp");
            ui.selectable_value(&mut profile.falloff, Falloff::Soft, "Soft");
        });
    });
    ui.add(egui::Slider::new(spacing, 0.02..=2.0).logarithmic(true).text("spacing"))
        .on_hover_text("gap between dabs, as a fraction of the radius — low smears, high stamps");
    brush_preview(ui, *profile);
}

/// A live cross-section of the brush weight. Two knobs are abstract; a curve is not —
/// you can see a hard edge instead of discovering it mid-stroke.
fn brush_preview(ui: &mut egui::Ui, profile: floptle_field::BrushProfile) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 40.0), egui::Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
    let n = 64;
    let pts: Vec<egui::Pos2> = (0..=n)
        .map(|i| {
            // Mirror the profile so it reads as a brush cross-section, not a ramp.
            let x = i as f32 / n as f32;
            let d = (x * 2.0 - 1.0).abs();
            let w = profile.weight(d, 1.0);
            egui::pos2(rect.left() + x * rect.width(), rect.bottom() - w * (rect.height() - 4.0) - 2.0)
        })
        .collect();
    p.line(pts, egui::Stroke::new(1.5, ui.visuals().selection.bg_fill));
}

/// A "fill the whole terrain" request from the Terrain tab.
#[derive(Clone, Copy)]
pub(crate) enum TerrainFill {
    Color([f32; 3]),
    /// A palette slot stored as slot+1 (0 = untextured).
    Texture(u8),
}

impl Default for TerrainBrush {
    fn default() -> Self {
        Self {
            mode: floptle_field::Brush::Raise,
            radius: 2.5,
            strength: 0.5,
            profile: floptle_field::BrushProfile::default(),
            spacing: 0.34,
            color: [0.45, 0.32, 0.2],
            tex_slot: -1,
            fill_top: 0.0,
            fill_floor: -8.0,
            fill_inset: 0.0,
        }
    }
}

/// Config gathered by the "New terrain" dialog before the node is actually created —
/// footprint size, thickness, and an initial color/texture painted across the whole
/// slab, so a terrain arrives already the size/look you want instead of always
/// starting as the same small default patch you have to sculpt/fill out by hand.
#[derive(Clone)]
pub(crate) struct NewTerrainCfg {
    /// Full width/depth of the flat slab (X and Z), world units.
    pub(crate) size_xz: f32,
    /// Full height of the slab (Y), world units.
    pub(crate) thickness: f32,
    pub(crate) color: [f32; 3],
    /// An asset texture path painted across the whole terrain (empty = none / flat
    /// color only). Resolved to a palette slot at creation time.
    pub(crate) texture: String,
}

impl Default for NewTerrainCfg {
    fn default() -> Self {
        Self { size_xz: 32.0, thickness: 12.0, color: [0.35, 0.6, 0.28], texture: String::new() }
    }
}
impl EditorTabViewer<'_> {
    /// The Terrain dock tab: detail, sculpt brush, and texture palette controls.
    /// (Rebinds fields to locals so each egui closure captures disjoint state.)
    pub(crate) fn terrain_ui(&mut self, ui: &mut egui::Ui) {
        use floptle_field::Brush;
        let cmd = &mut *self.cmd;
        let terrain_brush = &mut *self.terrain_brush;
        let terrain_voxel = &mut *self.terrain_voxel;
        let terrain_textures = &mut *self.terrain_textures;
        let terrain_glow = &mut *self.terrain_glow;
        let materials = self.materials;
        let asset_tree = self.asset_tree;
        let project_root = self.project_root;
        let terrain_present = self.terrain_present;
        let terrain_stats = self.terrain_stats;

        // Voxel density for NEW terrains — an honest units-per-voxel (Terrain 2.0),
        // not a cell count. Cells are always cubic; existing terrains keep theirs.
        ui.horizontal(|ui| {
            ui.label("voxel");
            ui.add(
                egui::Slider::new(terrain_voxel, 0.25..=4.0)
                    .step_by(0.25)
                    .suffix(" u")
                    .logarithmic(true),
            )
            .on_hover_text(
                "The cubic voxel edge new terrains are created at, in world units. \
                 Smaller = finer sculpt detail, more chunks. Applies to NEW terrains; \
                 an existing terrain keeps the density it was created with.",
            );
        });
        if let Some((n, chunks, bytes)) = terrain_stats {
            ui.small(format!(
                "{n} volume(s) · {chunks} chunks · {:.1} MB resident (sparse, unbounded)",
                bytes as f64 / 1.0e6
            ));
        }
        // New terrains can be added any time — each is a node you place + blend.
        if ui.button("✚ New terrain").on_hover_text("adds another terrain node at the cursor; overlapping terrains blend").clicked() {
            cmd.open_new_terrain = true;
        }
        if !terrain_present {
            ui.small("Adds a flat slab; then press 5 (Sculpt) and LMB-drag. Add more — they fuse where they overlap.");
            return;
        }
        ui.separator();
        ui.label("Sculpt tool (key 5) — LMB-drag brushes the terrain under the");
        ui.label("cursor. Sculpt past an edge to grow it (infinite bounds).");
        ui.label("Ctrl+Z/Y undo strokes. Move a terrain with the gizmo to blend.");
        ui.label("Brush");
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(&mut terrain_brush.mode, Brush::Raise, "⏶ Raise");
            ui.selectable_value(&mut terrain_brush.mode, Brush::Lower, "⏷ Lower");
            ui.selectable_value(&mut terrain_brush.mode, Brush::Flatten, "⊟ Flatten");
            ui.selectable_value(&mut terrain_brush.mode, Brush::Smooth, "≈ Smooth");
            ui.selectable_value(&mut terrain_brush.mode, Brush::Paint, "◑ Paint");
        });
        // Logarithmic: the old 0.5..=8 cap was far too small for large terrain, but a
        // linear slider over a wide range makes small brushes unselectable. Log gives
        // both — fine control down low, room to go big.
        ui.add(
            egui::Slider::new(&mut terrain_brush.radius, 0.1..=200.0)
                .logarithmic(true)
                .text("radius"),
        );
        ui.add(egui::Slider::new(&mut terrain_brush.strength, 0.01..=1.0).text("strength"));
        brush_profile_ui(ui, &mut terrain_brush.profile, &mut terrain_brush.spacing);
        if terrain_brush.mode == Brush::Paint {
            ui.separator();
            ui.horizontal(|ui| {
                ui.label("paint:");
                ui.selectable_value(&mut terrain_brush.tex_slot, -1, "Color");
            });
            // Fill the whole terrain with the current paint target.
            if terrain_brush.tex_slot < 0 {
                if ui.button("▣ Fill terrain with this color").on_hover_text("fills the active terrain (or selected terrain node)").clicked() {
                    cmd.fill_terrain = Some(TerrainFill::Color(terrain_brush.color));
                }
            } else if ui.button("▣ Fill terrain with this texture").clicked() {
                cmd.fill_terrain = Some(TerrainFill::Texture(terrain_brush.tex_slot as u8 + 1));
            }
            if terrain_brush.tex_slot < 0 {
                ui.horizontal(|ui| {
                    ui.label("color");
                    ui.color_edit_button_rgb(&mut terrain_brush.color);
                    if !materials.is_empty() {
                        ui.menu_button("from material", |ui| {
                            for (name, doc) in materials {
                                if ui.button(name).clicked() {
                                    terrain_brush.color = doc.color;
                                    ui.close();
                                }
                            }
                        });
                    }
                });
            }
            // Texture palette: assign an image per slot, then click a slot to paint
            // that texture (triplanar) onto the terrain.
            ui.label("Texture palette");
            let mut tex_list = Vec::new();
            collect_texture_paths(asset_tree, &mut tex_list);
            // Show the ASSIGNED slots plus one empty "add" row — not all 32 at once.
            // Slots keep their index forever (the painted terrain stores it), so a
            // cleared middle slot stays visible until the trailing empties.
            let last_used = terrain_textures.iter().rposition(|t| !t.is_empty());
            let visible = (last_used.map_or(0, |i| i + 1) + 1).min(terrain_textures.len());
            for (slot, tex) in terrain_textures.iter_mut().enumerate().take(visible) {
                ui.horizontal(|ui| {
                    let sel = terrain_brush.tex_slot == slot as i32;
                    let label = if tex.is_empty() {
                        format!("slot {}", slot + 1)
                    } else {
                        Path::new(tex.as_str())
                            .file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default()
                    };
                    if ui.selectable_label(sel, format!("🖊 {label}")).clicked() {
                        terrain_brush.tex_slot = slot as i32;
                    }
                    egui::ComboBox::from_id_salt(("tslot", slot))
                        .selected_text("set…")
                        .width(70.0)
                        .show_ui(ui, |ui| {
                            if ui.selectable_label(false, "(none)").clicked() {
                                tex.clear();
                                cmd.terrain_palette_changed = true;
                            }
                            for p in &tex_list {
                                let n = Path::new(p)
                                    .file_name()
                                    .map(|s| s.to_string_lossy().to_string())
                                    .unwrap_or_default();
                                // Store the PORTABLE project-relative form (tree paths
                                // embed how the editor was launched); match either
                                // spelling so legacy slots still show as selected.
                                let rel = crate::assets::asset_rel_path(p, project_root);
                                if ui.selectable_label(*tex == *p || *tex == rel, n).clicked() {
                                    *tex = rel;
                                    cmd.terrain_palette_changed = true;
                                }
                            }
                        });
                    // Self-lit slot: its texture stays visible with no light at all —
                    // glowing crystals, magma veins, anything meant to read in caves.
                    if !tex.is_empty() {
                        let mut glows = *terrain_glow & (1 << slot) != 0;
                        if ui
                            .checkbox(&mut glows, "✨")
                            .on_hover_text("glow — this texture is self-lit (visible in unlit caves)")
                            .changed()
                        {
                            if glows {
                                *terrain_glow |= 1 << slot;
                            } else {
                                *terrain_glow &= !(1 << slot);
                            }
                            cmd.terrain_palette_changed = true;
                        }
                    }
                });
            }
            ui.small("Extract a model's textures (Inspector) or add PNGs to textures/, assign them to slots, then paint. Color tints the texture.");
        }
        ui.separator();
        // Fill-bounds tool. Sculpting near an edge now grows only the BOUNDS (the
        // surface no longer auto-extends into flat land), so this is the deliberate way
        // to lay flat ground: pour solid up to `height`, from `floor` below, kept
        // `inset` in from the walls.
        egui::CollapsingHeader::new("▦ Fill bounds (flat ground)").default_open(false).show(ui, |ui| {
            ui.add(egui::Slider::new(&mut terrain_brush.fill_top, -20.0..=20.0).text("fill height (top)"));
            ui.add(egui::Slider::new(&mut terrain_brush.fill_floor, -40.0..=20.0).text("floor (bottom)"));
            ui.add(egui::Slider::new(&mut terrain_brush.fill_inset, 0.0..=20.0).text("edge inset"));
            if ui.button("▦ Fill bounds with flat ground")
                .on_hover_text("union solid ground into the active terrain up to the height (uses the brush color)")
                .clicked()
            {
                cmd.fill_bounds = true;
            }
        });
        ui.separator();
        if ui.button("🗑 Clear all terrain").on_hover_text("delete every terrain node (or select one + Delete)").clicked() {
            cmd.clear_terrain = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::new_terrain_preview;

    /// The dialog estimate scales with the slab's SURFACE (chunks over the footprint),
    /// never its volume, and stays sane across sizes and densities. Historical note:
    /// the dense grid's cell-count policy shipped an 18:1 stretched terrain and a
    /// 384-cell cap — both retired by the sparse field, cells are cubic by
    /// construction and there is nothing left to warn about.
    #[test]
    fn preview_scales_with_surface_not_volume() {
        // Thickness must be (nearly) free — the interior collapses to sentinels.
        let (thin, _) = new_terrain_preview(200.0, 5.0, 1.5);
        let (thick, _) = new_terrain_preview(200.0, 500.0, 1.5);
        assert_eq!(thin, thick, "thickness must not change the estimate");
        // Finer voxels = more chunks over the same footprint; both finite and > 0.
        let (coarse, mb_c) = new_terrain_preview(578.0, 12.0, 3.0);
        let (fine, mb_f) = new_terrain_preview(578.0, 12.0, 0.75);
        assert!(fine > coarse, "finer voxels must cost more chunks ({fine} vs {coarse})");
        assert!(mb_f > mb_c && mb_f.is_finite() && mb_c > 0.0);
        // Ty's real 578-unit map at the default density: single-digit-to-tens of MB
        // resident — versus the 192 MB the dense field cost.
        let (_, mb) = new_terrain_preview(578.0, 12.0, 1.5);
        assert!(mb < 64.0, "{mb:.0} MB estimate is too much for one terrain");
    }
}
