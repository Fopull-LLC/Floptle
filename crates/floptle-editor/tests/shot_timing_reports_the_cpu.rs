//! **`shot --timing` reports what the CPU spent on the frames it timed.**
//! The GPU passes alone cannot say whether a scene of many nodes is slow
//! because of the GPU or because of the work of building its frame: a
//! thousand nodes cost the CPU a walk, a cull and a repack a pass before
//! the GPU sees any of them. The report carries both, over the same frames.
//!
//! Needs a GPU with timestamp queries; without one the test says so and
//! passes, as the other render probes do.

// The `floptle` binary needs the authoring half; see the note at the top of
// `the_json_verbs_emit_only_json.rs`.
#![cfg(feature = "editor-ui")]

use std::process::Command;

#[test]
fn a_timed_shot_says_what_building_the_frame_cost() {
    let d = std::env::temp_dir().join(format!("flshotcpu-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("scenes")).unwrap();
    let mut nodes = vec![
        "(name: \"Camera\", transform: (translation: (0.0, 6.0, 20.0)), matter: Camera(fov_y: 1.0, active: true))".to_string(),
    ];
    for k in 0..400 {
        nodes.push(format!(
            "(name: \"B{k}\", transform: (translation: ({:.1}, 0.5, {:.1})), matter: Primitive(shape: Cube, color: (0.6, 0.6, 0.7)))",
            (k % 20) as f32 * 2.0 - 20.0,
            -((k / 20) as f32) * 2.0
        ));
    }
    std::fs::write(d.join("project.ron"), "(title: Some(\"t\"), entry_scene: Some(\"scenes/first.ron\"), retro: false)").unwrap();
    std::fs::write(
        d.join("scenes/first.ron"),
        format!("(name: \"t\", lighting: (direction: (0.3, 0.8, 0.4)), nodes: [{}])", nodes.join(", ")),
    )
    .unwrap();
    let r = Command::new(env!("CARGO_BIN_EXE_floptle"))
        .args(["shot", &d.to_string_lossy(), "--size", "320x240", "--timing", "--json"])
        .args(["--out", &d.join("shot.png").to_string_lossy()])
        .output()
        .expect("run shot");
    let _ = std::fs::remove_dir_all(&d);
    if !r.status.success() {
        let why = String::from_utf8_lossy(&r.stderr);
        assert!(
            why.contains("no GPU") || why.contains("could not build the renderer") || why.contains("timestamp"),
            "shot failed for a reason that is not the adapter:\n{why}"
        );
        eprintln!("skipped — this machine cannot render or time:\n{why}");
        return;
    }
    let out = String::from_utf8_lossy(&r.stdout);
    let v: serde_json::Value = serde_json::from_str(out.trim().lines().last().unwrap()).expect("shot --json");
    let cpu = &v["timing"]["cpu"];
    let total = cpu["cpu_ms"].as_f64().unwrap_or_else(|| panic!("no CPU report under timing: {v}"));
    assert!(total > 0.0, "the CPU report says the frames cost nothing: {cpu}");
    let parts: Vec<&str> = cpu["parts"].as_array().unwrap().iter().filter_map(|p| p["label"].as_str()).collect();
    for want in ["render", "renderCull", "renderInstancing", "renderPasses"] {
        assert!(parts.contains(&want), "the CPU report has no `{want}`: {parts:?}");
    }
}
