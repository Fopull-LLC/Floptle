//! **`floptle shot` draws the UI** (`floptle/0224`).
//!
//! `shot` is how a project is looked at from a terminal — `run` reports what
//! raised, `shot` shows what a player would see. Except that it ran the world
//! passes, post and the retro upscale and stopped: no UI pass, so a scene whose
//! whole point is a screen — a main menu, a character creator, a dialogue box,
//! a HUD — came out as its backdrop. Three of the four scenes in the project
//! that reported it were UI-first, every one "verified" by `run` and wrong on
//! first sight: token names where `ui.make` wanted colours, a button that did
//! not grow, a panel with its text off the edge. No headless verb could have
//! shown any of it.
//!
//! The guard is the card's: one screen-space layer holding one 100×100 filled
//! box pinned top-left, photographed, and the box's pixels read back. Delete
//! the UI pass and it fails. A second scene builds the same box with `ui.make`
//! in `start`, which is play-only, so it is in the picture only under
//! `--after` — the other half of the card.

// The `floptle` binary needs the authoring half; see the note at the top of
// `the_json_verbs_emit_only_json.rs`.
#![cfg(feature = "editor-ui")]

use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_floptle")
}

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("flshotui-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn scaffold(dir: &Path) {
    let out = Command::new(bin())
        .args(["new", &dir.to_string_lossy(), "--template", "platformer"])
        .output()
        .expect("run floptle new");
    assert!(out.status.success(), "scaffold failed: {}", String::from_utf8_lossy(&out.stderr));
}

const NODE_HEAD: &str = "            transform: (translation: (0.0, 0.0, 0.0), \
    rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)),\n";

/// A camera looking at nothing, one screen-space layer sized so that at
/// 160×90 one design unit is one pixel, and — when `authored` — a red 100×100
/// box pinned to the layer's top-left. `script` is a script on the layer.
fn scene(authored: bool, script: Option<&str>) -> String {
    let mut s = String::from("(\n    name: \"menu\",\n    nodes: [\n");
    s.push_str("        (\n            name: \"Camera\",\n");
    s.push_str(NODE_HEAD);
    s.push_str("            matter: Camera(fov_y: 1.0, active: true),\n            scripts: [],\n        ),\n");
    s.push_str("        (\n            name: \"Menu\",\n");
    s.push_str(NODE_HEAD);
    s.push_str("            matter: Empty,\n");
    match script {
        Some(k) => s.push_str(&format!(
            "            scripts: [(kind: \"{k}\", enabled: true, params: [])],\n"
        )),
        None => s.push_str("            scripts: [],\n"),
    }
    s.push_str(
        "            ui_layer: Some((design_height: 90.0, reference_width: 160.0, \
         scale_mode: MatchHeight, match_wh: 0.5, z: 0, enabled: true, space: Screen, \
         canvas_scale: 0.01, nav_wrap: true, tooltip_delay: 0.35)),\n        ),\n",
    );
    if authored {
        s.push_str("        (\n            name: \"Box\",\n");
        s.push_str(NODE_HEAD);
        s.push_str("            matter: Empty,\n            scripts: [],\n            parent: Some(1),\n");
        s.push_str(
            "            ui: Some((place: Free(pos: (0.0, 0.0)), size: (Fixed(100.0), Fixed(100.0)), \
             shape: Some((fill: (1.0, 0.0, 0.0, 1.0), border_color: (0.0, 0.0, 0.0, 0.0), \
             gradient: None)))),\n        ),\n",
        );
    }
    s.push_str("    ],\n)\n");
    s
}

fn shoot(d: &Path, scene: &str, extra: &[&str]) -> Result<image::RgbaImage, String> {
    let out = d.join(format!("{scene}{}.png", extra.join("")));
    let r = Command::new(bin())
        .args(["shot", &d.to_string_lossy(), "--scene", scene, "--size", "160x90", "--out", &out.to_string_lossy()])
        .args(extra)
        .output()
        .expect("run shot");
    if !r.status.success() {
        return Err(String::from_utf8_lossy(&r.stderr).into_owned());
    }
    Ok(image::open(&out).expect("a PNG was written").to_rgba8())
}

fn is_red(p: &image::Rgba<u8>) -> bool {
    p[0] > 200 && p[1] < 60 && p[2] < 60
}

/// True when this machine cannot render at all, said out loud. Two ways: no
/// adapter, or one that cannot build the renderer (CI's is OpenGL). Neither is
/// a failure of the thing under test — see the same branch in
/// `a_shot_shows_the_post_processing_the_scene_asks_for`.
fn cannot_render(why: &str) -> bool {
    let cannot = why.contains("no GPU") || why.contains("could not build the renderer");
    if cannot {
        eprintln!("skipped — this machine cannot render:\n{why}");
    }
    cannot
}

#[test]
fn a_shot_draws_the_screen_space_ui_layers_over_the_picture() {
    let d = temp("authored");
    scaffold(&d);
    std::fs::write(d.join("scenes/menu.ron"), scene(true, None)).expect("write the scene");

    let img = match shoot(&d, "menu", &[]) {
        Ok(i) => i,
        Err(why) => {
            assert!(cannot_render(&why), "shot failed for a reason that is not the adapter:\n{why}");
            let _ = std::fs::remove_dir_all(&d);
            return;
        }
    };
    // Inside the box, well clear of any edge…
    for (x, y) in [(10, 10), (50, 45), (90, 80)] {
        assert!(
            is_red(img.get_pixel(x, y)),
            "({x},{y}) is {:?} — the layer's 100×100 box is not in the picture",
            img.get_pixel(x, y)
        );
    }
    // …and outside it, so a picture that is red all over does not pass.
    for (x, y) in [(120, 45), (150, 10), (150, 80)] {
        assert!(!is_red(img.get_pixel(x, y)), "({x},{y}) is red outside the box");
    }

    // `--no-ui`: the world alone, for the cases that want it.
    let bare = shoot(&d, "menu", &["--no-ui"]).expect("the bare shot");
    assert!(!is_red(bare.get_pixel(10, 10)), "--no-ui still drew the layer");

    let _ = std::fs::remove_dir_all(&d);
}

/// **`ui.make` is play-only, so its screen exists only under `--after`.**
///
/// Without `--after` the layer is empty, as it is in edit mode; with it, the
/// box the script built in `start` is in the picture.
#[test]
fn a_screen_built_by_a_script_in_start_is_photographed_under_after() {
    let d = temp("made");
    scaffold(&d);
    std::fs::write(d.join("scenes/menu.ron"), scene(false, Some("menu"))).expect("write the scene");
    std::fs::write(
        d.join("scripts/menu.lua"),
        "function start(node)\n  ui.make(node, { \"box\", pin = \"topLeft\", w = 100, h = 100, fill = \"#ff0000\" })\nend\n",
    )
    .expect("write the script");

    let unplayed = match shoot(&d, "menu", &[]) {
        Ok(i) => i,
        Err(why) => {
            assert!(cannot_render(&why), "shot failed for a reason that is not the adapter:\n{why}");
            let _ = std::fs::remove_dir_all(&d);
            return;
        }
    };
    assert!(!is_red(unplayed.get_pixel(10, 10)), "a made element was drawn without playing");

    let played = shoot(&d, "menu", &["--after", "5f"]).expect("the played shot");
    assert!(
        is_red(played.get_pixel(10, 10)) && is_red(played.get_pixel(90, 80)),
        "the box `ui.make` built in start is not in the played picture: {:?}",
        played.get_pixel(10, 10)
    );
    assert!(!is_red(played.get_pixel(150, 80)));

    let _ = std::fs::remove_dir_all(&d);
}
