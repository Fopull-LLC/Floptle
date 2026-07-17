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

/// Cells + voxel edge a "New terrain" config will actually produce — shown live in the
/// dialog, because the size/detail pair is what silently decides quality and the old UI
/// never revealed it.
pub(crate) fn new_terrain_preview(size_xz: f32, thickness: f32, detail: u32) -> ([u32; 3], f32) {
    let size = [size_xz.max(0.1), thickness.max(0.1), size_xz.max(0.1)];
    let dims = terrain_dims_for_size(size, detail);
    let edge = |i: usize| size[i] / dims[i].max(1) as f32;
    (dims, edge(0).max(edge(1)).max(edge(2)))
}

/// The voxel grid for a slab of `size` (full extents), at `detail`. THE single
/// resolution policy — the New-terrain preview and the real `create_terrain` both call
/// it, so the number the dialog shows is the number you get.
pub(crate) fn terrain_dims_for_size(size: [f32; 3], detail: u32) -> [u32; 3] {
    const MAX_DIM: u32 = 384;
    let d = detail.clamp(24, 192) as f32;
    let size = [size[0].max(0.1), size[1].max(0.1), size[2].max(0.1)];
    let longest = size[0].max(size[1]).max(size[2]).max(0.001);
    let shortest = size[0].min(size[1]).min(size[2]).max(0.001);
    // Three constraints on ONE voxel edge, so cells come out cubic:
    //   * detail asks for `longest/d`;
    //   * the THINNEST axis needs ≥ ~8 cells to hold a surface at all, so the edge can
    //     never exceed shortest/8 — ignoring this is what forced the 8-cell floor to
    //     re-introduce stretch on a wide, thin slab;
    //   * nothing finer than MAX_DIM on the longest axis is affordable.
    let vs = (longest / d).min(shortest / 8.0).max(longest / MAX_DIM as f32).max(1e-4);
    [
        ((size[0] / vs).round() as u32).clamp(8, MAX_DIM),
        ((size[1] / vs).round() as u32).clamp(8, MAX_DIM),
        ((size[2] / vs).round() as u32).clamp(8, MAX_DIM),
    ]
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
        let terrain_detail = &mut *self.terrain_detail;
        let terrain_textures = &mut *self.terrain_textures;
        let materials = self.materials;
        let asset_tree = self.asset_tree;
        let terrain_present = self.terrain_present;
        let terrain_voxels = self.terrain_voxels;
        let terrain_worst_voxel = self.terrain_worst_voxel;

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
        // Voxel size decides whether sculpted terrain reads as a surface or as a
        // visible lattice — so it is STATED. It being invisible is how a
        // 9.17 × 0.50 × 9.17 terrain got authored without anyone being warned.
        if let Some((v, aniso)) = terrain_worst_voxel {
            let coarse = v[0].max(v[1]).max(v[2]);
            let txt = format!("voxel {:.2} × {:.2} × {:.2}", v[0], v[1], v[2]);
            if coarse > 1.5 || aniso > 2.0 {
                ui.colored_label(egui::Color32::from_rgb(235, 170, 90), format!("⚠ {txt}"))
                    .on_hover_text(
                        "Coarse or stretched voxels show up as dark lattice lines and \
                         terraced steps on sculpted ground — that IS the grid, not a \
                         shading bug. Prefer several smaller blended terrains over one \
                         huge one; overlapping terrains fuse.",
                    );
                if aniso > 2.0 {
                    ui.small(format!("   stretched {aniso:.0}:1 — cells this uneven facet badly"));
                }
            } else {
                ui.small(txt);
            }
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

#[cfg(test)]
mod tests {
    use super::new_terrain_preview;

    /// The shipped bug, in one assertion. `terrain_dims` was `[d, d*3/8, d]` — a fixed
    /// cell count with a hardcoded 8:3 aspect that never looked at the slab's real
    /// shape. Ty's terrain came out 578 × 12 units at 64 × 24 × 64 cells, i.e. voxels of
    /// 9.17 × 0.50 × 9.17 — stretched **18:1**. Trilinear interpolation across cells
    /// that uneven is visibly faceted: dark lattice lines across the surface and
    /// terraced steps edge-on. Cells must be cubic whatever the slab's proportions.
    #[test]
    fn cells_stay_cubic_for_any_slab_shape() {
        // NOTE: extreme aspects (e.g. 4000 × 0.5) genuinely cannot have cubic cells
        // inside the 384-cell cap — 8 cells across 0.5 units forces a 0.06 edge while
        // 4000 units can't go below ~10. That's a limit of a dense grid, not a policy
        // bug; the dialog warns instead of pretending. These are the shapes that CAN.
        for &(size_xz, thickness) in &[
            (578.0f32, 12.0f32), // the real-world case that broke
            (16.0, 6.0),
            (128.0, 20.0),
            (2.0, 200.0), // a tall column
            (1.0, 1.0),
        ] {
            for detail in [24u32, 40, 64, 96, 144, 192] {
                let (dims, vs) = new_terrain_preview(size_xz, thickness, detail);
                let edge = |axis: usize, full: f32| full / dims[axis] as f32;
                let (ex, ey, ez) = (edge(0, size_xz), edge(1, thickness), edge(2, size_xz));
                let aniso = ex.max(ey).max(ez) / ex.min(ey).min(ez);
                // Cells are cubic up to one ceil() of rounding per axis — the slack has
                // to be generous for extreme aspects clamped by the 8-cell floor.
                // 1.6 covers rounding. Extreme aspects can't do better than the
                // MAX_DIM clamp allows (a 2 × 200 column lands ~2.1:1) — a dense grid
                // limit, not a policy bug, and nothing like the 18:1 that shipped.
                assert!(
                    aniso < 2.5,
                    "{size_xz}×{thickness} @ detail {detail}: cells stretched {aniso:.1}:1 \
                     (dims {dims:?}, voxel {vs:.3}) — the old policy shipped 18:1"
                );
                assert!(dims.iter().all(|&d| (8..=384).contains(&d)), "dims {dims:?} out of range");
                assert!(vs > 0.0 && vs.is_finite(), "voxel size {vs}");
            }
        }
    }

    /// Detail must actually buy resolution — the old policy's count was independent of
    /// size, so the slider did nothing on a big terrain.
    #[test]
    fn more_detail_means_smaller_voxels() {
        let mut prev = f32::INFINITY;
        for detail in [24u32, 40, 64, 96, 144, 192] {
            let (_, vs) = new_terrain_preview(200.0, 60.0, detail);
            assert!(vs <= prev, "detail {detail}: voxel {vs} grew (was {prev})");
            prev = vs;
        }
        assert!(prev < 200.0 / 24.0, "max detail must beat min detail");
    }

    /// Ty's terrain, as authored. The old policy gave 9.17 × 0.50 × 9.17 — stretched
    /// 18:1, which is the dark lattice he photographed.
    #[test]
    fn the_reported_terrain_is_no_longer_stretched() {
        let (dims, vs) = new_terrain_preview(578.0, 12.0, 64);
        let ex = 578.0 / dims[0] as f32;
        let ey = 12.0 / dims[1] as f32;
        let aniso = ex.max(ey) / ex.min(ey);
        assert!(aniso < 1.6, "still stretched {aniso:.1}:1 (dims {dims:?})");
        assert!(vs < 2.0, "voxel {vs:.2} — was 9.17 under the old policy");
        // Still affordable: the whole point is that cubic cells here aren't extravagant.
        let mb = dims.iter().map(|&d| d as u64).product::<u64>() as f64 * 8.0 / 1.0e6;
        assert!(mb < 64.0, "{mb:.0} MB is too much for one terrain");
    }
}
