//! **A reflection on a world that moves every tick is where it would be on a
//! still one.**
//!
//! Screen-space reflections read last frame's picture, looked up where each
//! reflected point was a frame ago. A game whose world is carried along every
//! tick (a planet on its orbit, the camera standing on it) moved every wall
//! and rock that far between two frames, and read as still in absolute space
//! each reflection was looked up metres from where it was: Solar's metal walls
//! flickered and its glass showed torn, sliced reflections. The lookup now
//! follows the world the camera stands on.

// The `floptle` binary needs the authoring half; see the note at the top of
// `the_json_verbs_emit_only_json.rs`.
#![cfg(feature = "editor-ui")]

use std::path::{Path, PathBuf};
use std::process::Command;

/// A planet carried `speed` m a tick, a mirror floor on it, two boxes and the
/// camera on it too. Returns the last of four frames, or `None` where this
/// machine cannot render.
fn shot(speed: f32) -> Option<image::RgbImage> {
    let d = std::env::temp_dir().join(format!("flssrmove-{speed}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    for sub in ["scenes", "scripts"] {
        std::fs::create_dir_all(d.join(sub)).unwrap();
    }
    std::fs::write(d.join("project.ron"), "(title: Some(\"s\"), entry_scene: Some(\"scenes/first.ron\"), retro: false)").unwrap();
    std::fs::write(d.join("scripts/orbit.lua"), format!("function update(node, dt)\n  node.x = node.x + {speed}\nend\n")).unwrap();
    std::fs::write(
        d.join("scenes/first.ron"),
        "(name: \"s\", lighting: (direction: (0.4, 1.0, 0.2), ambient: (0.2, 0.2, 0.2), reflections: true, reflection_distance: 40.0), nodes: [\
           (name: \"Planet\", scripts: [(kind: \"orbit\")], celestial: Some((body_radius: 30.0))),\
           (name: \"Floor\", parent: Some(0), transform: (translation: (0.0, 30.0, 0.0), scale: (20.0, 0.1, 20.0)), matter: Primitive(shape: Cube, color: (0.3, 0.3, 0.3)), \
              material: Some((shading: Physical, color: (0.3, 0.3, 0.3), roughness: 0.0, metallic: 1.0, reflectivity: 1.0))),\
           (name: \"Red\", parent: Some(0), transform: (translation: (0.0, 32.0, -8.0), scale: (6.0, 4.0, 1.0)), matter: Primitive(shape: Cube, color: (1.0, 0.1, 0.1))),\
           (name: \"Green\", parent: Some(0), transform: (translation: (4.0, 31.5, -3.0), scale: (1.0, 3.0, 1.0)), matter: Primitive(shape: Cube, color: (0.1, 1.0, 0.1))),\
           (name: \"Cam\", parent: Some(0), transform: (translation: (0.0, 33.0, 6.0), rotation: (-0.17364818, 0.0, 0.0, 0.98480775)), matter: Camera(fov_y: 1.0, active: true))])",
    )
    .unwrap();
    let out = d.join("shot.png");
    let run = Command::new(env!("CARGO_BIN_EXE_floptle"))
        .args(["shot", &d.to_string_lossy(), "--size", "320x240", "--no-ui", "--after", "1", "--frames", "4", "--out"])
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
    let last: PathBuf = d.join("shot-0003.png");
    let img = image::open(&last).unwrap_or_else(|e| panic!("no frame at {}: {e}", last.display())).to_rgb8();
    let _ = std::fs::remove_dir_all(Path::new(&d));
    Some(img)
}

/// The mean difference a channel between two pictures.
fn mean_diff(a: &image::RgbImage, b: &image::RgbImage) -> f64 {
    let sum: u64 = a.pixels().zip(b.pixels()).map(|(x, y)| x.0.iter().zip(y.0.iter()).map(|(p, q)| (*p as i32 - *q as i32).unsigned_abs() as u64).sum::<u64>()).sum();
    sum as f64 / (a.width() * a.height() * 3) as f64
}

#[test]
fn a_reflection_on_a_moving_world_is_where_it_is_on_a_still_one() {
    let (Some(still), Some(moving)) = (shot(0.0), shot(6.0)) else { return };
    // The fixture: the floor reflects the boxes, so a misplaced lookup shows.
    // Below the boxes the floor holds their red and green.
    let tinted = still.enumerate_pixels().filter(|(_, y, p)| *y > 130 && (p.0[0] > p.0[2] + 30 || p.0[1] > p.0[2] + 30)).count();
    assert!(tinted > 200, "the fixture: the mirror floor shows the boxes at {tinted} pixels");
    let d = mean_diff(&still, &moving);
    assert!(d < 1.0, "the picture on a world moving 6 m a tick differs from the still one by {d:.2} a channel");
}
