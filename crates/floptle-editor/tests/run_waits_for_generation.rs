//! **`floptle run` gives background terrain generation the real time it needs.**
//!
//! A planet generated from `terrain.generatePlanet` is built on a worker
//! thread against the wall clock, and `run` steps as fast as the CPU allows:
//! a two-second run was over in milliseconds, long before the planet was, so a
//! generated world never finished loading and the run tested only its loading
//! screen. The run now holds each step while that work is in flight, and the
//! report says how long it held and whether anything was left unfinished.

// The `floptle` binary needs the authoring half; see the note at the top of
// `the_json_verbs_emit_only_json.rs`.
#![cfg(feature = "editor-ui")]

use std::process::Command;

#[test]
fn a_run_holds_its_steps_until_a_generated_planet_is_finished() {
    let d = std::env::temp_dir().join(format!("flrungen-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("scenes")).unwrap();
    std::fs::create_dir_all(d.join("scripts")).unwrap();
    std::fs::write(d.join("project.ron"), "(title: Some(\"g\"), entry_scene: Some(\"scenes/first.ron\"))").unwrap();
    std::fs::write(
        d.join("scripts/gen.lua"),
        "function start(node)\n  node:setTerrain(1)\n  terrain.generatePlanet(1, { radius = 24, voxel = 1, seed = 3 })\nend\n",
    )
    .unwrap();
    std::fs::write(d.join("scenes/first.ron"), "(name: \"s\", nodes: [(name: \"Planet\", scripts: [(kind: \"gen\")])])")
        .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_floptle"))
        .args(["run", &d.to_string_lossy(), "--seconds", "0.5", "--json"])
        .output()
        .expect("run floptle run");
    let text = String::from_utf8_lossy(&out.stdout);
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"));
    assert_eq!(doc["terrain_still_generating"], false, "the run ended with the planet unfinished: {text}");
    assert!(doc["terrain_held_s"].as_f64().unwrap() > 0.0, "the run never waited for the planet: {text}");
    assert!(
        doc["log"].as_array().unwrap().iter().any(|l| l["message"].as_str().is_some_and(|m| m.contains("ready"))),
        "the planet never reported ready: {text}"
    );
    let _ = std::fs::remove_dir_all(&d);
}
