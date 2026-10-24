//! **A surface faded for the camera still stands in the sunlight.**
//!
//! A wall between the camera and the player can dissolve by distance
//! (`near_fade`), pixel by pixel, in the colour pass and the depth prepass
//! alike. A surface faded by its screen door (`dither_alpha`) used to drop out
//! of the depth prepass and the sun's shadow map altogether, so a wall fading
//! out of the way stopped shading the room behind it and the light popped as
//! it faded. Now the sun's view draws it whole.

// The `floptle` binary needs the authoring half; see the note at the top of
// `the_json_verbs_emit_only_json.rs`.
#![cfg(feature = "editor-ui")]

use std::path::{Path, PathBuf};
use std::process::Command;

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("flfade-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("scenes")).unwrap();
    std::fs::write(d.join("project.ron"), "(title: Some(\"f\"), entry_scene: Some(\"scenes/first.ron\"), retro: false)").unwrap();
    d
}

/// A shot of the project's entry scene, or `None` where there is no GPU.
fn shoot(d: &Path) -> Option<image::RgbaImage> {
    let out = d.join("shot.png");
    let r = Command::new(env!("CARGO_BIN_EXE_floptle"))
        .args(["shot", &d.to_string_lossy(), "--size", "256x256", "--out", &out.to_string_lossy()])
        .output()
        .expect("run shot");
    if !r.status.success() {
        let why = String::from_utf8_lossy(&r.stderr);
        assert!(why.contains("no GPU") || why.contains("could not build the renderer"), "shot failed:\n{why}");
        return None;
    }
    Some(image::open(&out).expect("a PNG was written").to_rgba8())
}

/// The share of the centre 64×64 pixels that are the wall's pure green.
fn green_share(img: &image::RgbaImage) -> f32 {
    let mut n = 0;
    for y in 96..160 {
        for x in 96..160 {
            let p = img.get_pixel(x, y);
            if p[1] > 200 && p[0] < 60 && p[2] < 60 {
                n += 1;
            }
        }
    }
    n as f32 / (64.0 * 64.0)
}

/// A camera looking down −z at a big unlit green wall `dist` metres away,
/// wearing `material`.
fn wall(d: &Path, dist: f32, fade: &str) {
    std::fs::write(
        d.join("scenes/first.ron"),
        format!(
            "(name: \"w\", nodes: [\
               (name: \"Camera\", transform: (translation: (0.0, 0.0, 0.0)), matter: Camera(fov_y: 0.5, active: true)), \
               (name: \"Wall\", transform: (translation: (0.0, 0.0, -{dist}), scale: (40.0, 40.0, 0.1)), \
                matter: Primitive(shape: Cube, color: (0.0, 1.0, 0.0)), material: Some((color: (0.0, 1.0, 0.0), unlit: true{fade})))\
             ])"
        ),
    )
    .unwrap();
}

/// **A wall with `near_fade (1, 5)` is half gone at 3 m and whole at 10 m.**
#[test]
fn a_wall_dissolves_toward_the_camera_between_its_fade_distances() {
    let d = temp("near");
    wall(&d, 3.0, "");
    let Some(plain) = shoot(&d) else { return };
    assert!(green_share(&plain) > 0.98, "the fixture: an unfaded wall fills the view ({})", green_share(&plain));
    wall(&d, 3.0, ", near_fade: (1.0, 5.0)");
    let near = green_share(&shoot(&d).expect("the near shot"));
    assert!((0.35..=0.65).contains(&near), "at 3 m, half way through its fade, {:.0}% of the wall was drawn", near * 100.0);
    wall(&d, 10.0, ", near_fade: (1.0, 5.0)");
    let far = green_share(&shoot(&d).expect("the far shot"));
    assert!(far > 0.98, "at 10 m, past its fade, only {:.0}% of the wall was drawn", far * 100.0);
    let _ = std::fs::remove_dir_all(&d);
}

/// A top-down camera over a floor, the sun slanted so a slab floating to
/// one side shades the middle of the shot through the sun's shadow map.
/// `slab` is the slab's material, or `None` for no slab.
fn floor_under(d: &Path, slab: Option<&str>) {
    let slab = slab
        .map(|m| {
            format!(
                ", (name: \"Slab\", transform: (translation: (4.0, 4.0, 0.0), scale: (2.0, 0.1, 2.0)), \
                   matter: Primitive(shape: Cube, color: (0.8, 0.8, 0.8)){m})"
            )
        })
        .unwrap_or_default();
    std::fs::write(
        d.join("scenes/first.ron"),
        format!(
            "(name: \"s\", lighting: (direction: (1.0, 1.0, 0.0), ambient: (0.1, 0.1, 0.1), shadows: true, shadow_softness: 0.2, shadow_map: true), nodes: [\
               (name: \"Camera\", transform: (translation: (0.0, 15.0, 0.0), rotation: (-0.70710677, 0.0, 0.0, 0.70710677)), matter: Camera(fov_y: 0.8, active: true)), \
               (name: \"Floor\", transform: (translation: (0.0, -0.1, 0.0), scale: (20.0, 0.1, 20.0)), matter: Primitive(shape: Cube, color: (0.8, 0.8, 0.8))){slab}\
             ])"
        ),
    )
    .unwrap();
}

fn centre_luma(img: &image::RgbaImage) -> f32 {
    let (mut sum, mut n) = (0.0f32, 0.0f32);
    for y in 118..138 {
        for x in 118..138 {
            let p = img.get_pixel(x, y);
            sum += (u32::from(p[0]) + u32::from(p[1]) + u32::from(p[2])) as f32 / 3.0;
            n += 1.0;
        }
    }
    sum / n
}

/// **A slab half faded by its screen door still shades the floor.** Its
/// shadow is the solid slab's, not half of it and not none.
#[test]
fn a_dithered_slab_keeps_its_sun_shadow() {
    let d = temp("shadow");
    floor_under(&d, None);
    let Some(open) = shoot(&d) else { return };
    let lit = centre_luma(&open);
    floor_under(&d, Some(""));
    let solid = centre_luma(&shoot(&d).expect("the solid slab"));
    floor_under(&d, Some(", material: Some((alpha: 0.5, retro: (dither_alpha: true)))"));
    let dithered = centre_luma(&shoot(&d).expect("the dithered slab"));
    assert!(solid < lit * 0.6, "the fixture: the solid slab shades the floor ({solid} against {lit} lit)");
    assert!(
        (dithered - solid).abs() < lit * 0.1,
        "the dithered slab's shadow reads {dithered}, the solid slab's {solid}, the open floor {lit}"
    );
    let _ = std::fs::remove_dir_all(&d);
}
