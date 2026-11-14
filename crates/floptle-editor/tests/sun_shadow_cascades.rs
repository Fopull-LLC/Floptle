//! **At a long shadow reach the sun map draws a near cascade, so a small
//! caster near the camera keeps its shadow.**
//!
//! One shadow map's texel is `shadow_distance` × 1.7 / `shadow_map_size`: at
//! 364 m and 2048 texels, 30 cm. The filter, the normal offset and the depth
//! bias all scale with it, so a person-sized caster gets two or three texels
//! and its shadow goes blotchy or vanishes. The near cascade is a second map
//! of the same size over `shadow_near_distance` around the camera.
//!
//! Three things are held here: a 1.8 m figure at 364 m with the cascade looks
//! like the same figure at a 40 m reach and without it does not; the cascade
//! hands over to the far map without a seam; and a scene whose one map was
//! already fine (the default 150 m reach) renders exactly as it did.

// The `floptle` binary needs the authoring half; see the note at the top of
// `the_json_verbs_emit_only_json.rs`.
#![cfg(feature = "editor-ui")]

use std::path::{Path, PathBuf};
use std::process::Command;

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("flsuncasc-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("scenes")).unwrap();
    d
}

/// Half the edge of the built-in cube (it is 1.4 across at scale 1).
const H: f32 = 0.7;

fn cube(name: &str, at: [f32; 3], size: [f32; 3]) -> String {
    format!(
        "(name: \"{name}\", transform: (translation: ({}, {}, {}), scale: ({}, {}, {})), matter: Primitive(shape: Cube, color: (0.8, 0.8, 0.8)))",
        at[0], at[1], at[2], size[0] / (2.0 * H), size[1] / (2.0 * H), size[2] / (2.0 * H)
    )
}

/// The built-in capsule is 1 wide and 2 tall.
fn capsule(name: &str, at: [f32; 3], width: f32, height: f32) -> String {
    format!(
        "(name: \"{name}\", transform: (translation: ({}, {}, {}), scale: ({width}, {}, {width})), matter: Primitive(shape: Capsule, color: (0.8, 0.8, 0.8)))",
        at[0], at[1], at[2], height / 2.0
    )
}

fn sphere(name: &str, at: [f32; 3], radius: f32) -> String {
    let s = radius / 0.85;
    format!(
        "(name: \"{name}\", transform: (translation: ({}, {}, {}), scale: ({s}, {s}, {s})), matter: Primitive(shape: Sphere, color: (0.8, 0.8, 0.8)))",
        at[0], at[1], at[2]
    )
}

/// What stands on the ground.
enum Fixture {
    /// A 1.8 m figure (legs, torso, arms, head; 0.6 m across) at the origin,
    /// seen from 3.2 m.
    Figure,
    /// A slatted roof 5 m up over 40 × 80 m of ground, 0.2 m slats every
    /// 0.8 m, seen looking out along them: crisp stripes under a fine map,
    /// grey blotches under a coarse one, so the two maps differ over the
    /// whole ground, and wherever one hands over to the other, it shows.
    Slats,
}

/// A project with the sun `elev` degrees up and the map's settings `light`
/// (RON fields after `shadow_map: true`).
fn project(d: &Path, fixture: Fixture, elev: f32, light: &str) {
    let (az, cam, nodes) = match fixture {
        Fixture::Figure => (
            60f32,
            (1.3, 3.2, -8f32),
            vec![
                capsule("LegL", [-0.1, 0.45, 0.0], 0.15, 0.9),
                capsule("LegR", [0.1, 0.45, 0.0], 0.15, 0.9),
                cube("Torso", [0.0, 1.21, 0.0], [0.38, 0.62, 0.24]),
                sphere("Head", [0.0, 1.67, 0.0], 0.13),
                capsule("ArmL", [-0.26, 1.18, 0.0], 0.1, 0.66),
                capsule("ArmR", [0.26, 1.18, 0.0], 0.1, 0.66),
            ],
        ),
        Fixture::Slats => {
            (20f32, (1.7, 0.0, -12f32), (0..51).map(|i| cube(&format!("Slat{i}"), [-20.0 + 0.8 * i as f32, 5.0, -30.0], [0.2, 0.1, 80.0])).collect())
        }
    };
    let (e, a) = (elev.to_radians(), az.to_radians());
    let sun = [e.cos() * a.sin(), e.sin(), e.cos() * a.cos()];
    let p = cam.2.to_radians() / 2.0;
    let mut all = vec![
        format!(
            "(name: \"Camera\", transform: (translation: (0.0, {}, {}), rotation: ({}, 0.0, 0.0, {})), matter: Camera(fov_y: 0.8, active: true))",
            cam.0,
            cam.1,
            p.sin(),
            p.cos()
        ),
        cube("Ground", [0.0, -0.05, 0.0], [400.0, 0.1, 400.0]),
    ];
    all.extend(nodes);
    std::fs::write(d.join("project.ron"), "(title: Some(\"s\"), entry_scene: Some(\"scenes/first.ron\"), retro: false)").unwrap();
    std::fs::write(
        d.join("scenes/first.ron"),
        format!(
            "(name: \"s\", lighting: (direction: ({}, {}, {}), ambient: (0.15, 0.15, 0.17), shadows: true, shadow_map: true, {light}), nodes: [{}])",
            sun[0],
            sun[1],
            sun[2],
            all.join(", ")
        ),
    )
    .unwrap();
}

/// The shot's luminance, row by row, or `None` on a machine that cannot
/// render.
fn shot(name: &str, fixture: Fixture, elev: f32, light: &str) -> Option<(usize, Vec<f32>)> {
    let d = temp(name);
    project(&d, fixture, elev, light);
    let out = d.join("shot.png");
    let r = Command::new(env!("CARGO_BIN_EXE_floptle"))
        .args(["shot", &d.to_string_lossy(), "--size", "960x540", "--no-ui", "--out", &out.to_string_lossy()])
        .output()
        .expect("run shot");
    if !r.status.success() {
        let why = String::from_utf8_lossy(&r.stderr).into_owned();
        let cannot = why.contains("no GPU") || why.contains("could not build the renderer");
        assert!(cannot, "shot failed for a reason that is not the adapter:\n{why}");
        eprintln!("skipped — this machine cannot render:\n{why}");
        return None;
    }
    let log = String::from_utf8_lossy(&r.stderr);
    assert!(!log.contains("failed to load"), "the fixture's scene did not load:\n{log}");
    let img = image::open(&out).expect("a PNG").to_luma8();
    let w = img.width() as usize;
    let px = img.pixels().map(|p| p[0] as f32).collect();
    let _ = std::fs::remove_dir_all(&d);
    Some((w, px))
}

fn mean_diff(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum::<f32>() / a.len() as f32
}

/// **A 1.8 m figure at a 364 m reach shadows like the same figure at 40 m.**
/// With one map its shadow on the ground is a faint smear and its limbs'
/// shadows on each other are gone; with the near cascade (on by default at
/// this reach) the picture matches the 40 m one. Three sun heights, the
/// card's 8–35°.
#[test]
fn a_small_caster_at_a_long_reach_shadows_like_it_does_at_a_short_one() {
    for elev in [8.0, 20.0, 35.0] {
        let Some((_, reference)) = shot("ref", Fixture::Figure, elev, "shadow_distance: 40") else { return };
        let (_, single) = shot("single", Fixture::Figure, elev, "shadow_distance: 364, shadow_near_distance: 0").unwrap();
        let (_, cascaded) = shot("cascade", Fixture::Figure, elev, "shadow_distance: 364").unwrap();
        let (one, two) = (mean_diff(&single, &reference), mean_diff(&cascaded, &reference));
        eprintln!("sun {elev}°: off the 40 m picture by {one:.3} with one map, {two:.3} with the near cascade");
        assert!(one > 0.8, "the fixture: at 364 m one map should lose the figure's shadow detail, but it is off by only {one:.3}");
        assert!(two < 0.1, "with the near cascade the figure at 364 m is off the 40 m picture by {two:.3} (one map: {one:.3})");
    }
}

/// **The near cascade hands over to the far map without a seam.** Under the
/// slatted roof the two maps disagree everywhere: the near one draws crisp
/// stripes, the far one grey blotches. Wherever the cascaded picture jumps
/// between neighbouring pixels by more than either map's own picture does
/// there, the jump is the hand-over itself. Across the blend band there is
/// no such jump; with the band at nothing, a line of them runs along the
/// near box's edge.
#[test]
fn the_near_cascade_hands_over_without_a_seam() {
    // A near box of 12 m, so its far edge is in view.
    let near = "shadow_distance: 364, shadow_near_distance: 12";
    let Some((w, c)) = shot("seam-casc", Fixture::Slats, 55.0, near) else { return };
    let (_, f) = shot("seam-far", Fixture::Slats, 55.0, "shadow_distance: 364, shadow_near_distance: 0").unwrap();
    let (_, n) = shot("seam-near", Fixture::Slats, 55.0, "shadow_distance: 12").unwrap();
    assert!(mean_diff(&f, &n) > 10.0, "the fixture: the two maps should disagree under the slats");
    // Jumps down each column (the box's far edge runs across the picture)
    // and along each row (its sides), past what either map has there.
    let at = |v: &[f32], i: usize| v[i];
    let mut seam = 0usize;
    let mut worst = 0f32;
    for i in 0..c.len() {
        for j in [i + 1, i + w] {
            if j >= c.len() || (j == i + 1 && j % w == 0) {
                continue;
            }
            let own = (at(&f, j) - at(&f, i)).abs().max((at(&n, j) - at(&n, i)).abs());
            let excess = (at(&c, j) - at(&c, i)).abs() - own;
            worst = worst.max(excess);
            seam += (excess > 12.0) as usize;
        }
    }
    eprintln!("pixel pairs jumping past both maps by more than 12: {seam} (worst {worst})");
    assert!(seam < 40, "the near cascade's edge is a seam: {seam} neighbouring pixels jump past both maps (worst by {worst})");
}

/// **A scene whose one map was already fine renders exactly as it did.** At
/// the default 150 m reach the near cascade is not drawn, at the default
/// 2048 texels and at a small map's 512 alike (a small map is a settings
/// screen's soft, cheap "low", and must stay that), so the picture is the
/// one a scene with the cascade switched off draws, to the bit.
#[test]
fn a_scene_at_the_default_reach_renders_as_it_did() {
    for size in [2048, 512] {
        let Some((_, default)) = shot("parity-default", Fixture::Figure, 20.0, &format!("shadow_distance: 150, shadow_map_size: {size}")) else {
            return;
        };
        let (_, single) =
            shot("parity-single", Fixture::Figure, 20.0, &format!("shadow_distance: 150, shadow_map_size: {size}, shadow_near_distance: 0")).unwrap();
        let max = default.iter().zip(&single).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
        assert_eq!(max, 0.0, "the default 150 m scene at {size} texels changed with the near cascade's default: a pixel moved by {max}");
    }
}
