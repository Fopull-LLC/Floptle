//! **Plants draw like plants.** A material with `sway` bends in the game's
//! wind: the wind a script declares with `weather.addWind` reaches the vertex
//! stage, so a blade whose material sways leans downwind at its tip and stays
//! rooted at its base, and the same blade with no wind stands straight. A
//! material with `backlight` lets the sun on its back through: a leaf against
//! the sun glows where a plain surface is in its own shade.

#![cfg(feature = "editor-ui")]

use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_floptle")
}

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("flshotsway-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// A camera, a ground, a script that declares `wind` (or none), and one tall
/// green blade at the origin whose material sways.
fn project(dir: &Path, wind: Option<&str>) {
    let out = Command::new(bin()).args(["new", &dir.to_string_lossy()]).output().expect("run floptle new");
    assert!(out.status.success(), "scaffold failed: {}", String::from_utf8_lossy(&out.stderr));
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    std::fs::write(dir.join("scripts/windy.lua"), wind.map(|w| format!("weather.addWind{{ velocity = {w} }}\n")).unwrap_or_default())
        .unwrap();
    let scene = r#"(name: "sway", nodes: [
        (name: "Camera", transform: (translation: (0.0, 1.5, 6.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)), matter: Camera(fov_y: 1.0, active: true), scripts: []),
        (name: "Weather", transform: (translation: (0.0, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)), matter: Empty, scripts: [(kind: "windy")]),
        (name: "Blade", transform: (translation: (0.0, 1.5, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (0.15, 3.0, 0.15)), matter: Primitive(shape: Cube, color: (0.1, 0.9, 0.1)), scripts: [], material: Some((color: (0.1, 0.9, 0.1), unlit: true, sway: 0.6))),
    ])"#;
    std::fs::write(dir.join("scenes/sway.ron"), scene).unwrap();
}

/// The mean x of the blade's green pixels in rows `rows` of the picture.
fn blade_x(img: &image::RgbaImage, rows: std::ops::Range<u32>) -> Option<f32> {
    let (mut sum, mut n) = (0.0, 0.0);
    for y in rows {
        for x in 0..img.width() {
            let p = img.get_pixel(x, y);
            if p[1] > 150 && p[0] < 100 && p[2] < 100 {
                sum += x as f32;
                n += 1.0;
            }
        }
    }
    (n > 0.0).then(|| sum / n)
}

fn shoot(wind: Option<&str>, tag: &str) -> Option<image::RgbaImage> {
    let d = temp(tag);
    project(&d, wind);
    let out = d.join("sway.png");
    let r = Command::new(bin())
        .args(["shot", &d.to_string_lossy(), "--scene", "sway", "--size", "320x180", "--after", "0.5"])
        .args(["--out", &out.to_string_lossy()])
        .output()
        .expect("run shot");
    if !r.status.success() {
        let why = String::from_utf8_lossy(&r.stderr);
        assert!(why.contains("no GPU") || why.contains("could not build the renderer"), "shot failed:\n{why}");
        let _ = std::fs::remove_dir_all(&d);
        return None;
    }
    let img = image::open(&out).expect("a PNG was written").to_rgba8();
    let _ = std::fs::remove_dir_all(&d);
    Some(img)
}

#[test]
fn a_swaying_blade_leans_downwind_and_stays_rooted() {
    let (Some(still), Some(windy)) = (shoot(None, "still"), shoot(Some("vec3(10, 0, 0)"), "windy")) else { return };
    // The tip is the top of the blade and the root the last rows it reaches,
    // both in view. The blade is a box, so it shears straight from its root.
    let rows = |img: &image::RgbaImage| {
        let green = |y| blade_x(img, y..y + 1).is_some();
        let top = (0..img.height()).find(|&y| green(y)).expect("a blade");
        let bottom = (0..img.height()).rev().find(|&y| green(y)).expect("a blade");
        (top..top + 6, bottom - 2..bottom + 1)
    };
    let ((tips, roots), (tipw, rootw)) = (rows(&still), rows(&windy));
    let (tip_still, tip_windy) = (blade_x(&still, tips).unwrap(), blade_x(&windy, tipw).unwrap());
    let (root_still, root_windy) = (blade_x(&still, roots).unwrap(), blade_x(&windy, rootw).unwrap());
    assert!(tip_windy > tip_still + 20.0, "the tip did not lean downwind: {tip_still} → {tip_windy}");
    assert!((root_windy - root_still).abs() < 4.0, "the root moved: {root_still} → {root_windy}");
}

/// **A backlit leaf glows against the sun.** Two thin leaves with the sun
/// behind them: the plain one shows its unlit side, the one with `backlight`
/// shows the light coming through it.
#[test]
fn a_backlit_leaf_glows_against_the_sun() {
    let d = temp("leaves");
    let out = Command::new(bin()).args(["new", &d.to_string_lossy()]).output().expect("run floptle new");
    assert!(out.status.success(), "scaffold failed: {}", String::from_utf8_lossy(&out.stderr));
    let leaf = |name: &str, x: f32, b: f32| {
        format!(
            "(name: \"{name}\", transform: (translation: ({x}, 0.0, -4.0), rotation: (0.0, 0.0, 0.0, 1.0), \
             scale: (1.5, 1.5, 0.02)), matter: Primitive(shape: Cube, color: (0.3, 0.8, 0.2)), scripts: [], \
             material: Some((color: (0.3, 0.8, 0.2), backlight: {b})))"
        )
    };
    let scene = format!(
        "(name: \"leaves\", lighting: (direction: (0.0, 0.25, -1.0), color: (1.0, 0.95, 0.85), \
         ambient: (0.08, 0.08, 0.1), intensity: 1.2), nodes: [(name: \"Camera\", transform: (translation: \
         (0.0, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)), matter: Camera(fov_y: 1.0, \
         active: true), scripts: []), {}, {}])",
        leaf("Plain", -1.0, 0.0),
        leaf("Leaf", 1.0, 0.8)
    );
    std::fs::write(d.join("scenes/leaves.ron"), scene).unwrap();
    let png = d.join("leaves.png");
    let r = Command::new(bin())
        .args(["shot", &d.to_string_lossy(), "--scene", "leaves", "--size", "320x180", "--out", &png.to_string_lossy()])
        .output()
        .expect("run shot");
    if !r.status.success() {
        let why = String::from_utf8_lossy(&r.stderr);
        assert!(why.contains("no GPU") || why.contains("could not build the renderer"), "shot failed:\n{why}");
        let _ = std::fs::remove_dir_all(&d);
        return;
    }
    let img = image::open(&png).expect("a PNG was written").to_rgba8();
    let lum = |x, y| {
        let p = img.get_pixel(x, y);
        u32::from(p[0]) + u32::from(p[1]) + u32::from(p[2])
    };
    let (plain, backlit) = (lum(100, 90), lum(220, 90));
    assert!(backlit > plain * 2, "the backlit leaf {backlit} is no brighter than the plain one {plain}");
    let _ = std::fs::remove_dir_all(&d);
}

/// A box model `w` wide and `h` tall, as a `.glb` (the importer centres it).
fn box_glb(w: f32, h: f32) -> Vec<u8> {
    let (x, y) = (w * 0.5, h * 0.5);
    let mut positions = Vec::new();
    let mut indices = Vec::new();
    // Six faces, each its own four corners.
    let faces: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
        ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
        ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
        ([0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]),
        ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
        ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        ([0.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]),
    ];
    let mut normals = Vec::new();
    for (n, a, b) in faces {
        let base = positions.len() as u32;
        for (sa, sb) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            let p = [0, 1, 2].map(|k| (n[k] + a[k] * sa + b[k] * sb) * [x, y, x][k]);
            positions.push(p);
            normals.push(n);
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    let mesh = floptle_assets::WriteMesh {
        positions,
        normals,
        uvs: None,
        colors: None,
        joints: None,
        weights: None,
        indices,
        base_color: [1.0; 4],
        texture: None,
    };
    floptle_assets::write_glb(&[floptle_assets::WriteNode::mesh_node("blade", mesh)], &[], &[], &[])
}

/// **A scattered plant sways like the same plant on a node.** A prefab's
/// Material used to be dropped when it was baked into a scatter prototype, so
/// a field of grass could not move however its prefab was made. A blade
/// scattered from a prefab whose Material sways leans downwind in a 15 m/s
/// wind and stands straight in still air.
#[test]
fn a_scattered_plant_sways_like_the_same_plant_on_a_node() {
    let shoot_field = |wind: f32, tag: &str| -> Option<image::RgbaImage> {
        let d = temp(tag);
        let out = Command::new(bin()).args(["new", &d.to_string_lossy()]).output().expect("run floptle new");
        assert!(out.status.success(), "scaffold failed: {}", String::from_utf8_lossy(&out.stderr));
        for dir in ["models", "prefabs", "scripts"] {
            std::fs::create_dir_all(d.join(dir)).unwrap();
        }
        std::fs::write(d.join("models/blade.glb"), box_glb(0.15, 3.0)).unwrap();
        std::fs::write(
            d.join("prefabs/blade.prefab.ron"),
            "[(name: \"Blade\", transform: (translation: (0.0, 1.5, 0.0)), matter: Mesh(asset_path: \"models/blade.glb\"), \
             material: Some((color: (0.1, 0.9, 0.1), unlit: true, sway: 0.6)))]",
        )
        .unwrap();
        std::fs::write(
            d.join("scripts/field.lua"),
            format!(
                "weather.addWind{{ velocity = vec3({wind}, 0, 0) }}\n\
                 scatter.create{{ asset = \"prefabs/blade.prefab.ron\", seed = 3, perChunk = 1, chunk = 1, \
                 center = vec3(0, 0, 0), halfX = 0.5, halfZ = 0.5, align = \"world\" }}\n"
            ),
        )
        .unwrap();
        let scene = r#"(name: "field", nodes: [
            (name: "Camera", transform: (translation: (0.0, 1.5, 6.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)), matter: Camera(fov_y: 1.0, active: true), scripts: []),
            (name: "Ground", transform: (translation: (0.0, -0.5, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (20.0, 1.0, 20.0)), matter: Primitive(shape: Cube, color: (0.4, 0.3, 0.2)), scripts: [], collidable: true),
            (name: "Field", transform: (translation: (0.0, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)), matter: Empty, scripts: [(kind: "field")]),
        ])"#;
        std::fs::write(d.join("scenes/field.ron"), scene).unwrap();
        let png = d.join("field.png");
        let r = Command::new(bin())
            .args(["shot", &d.to_string_lossy(), "--scene", "field", "--size", "320x180", "--after", "0.5"])
            .args(["--out", &png.to_string_lossy()])
            .output()
            .expect("run shot");
        if !r.status.success() {
            let why = String::from_utf8_lossy(&r.stderr);
            assert!(why.contains("no GPU") || why.contains("could not build the renderer"), "shot failed:\n{why}");
            let _ = std::fs::remove_dir_all(&d);
            return None;
        }
        let img = image::open(&png).expect("a PNG was written").to_rgba8();
        let _ = std::fs::remove_dir_all(&d);
        Some(img)
    };
    let (Some(still), Some(windy)) = (shoot_field(0.0, "field-still"), shoot_field(15.0, "field-windy")) else { return };
    let top = |img: &image::RgbaImage| {
        let y = (0..img.height()).find(|&y| blade_x(img, y..y + 1).is_some()).expect("no blade was scattered");
        blade_x(img, y..y + 4).unwrap()
    };
    let (still_x, windy_x) = (top(&still), top(&windy));
    assert!(windy_x > still_x + 20.0, "the scattered blade did not lean downwind: {still_x} → {windy_x}");
}
