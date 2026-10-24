//! **Clouds shade the ground.** The cloud deck drew over the sky and nothing
//! under it changed: a planet under a full storm deck lit its ground exactly as
//! under a clear sky. The sun's beam is now dimmed where it crosses the deck,
//! by the deck's own drifting noise, and a darker deck lets less through.

// The `floptle` binary needs the authoring half; see the note at the top of
// `the_json_verbs_emit_only_json.rs`.
#![cfg(feature = "editor-ui")]

use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_floptle")
}

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("flshotclouds-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// A camera standing on a planet with an atmosphere, the sun high, looking at
/// grey ground, under a deck of `clouds` cover and `darkness`.
fn scene(name: &str, clouds: f32, darkness: f32) -> String {
    format!(
        "(name: \"{name}\", lighting: (direction: (0.3, 1.0, 0.2), color: (1.0, 0.95, 0.85), ambient: (0.08, 0.08, 0.1), \
         intensity: 1.2), nodes: [\
         (name: \"Camera\", transform: (translation: (0.0, 2.0, 6.0), rotation: (-0.2588, 0.0, 0.0, 0.9659), \
         scale: (1.0, 1.0, 1.0)), matter: Camera(fov_y: 1.0, active: true), scripts: []), \
         (name: \"Planet\", transform: (translation: (0.0, -200.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), \
         scale: (1.0, 1.0, 1.0)), matter: Empty, scripts: [], celestial: Some((body_radius: 200.0, \
         atmo_color: (0.4, 0.6, 1.0), atmo_height: 60.0, clouds: {clouds}, cloud_darkness: {darkness}))), \
         (name: \"Ground\", transform: (translation: (0.0, -0.5, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), \
         scale: (60.0, 1.0, 60.0)), matter: Primitive(shape: Cube, color: (0.6, 0.6, 0.6)), scripts: [])])"
    )
}

/// The mean brightness of the bottom quarter of a shot of `scene`: ground
/// only. `None` where there is no GPU to draw with.
fn ground(d: &Path, name: &str, clouds: f32, darkness: f32) -> Option<f32> {
    std::fs::write(d.join(format!("scenes/{name}.ron")), scene(name, clouds, darkness)).unwrap();
    let out = d.join(format!("{name}.png"));
    let r = Command::new(bin())
        .args(["shot", &d.to_string_lossy(), "--scene", name, "--size", "160x90", "--out", &out.to_string_lossy()])
        .output()
        .expect("run shot");
    if !r.status.success() {
        let why = String::from_utf8_lossy(&r.stderr);
        assert!(why.contains("no GPU") || why.contains("could not build the renderer"), "shot failed:\n{why}");
        return None;
    }
    let img = image::open(&out).expect("a PNG was written").to_rgba8();
    let (mut sum, mut n) = (0.0f32, 0.0f32);
    for y in img.height() * 3 / 4..img.height() {
        for x in 0..img.width() {
            let p = img.get_pixel(x, y);
            sum += (u32::from(p[0]) + u32::from(p[1]) + u32::from(p[2])) as f32 / 3.0;
            n += 1.0;
        }
    }
    Some(sum / n)
}

#[test]
fn a_cloud_deck_shades_the_ground_and_a_storm_more() {
    let d = temp("deck");
    let out = Command::new(bin()).args(["new", &d.to_string_lossy()]).output().expect("run floptle new");
    assert!(out.status.success(), "scaffold failed: {}", String::from_utf8_lossy(&out.stderr));
    let (Some(clear), Some(overcast), Some(storm)) =
        (ground(&d, "clear", 0.0, 0.0), ground(&d, "overcast", 1.0, 0.0), ground(&d, "storm", 1.0, 0.95))
    else {
        let _ = std::fs::remove_dir_all(&d);
        return;
    };
    assert!(overcast < clear * 0.85, "a full deck left the ground at {overcast:.1} against a clear sky's {clear:.1}");
    assert!(storm < overcast * 0.75, "a storm deck lit the ground at {storm:.1}, an ordinary deck at {overcast:.1}");
    let _ = std::fs::remove_dir_all(&d);
}
