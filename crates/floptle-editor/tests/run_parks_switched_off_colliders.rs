//! **A node switched off by a script takes its colliders with it.**
//!
//! The sim is built once, from the nodes that were on at the time, so a level
//! a script turned off kept its invisible floors: rays hit them and bodies
//! stood on them. Its colliders now leave the sim when the node goes off and
//! come back when it comes on.

// The `floptle` binary needs the authoring half; see the note at the top of
// `the_json_verbs_emit_only_json.rs`.
#![cfg(feature = "editor-ui")]

use std::process::Command;

#[test]
fn a_switched_off_floor_stops_answering_rays_and_comes_back() {
    let d = std::env::temp_dir().join(format!("flrunpark-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("scenes")).unwrap();
    std::fs::create_dir_all(d.join("scripts")).unwrap();
    std::fs::write(d.join("project.ron"), "(title: Some(\"p\"), entry_scene: Some(\"scenes/first.ron\"))").unwrap();
    std::fs::write(
        d.join("scripts/probe.lua"),
        "local frames, floor = 0, nil\n\
         function update(node, dt)\n\
         \x20 frames = frames + 1\n\
         \x20 floor = floor or find('Level')\n\
         \x20 if frames == 5 then print('on ' .. tostring(raycast(0, 10, 0, 0, -1, 0, 20) ~= nil)); floor.enabled = false end\n\
         \x20 if frames == 8 then print('off ' .. tostring(raycast(0, 10, 0, 0, -1, 0, 20) ~= nil)); floor.enabled = true end\n\
         \x20 if frames == 11 then print('back ' .. tostring(raycast(0, 10, 0, 0, -1, 0, 20) ~= nil)) end\n\
         end\n",
    )
    .unwrap();
    // The floor is a child of `Level`: switching the parent off is how a game
    // turns off a whole level, and it has to reach the colliders under it.
    std::fs::write(
        d.join("scenes/first.ron"),
        "(name: \"s\", nodes: [\
         (id: Some(1), name: \"Level\"),\
         (id: Some(2), name: \"Floor\", parent_id: Some(1), transform: (scale: (10.0, 1.0, 10.0)), matter: Primitive(shape: Cube, color: (0.5, 0.5, 0.5)), collidable: true),\
         (name: \"Probe\", scripts: [(kind: \"probe\")])])",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_floptle"))
        .args(["run", &d.to_string_lossy(), "--seconds", "0.3", "--json"])
        .output()
        .expect("run floptle run");
    let text = String::from_utf8_lossy(&out.stdout);
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"));
    let said = |needle: &str| {
        doc["log"].as_array().unwrap().iter().any(|l| l["message"].as_str().is_some_and(|m| m.contains(needle)))
    };
    assert!(said("on true"), "the fixture: the floor answers a ray while on: {text}");
    assert!(said("off false"), "a switched-off level still answered a ray: {text}");
    assert!(said("back true"), "the floor did not come back: {text}");
    let _ = std::fs::remove_dir_all(&d);
}

/// **A spawned prefab's static geometry collides the frame after it appears.**
/// A Collidable with no RigidBody is world geometry; the spawn wired bodies
/// only, so a spawned wall could be seen and walked through until something
/// rebuilt the whole sim.
#[test]
fn a_spawned_collidable_prefab_answers_rays() {
    let d = std::env::temp_dir().join(format!("flrunspawnwall-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    for sub in ["scenes", "scripts", "prefabs"] {
        std::fs::create_dir_all(d.join(sub)).unwrap();
    }
    std::fs::write(d.join("project.ron"), "(title: Some(\"w\"), entry_scene: Some(\"scenes/first.ron\"))").unwrap();
    std::fs::write(
        d.join("prefabs/wall.prefab.ron"),
        "[(name: \"Wall\", transform: (scale: (4.0, 1.0, 4.0)), matter: Primitive(shape: Cube, color: (0.5, 0.5, 0.5)), collidable: true)]",
    )
    .unwrap();
    std::fs::write(
        d.join("scripts/builder.lua"),
        "local frames = 0\n\
         function update(node, dt)\n\
         \x20 frames = frames + 1\n\
         \x20 if frames == 3 then print('before ' .. tostring(raycast(0, 10, 0, 0, -1, 0, 20) ~= nil)); spawn('wall', vec3(0, 0, 0)) end\n\
         \x20 if frames == 6 then print('after ' .. tostring(raycast(0, 10, 0, 0, -1, 0, 20) ~= nil)) end\n\
         end\n",
    )
    .unwrap();
    std::fs::write(d.join("scenes/first.ron"), "(name: \"s\", nodes: [(name: \"Builder\", scripts: [(kind: \"builder\")])])")
        .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_floptle"))
        .args(["run", &d.to_string_lossy(), "--seconds", "0.3", "--json"])
        .output()
        .expect("run floptle run");
    let text = String::from_utf8_lossy(&out.stdout);
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"));
    let said = |needle: &str| {
        doc["log"].as_array().unwrap().iter().any(|l| l["message"].as_str().is_some_and(|m| m.contains(needle)))
    };
    assert!(said("before false"), "the fixture: nothing there before the spawn: {text}");
    assert!(said("after true"), "the spawned wall answered no ray: {text}");
    let _ = std::fs::remove_dir_all(&d);
}
