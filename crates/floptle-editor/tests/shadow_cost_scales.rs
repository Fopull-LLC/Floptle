//! **What shadows cost does not climb with the boxes around you.**
//!
//! A building made of many box colliders made the lighting pass inside it
//! several times dearer than outside: the sun's shadow march tested every box
//! at every step of every ray. These time the lighting pass ("opaque +
//! lighting") with 32 box casters overhead against 2, in the same run, and
//! hold the ratio to a budget, never a duration: a machine's speed cancels
//! out of a ratio.
//!
//! Needs a GPU with timestamp queries; without one the test says so and
//! passes, as the other render probes do.

// The `floptle` binary needs the authoring half; see the note at the top of
// `the_json_verbs_emit_only_json.rs`.
#![cfg(feature = "editor-ui")]

use std::path::{Path, PathBuf};
use std::process::Command;

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("flshadowcost-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("scenes")).unwrap();
    d
}

/// A floor under a lattice of `boxes` box colliders, looked at from under
/// it, so every pixel's shadow ray passes between boxes. `stars` lights it
/// with a star and the shadow map instead of the Lighting sun and the march;
/// the boxes are then drawn too, since the map sees what is drawn.
fn lattice(d: &Path, boxes: usize, stars: bool) {
    let mut nodes = vec![
        "(name: \"Camera\", transform: (translation: (0.0, 2.6, 0.0), rotation: (-0.70710677, 0.0, 0.0, 0.70710677)), matter: Camera(fov_y: 1.1, active: true))".to_string(),
        "(name: \"Floor\", transform: (translation: (0.0, -0.1, 0.0), scale: (30.0, 0.1, 30.0)), matter: Primitive(shape: Cube, color: (0.8, 0.8, 0.8)))".to_string(),
    ];
    for k in 0..boxes {
        let (ix, iz) = (k / 4, k % 4);
        let (x, z) = (-7.2 + ix as f32 * 1.6, -3.0 + iz as f32 * 2.0);
        let drawn = if stars { ", matter: Primitive(shape: Cube, color: (0.5, 0.5, 0.6))" } else { "" };
        nodes.push(format!(
            "(name: \"B{k}\", transform: (translation: ({x}, 3.0, {z}), scale: (1.0, 0.3, 1.4)){drawn}, \
             rigidbody: Some((boxed: true, mode: Static, half_extents: (0.5, 0.15, 0.7))))"
        ));
    }
    if stars {
        nodes.push(
            "(name: \"Star\", transform: (translation: (1400.0, 4000.0, 1000.0)), celestial: Some((luminosity: 40.0, body_radius: 10.0)))"
                .to_string(),
        );
    }
    std::fs::write(d.join("project.ron"), "(title: Some(\"c\"), entry_scene: Some(\"scenes/first.ron\"), retro: false)").unwrap();
    std::fs::write(
        d.join("scenes/first.ron"),
        format!(
            "(name: \"c\", lighting: (direction: (0.35, 1.0, 0.25), ambient: (0.1, 0.1, 0.1), shadows: true, \
             shadow_softness: 0.3, shadow_map: {stars}, stars: {stars}), nodes: [{}])",
            nodes.join(", ")
        ),
    )
    .unwrap();
}

/// The lighting pass's GPU milliseconds, the median `shot --timing` reports.
/// `None` when this machine cannot render or cannot time.
fn lighting_ms(d: &Path) -> Option<f64> {
    let r = Command::new(env!("CARGO_BIN_EXE_floptle"))
        .args(["shot", &d.to_string_lossy(), "--size", "1280x720", "--timing", "--json"])
        .args(["--out", &d.join("shot.png").to_string_lossy()])
        .output()
        .expect("run shot");
    if !r.status.success() {
        let why = String::from_utf8_lossy(&r.stderr);
        assert!(
            why.contains("no GPU") || why.contains("could not build the renderer"),
            "shot failed for a reason that is not the adapter:\n{why}"
        );
        eprintln!("skipped — this machine cannot render:\n{why}");
        return None;
    }
    let out = String::from_utf8_lossy(&r.stdout);
    let v: serde_json::Value = serde_json::from_str(out.trim().lines().last()?).expect("shot --json");
    let Some(passes) = v["timing"]["passes"].as_array() else {
        eprintln!("skipped — this GPU reports no timestamps");
        return None;
    };
    passes.iter().find(|p| p["label"] == "opaque + lighting").and_then(|p| p["ms"].as_f64())
}

fn ratio(many: usize, few: usize, stars: bool) -> Option<(f64, f64)> {
    let (dm, df) = (temp(&format!("{many}-{stars}")), temp(&format!("{few}-{stars}")));
    lattice(&dm, many, stars);
    lattice(&df, few, stars);
    // Each twice and the faster kept: the first draw of a scene also builds
    // its pipelines, and a background hiccup only ever adds time.
    let best = |d: &Path| Some(lighting_ms(d)?.min(lighting_ms(d)?));
    let out = (best(&dm)?, best(&df)?);
    for d in [&dm, &df] {
        let _ = std::fs::remove_dir_all(d);
    }
    Some(out)
}

/// One test, not two, so the two measurements never share the GPU with each
/// other: a ratio taken while another scene renders beside it is noise.
///
/// The march: 32 casters overhead cost about four times 2 casters. Testing
/// every box at every step made it nine.
///
/// Stars mode with the shadow map: the brightest star reads the map, whose
/// cost is the same however many boxes are around.
#[test]
fn shadow_cost_stays_flat_as_box_casters_are_added() {
    let Some((many, few)) = ratio(32, 2, false) else { return };
    assert!(
        many < few * 6.0,
        "the march: 32 box casters cost {many:.3} ms of lighting against {few:.3} ms for 2 ({:.1}x)",
        many / few
    );
    let Some((many, few)) = ratio(40, 2, true) else { return };
    assert!(
        many < few * 1.15,
        "the shadow map: 40 box casters cost {many:.3} ms of lighting against {few:.3} ms for 2"
    );
}
