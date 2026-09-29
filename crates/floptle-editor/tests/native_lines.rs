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
