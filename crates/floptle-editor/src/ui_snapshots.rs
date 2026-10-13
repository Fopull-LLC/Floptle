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
    h.ctx.set_fonts(crate::fonts::definitions(&floptle_theme::default_theme(), &[]));
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

/// The Docs tab's two-pane layout with a real guide page in it.
#[test]
#[ignore]
fn snapshot_docs_layout() {
    use crate::ide::{DOC_SECTIONS, docs_two_pane};
    let mut h = harness(egui::vec2(900.0, 420.0), move |ui| {
        docs_two_pane(
            ui,
            "snap",
            |ui, _| {
                for (i, (_, title, _)) in DOC_SECTIONS.iter().take(14).enumerate() {
                    let _ = ui.selectable_label(i == 10, *title);
                }
            },
            |ui| {
                let (chapter, title, body) = DOC_SECTIONS[10];
                ui.small(chapter);
                ui.heading(title);
                ui.label(body);
            },
        );
    });
    let out = out_path("docs-layout");
    h.run();
    h.render().expect("no GPU?").save(&out).unwrap();
    floptle_say::say!("wrote {}", out.display());
}

/// The Input settings' editor on the starter map: the Move direction open in a
/// wide dock, and the Jump action open in a narrow one.
#[test]
#[ignore]
fn snapshot_input_editor() {
    use crate::input_editor::{InputUiState, Sel, editor};
    let map = floptle_input::InputMap::starter();
    let scan = crate::input_scan::InputScan::default();
    let test = floptle_input::ActionState::default();
    for (name, size, sel) in [
        ("input-editor-wide", egui::vec2(900.0, 900.0), Sel::Axis2("Move".into())),
        ("input-editor-narrow", egui::vec2(340.0, 1500.0), Sel::Action("Jump".into())),
    ] {
        let mut state = InputUiState::default();
        state.selected = Some(sel);
        let map = map.clone();
        let mut h = harness(size, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                let mut edits = crate::input_ui::InputEdits::default();
                editor(ui, &map, &mut state, None, &scan, &test, "", &mut edits);
            });
        });
        let out = out_path(name);
        h.run();
        h.render().expect("no GPU?").save(&out).unwrap();
        floptle_say::say!("wrote {}", out.display());
    }
}

/// The ◫ Tiles panel for a 2D game's first minute: a bare layer asking for its
/// spritesheet, and the same layer once one has been dropped on it.
#[test]
#[ignore]
fn snapshot_tiles_panel() {
    use crate::tile_edit::{TileStore, TileTools};
    use crate::tile_ui::TileCtx;
    let root = std::env::temp_dir().join(format!("floptle-tiles-snap-{}", std::process::id()));
    std::fs::create_dir_all(root.join("textures")).unwrap();
    // A 4×2 sheet of 16-px tiles, each its own colour, so the palette is legible.
    let (w, h) = (64u32, 32u32);
    let mut px = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let cell = (y / 16) * 4 + x / 16;
            let edge = x % 16 == 0 || y % 16 == 0;
            let o = ((y * w + x) * 4) as usize;
            let c = if edge { [20, 20, 30] } else { [40 + cell as u8 * 25, 160 - cell as u8 * 12, 90 + cell as u8 * 18] };
            px[o..o + 4].copy_from_slice(&[c[0], c[1], c[2], 255]);
        }
    }
    floptle_image::io::save_png(&root.join("textures/Tiles1.png"), &px, w, h).unwrap();
    for (name, tileset) in [("tiles-panel-bare", ""), ("tiles-panel-with-sheet", "tilesets/Tiles1.tileset.ron")] {
        let mut world = floptle_core::World::default();
        let e = world.spawn();
        world.insert(e, floptle_core::Name("Tiles 1".into()));
        world.insert(
            e,
            floptle_core::Matter::Tilemap { cols: 16, rows: 16, tile: 1.0, data: vec![], tileset: tileset.into() },
        );
        let mut store = TileStore::default();
        if !tileset.is_empty() {
            store.sets.insert(
                tileset.into(),
                floptle_tiles::TileSet {
                    name: "Tiles1".into(),
                    texture: "textures/Tiles1.png".into(),
                    sheet_cols: 4,
                    sheet_rows: 2,
                    ..Default::default()
                },
            );
        }
        let mut tools = TileTools { layer: Some(e), ..Default::default() };
        let mut cmds = Vec::new();
        let mut hn = harness(egui::vec2(360.0, 1100.0), |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                TileCtx {
                    store: &mut store,
                    tools: &mut tools,
                    world: &world,
                    project_root: &root,
                    cmds: &mut cmds,
                    playing: false,
                }
                .ui(ui);
            });
        });
        hn.run();
        hn.run();
        let out = out_path(name);
        hn.render().expect("no GPU?").save(&out).unwrap();
        floptle_say::say!("wrote {}", out.display());
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// The Rotate gizmo on a 3D node and on a flat (2D) node, seen from the front
/// as a 2D scene is.
#[test]
#[ignore]
fn snapshot_flat_rotate_gizmo() {
    use crate::gizmo::{Tool, build_gizmo, paint_gizmo};
    use floptle_core::math::{DVec3, Mat4, Vec2};
    let mut world = floptle_core::World::new();
    let e = world.spawn();
    world.insert(e, floptle_core::transform::Transform { translation: DVec3::new(0.0, 0.0, -10.0), ..Default::default() });
    let vp = Mat4::perspective_rh(60f32.to_radians(), 1.0, 0.1, 1000.0);
    let shift = |mut g: crate::gizmo::GizmoFrame, dx: f32| {
        let d = Vec2::new(dx, 0.0);
        g.center += d;
        for r in &mut g.ring_pts {
            r.iter_mut().for_each(|p| *p += d);
        }
        g.center_ring.iter_mut().for_each(|p| *p += d);
        g.tips.iter_mut().flatten().for_each(|p| *p += d);
        g
    };
    let g3 = shift(build_gizmo(Tool::Rotate, Some(e), &world, None, DVec3::ZERO, vp, 400.0, 400.0, None, None, false).unwrap(), -100.0);
    let g2 = shift(build_gizmo(Tool::Rotate, Some(e), &world, None, DVec3::ZERO, vp, 400.0, 400.0, None, None, true).unwrap(), 100.0);
    let mut h = harness(egui::vec2(400.0, 400.0), move |ui| {
        ui.painter().rect_filled(ui.max_rect(), 0.0, egui::Color32::from_rgb(60, 66, 74));
        paint_gizmo(ui.painter(), &g3, Tool::Rotate, None, 1.0);
        paint_gizmo(ui.painter(), &g2, Tool::Rotate, None, 1.0);
    });
    let out = out_path("flat-rotate-gizmo");
    h.run();
    h.render().expect("no GPU?").save(&out).unwrap();
    floptle_say::say!("wrote {}", out.display());
}
