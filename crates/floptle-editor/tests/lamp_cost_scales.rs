//! **Lamps out of reach cost nothing.**
//!
//! Every lit pixel ran the area-light maths of all sixteen lamps it was
//! given (a rect's four corners and a polygon integral) before asking
//! whether the lamp could reach it at all, so a base lit by a pool of kit
//! lamps paid for every one of them on every pixel, near or not. These time
//! the lighting pass ("opaque + lighting") of a floor with sixteen rect
//! lamps hung far out of their range against the same floor with none, in
//! the same run, and hold the ratio to a budget, never a duration. The two
//! pictures must also be the same, pixel for pixel: a lamp out of reach
//! lights nothing.
//!
//! Needs a GPU with timestamp queries; without one the test says so and
//! passes, as the other render probes do.

// The `floptle` binary needs the authoring half; see the note at the top of
// `the_json_verbs_emit_only_json.rs`.
#![cfg(feature = "editor-ui")]

use std::path::{Path, PathBuf};
use std::process::Command;

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fllampcost-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("scenes")).unwrap();
    d
}

/// A floor filling the view from above, and `lamps` rect lamps 3 m in range
/// hung 40 m over it: given to every pixel, reaching none.
fn floor(d: &Path, lamps: usize) {
    let mut nodes = vec![
        "(name: \"Camera\", transform: (translation: (0.0, 6.0, 0.0), rotation: (-0.70710677, 0.0, 0.0, 0.70710677)), matter: Camera(fov_y: 1.1, active: true))".to_string(),
        "(name: \"Floor\", transform: (translation: (0.0, -0.1, 0.0), scale: (40.0, 0.1, 40.0)), matter: Primitive(shape: Cube, color: (0.8, 0.8, 0.8)))".to_string(),
    ];
    for k in 0..lamps {
        let (x, z) = ((k % 4) as f32 * 3.0 - 4.5, (k / 4) as f32 * 3.0 - 4.5);
        nodes.push(format!(
            "(name: \"L{k}\", transform: (translation: ({x}, 40.0, {z})), \
             matter: PointLight(color: (1.0, 0.9, 0.8), intensity: 4.0, range: 3.0, shape: Rect(width: 2.0, height: 1.0)))"
        ));
    }
    std::fs::write(d.join("project.ron"), "(title: Some(\"l\"), entry_scene: Some(\"scenes/first.ron\"), retro: false)").unwrap();
    std::fs::write(
        d.join("scenes/first.ron"),
        format!(
            "(name: \"l\", lighting: (direction: (0.35, 1.0, 0.25), ambient: (0.1, 0.1, 0.1), shadows: false), nodes: [{}])",
            nodes.join(", ")
        ),
    )
    .unwrap();
}

/// The lighting pass's GPU milliseconds and the picture. `None` when this
/// machine cannot render or cannot time.
fn shoot(d: &Path) -> Option<(f64, Vec<u8>)> {
    let png = d.join("shot.png");
    let r = Command::new(env!("CARGO_BIN_EXE_floptle"))
        .args(["shot", &d.to_string_lossy(), "--size", "1280x720", "--timing", "--json"])
        .args(["--out", &png.to_string_lossy()])
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
    let ms = passes.iter().find(|p| p["label"] == "opaque + lighting").and_then(|p| p["ms"].as_f64())?;
    Some((ms, std::fs::read(&png).expect("the picture")))
}

/// Sixteen far lamps cost at most twice what none do, and draw the same
/// picture. Measured on an RTX 4060 at 1280×720: 0.41 ms with none; with
/// sixteen, 1.47 ms (3.6×) before the reach test and 0.66 ms (1.6×) after.
/// What is left is the loop asking each lamp its distance; clustered lights
/// would take that too.
#[test]
fn lamps_out_of_reach_cost_nothing() {
    let (dm, dn) = (temp("16"), temp("0"));
    floor(&dm, 16);
    floor(&dn, 0);
    // Each twice and the faster kept: the first draw of a scene also builds
    // its pipelines, and a background hiccup only ever adds time.
    let best = |d: &Path| {
        let (a, pa) = shoot(d)?;
        let (b, _) = shoot(d)?;
        Some((a.min(b), pa))
    };
    let Some((many, pic_many)) = best(&dm) else { return };
    let Some((none, pic_none)) = best(&dn) else { return };
    for d in [&dm, &dn] {
        let _ = std::fs::remove_dir_all(d);
    }
    assert!(pic_many == pic_none, "sixteen lamps out of reach changed the picture");
    assert!(
        many < none * 2.0,
        "sixteen lamps out of reach cost {many:.3} ms of lighting against {none:.3} ms for none ({:.1}x)",
        many / none
    );
}
