//! The scene's textures are warmed only when something they depend on moved.
//!
//! `ensure_scene_textures` registers every image the scene's materials name
//! before the frame's gathers look them up. It ran over every `Material`,
//! `ObjectMaterials`, `MaterialPatch` and tilemap on every frame, to find on
//! almost every frame that there was nothing to do: 0.4 to 0.7 ms of `assets`
//! on a steady frame of a large game. It now reads the world's change log and
//! the registry's eviction count, and looks only when one moved, at only what
//! moved. These are the paths that must still reach it.

use floptle_core::{Material, MaterialPatch, Matter, ObjectMaterials, Transform};

/// A node wearing a material with `texture`.
fn node(ed: &mut crate::Editor, texture: &str) -> floptle_core::Entity {
    let e = ed.world.spawn();
    ed.world.insert(e, Transform::IDENTITY);
    ed.world.insert(e, Matter::Primitive { shape: floptle_core::Shape::Cube, color: [1.0; 3] });
    ed.world.insert(e, Material { texture: Some(texture.into()), ..Default::default() });
    e
}

/// One frame's warm-up, as the frame calls it.
fn frame(ed: &mut crate::Editor) {
    ed.frame_no += 1;
    ed.ensure_scene_textures();
}

/// Whether the last frame asked for `path`. With no GPU nothing can be
/// registered, so a path that was asked for is left among the missing.
fn asked(ed: &crate::Editor, path: &str) -> bool {
    ed.scene_tex_gate.missing.iter().any(|p| p == path)
}

/// Pretend every path the scene names is already on the GPU.
fn register_all(ed: &mut crate::Editor) {
    for (i, p) in std::mem::take(&mut ed.scene_tex_gate.missing).into_iter().enumerate() {
        ed.texture_registry.insert(p, floptle_render::TexId(i as u32 + 1));
    }
}

/// **A quiet frame reads no material, and every kind of change is still
/// seen.** An Inspector-style edit through the world, a material put back
/// whole (an undo, a paste), a model override, a script's patch, a tilemap,
/// a texture dropped from the registry (reloaded from disk, given new import
/// settings) and a new scene: each makes the next frame ask for what it now
/// needs, and nothing else does.
#[test]
fn a_quiet_frame_reads_no_material_and_every_change_is_seen() {
    let mut ed = crate::Editor::default();
    let nodes: Vec<_> = (0..50).map(|i| node(&mut ed, &format!("textures/t{i}.png"))).collect();
    frame(&mut ed);
    assert!(asked(&ed, "textures/t7.png"), "the first frame did not look at the scene");
    register_all(&mut ed);
    frame(&mut ed);
    let looks = ed.scene_tex_gate.looks;
    for _ in 0..5 {
        frame(&mut ed);
    }
    assert_eq!(ed.scene_tex_gate.looks, looks, "a quiet frame read the materials again");
    assert!(ed.scene_tex_gate.missing.is_empty());

    // A transform moving is not a material changing.
    if let Some(t) = ed.world.get_mut::<Transform>(nodes[3]) {
        t.translation.x += 1.0;
    }
    frame(&mut ed);
    assert_eq!(ed.scene_tex_gate.looks, looks, "a node moving made it look");

    // The Inspector writes through `get_mut`.
    if let Some(m) = ed.world.get_mut::<Material>(nodes[3]) {
        m.normal_map = Some("textures/edited_normal.png".into());
    }
    frame(&mut ed);
    assert!(asked(&ed, "textures/edited_normal.png"), "an Inspector-style edit was not seen");
    assert_eq!(ed.scene_tex_gate.looks, looks + 1);

    // An undo or a paste puts a whole component back.
    ed.world.insert(nodes[4], Material { texture: Some("textures/undone.png".into()), ..Default::default() });
    frame(&mut ed);
    assert!(asked(&ed, "textures/undone.png"), "a material put back whole was not seen");

    // A model's per-part override.
    let mut parts = std::collections::BTreeMap::new();
    parts.insert("Hull".to_string(), Material { texture: Some("textures/hull.png".into()), ..Default::default() });
    ed.world.insert(nodes[5], ObjectMaterials(parts));
    frame(&mut ed);
    assert!(asked(&ed, "textures/hull.png"), "an override was not seen");

    // A script's patch over a model's own materials.
    let mut patch = MaterialPatch::default();
    patch.mat.roughness_map = Some("textures/patched.png".into());
    patch.keys.push("roughnessMap".into());
    ed.world.insert(nodes[6], patch);
    frame(&mut ed);
    assert!(asked(&ed, "textures/patched.png"), "a patch was not seen");

    // Still asked for every frame while it cannot be registered, as before.
    frame(&mut ed);
    assert!(asked(&ed, "textures/patched.png"), "a path that would not load was given up on");
    register_all(&mut ed);
    frame(&mut ed);
    assert!(ed.scene_tex_gate.missing.is_empty());

    // A texture dropped from the registry is wanted again by what names it.
    ed.texture_registry.remove("textures/t9.png");
    frame(&mut ed);
    assert!(asked(&ed, "textures/t9.png"), "a reloaded texture was never registered again");
    register_all(&mut ed);

    // A new scene is a new world.
    let mut fresh = floptle_core::World::new();
    let e = fresh.spawn();
    fresh.insert(e, Material { texture: Some("textures/next_scene.png".into()), ..Default::default() });
    ed.world = fresh;
    frame(&mut ed);
    assert!(asked(&ed, "textures/next_scene.png"), "a new scene's materials were not looked at");
}

/// **A tileset's sheets are warmed when a tilemap arrives, and when a sheet
/// is added to the tileset later**, which changes the tileset rather than
/// the world.
#[test]
fn a_tilemaps_sheets_are_asked_for_when_it_arrives_and_when_one_is_added() {
    let mut ed = crate::Editor::default();
    let ts = floptle_tiles::TileSet { texture: "textures/sheet_a.png".into(), sheet_cols: 4, sheet_rows: 4, ..Default::default() };
    ed.tiles.sets.insert("tiles/ground.tileset.ron".into(), ts);
    frame(&mut ed);
    assert!(!asked(&ed, "textures/sheet_a.png"), "a tileset no tilemap uses was warmed");
    let e = ed.world.spawn();
    ed.world.insert(e, Transform::IDENTITY);
    ed.world.insert(
        e,
        Matter::Tilemap { cols: 4, rows: 4, tile: 1.0, data: vec![0; 16], tileset: "tiles/ground.tileset.ron".into() },
    );
    frame(&mut ed);
    assert!(asked(&ed, "textures/sheet_a.png"), "a tilemap's sheet was not asked for");
    register_all(&mut ed);
    frame(&mut ed);
    let looks = ed.scene_tex_gate.looks;
    if let Some(set) = ed.tiles.get_mut("tiles/ground.tileset.ron") {
        set.pages.push(floptle_tiles::TilePage { texture: "textures/sheet_b.png".into(), cols: 4, rows: 4 });
    }
    frame(&mut ed);
    assert!(asked(&ed, "textures/sheet_b.png"), "a sheet added to the tileset was not asked for");
    assert_eq!(ed.scene_tex_gate.looks, looks, "a sheet added made it read every material");
}

/// **A script's `node:setMaterial` reaches it.** The script writes during the
/// play step; the frame after asks for the image it named.
#[test]
fn a_scripts_set_material_is_seen() {
    let dir = std::env::temp_dir().join(format!("floptle-texgate-script-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    floptle_vfs::create_dir_all(dir.join("scripts")).unwrap();
    floptle_vfs::write(
        dir.join("scripts/paint.lua"),
        "local n = 0\nfunction update(node)\n  n = n + 1\n  if n == 3 then node:setMaterial{ texture = \"textures/scripted.png\" } end\nend\n",
    )
    .unwrap();
    let mut ed = crate::Editor { project_root: dir.clone(), ..Default::default() };
    let e = node(&mut ed, "textures/start.png");
    ed.world.insert(e, floptle_core::Scripts(vec![floptle_core::ScriptInst::new("paint")]));
    ed.toggle_play();
    assert!(ed.playing);
    let mut seen = false;
    for _ in 0..8 {
        ed.play_step(1.0 / 60.0, true);
        frame(&mut ed);
        register_all(&mut ed);
        seen |= ed.texture_registry.contains_key("textures/scripted.png");
    }
    assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
    assert!(seen, "the script's new texture was never asked for");
    let _ = std::fs::remove_dir_all(&dir);
}

/// What the frame's `assets` bucket does on a steady frame, `rounds` times,
/// in ms per frame.
fn steady_assets_ms(ed: &mut crate::Editor, rounds: usize) -> f64 {
    let t = std::time::Instant::now();
    for _ in 0..rounds {
        ed.frame_no += 1;
        ed.ensure_vfx_assets();
        ed.ensure_scene_textures();
        ed.ensure_flsl_materials();
        ed.ensure_ui_shaders();
        ed.ensure_post_shaders();
    }
    t.elapsed().as_secs_f64() * 1e3 / rounds as f64
}

/// A scene of `n` nodes, each with a texture and two maps of its own, every
/// one of them already registered.
fn steady_scene(n: usize) -> crate::Editor {
    let mut ed = crate::Editor::default();
    for i in 0..n {
        let e = node(&mut ed, &format!("textures/t{i}.png"));
        if let Some(m) = ed.world.get_mut::<Material>(e) {
            m.normal_map = Some(format!("textures/n{i}.png"));
            m.roughness_map = Some(format!("textures/r{i}.png"));
        }
    }
    frame(&mut ed);
    register_all(&mut ed);
    frame(&mut ed);
    ed
}

/// **A steady frame's `assets` work does not grow with the materials in the
/// scene.** Measured at 500 and 4,000 nodes, interleaved, best of several:
/// a walk over every material is eight times the work at the larger size,
/// and a gated frame is the same work at both. The sizes alternate so the
/// two sides see the same clock, and only a growth far past noise fails it.
#[test]
fn a_steady_frames_assets_do_not_grow_with_the_materials() {
    let (mut small, mut large) = (steady_scene(500), steady_scene(4_000));
    let looks = (small.scene_tex_gate.looks, large.scene_tex_gate.looks);
    let (mut s, mut l) = (f64::MAX, f64::MAX);
    for _ in 0..5 {
        s = s.min(steady_assets_ms(&mut small, 200));
        l = l.min(steady_assets_ms(&mut large, 200));
    }
    assert_eq!(
        (small.scene_tex_gate.looks, large.scene_tex_gate.looks),
        looks,
        "a steady frame looked at the materials"
    );
    let growth = l / s.max(1e-6);
    assert!(
        growth < 3.0,
        "a steady frame's assets cost {s:.4} ms at 500 nodes and {l:.4} ms at 4,000: {growth:.1}x, \
         where a frame that walks every material is about 8x"
    );
}

/// **On a real GPU, a material changed by an Inspector-style edit, by a
/// script's `node:setMaterial`, and a texture reloaded from disk all end up
/// drawn with a registered texture** — the gate holds back only frames where
/// nothing changed.
#[test]
#[cfg(feature = "editor-ui")]
fn a_changed_material_is_registered_on_the_gpu() {
    let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
    let dir = std::env::temp_dir().join(format!("floptle-texgate-gpu-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    floptle_vfs::create_dir_all(dir.join("textures")).unwrap();
    floptle_vfs::create_dir_all(dir.join("scripts")).unwrap();
    for name in ["a", "b", "c"] {
        image::RgbaImage::from_pixel(4, 4, image::Rgba([200, 40, 40, 255])).save(dir.join(format!("textures/{name}.png"))).unwrap();
    }
    floptle_vfs::write(
        dir.join("scripts/paint.lua"),
        "local n = 0\nfunction update(node)\n  n = n + 1\n  if n == 2 then node:setMaterial{ texture = \"textures/c.png\" } end\nend\n",
    )
    .unwrap();
    ed.project_root = dir.clone();
    let e = node(&mut ed, "textures/a.png");
    frame(&mut ed);
    assert!(ed.texture_registry.contains_key("textures/a.png"), "the scene's texture was not registered");
    let looks = ed.scene_tex_gate.looks;
    frame(&mut ed);
    assert_eq!(ed.scene_tex_gate.looks, looks, "a quiet frame looked again");

    if let Some(m) = ed.world.get_mut::<Material>(e) {
        m.texture = Some("textures/b.png".into());
    }
    frame(&mut ed);
    assert!(ed.texture_registry.contains_key("textures/b.png"), "an Inspector-style edit's texture was not registered");

    ed.world.insert(e, floptle_core::Scripts(vec![floptle_core::ScriptInst::new("paint")]));
    ed.toggle_play();
    for _ in 0..4 {
        ed.play_step(1.0 / 60.0, true);
        frame(&mut ed);
    }
    assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
    assert!(ed.texture_registry.contains_key("textures/c.png"), "a script's setMaterial texture was not registered");

    // Saved again on disk: the hot reload drops it, and it comes back.
    ed.invalidate_texture(&dir.join("textures/c.png"));
    assert!(!ed.texture_registry.contains_key("textures/c.png"));
    ed.play_step(1.0 / 60.0, true);
    frame(&mut ed);
    assert!(ed.texture_registry.contains_key("textures/c.png"), "a reloaded texture was not registered again");
    let _ = std::fs::remove_dir_all(&dir);
}
