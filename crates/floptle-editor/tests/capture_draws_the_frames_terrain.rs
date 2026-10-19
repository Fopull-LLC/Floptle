//! **A `camera.capture` taken while `shot --after` plays draws the terrain the
//! frame has.**
//!
//! A headless play has no frame loop, and the frame loop is what brings the
//! terrain's chunk meshes up to date before anything draws. So a capture taken
//! mid-play photographed the chunks as they were when Play began — none, for
//! a planet generated in `start` — and the sea under the ground showed through
//! where the finished frame drew land. The final picture settles its meshes on
//! its own, so it never showed the difference.

// The `floptle` binary needs the authoring half; see the note at the top of
// `the_json_verbs_emit_only_json.rs`.
#![cfg(feature = "editor-ui")]

use std::process::Command;

/// A generated planet with a blue sea ball just inside its ground, a camera on
/// it, and a script that captures the view a second in. The shot
/// at three seconds and the capture must be the same picture.
#[test]
fn a_capture_mid_play_matches_the_frame() {
    let d = std::env::temp_dir().join(format!("flcapterrain-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    for sub in ["scenes", "scripts"] {
        std::fs::create_dir_all(d.join(sub)).unwrap();
    }
    std::fs::write(d.join("project.ron"), "(title: Some(\"p\"), entry_scene: Some(\"scenes/first.ron\"))").unwrap();
    std::fs::write(
        d.join("scripts/gen.lua"),
        "local t = 0\nfunction start(node)\n  node:setTerrain(1)\n  \
         terrain.generatePlanet(1, { radius = 24, voxel = 1, seed = 3 })\nend\n\
         function update(node, dt)\n  t = t + dt\n  if t > 1.0 and not snapped then\n    snapped = true\n    \
         camera.capture({ w = 160, h = 90 }, function(bytes, err)\n      \
         if bytes then assets.writeBytes('user://cap.png', bytes) end\n    end)\n  end\nend\n",
    )
    .unwrap();
    // The sea: a ball a little smaller than the ground, so the ground hides
    // it wherever the ground is drawn.
    std::fs::write(
        d.join("scenes/first.ron"),
        "(name: \"s\", nodes: [\
         (name: \"Planet\", scripts: [(kind: \"gen\")]),\
         (name: \"Sea\", transform: (scale: (25.0, 25.0, 25.0)), matter: Primitive(shape: Sphere, color: (0.0, 0.2, 1.0))),\
         (name: \"Cam\", transform: (translation: (0.0, 0.0, 70.0)), matter: Camera(fov_y: 1.0, active: true))])",
    )
    .unwrap();
    let out = d.join("shot.png");
    let run = Command::new(env!("CARGO_BIN_EXE_floptle"))
        .args(["shot", &d.to_string_lossy(), "--size", "160x90", "--no-ui", "--after", "3", "--out"])
        .arg(&out)
        .output()
        .expect("run floptle shot");
    if !run.status.success() {
        let why = String::from_utf8_lossy(&run.stderr).into_owned();
        let cannot = why.contains("no GPU") || why.contains("could not build the renderer");
        assert!(cannot, "shot failed for a reason that is not the adapter:\n{why}");
        eprintln!("skipped — this machine cannot render:\n{why}");
        return;
    }
    let frame = image::open(&out).expect("the shot is a PNG").to_rgb8();
    let cap_path = d.join("save/user/cap.png");
    let cap = image::open(&cap_path)
        .unwrap_or_else(|e| panic!("no capture at {}: {e}\n{}", cap_path.display(), String::from_utf8_lossy(&run.stderr)))
        .to_rgb8();
    assert_eq!(cap.dimensions(), frame.dimensions());
    let blue = |p: &image::Rgb<u8>| p.0[2] > 150 && p.0[0] < 60;
    assert!(!blue(frame.get_pixel(80, 45)), "the fixture: the frame shows ground, not the sea, at the middle");
    let (mut sum, mut seas) = (0u64, 0usize);
    for (a, b) in frame.pixels().zip(cap.pixels()) {
        sum += a.0.iter().zip(b.0.iter()).map(|(x, y)| (*x as i32 - *y as i32).unsigned_abs() as u64).sum::<u64>();
        seas += usize::from(blue(b) && !blue(a));
    }
    let mean = sum as f64 / (160.0 * 90.0 * 3.0);
    assert!(seas < 50, "the capture shows the sea at {seas} pixels where the frame has ground");
    assert!(mean < 6.0, "the capture differs from the frame by {mean:.1} a channel on average");
    let _ = std::fs::remove_dir_all(&d);
}
