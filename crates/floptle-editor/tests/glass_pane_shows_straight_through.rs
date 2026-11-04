//! **A pane of glass is not a lens.** A sheet is entered and left through two
//! parallel faces, so the view through it comes out heading the way it went
//! in and what is behind shows straight through. A `thin` material does that;
//! a solid one (the default, right for a ball or a gem) bends the view once
//! and follows it to the far wall, which through a pane seen at an angle threw
//! the wall about by metres (0633: Solar's lab glazing).

#![cfg(feature = "editor-ui")]

use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_floptle")
}

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("flshotpane-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// A camera looking down −Z at a tall red bar on a far white wall, and a
/// 12 mm pane between them, turned 50° about the vertical, covering the middle
/// of the picture and not its top.
fn project(dir: &Path, thin: bool, refl: bool) {
    let out = Command::new(bin()).args(["new", &dir.to_string_lossy()]).output().expect("run floptle new");
    assert!(out.status.success(), "scaffold failed: {}", String::from_utf8_lossy(&out.stderr));
    // 50° about Y: (0, sin 25°, 0, cos 25°).
    let (s, c) = (25f32.to_radians().sin(), 25f32.to_radians().cos());
    let scene = format!(
        r#"(name: "pane", lighting: (reflections: {refl}), nodes: [
        (name: "Camera", transform: (translation: (0.0, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)), matter: Camera(fov_y: 1.0, active: true), scripts: []),
        (name: "Wall", transform: (translation: (0.0, 0.0, -12.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (40.0, 40.0, 0.1)), matter: Primitive(shape: Cube, color: (0.9, 0.9, 0.9)), scripts: [], material: Some((color: (0.9, 0.9, 0.9), unlit: true))),
        (name: "Bar", transform: (translation: (0.0, 0.0, -11.9), rotation: (0.0, 0.0, 0.0, 1.0), scale: (0.6, 40.0, 0.1)), matter: Primitive(shape: Cube, color: (0.9, 0.1, 0.1)), scripts: [], material: Some((color: (0.9, 0.1, 0.1), unlit: true))),
        (name: "Pane", transform: (translation: (0.0, -0.6, -2.5), rotation: (0.0, {s}, 0.0, {c}), scale: (3.0, 2.0, 0.012)), matter: Primitive(shape: Cube, color: (1.0, 1.0, 1.0)), scripts: [], material: Some((color: (1.0, 1.0, 1.0), shading: Physical, roughness: 0.0, reflectivity: 0.0, transmission: 1.0, ior: 1.5, thickness: 0.012, thin: {thin}))),
    ])"#
    );
    std::fs::write(dir.join("scenes/pane.ron"), scene).unwrap();
}

/// The mean x of the bar's red pixels in rows `rows` of the picture.
fn bar_x(img: &image::RgbaImage, rows: std::ops::Range<u32>) -> Option<f32> {
    let (mut sum, mut n) = (0.0, 0.0);
    for y in rows {
        for x in 0..img.width() {
            let p = img.get_pixel(x, y);
            if p[0] > 120 && p[1] < 90 && p[2] < 90 {
                sum += x as f32;
                n += 1.0;
            }
        }
    }
    (n > 0.0).then(|| sum / n)
}

fn shoot(thin: bool, refl: bool) -> Option<image::RgbaImage> {
    let tag = format!("{}-{}", if thin { "thin" } else { "solid" }, if refl { "refl" } else { "plain" });
    let d = temp(&tag);
    project(&d, thin, refl);
    let out = d.join("pane.png");
    let r = Command::new(bin())
        .args(["shot", &d.to_string_lossy(), "--scene", "pane", "--size", "320x240"])
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
    if let Ok(dir) = std::env::var("FLOPTLE_PANE_SHOTS") {
        let _ = img.save(Path::new(&dir).join(format!("pane-{tag}.png")));
    }
    let _ = std::fs::remove_dir_all(&d);
    Some(img)
}

#[test]
fn a_thin_pane_shows_what_is_behind_it_straight_through() {
    let (Some(solid), Some(thin)) = (shoot(false, false), shoot(true, false)) else { return };
    // Above the pane the bar is seen bare; through it, lower down.
    let (bare, through) = (10..40, 150..200);
    let bare_x = |img: &image::RgbaImage| bar_x(img, bare.clone()).expect("the fixture: the bar above the pane");
    let (solid_bare, thin_bare) = (bare_x(&solid), bare_x(&thin));
    let thin_through = bar_x(&thin, through.clone()).expect("the bar through the thin pane");
    // The fixture: a solid pane, seen at this angle, throws the bar a long way
    // (or out of the picture altogether).
    let solid_through = bar_x(&solid, through);
    assert!(
        solid_through.is_none_or(|x| (x - solid_bare).abs() > 15.0),
        "the fixture: the solid pane barely moved the bar ({solid_bare} → {solid_through:?})"
    );
    assert!((thin_through - thin_bare).abs() < 3.0, "through the thin pane the bar moved from x {thin_bare} to {thin_through}");
}
