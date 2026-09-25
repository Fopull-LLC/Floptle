//! **Screen-space AO leaves distant and fogged geometry alone.**
//!
//! A field of large mounds from 60 m to a kilometre out, under fog that is
//! total by 112 m, photographed with `ao: ScreenSpace` and with `ao: Off`. The
//! far half of the two pictures must match. Before the fix the AO pass laid
//! dark blotches over all of it: its sampling disc was under a pixel at that
//! range, where the depth buffer's own error self-occludes the ground, and it
//! was multiplied over the finished, fogged colour.

// The `floptle` binary needs the authoring half; see the note at the top of
// `the_json_verbs_emit_only_json.rs`.
#![cfg(feature = "editor-ui")]

use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_floptle")
}

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("flssao-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn node(name: &str, t: [f32; 3], s: [f32; 3], matter: &str, rot: [f32; 4]) -> String {
    format!(
        "        (\n            name: \"{name}\",\n            transform: (translation: ({}, {}, {}), \
         rotation: ({}, {}, {}, {}), scale: ({}, {}, {})),\n            matter: {matter},\n            \
         scripts: [],\n        ),\n",
        t[0], t[1], t[2], rot[0], rot[1], rot[2], rot[3], s[0], s[1], s[2]
    )
}

/// The dune field. Positions come from a fixed walk, so both shots and every
/// run see the same field.
fn scene(ao: &str) -> String {
    const ID: [f32; 4] = [0.0, 0.0, 0.0, 1.0];
    let mut nodes = node("Camera", [0.0, 4.0, 0.0], [1.0; 3], "Camera(fov_y: 1.0, active: true)", [
        -0.06, 0.0, 0.0, 0.998,
    ]);
    nodes += &node(
        "Ground",
        [0.0, -0.5, -1000.0],
        [4000.0, 1.0, 4000.0],
        "Primitive(shape: Cube, color: (0.8, 0.7, 0.5))",
        ID,
    );
    for i in 0..120 {
        let u = (i as f32 * 0.618_034).fract();
        let v = (i as f32 * 0.414_213_5).fract();
        let d = 60.0 * (1000.0f32 / 60.0).powf(u);
        let a = (v - 0.5) * 1.4;
        let r = d * 0.08;
        nodes += &node(
            &format!("Dune{i}"),
            [a.sin() * d, -r * 0.6, -a.cos() * d],
            [r * 2.5, r, r * 1.6],
            "Primitive(shape: Sphere, color: (0.82, 0.7, 0.5))",
            ID,
        );
    }
    nodes += &node(
        "Post",
        [0.0; 3],
        [1.0; 3],
        &format!("PostProcess(ao: {ao}, ao_radius: 0.5, ao_strength: 0.7)"),
        ID,
    );
    format!(
        "(\n    name: \"dunes\",\n    lighting: (\n        direction: (0.35, 0.85, 0.4),\n        \
         color: (1.0, 0.97, 0.9),\n        ambient: (0.3, 0.3, 0.33),\n        intensity: 1.0,\n        \
         fog: true,\n        fog_color: (0.75, 0.72, 0.68),\n        fog_start: 5.0,\n        \
         fog_end: 112.0,\n    ),\n    nodes: [\n{nodes}    ],\n)\n"
    )
}

fn shoot(d: &Path, scene: &str) -> Result<image::RgbaImage, String> {
    let out = d.join(format!("{scene}.png"));
    let r = Command::new(bin())
        .args(["shot", &d.to_string_lossy(), "--scene", scene, "--size", "480x270", "--out", &out.to_string_lossy()])
        .output()
        .expect("run shot");
    if !r.status.success() {
        return Err(String::from_utf8_lossy(&r.stderr).into_owned());
    }
    Ok(image::open(&out).expect("a PNG was written").to_rgba8())
}

/// Same branch as `shot_draws_the_ui.rs`: no adapter, or one that cannot build
/// the renderer, is not a failure of the thing under test.
fn cannot_render(why: &str) -> bool {
    let cannot = why.contains("no GPU") || why.contains("could not build the renderer");
    if cannot {
        eprintln!("skipped — this machine cannot render:\n{why}");
    }
    cannot
}

#[test]
fn distant_fogged_ground_looks_the_same_with_ssao_on() {
    let d = temp("dunes");
    let out = Command::new(bin())
        .args(["new", &d.to_string_lossy(), "--template", "platformer"])
        .output()
        .expect("run floptle new");
    assert!(out.status.success(), "scaffold failed: {}", String::from_utf8_lossy(&out.stderr));
    std::fs::write(d.join("scenes/dunes_on.ron"), scene("ScreenSpace")).unwrap();
    std::fs::write(d.join("scenes/dunes_off.ron"), scene("Off")).unwrap();

    let on = match shoot(&d, "dunes_on") {
        Ok(i) => i,
        Err(why) => {
            assert!(cannot_render(&why), "shot failed for a reason that is not the adapter:\n{why}");
            let _ = std::fs::remove_dir_all(&d);
            return;
        }
    };
    let off = shoot(&d, "dunes_off").expect("the AO-off shot");

    // The far half: the horizon band and the fogged field below it.
    let (w, h) = on.dimensions();
    let (mut total, mut dark) = (0u32, 0u32);
    for y in 0..h / 2 {
        for x in 0..w {
            let (a, b) = (on.get_pixel(x, y), off.get_pixel(x, y));
            let diff = (0..3).map(|c| (a[c] as i32 - b[c] as i32).abs()).max().unwrap();
            total += 1;
            if diff > 8 {
                dark += 1;
            }
        }
    }
    let pct = dark as f32 * 100.0 / total as f32;
    // Measured: 18% before the fix, about 1% after.
    assert!(pct < 4.0, "{pct:.1}% of the far half changed with SSAO on — AO is darkening distance or fog");

    let _ = std::fs::remove_dir_all(&d);
}
