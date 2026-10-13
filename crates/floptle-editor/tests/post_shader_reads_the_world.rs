//! **A `stage post` shader can tell ground from sky and place a pixel in the
//! world.**
//!
//! A screen pass that paints the sky (an atmosphere, a height fog, a grid) needs
//! two things from the frame: which pixels are sky, and which way each pixel
//! looks. The first is `sceneDepth` reading the real distance on terrain, not
//! the sky's huge number; the second is `sceneRay` and `cameraPosition`, which
//! carry the camera's turn and position into the pass.

// The `floptle` binary needs the authoring half; see the note at the top of
// `the_json_verbs_emit_only_json.rs`.
#![cfg(feature = "editor-ui")]

use std::path::{Path, PathBuf};
use std::process::Command;

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("flpostworld-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    for sub in ["scenes", "scripts", "shaders"] {
        std::fs::create_dir_all(d.join(sub)).unwrap();
    }
    std::fs::write(d.join("project.ron"), "(title: Some(\"p\"), entry_scene: Some(\"scenes/first.ron\"))").unwrap();
    d
}

/// Shoot the project; `None` when this machine has nothing to render on.
fn shoot(d: &Path, after: Option<&str>) -> Option<image::RgbImage> {
    let out = d.join("shot.png");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_floptle"));
    cmd.args(["shot", &d.to_string_lossy(), "--size", "160x90", "--out", &out.to_string_lossy()]);
    if let Some(s) = after {
        cmd.args(["--after", s]);
    }
    let run = cmd.output().expect("run floptle shot");
    if !run.status.success() {
        let why = String::from_utf8_lossy(&run.stderr).into_owned();
        let cannot = why.contains("no GPU") || why.contains("could not build the renderer");
        assert!(cannot, "shot failed for a reason that is not the adapter:\n{why}");
        eprintln!("skipped — this machine cannot render:\n{why}");
        return None;
    }
    Some(image::open(&out).expect("the shot is a PNG").to_rgb8())
}

/// A generated planet fills the middle of the view; the pass paints magenta
/// wherever `sceneDepth` says sky. The planet must come through untouched.
#[test]
fn a_post_pass_that_paints_the_sky_leaves_the_terrain_alone() {
    let d = temp("terrain");
    std::fs::write(
        d.join("scripts/gen.lua"),
        "function start(node)\n  node:setTerrain(1)\n  terrain.generatePlanet(1, { radius = 24, voxel = 1, seed = 3 })\nend\n",
    )
    .unwrap();
    std::fs::write(
        d.join("shaders/skymark.flsl"),
        "shader skymark {\n  stage post\n  let sky = step(500000, sceneDepth())\n  \
         output color = vec4(mix(sceneColor().rgb, vec3(1, 0, 1), sky), 1)\n}\n",
    )
    .unwrap();
    std::fs::write(
        d.join("scenes/first.ron"),
        "(name: \"s\", nodes: [\
         (name: \"Planet\", scripts: [(kind: \"gen\")]),\
         (name: \"Cam\", transform: (translation: (0.0, 0.0, 70.0)), matter: Camera(fov_y: 1.0, active: true)),\
         (name: \"Post\", matter: PostProcess(screen_shaders: [(shader: \"shaders/skymark.flsl\")]))])",
    )
    .unwrap();
    let Some(img) = shoot(&d, Some("1")) else { return };
    let magenta = |p: &image::Rgb<u8>| p.0[0] > 200 && p.0[1] < 40 && p.0[2] > 200;
    assert!(magenta(img.get_pixel(2, 2)), "the corner is sky and the pass did not mark it: {:?}", img.get_pixel(2, 2));
    // The planet's middle, and a ring of points across its face.
    for (x, y) in [(80, 45), (70, 40), (90, 50), (80, 35), (80, 55)] {
        let p = img.get_pixel(x, y);
        assert!(!magenta(p), "terrain at ({x}, {y}) read as sky: {p:?}");
    }
    let _ = std::fs::remove_dir_all(&d);
}

/// The pass encodes the ray: red where it points down −X, blue down −Z, and
/// green where the camera stands past x = 5. A camera turned to face −X from
/// x = 7 has to come out red and green, not blue.
#[test]
fn scene_ray_and_camera_position_follow_the_camera() {
    let d = temp("ray");
    std::fs::write(
        d.join("shaders/ray.flsl"),
        "shader ray {\n  stage post\n  let r = sceneRay()\n  \
         output color = vec4(step(0.7, -r.x), step(5, cameraPosition().x), step(0.7, -r.z), 1)\n}\n",
    )
    .unwrap();
    let scene = |d: &Path, x: f32, turned: bool| {
        let rot = if turned { "(0.0, 0.70710677, 0.0, 0.70710677)" } else { "(0.0, 0.0, 0.0, 1.0)" };
        std::fs::write(
            d.join("scenes/first.ron"),
            format!(
                "(name: \"s\", nodes: [\
                 (name: \"Cam\", transform: (translation: ({x}, 0.0, 0.0), rotation: {rot}), \
                 matter: Camera(fov_y: 1.0, active: true)),\
                 (name: \"Post\", matter: PostProcess(screen_shaders: [(shader: \"shaders/ray.flsl\")]))])"
            ),
        )
        .unwrap();
    };
    scene(&d, 7.0, true);
    let Some(turned) = shoot(&d, None) else { return };
    let c = turned.get_pixel(80, 45).0;
    assert!(c[0] > 128 && c[2] < 64, "a camera facing −X saw a ray that is not −X: {c:?}");
    assert!(c[1] > 128, "cameraPosition did not carry the camera's x = 7: {c:?}");

    scene(&d, 0.0, false);
    let Some(ahead) = shoot(&d, None) else { return };
    let c = ahead.get_pixel(80, 45).0;
    assert!(c[2] > 128 && c[0] < 64, "an unturned camera saw a ray that is not −Z: {c:?}");
    assert!(c[1] < 64, "cameraPosition is past x = 5 for a camera at the origin: {c:?}");
    let _ = std::fs::remove_dir_all(&d);
}
