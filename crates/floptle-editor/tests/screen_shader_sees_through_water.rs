//! **A screen shader tells ground seen through water from bare ground.**
//!
//! `sceneSurfaceDepth()` in a `stage post` shader is the distance to the
//! nearest see-through surface over a pixel, and a very large number where
//! there is none. Shot through the real `floptle shot`, so every seam is in
//! the picture: a scene file's blended material and its glass, the
//! `surface_depth: false` a decal's material carries, the pass a frame draws
//! only when a screen shader in the scene's list reads the op, and the
//! shader's own read.
//!
//! The scene is a wall ten metres ahead with three things in front of it: a
//! sheet of water across the left third, a pane of glass top centre, and a
//! blended decal (left out by its material) bottom centre. The right third is
//! bare wall.
//!
//! Set `FLOPTLE_KEEP_SHOTS=<dir>` to keep the PNGs and look at them.

// The `floptle` binary needs the authoring half; see the note at the top of
// `the_json_verbs_emit_only_json.rs`.
#![cfg(feature = "editor-ui")]

use std::path::{Path, PathBuf};
use std::process::Command;

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("flseethrough-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    for sub in ["scenes", "shaders"] {
        std::fs::create_dir_all(d.join(sub)).unwrap();
    }
    std::fs::write(d.join("project.ron"), "(title: Some(\"p\"), entry_scene: Some(\"scenes/first.ron\"), retro: false)").unwrap();
    d
}

/// Magenta over every pixel seen through something; green where that
/// something is the water sheet's six metres away.
const TINT: &str = "shader seenThrough {\n  stage post\n  let s = sceneSurfaceDepth()\n  \
    let through = step(s, sceneDepth())\n  \
    let sheet = step(abs(s - 6.0), 0.3)\n  \
    output color = mix(mix(sceneColor().rgb, vec3(1, 0, 1), through), vec3(0, 1, 0), sheet)\n}\n";
/// A pass that reads nothing of it.
const PLAIN: &str = "shader plain {\n  stage post\n  output color = sceneColor().rgb\n}\n";

fn project(name: &str, decal_counts: bool) -> PathBuf {
    let d = temp(name);
    std::fs::write(d.join("shaders/tint.flsl"), TINT).unwrap();
    std::fs::write(d.join("shaders/plain.flsl"), PLAIN).unwrap();
    scene(&d, "tint", decal_counts);
    d
}

fn scene(d: &Path, pass: &str, decal_counts: bool) {
    let decal = if decal_counts { "" } else { ", surface_depth: false" };
    std::fs::write(
        d.join("scenes/first.ron"),
        format!(
            "(name: \"s\", nodes: [\
             (name: \"Cam\", matter: Camera(fov_y: 1.0, active: true)),\
             (name: \"Wall\", transform: (translation: (0.0, 0.0, -10.0), scale: (40.0, 40.0, 0.2)), \
              matter: Primitive(shape: Cube, color: (0.8, 0.7, 0.5)), material: Some((color: (0.8, 0.7, 0.5), unlit: true))),\
             (name: \"Water\", transform: (translation: (-4.0, 0.0, -6.0), scale: (4.0, 12.0, 0.02)), \
              matter: Primitive(shape: Cube, color: (0.1, 0.3, 0.9)), \
              material: Some((color: (0.1, 0.3, 0.9), unlit: true, alpha: 0.5, blend: Some(Blend)))),\
             (name: \"Glass\", transform: (translation: (0.0, 1.8, -5.0), scale: (2.4, 2.4, 0.02)), \
              matter: Primitive(shape: Cube, color: (0.9, 1.0, 0.9)), \
              material: Some((color: (0.9, 1.0, 0.9), shading: Physical, transmission: 0.9))),\
             (name: \"Decal\", transform: (translation: (0.0, -3.0, -9.5), scale: (4.0, 4.0, 0.02)), \
              matter: Primitive(shape: Cube, color: (0.2, 0.2, 0.2)), \
              material: Some((color: (0.2, 0.2, 0.2), unlit: true, alpha: 0.6, blend: Some(Blend){decal}))),\
             (name: \"Post\", matter: PostProcess(screen_shaders: [(shader: \"shaders/{pass}.flsl\")]))])"
        ),
    )
    .unwrap();
}

/// Shoot the project; `None` when this machine has nothing to render on. The
/// second value is the timing's region labels, when the GPU reports any.
fn shoot(d: &Path, keep_as: &str) -> Option<(image::RgbImage, Option<Vec<String>>)> {
    let out = d.join("shot.png");
    let run = Command::new(env!("CARGO_BIN_EXE_floptle"))
        .args(["shot", &d.to_string_lossy(), "--size", "160x90", "--timing", "--json", "--out", &out.to_string_lossy()])
        .output()
        .expect("run floptle shot");
    if !run.status.success() {
        let why = String::from_utf8_lossy(&run.stderr).into_owned();
        let cannot = why.contains("no GPU") || why.contains("could not build the renderer");
        assert!(cannot, "shot failed for a reason that is not the adapter:\n{why}");
        eprintln!("skipped — this machine cannot render:\n{why}");
        return None;
    }
    if let Ok(keep) = std::env::var("FLOPTLE_KEEP_SHOTS") {
        std::fs::create_dir_all(&keep).ok();
        std::fs::copy(&out, Path::new(&keep).join(format!("{keep_as}.png"))).ok();
    }
    let stdout = String::from_utf8_lossy(&run.stdout);
    let v: serde_json::Value = serde_json::from_str(stdout.trim().lines().last().unwrap_or("")).expect("shot --json");
    let labels = v["timing"]["passes"]
        .as_array()
        .map(|passes| passes.iter().filter_map(|p| p["label"].as_str().map(str::to_string)).collect());
    Some((image::open(&out).expect("the shot is a PNG").to_rgb8(), labels))
}

const WATER: (u32, u32) = (25, 45);
const GLASS: (u32, u32) = (80, 16);
const DECAL: (u32, u32) = (80, 68);
const BARE: (u32, u32) = (135, 45);

fn magenta(p: &image::Rgb<u8>) -> bool {
    p.0[0] > 180 && p.0[1] < 80 && p.0[2] > 180
}
fn green(p: &image::Rgb<u8>) -> bool {
    p.0[0] < 80 && p.0[1] > 180 && p.0[2] < 80
}

#[test]
fn ground_under_water_and_glass_reads_as_seen_through_and_bare_ground_does_not() {
    let d = project("tint", false);
    let Some((img, _)) = shoot(&d, "seen_through") else { return };
    let at = |(x, y): (u32, u32)| *img.get_pixel(x, y);
    // The sheet is six metres off: the shader's own distance check.
    assert!(green(&at(WATER)), "the wall under the water sheet did not read the sheet's 6 m: {:?}", at(WATER));
    // Glass, five metres off: seen through, and not the sheet's distance.
    assert!(magenta(&at(GLASS)), "the wall behind the glass did not read as seen through: {:?}", at(GLASS));
    // Bare wall: nothing in front, the frame's own colour.
    assert!(!magenta(&at(BARE)) && !green(&at(BARE)), "bare wall read as seen through something: {:?}", at(BARE));
    // The decal's material leaves it out.
    assert!(!magenta(&at(DECAL)) && !green(&at(DECAL)), "a decal with surface_depth off read as a surface: {:?}", at(DECAL));

    // The same decal with the setting left alone is a blended surface like
    // any other.
    scene(&d, "tint", true);
    let (img, _) = shoot(&d, "seen_through_decal_counts").expect("it rendered a moment ago");
    assert!(magenta(img.get_pixel(DECAL.0, DECAL.1)), "a blended surface that says nothing is not counted: {:?}", img.get_pixel(DECAL.0, DECAL.1));
    let _ = std::fs::remove_dir_all(&d);
}

/// The pass that draws the surfaces' depth runs in a frame whose screen
/// shaders read it, and in no other: its region is in the timing with the
/// tinting pass listed and absent with a pass that never calls the op.
#[test]
fn the_see_through_depth_is_drawn_only_when_a_screen_shader_reads_it() {
    let d = project("cost", false);
    let Some((_, labels)) = shoot(&d, "seen_through_timed") else { return };
    let Some(labels) = labels else {
        eprintln!("skipped — this GPU reports no timestamps");
        return;
    };
    assert!(labels.iter().any(|l| l == "see-through depth"), "a pass reads sceneSurfaceDepth and nothing drew it: {labels:?}");

    scene(&d, "plain", false);
    let (_, labels) = shoot(&d, "seen_through_unused").expect("it rendered a moment ago");
    let labels = labels.expect("it reported timestamps a moment ago");
    assert!(
        !labels.iter().any(|l| l == "see-through depth"),
        "no screen shader reads sceneSurfaceDepth and the frame drew it anyway: {labels:?}"
    );
    let _ = std::fs::remove_dir_all(&d);
}
