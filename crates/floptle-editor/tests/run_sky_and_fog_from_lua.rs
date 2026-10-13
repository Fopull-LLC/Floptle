//! **A script can fade a sky.** The fog's sky blend (`fogSky`), the round-world
//! fog switch (`fogFollowsBody`) and the Skybox's colour and tint are all read
//! and written from Lua, so a game climbing out of the air can thin them
//! without a sky shader of its own.

// The `floptle` binary needs the authoring half; see the note at the top of
// `the_json_verbs_emit_only_json.rs`.
#![cfg(feature = "editor-ui")]

use std::process::Command;

#[test]
fn a_script_reads_and_writes_the_sky_and_the_fogs_sky_blend() {
    let d = std::env::temp_dir().join(format!("flrunsky-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("scenes")).unwrap();
    std::fs::create_dir_all(d.join("scripts")).unwrap();
    std::fs::write(d.join("project.ron"), "(title: Some(\"k\"), entry_scene: Some(\"scenes/first.ron\"))").unwrap();
    std::fs::write(
        d.join("scripts/fade.lua"),
        r#"local frames = 0
function update(node, dt)
  frames = frames + 1
  local sky = find('Sky'):getcomponent('Skybox')
  local light = find('Lighting'):getcomponent('Light')
  if frames == 2 then
    print(string.format("before sky %.2f %.2f fogSky %.2f round %s", sky.colorB, sky.tintR, light.fogSky, tostring(light.fogFollowsBody)))
    sky.colorB = 0.25
    sky.tintR = 0.5
    light.fogSky = 0.3
    light.fogFollowsBody = true
  elseif frames == 4 then
    print(string.format("after sky %.2f %.2f fogSky %.2f round %s", sky.colorB, sky.tintR, light.fogSky, tostring(light.fogFollowsBody)))
  end
end
"#,
    )
    .unwrap();
    std::fs::write(
        d.join("scenes/first.ron"),
        "(name: \"s\", nodes: [\
         (name: \"Sky\", matter: Skybox(color: (0.1, 0.1, 0.9))),\
         (name: \"Driver\", scripts: [(kind: \"fade\")])])",
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_floptle"))
        .args(["run", &d.to_string_lossy(), "--seconds", "0.2", "--json"])
        .output()
        .expect("run floptle run");
    let text = String::from_utf8_lossy(&out.stdout);
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"));
    assert_eq!(doc["errors"], 0, "the script raised: {text}");
    let line = |tag: &str| {
        doc["log"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|l| l["message"].as_str().filter(|m| m.starts_with(tag)).map(str::to_string))
            .unwrap_or_else(|| panic!("no `{tag}` line: {text}"))
    };
    assert_eq!(line("before "), "before sky 0.90 1.00 fogSky 1.00 round false");
    assert_eq!(line("after "), "after sky 0.25 0.50 fogSky 0.30 round true");
    let _ = std::fs::remove_dir_all(&d);
}
