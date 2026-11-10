//! **The post chain is timed stage by stage.** `shot --timing` named one
//! region "post" for ambient occlusion, bloom, depth of field, motion blur
//! and the finish together, so a profile could say the chain cost 4.7 ms
//! and not which part of it. Each stage that runs now marks its own region.
//!
//! Needs a GPU with timestamp queries; without one the test says so and
//! passes, as the other render probes do.

// The `floptle` binary needs the authoring half; see the note at the top of
// `the_json_verbs_emit_only_json.rs`.
#![cfg(feature = "editor-ui")]

use std::process::Command;

#[test]
fn each_post_stage_that_runs_is_its_own_region() {
    let d = std::env::temp_dir().join(format!("flpoststages-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("scenes")).unwrap();
    std::fs::write(d.join("project.ron"), "(title: Some(\"p\"), entry_scene: Some(\"scenes/first.ron\"), retro: false)").unwrap();
    std::fs::write(
        d.join("scenes/first.ron"),
        "(name: \"p\", lighting: (direction: (0.3, 0.8, 0.4), ambient: (0.2, 0.2, 0.2)), nodes: [\
         (name: \"Camera\", transform: (translation: (0.0, 2.0, 6.0)), matter: Camera(fov_y: 1.0, active: true)),\
         (name: \"Floor\", transform: (translation: (0.0, -1.0, 0.0), scale: (40.0, 1.0, 40.0)), matter: Primitive(shape: Cube, color: (0.6, 0.6, 0.6))),\
         (name: \"Box\", transform: (translation: (0.0, 0.5, 0.0)), matter: Primitive(shape: Cube, color: (0.8, 0.3, 0.2))),\
         (name: \"Post\", matter: PostProcess(ssao: true, bloom: true)),\
         ])",
    )
    .unwrap();
    let r = Command::new(env!("CARGO_BIN_EXE_floptle"))
        .args(["shot", &d.to_string_lossy(), "--size", "640x360", "--timing", "--json"])
        .args(["--out", &d.join("shot.png").to_string_lossy()])
        .output()
        .expect("run shot");
    let _ = std::fs::remove_dir_all(&d);
    if !r.status.success() {
        let why = String::from_utf8_lossy(&r.stderr);
        assert!(
            why.contains("no GPU") || why.contains("could not build the renderer"),
            "shot failed for a reason that is not the adapter:\n{why}"
        );
        eprintln!("skipped — this machine cannot render:\n{why}");
        return;
    }
    let out = String::from_utf8_lossy(&r.stdout);
    let v: serde_json::Value = serde_json::from_str(out.trim().lines().last().unwrap()).expect("shot --json");
    let Some(passes) = v["timing"]["passes"].as_array() else {
        eprintln!("skipped — this GPU reports no timestamps");
        return;
    };
    let labels: Vec<&str> = passes.iter().filter_map(|p| p["label"].as_str()).collect();
    for stage in ["post: AO", "post: bloom", "post: grade + finish"] {
        assert!(labels.contains(&stage), "no \"{stage}\" region in the timing: {labels:?}");
    }
    assert!(!labels.contains(&"post: depth of field"), "a stage that did not run was timed: {labels:?}");
}
