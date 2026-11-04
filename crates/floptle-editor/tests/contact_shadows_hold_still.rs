//! **Contact shadows hold still while the camera barely moves.**
//!
//! A contact shadow is traced through the depth buffer, reading the surface
//! back a texel at a time. On sloping ground the step from one texel to the
//! next is more than the trace's lift off the surface, so terrain shadowed
//! itself in a speckle that flipped as each step crossed into the next texel,
//! and the slightest drift of the camera (a camera riding a planet is never
//! quite still) made it flip every frame: the user saw the ground shimmer
//! under a parked bike. The lift is now at least a depth texel's span there,
//! which a real occluder stands well clear of.

// The `floptle` binary needs the authoring half; see the note at the top of
// `the_json_verbs_emit_only_json.rs`.
#![cfg(feature = "editor-ui")]

use std::path::Path;
use std::process::Command;

/// Run `floptle shot` on project `d`, with `extra` arguments (`--after`,
/// `--frames`). `false` when this machine cannot render.
fn shoot(d: &Path, extra: &[&str]) -> bool {
    let out = d.join("shot.png");
    let run = Command::new(env!("CARGO_BIN_EXE_floptle"))
        .args(["shot", &d.to_string_lossy(), "--size", "320x180", "--no-ui"])
        .args(extra)
        .arg("--out")
        .arg(&out)
        .output()
        .expect("run floptle shot");
    if !run.status.success() {
        let why = String::from_utf8_lossy(&run.stderr).into_owned();
        let cannot = why.contains("no GPU") || why.contains("could not build the renderer");
        assert!(cannot, "shot failed for a reason that is not the adapter:\n{why}");
        eprintln!("skipped — this machine cannot render:\n{why}");
        return false;
    }
    true
}

fn project(name: &str, contact: bool, scene: &str, scripts: &[(&str, &str)]) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("flcontact-{name}-{contact}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    for sub in ["scenes", "scripts"] {
        std::fs::create_dir_all(d.join(sub)).unwrap();
    }
    std::fs::write(d.join("project.ron"), "(title: Some(\"c\"), entry_scene: Some(\"scenes/first.ron\"), retro: false)").unwrap();
    for (file, src) in scripts {
        std::fs::write(d.join("scripts").join(file), src).unwrap();
    }
    std::fs::write(d.join("scenes/first.ron"), scene.replace("CONTACT", if contact { "true" } else { "false" })).unwrap();
    d
}

fn luma(p: &Path) -> image::GrayImage {
    image::open(p).unwrap_or_else(|e| panic!("no picture at {}: {e}", p.display())).to_luma8()
}

/// The mean change a pixel, 0..255, between consecutive frames of a camera
/// creeping a millimetre a frame over generated hills under a low sun.
fn creep_flicker(contact: bool) -> Option<f64> {
    let d = project(
        "creep",
        contact,
        "(name: \"s\", lighting: (direction: (1.0, 0.35, 0.3), ambient: (0.15, 0.15, 0.15), shadows: true, contact_shadows: CONTACT, fog: false), nodes: [\
           (name: \"Planet\", scripts: [(kind: \"gen\")]),\
           (name: \"Cam\", scripts: [(kind: \"creep\")], transform: (translation: (0.0, 316.0, 0.0), rotation: (-0.1305262, 0.0, 0.0, 0.9914449)), matter: Camera(fov_y: 1.0, active: true))])",
        &[
            ("gen.lua", "function start(node)\n  node:setTerrain(1)\n  terrain.generatePlanet(1, { radius = 300, voxel = 1.5, relief = 12, craters = 0, caveDepth = 0, seed = 7 })\nend\n"),
            ("creep.lua", "function update(node, dt)\n  node.x = node.x + 0.001\nend\n"),
        ],
    );
    if !shoot(&d, &["--after", "3", "--frames", "6"]) {
        return None;
    }
    let (mut sum, mut n) = (0u64, 0u64);
    for i in 2..5 {
        let a = luma(&d.join(format!("shot-{i:04}.png")));
        let b = luma(&d.join(format!("shot-{:04}.png", i + 1)));
        for (x, y) in a.pixels().zip(b.pixels()) {
            sum += (x.0[0] as i32 - y.0[0] as i32).unsigned_abs() as u64;
            n += 1;
        }
    }
    let _ = std::fs::remove_dir_all(&d);
    Some(sum as f64 / n as f64)
}

/// A small box on a floor 4 m in front of the camera under a low sun.
fn box_scene(contact: bool) -> Option<image::GrayImage> {
    let d = project(
        "box",
        contact,
        "(name: \"s\", lighting: (direction: (1.0, 0.45, 0.2), ambient: (0.2, 0.2, 0.2), shadows: true, contact_shadows: CONTACT, fog: false), nodes: [\
           (name: \"Floor\", transform: (translation: (0.0, -0.1, 0.0), scale: (60.0, 0.1, 60.0)), matter: Primitive(shape: Cube, color: (0.7, 0.7, 0.7))),\
           (name: \"Box\", transform: (translation: (0.0, 0.1, 0.0), scale: (0.1, 0.1, 0.1)), matter: Primitive(shape: Cube, color: (0.8, 0.3, 0.2))),\
           (name: \"Cam\", transform: (translation: (0.0, 1.2, 4.0), rotation: (-0.17364818, 0.0, 0.0, 0.98480775)), matter: Camera(fov_y: 1.0, active: true))])",
        &[],
    );
    if !shoot(&d, &[]) {
        return None;
    }
    let img = luma(&d.join("shot.png"));
    let _ = std::fs::remove_dir_all(&d);
    Some(img)
}

#[test]
fn contact_shadows_hold_still_while_the_camera_barely_moves() {
    let (Some(on), Some(off)) = (creep_flicker(true), creep_flicker(false)) else { return };
    assert!(on < off + 0.02, "contact shadows flicker as the camera creeps a millimetre a frame: {on:.3} a pixel a frame, {off:.3} without them");
}

#[test]
fn a_contact_shadow_still_falls_from_a_real_occluder() {
    let (Some(on), Some(off)) = (box_scene(true), box_scene(false)) else { return };
    let shaded = on.pixels().zip(off.pixels()).filter(|(a, b)| (b.0[0] as i32 - a.0[0] as i32) > 8).count();
    assert!(shaded > 30, "the box's contact shadow is gone: {shaded} pixels darker with contact shadows on");
}
