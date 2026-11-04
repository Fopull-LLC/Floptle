//! **Motion blur streaks what the camera moves past, not what moves with
//! it, on a world that moves every tick.**
//!
//! Motion blur reprojects each pixel to where it was a frame ago, as if
//! everything in the picture stood still in the world. What rides with the
//! camera does not: the bike under a rider, the astronaut a chase camera
//! follows. Read as standing still, it was streaked with the camera's whole
//! motion over the ground, and Solar's player saw their own bike smeared
//! into a blur, streaks running a different way from the ride. The last
//! frame's depth now tells the two apart. And on a planet carried along its
//! orbit and turning, with the camera riding it, the streaks are the ones
//! the same drive makes on a planet standing still.

// The `floptle` binary needs the authoring half; see the note at the top of
// `the_json_verbs_emit_only_json.rs`.
#![cfg(feature = "editor-ui")]

use std::path::{Path, PathBuf};
use std::process::Command;

/// A planet carried `speed` m a tick and turned `spin` rad a tick, with grey
/// ground and pale posts on it, the camera driving across it at 0.4 m a tick
/// with a red and blue striped box riding in front of it. The last of four frames (the
/// sequence's later frames carry the shutter), or `None` where this machine
/// cannot render.
fn shot(speed: f32, spin: f32, blur: f32) -> Option<image::RgbImage> {
    let d = std::env::temp_dir().join(format!("flmbmove-{speed}-{spin}-{blur}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    for sub in ["scenes", "scripts"] {
        std::fs::create_dir_all(d.join(sub)).unwrap();
    }
    std::fs::write(d.join("project.ron"), "(title: Some(\"s\"), entry_scene: Some(\"scenes/first.ron\"), retro: false)").unwrap();
    std::fs::write(d.join("scripts/orbit.lua"), format!("function update(node, dt)\n  node.x = node.x + {speed}\n  node.yaw = node.yaw + {spin}\nend\n")).unwrap();
    std::fs::write(d.join("scripts/drive.lua"), "function update(node, dt)\n  node.x = node.x + 0.4\nend\n").unwrap();
    let mut nodes = String::from("(name: \"Planet\", scripts: [(kind: \"orbit\")], celestial: Some((body_radius: 30.0))),");
    nodes += &format!(
        "(name: \"Post\", matter: PostProcess(motion_blur: {blur})),\
         (name: \"Ground\", parent: Some(0), transform: (translation: (0.0, 29.95, 0.0), scale: (80.0, 0.1, 80.0)), matter: Primitive(shape: Cube, color: (0.25, 0.25, 0.25))),\
         (name: \"Cam\", parent: Some(0), scripts: [(kind: \"drive\")], transform: (translation: (0.0, 32.0, 6.0), rotation: (-0.17364818, 0.0, 0.0, 0.98480775)), matter: Camera(fov_y: 1.0, active: true)),\
"
    );
    // The rider: stripes, so a streak across it shows, as one across a real
    // bike's detail does.
    for k in 0..6 {
        let c = if k % 2 == 0 { "(1.0, 0.05, 0.05)" } else { "(0.05, 0.05, 1.0)" };
        nodes += &format!(
            "(name: \"Rider{k}\", parent: Some(3), transform: (translation: ({}, -0.5, -2.0), scale: (0.1, 0.3, 0.3)), matter: Primitive(shape: Cube, color: {c})),",
            -0.25 + 0.1 * k as f32
        );
    }
    // Posts in a grid, pale against the ground: they streak where the
    // picture moves.
    for i in -6..=6 {
        for j in 1..=4 {
            nodes += &format!(
                "(name: \"P{i}_{j}\", parent: Some(0), transform: (translation: ({}, 31.0, {}), scale: (0.3, 2.0, 0.3)), matter: Primitive(shape: Cube, color: (1.0, 0.9, 0.6))),",
                i as f32 * 3.0,
                -(j as f32) * 4.0
            );
        }
    }
    std::fs::write(
        d.join("scenes/first.ron"),
        format!("(name: \"s\", lighting: (direction: (0.4, 1.0, 0.2), ambient: (0.3, 0.3, 0.3)), nodes: [{nodes}])"),
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

/// The mean difference a channel between two pictures over the pixels `keep`
/// picks out of the first.
fn mean_diff(a: &image::RgbImage, b: &image::RgbImage, keep: impl Fn(&image::Rgb<u8>) -> bool) -> f64 {
    let (mut sum, mut n) = (0u64, 0u64);
    for (x, y) in a.pixels().zip(b.pixels()) {
        if keep(x) {
            sum += x.0.iter().zip(y.0.iter()).map(|(p, q)| (*p as i32 - *q as i32).unsigned_abs() as u64).sum::<u64>();
            n += 3;
        }
    }
    sum as f64 / n.max(1) as f64
}

/// A pixel of the riding box: one of its red or blue stripes.
fn rider(p: &image::Rgb<u8>) -> bool {
    let [r, g, b] = p.0;
    g < 60 && ((r > 120 && b < 60) || (b > 120 && r < 60))
}

#[test]
fn motion_blur_streaks_what_the_camera_passes_and_not_what_rides_with_it() {
    let (Some(sharp), Some(still)) = (shot(0.0, 0.0, 0.0), shot(0.0, 0.0, 1.0)) else { return };
    // The fixture: the drive streaks the ground and its posts, and the box
    // riding with the camera is in the picture.
    let riders = sharp.pixels().filter(|p| rider(p)).count();
    assert!(riders > 300, "the fixture: the riding box covers only {riders} pixels");
    let drive = mean_diff(&sharp, &still, |p| !rider(p));
    let ride = mean_diff(&sharp, &still, rider);
    assert!(drive > 0.2, "the fixture: driving across the still world blurs the picture by only {drive:.2} a channel");
    // What rides with the camera is left sharp.
    assert!(ride < drive * 0.5, "the box riding with the camera is streaked: {ride:.2} a channel on it, {drive:.2} across the ground");
    // And a world carried along under the camera streaks as a still one
    // does: the same picture.
    let (Some(carried), Some(turning_sharp), Some(turning)) = (shot(6.0, 0.0, 1.0), shot(6.0, 0.003, 0.0), shot(6.0, 0.003, 1.0)) else { return };
    let d = mean_diff(&still, &carried, |_| true);
    assert!(d < drive * 0.25, "the blur on a world carried 6 m a tick differs from the still one's by {d:.2} a channel (the drive's own blur is {drive:.2})");
    // A turning one is lit differently under a fixed sun, so it is the blur
    // that is compared: as much of it as the still world's.
    let turned = mean_diff(&turning_sharp, &turning, |p| !rider(p));
    assert!((turned - drive).abs() < drive * 0.25, "the drive blurs a world turning 0.003 rad a tick by {turned:.2} a channel, a still one by {drive:.2}");
}
