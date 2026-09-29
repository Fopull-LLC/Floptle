//! **`draw.nativeLines(true)` draws script lines over the finished picture at
//! full resolution**.
//!
//! At a render scale below 1 a line drawn into the scene is rasterised at the
//! lowered resolution and stretched: at 0.5 a one-pixel line is two rows of
//! blended colour, soft and shimmering, most visibly against black space.
//! With the switch on, the line is drawn after post and the upscale, one pixel
//! wide, in exactly the colour the script gave it.

// The `floptle` binary needs the authoring half; see the note at the top of
// `the_json_verbs_emit_only_json.rs`.
#![cfg(feature = "editor-ui")]

use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_floptle")
}

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("flnativelines-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn project(d: &Path, native: bool) {
    let out = Command::new(bin())
        .args(["new", &d.to_string_lossy(), "--template", "platformer"])
        .output()
        .expect("run floptle new");
    assert!(out.status.success(), "scaffold failed: {}", String::from_utf8_lossy(&out.stderr));
    // The template is a retro project; the render scale is what is under test.
    let cfg = std::fs::read_to_string(d.join("project.ron")).expect("project.ron");
    std::fs::write(d.join("project.ron"), cfg.replace("retro: true", "retro: false")).expect("write project.ron");
    std::fs::write(
        d.join("scenes/lines.ron"),
        "(name: \"lines\", nodes: [\
           (name: \"Camera\", matter: Camera(fov_y: 1.0, active: true)), \
           (name: \"Drawer\", scripts: [(kind: \"lines\", enabled: true, params: [])])\
         ])",
    )
    .expect("write the scene");
    std::fs::write(
        d.join("scripts/lines.lua"),
        format!(
            "function start(node)\n  app.setRenderScale(0.5)\n  draw.nativeLines({native})\nend\n\
             function lateUpdate(node, dt)\n  draw.line(-20, 0, -5, 20, 0, -5, 1, 0, 0)\nend\n"
        ),
    )
    .expect("write the script");
}

fn shoot(d: &Path) -> Result<image::RgbaImage, String> {
    let out = d.join("lines.png");
    let r = Command::new(bin())
        .args(["shot", &d.to_string_lossy(), "--scene", "lines", "--after", "5f", "--size", "320x180"])
        .args(["--out", &out.to_string_lossy()])
        .output()
        .expect("run shot");
    if !r.status.success() {
        return Err(String::from_utf8_lossy(&r.stderr).into_owned());
    }
    Ok(image::open(&out).expect("a PNG was written").to_rgba8())
}

fn cannot_render(why: &str) -> bool {
    let cannot = why.contains("no GPU") || why.contains("could not build the renderer");
    if cannot {
        eprintln!("skipped — this machine cannot render:\n{why}");
    }
    cannot
}

/// Rows with any reddish pixel, and rows that are mostly exactly (255, 0, 0).
fn red_rows(img: &image::RgbaImage) -> (usize, usize) {
    let (w, h) = img.dimensions();
    let (mut any, mut exact) = (0, 0);
    for y in 0..h {
        let row: Vec<_> = (0..w).map(|x| img.get_pixel(x, y)).collect();
        if row.iter().any(|p| p[0] > 60 && p[0] as i32 > p[1] as i32 * 2 + 30) {
            any += 1;
        }
        if row.iter().filter(|p| p[0] >= 250 && p[1] <= 5 && p[2] <= 5).count() * 10 >= w as usize * 9 {
            exact += 1;
        }
    }
    (any, exact)
}

#[test]
fn native_lines_are_one_pixel_and_their_own_colour_at_half_render_scale() {
    let (dn, ds) = (temp("native"), temp("scene"));
    project(&dn, true);
    project(&ds, false);
    let native = match shoot(&dn) {
        Ok(i) => i,
        Err(why) => {
            assert!(cannot_render(&why), "shot failed for a reason that is not the adapter:\n{why}");
            for d in [&dn, &ds] {
                let _ = std::fs::remove_dir_all(d);
            }
            return;
        }
    };
    let scene = shoot(&ds).expect("the scene-resolution shot");
    let (n_any, n_exact) = red_rows(&native);
    let (s_any, s_exact) = red_rows(&scene);
    assert_eq!(n_exact, 1, "one full row of exactly the script's red (rows with red: {n_any})");
    assert!(n_any <= 1, "and nothing smeared around it: {n_any} rows");
    assert_eq!(s_exact, 0, "the fixture: a line in a half-resolution scene is not its own colour");
    assert!(s_any >= 2, "the fixture: stretched from half resolution it covers two rows or more: {s_any}");
    for d in [&dn, &ds] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// A project with a cube in front of a red line, drawn native or in the scene,
/// with or without `draw.depthTest`.
fn occluded_project(d: &Path, native: bool, depth: bool) {
    project(d, native);
    std::fs::write(
        d.join("scenes/lines.ron"),
        "(name: \"lines\", nodes: [\
           (name: \"Camera\", matter: Camera(fov_y: 1.0, active: true)), \
           (name: \"Wall\", transform: (translation: (0.0, 0.0, -3.0)), matter: Primitive(shape: Cube, color: (0.4, 0.4, 0.4))), \
           (name: \"Drawer\", scripts: [(kind: \"lines\", enabled: true, params: [])])\
         ])",
    )
    .expect("write the scene");
    std::fs::write(
        d.join("scripts/lines.lua"),
        format!(
            "function start(node)\n  app.setRenderScale(0.5)\n  draw.nativeLines({native})\n  draw.depthTest({depth})\nend\n\
             function lateUpdate(node, dt)\n  draw.line(-20, 0, -5, 20, 0, -5, 1, 0, 0)\nend\n"
        ),
    )
    .expect("write the script");
}

/// Red pixels in the middle tenth of the picture (behind the cube) and in the
/// outer fifths (beside it).
fn red_behind_and_beside(img: &image::RgbaImage) -> (usize, usize) {
    let (w, h) = img.dimensions();
    let red = |x: u32, y: u32| {
        let p = img.get_pixel(x, y);
        p[0] > 120 && p[0] as i32 > p[1] as i32 * 2 + 40
    };
    let (mut behind, mut beside) = (0, 0);
    for y in 0..h {
        for x in 0..w {
            if !red(x, y) {
                continue;
            }
            if x > w * 9 / 20 && x < w * 11 / 20 {
                behind += 1;
            } else if x < w / 5 || x > w * 4 / 5 {
                beside += 1;
            }
        }
    }
    (behind, beside)
}

/// **`draw.depthTest(true)` hides a line behind what is in front of it**, in
/// the scene and drawn native over the upscaled picture alike: the line shows
/// either side of the cube and not through it. Without it the line draws
/// through, as an orbit through its planet always has.
#[test]
fn a_depth_tested_line_hides_behind_the_scene_native_or_not() {
    for native in [true, false] {
        let mut seen = Vec::new();
        for depth in [false, true] {
            let d = temp(&format!("occl-{native}-{depth}"));
            occluded_project(&d, native, depth);
            let img = match shoot(&d) {
                Ok(i) => i,
                Err(why) => {
                    assert!(cannot_render(&why), "shot failed for a reason that is not the adapter:\n{why}");
                    let _ = std::fs::remove_dir_all(&d);
                    return;
                }
            };
            seen.push(red_behind_and_beside(&img));
            let _ = std::fs::remove_dir_all(&d);
        }
        let ((through_behind, through_beside), (tested_behind, tested_beside)) = (seen[0], seen[1]);
        assert!(through_behind > 0 && through_beside > 0, "native {native}: without depthTest the line draws through: {seen:?}");
        assert_eq!(tested_behind, 0, "native {native}: with depthTest the line showed through the cube: {seen:?}");
        assert!(tested_beside > 0, "native {native}: with depthTest the line vanished beside the cube too: {seen:?}");
    }
}
