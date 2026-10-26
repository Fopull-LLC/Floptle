//! **Fog shafts read from the sun's shadow map look like the marched ones.**
//!
//! A row of posts against a low sun in lit volumetric fog, shot three ways:
//! shafts marched through the field at every fog step (what a scene made
//! before `fog_shafts_map` does), shafts from the shadow map, and no shafts.
//! The map's picture must be close to the march's, and well away from the
//! one with no shafts, or the cheaper path has quietly stopped casting.

// The `floptle` binary needs the authoring half; see the note at the top of
// `the_json_verbs_emit_only_json.rs`.
#![cfg(feature = "editor-ui")]

use std::process::Command;

/// Make the project, shoot it, and give its pixels; `None` when this machine
/// cannot render.
fn shot(tag: &str, shafts: &str) -> Option<image::RgbImage> {
    let d = std::env::temp_dir().join(format!("flfogshaft-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("scenes")).unwrap();
    std::fs::write(d.join("project.ron"), "(title: Some(\"p\"), entry_scene: Some(\"scenes/first.ron\"), retro: false)")
        .unwrap();
    let mut nodes = vec![
        "(name: \"Ground\", transform: (translation: (0.0, -1.0, 0.0), scale: (400.0, 2.0, 400.0)), \
         matter: Primitive(shape: Cube, color: (0.3, 0.5, 0.25)), \
         rigidbody: Some((mode: Static, boxed: true, half_extents: (0.5, 0.5, 0.5))))"
            .to_string(),
    ];
    for i in 0..12 {
        nodes.push(format!(
            "(name: \"Post{i}\", transform: (translation: ({}, 8.0, -40.0), scale: (3.0, 16.0, 2.0)), \
             matter: Primitive(shape: Cube, color: (0.4, 0.4, 0.45)), \
             rigidbody: Some((mode: Static, boxed: true, half_extents: (0.5, 0.5, 0.5))))",
            -33.0 + i as f32 * 6.0
        ));
    }
    nodes.push("(name: \"Cam\", transform: (translation: (0.0, 1.6, 10.0)), matter: Camera(fov_y: 1.0, active: true))".into());
    std::fs::write(
        d.join("scenes/first.ron"),
        format!(
            "(name: \"s\", lighting: (direction: (0.15, 0.25, -1.0), shadows: true, shadow_map: true, fog: true, \
             fog_volumetric: true, fog_color: (0.6, 0.65, 0.75), fog_density: 0.06, fog_height: 20.0, fog_falloff: 8.0, \
             fog_noise: 0.0, fog_light: 1.0, fog_anisotropy: 0.7, fog_steps: 24, fog_end: 480.0, {shafts}), nodes: [{}])",
            nodes.join(",\n")
        ),
    )
    .unwrap();
    let out = d.join("shot.png");
    let run = Command::new(env!("CARGO_BIN_EXE_floptle"))
        .args(["shot", &d.to_string_lossy(), "--size", "160x90", "--no-ui", "--out"])
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
    let _ = std::fs::remove_dir_all(&d);
    Some(img)
}

/// Mean absolute difference of two pictures, 0..1.
fn mae(a: &image::RgbImage, b: &image::RgbImage) -> f32 {
    let n = a.as_raw().len() as f32;
    a.as_raw().iter().zip(b.as_raw()).map(|(x, y)| (*x as f32 - *y as f32).abs()).sum::<f32>() / n / 255.0
}

#[test]
fn fog_shafts_from_the_shadow_map_look_like_the_marched_ones() {
    let Some(marched) = shot("marched", "fog_shafts: true") else { return };
    let Some(mapped) = shot("mapped", "fog_shafts: true, fog_shafts_map: true") else { return };
    let Some(none) = shot("none", "fog_shafts: false") else { return };
    let (close, apart) = (mae(&mapped, &marched), mae(&mapped, &none));
    assert!(mae(&marched, &none) > 0.08, "the fixture: shafts change the picture by only {:.3}", mae(&marched, &none));
    assert!(close < 0.05, "shafts from the shadow map are {close:.3} from the marched ones");
    assert!(apart > 0.08, "shafts from the shadow map are only {apart:.3} from no shafts at all: they stopped casting");
}
