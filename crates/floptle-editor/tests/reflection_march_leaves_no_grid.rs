//! **A screen-space reflection's misses are grain, not a woven grid.**
//!
//! The reflection march starts each pixel's ray at its own phase, so its step
//! size does not band. Where an edge in the reflection falls between two
//! steps, whether a ray hits it turns on that phase, and with the phases on a
//! 4×4 grid the hits and misses came out as a crosshatch: on Solar's dark
//! glossy lab benchtop, under its glass shelves and at the fading end of
//! every reflection, a woven pattern the scene does not have. The phases are
//! now spread without a period.

// The `floptle` binary needs the authoring half; see the note at the top of
// `the_json_verbs_emit_only_json.rs`.
#![cfg(feature = "editor-ui")]

use std::path::{Path, PathBuf};
use std::process::Command;

/// A mirror floor with a striped post on it, seen from half a metre up: the
/// third frame of a sequence (a reflection reads the last frame's picture),
/// or `None` where this machine cannot render.
fn shot() -> Option<image::GrayImage> {
    let d = std::env::temp_dir().join(format!("flssrgrid-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("scenes")).unwrap();
    std::fs::write(d.join("project.ron"), "(title: Some(\"s\"), entry_scene: Some(\"scenes/first.ron\"), retro: false)").unwrap();
    let mut nodes = String::from(
        "(name: \"Floor\", transform: (translation: (0.0, -0.05, 0.0), scale: (40.0, 0.1, 40.0)), matter: Primitive(shape: Cube, color: (0.2, 0.2, 0.2)), \
           material: Some((shading: Physical, color: (0.2, 0.2, 0.2), roughness: 0.0, metallic: 1.0, reflectivity: 1.0))),",
    );
    for k in 0..8 {
        let c = if k % 2 == 0 { "(1.0, 0.1, 0.1)" } else { "(0.1, 0.1, 1.0)" };
        nodes += &format!(
            "(name: \"S{k}\", transform: (translation: (0.0, {}, -6.0), scale: (0.6, 0.27, 0.6)), matter: Primitive(shape: Cube, color: {c})),",
            0.15 + 0.3 * k as f32
        );
    }
    nodes += "(name: \"Cam\", transform: (translation: (0.0, 0.5, 0.0), rotation: (-0.025, 0.0, 0.0, 0.99968755)), matter: Camera(fov_y: 1.0, active: true))";
    std::fs::write(
        d.join("scenes/first.ron"),
        format!("(name: \"s\", lighting: (direction: (0.4, 1.0, 0.6), ambient: (0.3, 0.3, 0.3), reflections: true, reflection_distance: 40.0), nodes: [{nodes}])"),
    )
    .unwrap();
    let out = d.join("shot.png");
    let run = Command::new(env!("CARGO_BIN_EXE_floptle"))
        .args(["shot", &d.to_string_lossy(), "--size", "480x360", "--no-ui", "--frames", "3", "--out"])
        .arg(&out)
        .output()
        .expect("run floptle shot");
    if !run.status.success() {
        let why = String::from_utf8_lossy(&run.stderr).into_owned();
        let cannot = why.contains("no GPU") || why.contains("could not build the renderer");
        assert!(cannot, "shot failed for a reason that is not the adapter:\n{why}");
        eprintln!("skipped — this machine cannot render:\n{why}");
        return None;
    }
    let last: PathBuf = d.join("shot-0002.png");
    let img = image::open(&last).unwrap_or_else(|e| panic!("no frame at {}: {e}", last.display())).to_luma8();
    let _ = std::fs::remove_dir_all(Path::new(&d));
    Some(img)
}

/// Over the `n`×`n` block at (`x0`, `y0`): the high-passed picture's energy
/// at a period of 4 and 2 pixels (the grid's), as a share of all of it, and
/// that energy's spread (so a flat block is not read as gridless).
fn grid_share(img: &image::GrayImage, x0: u32, y0: u32, n: usize) -> (f64, f64) {
    let at = |x: usize, y: usize| img.get_pixel(x0 + (x % n) as u32, y0 + (y % n) as u32).0[0] as f64;
    let mut hp = vec![vec![0.0f64; n]; n];
    for (y, row) in hp.iter_mut().enumerate() {
        for (x, v) in row.iter_mut().enumerate() {
            *v = at(x, y) - 0.25 * (at(x + 1, y) + at(x + n - 1, y) + at(x, y + 1) + at(x, y + n - 1));
        }
    }
    // The power at frequency (u, v), straight from its sum.
    let power = |u: usize, v: usize| {
        let (mut re, mut im) = (0.0, 0.0);
        for (y, row) in hp.iter().enumerate() {
            for (x, h) in row.iter().enumerate() {
                let a = -std::f64::consts::TAU * ((u * x) as f64 + (v * y) as f64) / n as f64;
                re += h * a.cos();
                im += h * a.sin();
            }
        }
        re * re + im * im
    };
    let total: f64 = hp.iter().flatten().map(|h| h * h).sum::<f64>() * (n * n) as f64;
    let q = n / 4;
    let grid = power(q, 0) + power(0, q) + power(q, q) + power(2 * q, 0) + power(0, 2 * q);
    let sd = (hp.iter().flatten().map(|h| h * h).sum::<f64>() / (n * n) as f64).sqrt();
    (grid / total.max(1e-9), sd)
}

#[test]
fn a_reflections_misses_are_grain_not_a_grid() {
    let Some(img) = shot() else { return };
    // The fading end of the post's reflection, where rays miss its top edge
    // as often as they hit it.
    let (share, sd) = grid_share(&img, 196, 296, 64);
    assert!(sd > 1.0, "the fixture: the reflection's fading end holds no misses to measure (high-pass spread {sd:.2})");
    assert!(share < 0.005, "the reflection's misses fall on a grid: {:.2}% of their energy at the grid's period", share * 100.0);
}
