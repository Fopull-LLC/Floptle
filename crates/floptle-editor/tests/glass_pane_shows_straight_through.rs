//! **Glass bends the view the way its shape does.** A pane is entered and left
//! through two parallel faces, so the view through it comes out heading the
//! way it went in and the room behind shows straight through. A ball's far side
//! curves back, and the ball is a lens. `Refraction::Auto` reads which from the
//! surface itself, and is what a new project's glass does; `Thin` is always a
//! sheet; `Solid` bends once and keeps it bent, which is what glass did before
//! the setting, and what a project from before it still gets. A solid pane
//! seen at an angle threw the wall behind it about by metres (0633: Solar's
//! lab glazing).

#![cfg(feature = "editor-ui")]

use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_floptle")
}

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("flshotglass-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// What stands between the camera and the bar.
#[derive(Clone, Copy)]
enum Glass {
    /// A 12 mm pane turned 50° about the vertical, covering the middle of the
    /// picture and not its top.
    Pane,
    /// A solid ball, 2 m across and that thick, off to the bar's left.
    Ball,
}

/// A camera looking down −Z at a tall red bar on a far white wall, with
/// `glass` between them. `refraction` is the material's own setting, `None`
/// to leave it to the project; `old` strips the project's Glass setting from
/// project.ron, as a project made before it has none.
fn project(dir: &Path, glass: Glass, refraction: Option<&str>, old: bool) {
    let out = Command::new(bin()).args(["new", &dir.to_string_lossy()]).output().expect("run floptle new");
    assert!(out.status.success(), "scaffold failed: {}", String::from_utf8_lossy(&out.stderr));
    let cfg = dir.join("project.ron");
    let text = std::fs::read_to_string(&cfg).unwrap();
    assert!(text.contains("glass_refraction: Some(Auto)"), "the fixture: a new project's glass is not auto:\n{text}");
    if old {
        let stripped: String = text.lines().filter(|l| !l.contains("glass_refraction")).map(|l| format!("{l}\n")).collect();
        std::fs::write(&cfg, stripped).unwrap();
    }
    let refraction = refraction.map(|r| format!(", refraction: {r}")).unwrap_or_default();
    let glass_node = match glass {
        Glass::Pane => {
            // 50° about Y: (0, sin 25°, 0, cos 25°).
            let (s, c) = (25f32.to_radians().sin(), 25f32.to_radians().cos());
            format!(
                r#"(name: "Pane", transform: (translation: (0.0, -0.6, -2.5), rotation: (0.0, {s}, 0.0, {c}), scale: (3.0, 2.0, 0.012)), matter: Primitive(shape: Cube, color: (1.0, 1.0, 1.0)), scripts: [], material: Some((color: (1.0, 1.0, 1.0), shading: Physical, roughness: 0.0, reflectivity: 0.0, transmission: 1.0, ior: 1.5, thickness: 0.012{refraction})))"#
            )
        }
        Glass::Ball => format!(
            r#"(name: "Ball", transform: (translation: (-0.9, 0.0, -5.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (2.0, 2.0, 2.0)), matter: Primitive(shape: Sphere, color: (1.0, 1.0, 1.0)), scripts: [], material: Some((color: (1.0, 1.0, 1.0), shading: Physical, roughness: 0.0, reflectivity: 0.0, transmission: 1.0, ior: 1.5, thickness: 2.0{refraction})))"#
        ),
    };
    let scene = format!(
        r#"(name: "glass", nodes: [
        (name: "Camera", transform: (translation: (0.0, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)), matter: Camera(fov_y: 1.0, active: true), scripts: []),
        (name: "Wall", transform: (translation: (0.0, 0.0, -12.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (40.0, 40.0, 0.1)), matter: Primitive(shape: Cube, color: (0.9, 0.9, 0.9)), scripts: [], material: Some((color: (0.9, 0.9, 0.9), unlit: true))),
        (name: "Bar", transform: (translation: (0.0, 0.0, -11.9), rotation: (0.0, 0.0, 0.0, 1.0), scale: (0.6, 40.0, 0.1)), matter: Primitive(shape: Cube, color: (0.9, 0.1, 0.1)), scripts: [], material: Some((color: (0.9, 0.1, 0.1), unlit: true))),
        {glass_node},
    ])"#
    );
    std::fs::write(dir.join("scenes/glass.ron"), scene).unwrap();
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

fn shoot(glass: Glass, refraction: Option<&str>, old: bool, tag: &str) -> Option<image::RgbaImage> {
    let d = temp(tag);
    project(&d, glass, refraction, old);
    let out = d.join("glass.png");
    let r = Command::new(bin())
        .args(["shot", &d.to_string_lossy(), "--scene", "glass", "--size", "320x240"])
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
    if let Ok(dir) = std::env::var("FLOPTLE_GLASS_SHOTS") {
        let _ = img.save(Path::new(&dir).join(format!("glass-{tag}.png")));
    }
    let _ = std::fs::remove_dir_all(&d);
    Some(img)
}

/// Where the bar is above the pane, and where it is through it.
fn through_pane(img: &image::RgbaImage) -> (f32, Option<f32>) {
    (bar_x(img, 10..40).expect("the fixture: the bar above the pane"), bar_x(img, 150..200))
}

#[test]
fn a_pane_shows_what_is_behind_it_straight_through_and_a_solid_one_bends_it() {
    let shots = (
        shoot(Glass::Pane, None, false, "pane-project"),
        shoot(Glass::Pane, Some("Thin"), false, "pane-thin"),
        shoot(Glass::Pane, Some("Solid"), false, "pane-solid"),
        shoot(Glass::Pane, None, true, "pane-old-project"),
    );
    let (Some(auto), Some(thin), Some(solid), Some(old)) = shots else { return };
    // A new project's glass reads its shape, and a pane's is flat: straight
    // through. So does one set thin, whatever its shape.
    for (img, what) in [(&auto, "a new project's (auto)"), (&thin, "a thin")] {
        let (bare, through) = through_pane(img);
        let through = through.unwrap_or_else(|| panic!("no bar through {what} pane"));
        assert!((through - bare).abs() < 3.0, "through {what} pane the bar moved from x {bare} to {through}");
    }
    // A solid bends once and keeps it bent: seen at this angle the bar is
    // thrown a long way, or out of the picture. A project from before the
    // setting keeps that.
    for (img, what) in [(&solid, "a solid"), (&old, "an older project's")] {
        let (bare, through) = through_pane(img);
        assert!(through.is_none_or(|x| (x - bare).abs() > 15.0), "{what} pane barely moved the bar ({bare} → {through:?})");
    }
}

#[test]
fn a_glass_ball_read_from_its_shape_is_a_lens() {
    let Some(auto) = shoot(Glass::Ball, Some("Auto"), false, "ball-auto") else { return };
    // Above the ball the bar is seen bare. Through the ball's middle, a lens
    // turns the room behind it over: the bar is no longer where it stands.
    let (bare, _) = through_pane(&auto);
    let lensed = bar_x(&auto, 105..135);
    assert!(lensed.is_none_or(|x| (x - bare).abs() > 20.0), "a glass ball read as auto is no lens: the bar stayed at {lensed:?}, bare {bare}");
}
