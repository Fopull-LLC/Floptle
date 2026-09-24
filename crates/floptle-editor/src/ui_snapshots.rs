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
    println!("wrote {}", out.display());
}
