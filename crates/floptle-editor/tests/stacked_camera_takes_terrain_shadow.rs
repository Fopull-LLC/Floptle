//! **A stacked camera's layer is shaded by the terrain the base camera stands
//! in.**
//!
//! A first-person view model on a stacked camera's own layer was lit as in
//! open sun inside a cave: the stacked pass draws no terrain, so its sun map
//! held only the view model, and where a sun map has an answer the shader
//! takes it and marches nothing. The ball here sits in a cave dug under a
//! small planet's surface, drawn once by the main camera and once by a
//! stacked one; the two must come out alike, and both darker than the same
//! ball in open sun.

// The `floptle` binary needs the authoring half; see the note at the top of
// `the_json_verbs_emit_only_json.rs`.
#![cfg(feature = "editor-ui")]

use std::process::Command;

/// Make the project, shoot it, and give the mean brightness of the ball's
/// top, the side the sun lights; `None` when this machine cannot render.
fn shot(tag: &str, cave: bool, stacked: bool) -> Option<f32> {
    let d = std::env::temp_dir().join(format!("flstackshade-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    for sub in ["scenes", "scripts"] {
        std::fs::create_dir_all(d.join(sub)).unwrap();
    }
    std::fs::write(
        d.join("project.ron"),
        "(title: Some(\"p\"), entry_scene: Some(\"scenes/first.ron\"), retro: false, layers: [\"Default\", \"Arms\"])",
    )
    .unwrap();
    // A cave under the surface: the sun, straight above, reaches nothing in it.
    std::fs::write(
        d.join("scripts/gen.lua"),
        "function start(node)\n  node:setTerrain(1)\n  \
         terrain.generatePlanet(1, { radius = 24, voxel = 1, relief = 0, seed = 3 })\nend\n\
         local dug = false\nfunction update(node, dt)\n  \
         if not dug and not terrain.busy() then\n    dug = true\n    \
         for i = 1, 12 do terrain.dig(0, 16, 0, 5, 1) end\n  end\nend\n",
    )
    .unwrap();
    let (eye, ball) = if cave { ("(0.0, 16.0, 2.0)", "(0.0, 16.0, -1.0)") } else { ("(0.0, 60.0, 2.0)", "(0.0, 60.0, -1.0)") };
    let (main_mask, ball_layer) =
        if stacked { ("cull_mask: 1, stack: [\"Arms\"], ", "layer: Some(\"Arms\"), ") } else { ("", "") };
    let arms = if stacked {
        format!(", (name: \"Arms\", transform: (translation: {eye}), matter: Camera(fov_y: 1.0, cull_mask: 2))")
    } else {
        String::new()
    };
    std::fs::write(
        d.join("scenes/first.ron"),
        format!(
            "(name: \"s\", lighting: (direction: (0.1, 1.0, 0.05), ambient: (0.05, 0.05, 0.05), shadows: true, shadow_map: true), nodes: [\
               (name: \"Planet\", scripts: [(kind: \"gen\")]),\
               (name: \"Ball\", {ball_layer}transform: (translation: {ball}, scale: (1.5, 1.5, 1.5)), matter: Primitive(shape: Sphere, color: (1.0, 1.0, 1.0))),\
               (name: \"Cam\", transform: (translation: {eye}), matter: Camera(fov_y: 1.0, active: true, {main_mask}))\
               {arms}])"
        ),
    )
    .unwrap();
    let out = d.join("shot.png");
    let run = Command::new(env!("CARGO_BIN_EXE_floptle"))
        .args(["shot", &d.to_string_lossy(), "--size", "160x90", "--no-ui", "--after", "4", "--out"])
        .arg(&out)
        .output()
        .expect("run floptle shot");
    if !run.status.success() {
        let why = String::from_utf8_lossy(&run.stderr).into_owned();
        let cannot = why.contains("no GPU") || why.contains("could not build the renderer");
        assert!(cannot, "shot failed for a reason that is not the adapter:\n{why}");
        eprintln!("skipped — this machine cannot render:\n{why}");
        return None;
    }
    let img = image::open(&out).expect("the shot is a PNG").to_rgb8();
    let (mut sum, mut n) = (0.0f32, 0.0f32);
    for y in 22..32 {
        for x in 75..85 {
            let p = img.get_pixel(x, y);
            sum += (p[0] as f32 + p[1] as f32 + p[2] as f32) / 3.0;
            n += 1.0;
        }
    }
    let _ = std::fs::remove_dir_all(&d);
    Some(sum / n)
}

#[test]
fn a_stacked_camera_layer_is_shaded_by_the_terrain_around_it() {
    let Some(open) = shot("open", false, false) else { return };
    let Some(main) = shot("main", true, false) else { return };
    let Some(stacked) = shot("stacked", true, true) else { return };
    assert!(main < open * 0.6, "fixture: the ball in the cave ({main:.0}) should be well darker than in open sun ({open:.0})");
    assert!(
        stacked < main * 1.25 + 4.0,
        "drawn by a stacked camera the ball in the cave is {stacked:.0}, by the main camera {main:.0} (in open sun {open:.0})"
    );
}
