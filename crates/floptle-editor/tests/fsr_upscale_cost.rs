//! **FSR 1 costs a few bilinear upscales, not ten.** In one pass, every
//! output pixel ran FSR's edge-adaptive upscale (EASU, twelve texels) five
//! times: its own and its four neighbours' for the sharpen (RCAS). It now
//! upscales once into a picture the output's size and sharpens that. These
//! time the "upscale" region of `shot --timing` at 2560×1440 from a 0.67
//! render scale, FSR 1 against bilinear in the same run, and hold the ratio
//! to a budget, never a duration.
//!
//! Needs a GPU with timestamp queries; without one the test says so and
//! passes, as the other render probes do.

// The `floptle` binary needs the authoring half; see the note at the top of
// `the_json_verbs_emit_only_json.rs`.
#![cfg(feature = "editor-ui")]

use std::path::{Path, PathBuf};
use std::process::Command;

fn project(up: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("flfsrcost-{up}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("scenes")).unwrap();
    std::fs::write(
        d.join("project.ron"),
        format!("(title: Some(\"u\"), entry_scene: Some(\"scenes/first.ron\"), retro: false, render_scale: 0.67, upscale: Some({up}))"),
    )
    .unwrap();
    std::fs::write(
        d.join("scenes/first.ron"),
        "(name: \"u\", lighting: (direction: (0.3, 0.8, 0.4), ambient: (0.2, 0.2, 0.2)), nodes: [\
         (name: \"Camera\", transform: (translation: (0.0, 2.0, 6.0)), matter: Camera(fov_y: 1.0, active: true)),\
         (name: \"Floor\", transform: (translation: (0.0, -1.0, 0.0), scale: (40.0, 1.0, 40.0)), matter: Primitive(shape: Cube, color: (0.6, 0.6, 0.6))),\
         (name: \"Box\", transform: (translation: (0.0, 0.5, 0.0)), matter: Primitive(shape: Cube, color: (0.8, 0.3, 0.2))),\
         ])",
    )
    .unwrap();
    d
}

/// The upscale's GPU milliseconds, the faster of two shots. `None` when
/// this machine cannot render or cannot time.
fn upscale_ms(d: &Path) -> Option<f64> {
    let one = || -> Option<f64> {
        let r = Command::new(env!("CARGO_BIN_EXE_floptle"))
            .args(["shot", &d.to_string_lossy(), "--size", "2560x1440", "--timing", "--json"])
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
        let ms = passes.iter().find(|p| p["label"] == "upscale").and_then(|p| p["ms"].as_f64());
        assert!(ms.is_some(), "no \"upscale\" region in the timing: {passes:?}");
        ms
    };
    Some(one()?.min(one()?))
}

/// Measured on an RTX 4060: bilinear 0.10 ms, FSR 1 0.42 in two passes
/// (4.2×) and 1.10 in one (8.5×).
#[test]
fn fsr1_costs_a_few_bilinear_upscales() {
    let (b, f) = (project("Bilinear"), project("Fsr1"));
    let Some(bilinear) = upscale_ms(&b) else { return };
    let Some(fsr) = upscale_ms(&f) else { return };
    for d in [&b, &f] {
        let _ = std::fs::remove_dir_all(d);
    }
    assert!(
        fsr < bilinear.max(0.05) * 6.0,
        "FSR 1 cost {fsr:.3} ms against {bilinear:.3} ms for bilinear ({:.1}x)",
        fsr / bilinear.max(0.05)
    );
}
