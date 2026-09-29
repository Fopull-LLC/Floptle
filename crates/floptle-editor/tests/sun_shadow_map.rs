//! **With `shadow_map` on, every drawn mesh casts a sun shadow.**
//!
//! The field march only knows what physics knows: colliders, their proxies,
//! terrain. A mesh that is drawn but does not collide (a floating sign, a
//! backdrop, map-tool geometry) cast nothing, so the shadows a level had were
//! both expensive and incomplete. The shadow map renders what is drawn.
//!
//! Here a box floats above a floor and does not collide. The sun is slanted so
//! its shadow lands at the centre of a top-down shot: dark with the map, lit
//! with the march.

// The `floptle` binary needs the authoring half; see the note at the top of
// `the_json_verbs_emit_only_json.rs`.
#![cfg(feature = "editor-ui")]

use std::path::{Path, PathBuf};
use std::process::Command;

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("flsunmap-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("scenes")).unwrap();
    d
}

fn project(d: &Path, map: bool) {
    std::fs::write(d.join("project.ron"), "(title: Some(\"s\"), entry_scene: Some(\"scenes/first.ron\"), retro: false)").unwrap();
    std::fs::write(
        d.join("scenes/first.ron"),
        format!(
            "(name: \"s\", lighting: (direction: (1.0, 1.0, 0.0), ambient: (0.1, 0.1, 0.1), shadows: true, shadow_softness: 0.2, shadow_map: {map}), nodes: [\
               (name: \"Camera\", transform: (translation: (0.0, 15.0, 0.0), rotation: (-0.70710677, 0.0, 0.0, 0.70710677)), matter: Camera(fov_y: 0.8, active: true)), \
               (name: \"Floor\", transform: (translation: (0.0, -0.1, 0.0), scale: (20.0, 0.1, 20.0)), matter: Primitive(shape: Cube, color: (0.8, 0.8, 0.8))), \
               (name: \"Sign\", transform: (translation: (4.0, 4.0, 0.0), scale: (2.0, 0.1, 2.0)), matter: Primitive(shape: Cube, color: (0.8, 0.8, 0.8)))\
             ])"
        ),
    )
    .unwrap();
}

fn centre_luma(d: &Path) -> Result<f32, String> {
    let out = d.join("shot.png");
    let r = Command::new(env!("CARGO_BIN_EXE_floptle"))
        .args(["shot", &d.to_string_lossy(), "--size", "256x256", "--out", &out.to_string_lossy()])
        .output()
        .expect("run shot");
    if !r.status.success() {
        return Err(String::from_utf8_lossy(&r.stderr).into_owned());
    }
    let img = image::open(&out).expect("a PNG").to_rgba8();
    let mut sum = 0.0;
    for y in 120..136 {
        for x in 120..136 {
            let p = img.get_pixel(x, y);
            sum += (p[0] as f32 + p[1] as f32 + p[2] as f32) / 3.0;
        }
    }
    Ok(sum / 256.0)
}

#[test]
fn a_mesh_that_does_not_collide_casts_under_the_shadow_map() {
    let (dm, df) = (temp("map"), temp("march"));
    project(&dm, true);
    project(&df, false);
    let mapped = match centre_luma(&dm) {
        Ok(v) => v,
        Err(why) => {
            let cannot = why.contains("no GPU") || why.contains("could not build the renderer");
            assert!(cannot, "shot failed for a reason that is not the adapter:\n{why}");
            eprintln!("skipped — this machine cannot render:\n{why}");
            return;
        }
    };
    let marched = centre_luma(&df).expect("the march shot");
    assert!(marched > 60.0, "the fixture: the floor's centre is lit under the march: {marched}");
    assert!(mapped < marched * 0.6, "the box cast no shadow under the map: centre {mapped} against {marched} lit");
    for d in [&dm, &df] {
        let _ = std::fs::remove_dir_all(d);
    }
}
