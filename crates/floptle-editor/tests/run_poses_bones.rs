//! **`floptle run` poses a rigged model, so a script can ask where a bone is.**
//!
//! A host with nothing to draw with never imported models at all, so a rigged
//! character had no skeleton under `run` or on a dedicated server:
//! `anim:boneWorld` answered nil and bone writes did nothing. The rig is now
//! read without its pictures and posed like anywhere else.

// The `floptle` binary needs the authoring half; see the note at the top of
// `the_json_verbs_emit_only_json.rs`.
#![cfg(feature = "editor-ui")]

use std::process::Command;

#[test]
fn a_run_answers_bone_world_for_a_rigged_model() {
    let d = std::env::temp_dir().join(format!("flrunbones-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("scenes")).unwrap();
    std::fs::create_dir_all(d.join("scripts")).unwrap();
    let sae = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/Sae.glb");
    std::fs::copy(sae, d.join("Sae.glb")).unwrap();
    std::fs::write(d.join("project.ron"), "(title: Some(\"b\"), entry_scene: Some(\"scenes/first.ron\"))").unwrap();
    std::fs::write(
        d.join("scripts/ask.lua"),
        "local frames = 0\n\
         function update(node, dt)\n\
         \x20 local a = find('Sae'):animator()\n\
         \x20 a:addBoneRot('Head', 0, 0, 0)\n\
         \x20 frames = frames + 1\n\
         \x20 if frames == 5 then\n\
         \x20   local p = a:boneWorld('Hand')\n\
         \x20   print(p and ('hand at ' .. p.x .. ' ' .. p.y .. ' ' .. p.z) or 'hand is nil')\n\
         \x20 end\n\
         end\n",
    )
    .unwrap();
    std::fs::write(
        d.join("scenes/first.ron"),
        "(name: \"s\", nodes: [\
         (name: \"Sae\", matter: Mesh(asset_path: \"Sae.glb\")),\
         (name: \"Driver\", scripts: [(kind: \"ask\")])])",
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
    assert!(said("hand at "), "boneWorld had no answer under run: {text}");
    assert!(!said("hand is nil"), "boneWorld answered nil under run: {text}");
    let _ = std::fs::remove_dir_all(&d);
}

/// The foot-planting recipe in `docs/animation.md` runs as written (with this
/// model's bone names), and a `reach` bends a straight chain to land its tip
/// where it was sent, read back through `boneWorld`, all under `run`.
#[test]
fn a_run_reaches_a_chain_to_a_point_and_the_recipe_runs() {
    let d = std::env::temp_dir().join(format!("flrunreach-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("scenes")).unwrap();
    std::fs::create_dir_all(d.join("scripts")).unwrap();
    let sae = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/models/Sae.mirrored.rigged.glb");
    std::fs::copy(sae, d.join("Sae.glb")).unwrap();
    std::fs::write(d.join("project.ron"), "(title: Some(\"r\"), entry_scene: Some(\"scenes/first.ron\"))").unwrap();
    std::fs::write(
        d.join("scripts/plant.lua"),
        r#"local HIP_DROP_MAX = 0.35
local frames, goal = 0, nil

function update(node, dt)
  local anim = find('Sae'):animator()
  local drop, hits = 0, {}
  for _, foot in ipairs({ "Heel.L", "Heel.R" }) do
    local p = anim:boneWorld(foot)
    if p then
      local hit = raycast(p + vec3(0, 0.5, 0), vec3(0, -1, 0), 1.5)
      if hit then
        hits[foot] = vec3(hit.x, hit.y, hit.z)
        drop = math.max(drop, p.y - hit.y)
      end
    end
  end
  drop = math.min(drop, HIP_DROP_MAX)
  anim:addBonePos("Hip", vec3(0, -drop, 0))
  local fwd = node.forward
  for foot, point in pairs(hits) do
    local knee = anim:boneWorld(foot) + fwd * 0.5 + vec3(0, 0.5, 0)
    anim:reach(foot, point, { pole = knee })
  end

  frames = frames + 1
  if frames == 3 then
    -- The hair hangs straight down: reach its second link a little way out
    -- to the side, closer than the chain is long, so it has to bend.
    local r, tip = anim:boneWorld("Hair_root"), anim:boneWorld("Hair_2")
    local dx, dy, dz = tip.x - r.x, tip.y - r.y, tip.z - r.z
    local len = math.sqrt(dx * dx + dy * dy + dz * dz)
    goal = r + vec3(0.6, -0.6, 0.2) * (len * 0.8 / math.sqrt(0.76))
    print("chain is " .. len .. " long")
  end
  if goal then
    anim:reach("Hair_2", goal)
  end
  if frames == 10 then
    local p = anim:boneWorld("Hair_2")
    local dx, dy, dz = p.x - goal.x, p.y - goal.y, p.z - goal.z
    print("tip off by " .. math.sqrt(dx * dx + dy * dy + dz * dz))
  end
end
"#,
    )
    .unwrap();
    std::fs::write(
        d.join("scenes/first.ron"),
        "(name: \"s\", nodes: [\
         (name: \"Sae\", matter: Mesh(asset_path: \"Sae.glb\")),\
         (name: \"Driver\", scripts: [(kind: \"plant\")])])",
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_floptle"))
        .args(["run", &d.to_string_lossy(), "--seconds", "0.3", "--json"])
        .output()
        .expect("run floptle run");
    let text = String::from_utf8_lossy(&out.stdout);
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"));
    assert_eq!(doc["errors"], 0, "the recipe raised: {text}");
    assert_eq!(doc["warnings"], 0, "the recipe warned: {text}");
    let off = doc["log"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|l| l["message"].as_str()?.strip_prefix("tip off by ")?.trim().parse::<f64>().ok())
        .unwrap_or_else(|| panic!("the tip never reported: {text}"));
    assert!(off < 0.005, "reach left the tip {off} m from where it was sent: {text}");
    let _ = std::fs::remove_dir_all(&d);
}
