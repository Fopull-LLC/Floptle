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
        let terrain_detail = &mut *self.terrain_detail;
        let terrain_textures = &mut *self.terrain_textures;
        let materials = self.materials;
        let asset_tree = self.asset_tree;
        let terrain_present = self.terrain_present;
        let terrain_voxels = self.terrain_voxels;

        // Detail (resolution) — higher = finer terrain, but heavier.
        ui.horizontal(|ui| {
            ui.label("detail");
            egui::ComboBox::from_id_salt("terrain_detail")
                .selected_text(match *terrain_detail {
                    d if d <= 48 => "Low",
                    d if d <= 80 => "Medium",
                    d if d <= 112 => "High",
                    _ => "Ultra",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut *terrain_detail, 40, "Low");
                    ui.selectable_value(&mut *terrain_detail, 64, "Medium");
                    ui.selectable_value(&mut *terrain_detail, 96, "High");
                    ui.selectable_value(&mut *terrain_detail, 144, "Ultra");
                });
        });
        if let Some((n, total)) = terrain_voxels {
            ui.small(format!("{n} volume(s) · {total} voxels (native per-volume)"));
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
        ui.add(egui::Slider::new(&mut terrain_brush.radius, 0.5..=8.0).text("radius"));
        ui.add(egui::Slider::new(&mut terrain_brush.strength, 0.05..=1.0).text("strength"));
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
            for (slot, tex) in terrain_textures.iter_mut().enumerate() {
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
                                if ui.selectable_label(*tex == *p, n).clicked() {
                                    *tex = p.clone();
                                    cmd.terrain_palette_changed = true;
                                }
                            }
                        });
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
