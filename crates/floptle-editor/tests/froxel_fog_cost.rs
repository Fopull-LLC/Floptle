//! **The grid fog's cost does not follow the pixels.**
//!
//! The lit volumetric fog was marched along every half-resolution pixel's
//! ray, every step lit, and twice at a foreground edge: four times the
//! pixels, four times the cost, and a field of grass is edges everywhere.
//! Built in a grid of cells over the view (`fog_method: Grid`), each cell
//! is lit once and every pixel reads it. These time the whole frame on the
//! GPU with the fog and without it, march and grid, at 1280×720 and
//! 2560×1440, in the same run, and hold the growth of the fog's own cost to
//! a ratio, never a duration.
//!
//! Needs a GPU with timestamp queries; without one the test says so and
//! passes, as the other render probes do.

// The `floptle` binary needs the authoring half; see the note at the top of
// `the_json_verbs_emit_only_json.rs`.
#![cfg(feature = "editor-ui")]

use std::path::{Path, PathBuf};
use std::process::Command;

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("flfroxel-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("scenes")).unwrap();
    d
}

/// Ground, boxes and a field of thin poles (every pixel of one an edge with
/// the ground behind it, as stems are) under lit volumetric fog, or none.
fn field(d: &Path, method: &str, fog: bool) {
    let mut nodes = vec![
        "(name: \"Camera\", transform: (translation: (0.0, 6.0, 6.0), rotation: (-0.22303, 0.0, 0.0, 0.97481)), matter: Camera(fov_y: 1.0, active: true))".to_string(),
        "(name: \"Floor\", transform: (translation: (0.0, -1.0, -40.0), scale: (200.0, 1.0, 200.0)), matter: Primitive(shape: Cube, color: (0.4, 0.5, 0.3)))".to_string(),
    ];
    for k in 0..12 {
        let (x, z) = ((k % 4) as f32 * 9.0 - 13.0, -8.0 - (k / 4) as f32 * 14.0);
        nodes.push(format!(
            "(name: \"B{k}\", transform: (translation: ({x}, 1.0, {z}), scale: (2.0, {}, 2.0)), matter: Primitive(shape: Cube, color: (0.8, 0.75, 0.7)))",
            3.0 + k as f32 * 0.4
        ));
    }
    for k in 0..300 {
        let (x, z) = ((k % 30) as f32 * 0.6 - 9.0, -3.0 - (k / 30) as f32 * 1.2);
        nodes.push(format!(
            "(name: \"P{k}\", transform: (translation: ({x}, 0.5, {z}), scale: (0.06, 3.0, 0.06)), matter: Primitive(shape: Cube, color: (0.3, 0.5, 0.2)))"
        ));
    }
    std::fs::write(
        d.join("project.ron"),
        format!("(title: Some(\"f\"), entry_scene: Some(\"scenes/first.ron\"), retro: false, fog_method: Some({method}))"),
    )
    .unwrap();
    std::fs::write(
        d.join("scenes/first.ron"),
        format!(
            "(name: \"f\", lighting: (direction: (0.3, 0.8, 0.4), ambient: (0.1, 0.1, 0.1), shadows: true, \
             fog: {fog}, fog_color: (0.85, 0.85, 0.9), fog_volumetric: {fog}, fog_density: 0.06, fog_height: 6.0, \
             fog_noise: 0.5, fog_light: 1.0, fog_steps: 16), nodes: [{}])",
            nodes.join(", ")
        ),
    )
    .unwrap();
}

/// The whole frame's GPU milliseconds at `size`, the median `shot --timing`
/// reports summed over its passes. `None` when this machine cannot render or
/// cannot time.
fn gpu_ms(d: &Path, size: &str) -> Option<f64> {
    let r = Command::new(env!("CARGO_BIN_EXE_floptle"))
        .args(["shot", &d.to_string_lossy(), "--size", size, "--timing", "--json"])
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
    Some(passes.iter().filter_map(|p| p["ms"].as_f64()).sum())
}

/// The fog's own cost: the frame with it less the frame without, each the
/// faster of two runs (the first draw also builds pipelines, and a
/// background hiccup only ever adds time).
fn fog_ms(method: &str, size: &str) -> Option<f64> {
    let (on, off) = (temp(&format!("{method}-{size}-on")), temp(&format!("{method}-{size}-off")));
    field(&on, method, true);
    field(&off, method, false);
    let best = |d: &Path| Some(gpu_ms(d, size)?.min(gpu_ms(d, size)?));
    let out = Some(best(&on)? - best(&off)?);
    for d in [&on, &off] {
        let _ = std::fs::remove_dir_all(d);
    }
    out
}

/// Measured on an RTX 4060, the fog's own cost: grid 0.94 ms at 720p and
/// 1.31 at 1440p (1.4×); march 1.03 and 3.83 (3.7×).
#[test]
fn grid_fog_cost_does_not_follow_the_pixels() {
    let Some(grid_small) = fog_ms("Grid", "1280x720") else { return };
    let Some(grid_big) = fog_ms("Grid", "2560x1440") else { return };
    let Some(march_small) = fog_ms("March", "1280x720") else { return };
    let Some(march_big) = fog_ms("March", "2560x1440") else { return };
    let said = format!("grid {grid_small:.2} ms at 720p, {grid_big:.2} at 1440p; march {march_small:.2}, {march_big:.2}");
    assert!(grid_big < grid_small.max(0.2) * 1.8, "the grid fog's cost followed the pixels: {said}");
    assert!(grid_big < march_big * 0.8, "at 1440p the grid fog costs no less than the march: {said}");
}
