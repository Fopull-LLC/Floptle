//! **Eighty cut-out cards shade like solid ones, by the GPU's clock.**
//! A field of leaves or grass is cards one behind another. The colour pass
//! that tested depth LessEqual with depth writes on, and could discard,
//! left the GPU testing depth late and shading every hidden card; the
//! depth-Equal pass over the primed prepass shades each visible pixel once.
//! A raymarched blob is in the frame, so the march runs between the
//! prepass and the colour pass, as it always does on a planet: until that
//! case drew depth-Equal too, a forest's leaves cost twice what they do.
//!
//! These time the "opaque + lighting" region of `shot --timing`, eighty
//! cut-out cards against the same cards solid in the same run, and hold the
//! ratio, never a duration.
//! Whole-frame wall clocks are no use here: reading the picture back is
//! most of a frame and hides the difference.
//!
//! Needs a GPU with timestamp queries; without one the test says so and
//! passes, as the other render probes do.

// The `floptle` binary needs the authoring half; see the note at the top of
// `the_json_verbs_emit_only_json.rs`.
#![cfg(feature = "editor-ui")]

use std::path::{Path, PathBuf};
use std::process::Command;

fn project(cutout: bool) -> PathBuf {
    let d = std::env::temp_dir().join(format!("flcutcost-{cutout}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("scenes")).unwrap();
    std::fs::create_dir_all(d.join("textures")).unwrap();
    // Holes in cells two texels across, so most of each card shows the cards
    // behind it.
    let mut seed = 0x9e37_79b9u32;
    let mut cells = [false; 32 * 32];
    for c in &mut cells {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *c = seed >> 24 < 115;
    }
    let leaf = image::RgbaImage::from_fn(64, 64, |x, y| {
        let hole = cutout && cells[((y / 2) * 32 + x / 2) as usize];
        image::Rgba([60 + x as u8 * 3, 140, 70 + y as u8 * 2, if hole { 0 } else { 255 }])
    });
    leaf.save(d.join("textures/leaf.png")).unwrap();
    let mut nodes = vec![
        "(name: \"Camera\", transform: (translation: (0.0, 0.0, 0.0)), matter: Camera(fov_y: 1.0, active: true))".to_string(),
        "(name: \"Blob\", transform: (translation: (-1.5, 0.0, -3.0)), matter: Blob(scale: 1.0))".to_string(),
    ];
    let look = if cutout { ", blend: Some(Cutout), alpha_cutoff: 0.5" } else { "" };
    for k in (0..80).rev() {
        nodes.push(format!(
            "(name: \"C{k}\", transform: (translation: ({:.2}, {:.2}, {:.3}), scale: (12.0, 12.0, 0.02)), \
             matter: Primitive(shape: Cube, color: (1.0, 1.0, 1.0)), \
             material: Some((texture: Some(\"textures/leaf.png\"){look})))",
            (k % 7) as f32 * 0.37,
            (k % 5) as f32 * 0.29,
            -4.0 - k as f32 * 0.125
        ));
    }
    std::fs::write(
        d.join("project.ron"),
        "(title: Some(\"c\"), entry_scene: Some(\"scenes/first.ron\"), retro: false, fog_method: Some(Grid), upscale: Some(Bilinear))",
    )
    .unwrap();
    // Volumetric fog reads the prepass, so the frame runs one, as a planet's
    // always does.
    std::fs::write(
        d.join("scenes/first.ron"),
        format!(
            "(name: \"c\", lighting: (direction: (0.3, 0.8, 0.4), ambient: (0.2, 0.2, 0.2), shadows: true, shadow_map: true, \
             fog: true, fog_volumetric: true, fog_density: 0.01, fog_height: 20.0, fog_steps: 8), nodes: [{}])",
            nodes.join(", ")
        ),
    )
    .unwrap();
    d
}

/// The colour pass's GPU milliseconds, the faster of two shots. `None` when
/// this machine cannot render or cannot time.
fn opaque_ms(d: &Path) -> Option<f64> {
    let one = || -> Option<f64> {
        let r = Command::new(env!("CARGO_BIN_EXE_floptle"))
            .args(["shot", &d.to_string_lossy(), "--size", "1600x1200", "--timing", "--json"])
            .args(["--out", &d.join("shot.png").to_string_lossy()])
            .output()
            .expect("run shot");
        if !r.status.success() {
            let why = String::from_utf8_lossy(&r.stderr);
            assert!(
                why.contains("no GPU") || why.contains("could not build the renderer"),
                "shot failed for a reason that is not the adapter:\n{why}"
            );
            eprintln!("skipped — this machine cannot render:\n{why}");
            return None;
        }
        let out = String::from_utf8_lossy(&r.stdout);
        let v: serde_json::Value = serde_json::from_str(out.trim().lines().last()?).expect("shot --json");
        let Some(passes) = v["timing"]["passes"].as_array() else {
            eprintln!("skipped — this GPU reports no timestamps");
            return None;
        };
        passes.iter().find(|p| p["label"] == "opaque + lighting").and_then(|p| p["ms"].as_f64())
    };
    Some(one()?.min(one()?))
}

/// The same eighty cards solid are the yardstick. Measured on an RTX 4060
/// at 1600×1200: cut-out 1.00 ms and solid 1.24 (0.8×) through the
/// depth-Equal pass; 3.32 and 2.28 (1.46×) through the old one, which was
/// what a planet's leaves drew through.
#[test]
fn eighty_cutout_cards_under_raymarched_matter_shade_like_solid_ones() {
    let (cut, solid) = (project(true), project(false));
    let Some(t_cut) = opaque_ms(&cut) else { return };
    let Some(t_solid) = opaque_ms(&solid) else { return };
    for d in [&cut, &solid] {
        let _ = std::fs::remove_dir_all(d);
    }
    assert!(
        t_cut < t_solid.max(0.05) * 1.15,
        "eighty cut-out cards cost {t_cut:.3} ms of colour pass against {t_solid:.3} ms for the same cards solid ({:.1}x)",
        t_cut / t_solid.max(0.05)
    );
}
