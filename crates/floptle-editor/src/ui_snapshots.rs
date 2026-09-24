//! Pictures of editor UI, rendered offscreen to PNGs under `target/ui-snapshots/`
//! so a visual change can be looked at rather than reasoned about.
//!
//! `#[ignore]`d — they need a GPU adapter. Run with
//! `cargo test -p floptle-editor --lib ui_snapshots -- --ignored --nocapture`.

use crate::assets::asset_kind;
use crate::assets_ui::{TilePicture, asset_tile};

fn out_path(name: &str) -> std::path::PathBuf {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/ui-snapshots");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(format!("{name}.png"))
}

fn harness<'a>(size: egui::Vec2, ui: impl FnMut(&mut egui::Ui) + 'a) -> egui_kittest::Harness<'a> {
    let mut h = egui_kittest::Harness::builder().with_size(size).build_ui(ui);
    h.ctx.set_fonts(crate::fonts::definitions(&[]));
    h.ctx.set_visuals(egui::Visuals::dark());
    h.run();
    h
}

/// Every asset kind as a grid tile, plus a texture and two materials.
#[test]
#[ignore]
fn snapshot_asset_tiles() {
    let files = [
        "crate.glb", "player.lua", "sky.flimg", "hero.spriteanim.ron", "walk.anim.ron",
        "hero.actl.ron", "spark.vfx.ron", "tree.prefab.ron", "water.flsl", "step.ogg",
        "level.map.ron", "Pixel.ttf", "scenes/level.ron", "config.ron", "README.md", "notes.bin",
    ];
    let mut tex: Option<egui::TextureHandle> = None;
    let mut h = harness(egui::vec2(620.0, 440.0), move |ui| {
        let t = tex
            .get_or_insert_with(|| {
                let img = egui::ColorImage::from_rgba_unmultiplied(
                    [16, 16],
                    &(0..256)
                        .flat_map(|i| if (i / 16 + i % 16) % 2 == 0 { [70, 150, 90, 255] } else { [150, 210, 120, 255] })
                        .collect::<Vec<u8>>(),
                );
                ui.ctx().load_texture("checker", img, egui::TextureOptions::NEAREST)
            })
            .clone();
        ui.horizontal_wrapped(|ui| {
            asset_tile(ui, asset_kind("grass.png"), TilePicture::Image(t.clone()), "grass.png", false);
            asset_tile(ui, asset_kind("materials/Brick.ron"), TilePicture::Swatch { color: [0.8, 0.35, 0.25], tex: None }, "Brick.ron", true);
            asset_tile(ui, asset_kind("materials/Moss.ron"), TilePicture::Swatch { color: [0.9, 1.0, 0.9], tex: Some(t.clone()) }, "Moss.ron", false);
            for f in files {
                asset_tile(ui, asset_kind(f), TilePicture::Glyph, f.rsplit('/').next().unwrap(), false);
            }
        });
    });
    let out = out_path("asset-tiles");
    h.run();
    h.render().expect("no GPU?").save(&out).unwrap();
    floptle_say::say!("wrote {}", out.display());
}

/// The Move gizmo, at rest and with its centre knob hovered.
#[test]
#[ignore]
fn snapshot_move_gizmo() {
    use crate::gizmo::{GizmoFrame, Handle, Tool, paint_gizmo};
    use floptle_core::math::Vec2;
    let frame = |c: Vec2, hovered| GizmoFrame {
        center: c,
        tips: [Some(c + Vec2::new(90.0, 20.0)), Some(c + Vec2::new(0.0, -90.0)), Some(c + Vec2::new(-55.0, 50.0))],
        neg_tips: [None; 3],
        box_edges: Vec::new(),
        ring_pts: Default::default(),
        ring_front: Default::default(),
        center_ring: Vec::new(),
        hovered,
    };
    let mut h = harness(egui::vec2(400.0, 200.0), move |ui| {
        ui.painter().rect_filled(ui.max_rect(), 0.0, egui::Color32::from_rgb(60, 66, 74));
        paint_gizmo(ui.painter(), &frame(Vec2::new(110.0, 120.0), None), Tool::Move, None, 1.0);
        paint_gizmo(ui.painter(), &frame(Vec2::new(290.0, 120.0), Some(Handle::Center)), Tool::Move, None, 1.0);
    });
    let out = out_path("move-gizmo");
    h.run();
    h.render().expect("no GPU?").save(&out).unwrap();
    floptle_say::say!("wrote {}", out.display());
}

/// Material chips as the Inspector and the Model tab show them: a material of
/// its own, one following a project material, and a selected part.
#[test]
#[ignore]
fn snapshot_material_chips() {
    use crate::material_view::{chip_subtitle, material_chip};
    use floptle_core::Material;
    let own = Material { color: [0.3, 0.55, 0.9], ..Default::default() };
    let linked = Material { color: [0.8, 0.35, 0.25], source: Some("materials/Brick.ron".into()), ..Default::default() };
    let mut h = harness(egui::vec2(360.0, 170.0), move |ui| {
        ui.add_space(4.0);
        material_chip(ui, &own, None, "Material", &chip_subtitle(&own), false);
        material_chip(ui, &linked, None, "Brick", &chip_subtitle(&linked), false);
        material_chip(ui, &linked, None, "Roof", &chip_subtitle(&linked), true);
    });
    let out = out_path("material-chips");
    h.run();
    h.render().expect("no GPU?").save(&out).unwrap();
    floptle_say::say!("wrote {}", out.display());
}
