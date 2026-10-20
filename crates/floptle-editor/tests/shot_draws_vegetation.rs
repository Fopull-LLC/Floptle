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
