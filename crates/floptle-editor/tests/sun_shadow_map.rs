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
    scene(d, map, false);
}

/// `stars`: lit by a star instead of the Lighting direction. The star sits far
/// out along the same slant, so the sign's shadow lands in the same place.
fn scene(d: &Path, map: bool, stars: bool) {
    let star = if stars {
        ", (name: \"Star\", transform: (translation: (4000.0, 4000.0, 0.0)), \
           celestial: Some((luminosity: 40.0, body_radius: 10.0)))"
    } else {
        ""
    };
    std::fs::write(d.join("project.ron"), "(title: Some(\"s\"), entry_scene: Some(\"scenes/first.ron\"), retro: false)").unwrap();
    std::fs::write(
        d.join("scenes/first.ron"),
        format!(
            "(name: \"s\", lighting: (direction: (1.0, 1.0, 0.0), ambient: (0.1, 0.1, 0.1), shadows: true, shadow_softness: 0.2, shadow_map: {map}, stars: {stars}), nodes: [\
               (name: \"Camera\", transform: (translation: (0.0, 15.0, 0.0), rotation: (-0.70710677, 0.0, 0.0, 0.70710677)), matter: Camera(fov_y: 0.8, active: true)), \
               (name: \"Floor\", transform: (translation: (0.0, -0.1, 0.0), scale: (20.0, 0.1, 20.0)), matter: Primitive(shape: Cube, color: (0.8, 0.8, 0.8))), \
               (name: \"Sign\", transform: (translation: (4.0, 4.0, 0.0), scale: (2.0, 0.1, 2.0)), matter: Primitive(shape: Cube, color: (0.8, 0.8, 0.8))){star}\
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

fn centre_luma_of(d: &Path) -> Option<f32> {
    match centre_luma(d) {
        Ok(v) => Some(v),
        Err(why) => {
            let cannot = why.contains("no GPU") || why.contains("could not build the renderer");
            assert!(cannot, "shot failed for a reason that is not the adapter:\n{why}");
            eprintln!("skipped — this machine cannot render:\n{why}");
            None
        }
    }
}

/// **Stars mode draws the shadow map too, toward the brightest star.** It
/// used to turn the map off and march every lit pixel, which a room full of
/// walls paid for three times over. The sign does not collide, so only the
/// map can see it: lit under the march, dark under the map.
#[test]
fn the_brightest_star_casts_through_the_shadow_map() {
    let (dm, df) = (temp("star-map"), temp("star-march"));
    scene(&dm, true, true);
    scene(&df, false, true);
    let Some(mapped) = centre_luma_of(&dm) else { return };
    let marched = centre_luma_of(&df).expect("the march shot");
    assert!(marched > 60.0, "the fixture: the star lights the floor's centre: {marched}");
    assert!(mapped < marched * 0.6, "the star cast no shadow through the map: centre {mapped} against {marched} lit");
    for d in [&dm, &df] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// A roof that is only a box body (nothing drawn), so only its shadow proxy
/// can cast, with the map off so the march decides. It sits under a parent at
/// `scale`, with half extents that make it 12 m wide at that scale. Offset so
/// the sun's ray from the floor's centre crosses its far side: a proxy at the
/// raw half extents misses it, one at the collider's size covers it.
fn roof_scene(d: &Path, scale: Option<f32>) {
    let roof = match scale {
        Some(s) => format!(
            ", (name: \"Shell\", transform: (translation: (7.0, 4.0, 0.0), scale: ({s}, {s}, {s}))), \
               (name: \"Roof\", parent: Some(3), rigidbody: Some((boxed: true, mode: Static, \
                half_extents: ({h}, {t}, {h}))))",
            h = 6.0 / s,
            t = 0.1 / s
        ),
        None => String::new(),
    };
    std::fs::write(d.join("project.ron"), "(title: Some(\"s\"), entry_scene: Some(\"scenes/first.ron\"), retro: false)").unwrap();
    std::fs::write(
        d.join("scenes/first.ron"),
        format!(
            "(name: \"s\", lighting: (direction: (1.0, 1.0, 0.0), ambient: (0.1, 0.1, 0.1), shadows: true, shadow_softness: 0.2), nodes: [\
               (name: \"Camera\", transform: (translation: (0.0, 15.0, 0.0), rotation: (-0.70710677, 0.0, 0.0, 0.70710677)), matter: Camera(fov_y: 0.8, active: true)), \
               (name: \"Floor\", transform: (translation: (0.0, -0.1, 0.0), scale: (20.0, 0.1, 20.0)), matter: Primitive(shape: Cube, color: (0.8, 0.8, 0.8))), \
               (name: \"Marker\")\
               {roof}\
             ])"
        ),
    )
    .unwrap();
}

/// **A box body under a scaled parent shadows at the size it collides at.**
/// Its proxy used the raw half extents, so a building's shell under a scale-6
/// parent cast a shadow a sixth of its size and the sun lit the rooms inside.
#[test]
fn a_scaled_roof_shadows_the_floor_under_it() {
    let (open, scaled, plain) = (temp("roof-none"), temp("roof-6"), temp("roof-1"));
    roof_scene(&open, None);
    roof_scene(&scaled, Some(6.0));
    roof_scene(&plain, Some(1.0));
    let Some(lit) = centre_luma_of(&open) else { return };
    let under_plain = centre_luma_of(&plain).expect("the scale-1 shot");
    let under_scaled = centre_luma_of(&scaled).expect("the scale-6 shot");
    assert!(lit > 60.0, "the fixture: with no roof the floor's centre is lit: {lit}");
    assert!(under_plain < lit * 0.6, "the fixture: a scale-1 roof of the same size shadows it: {under_plain} against {lit}");
    assert!(
        under_scaled < lit * 0.6,
        "the scale-6 roof let the sun through: {under_scaled} against {lit} lit, {under_plain} under the scale-1 roof"
    );
    for d in [&open, &scaled, &plain] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// **A caster the camera cannot see still casts through the map.** The map
/// was drawn from the camera's own draw list, so a roof above a camera looking
/// at the floor cast nothing, and the sun lit the room. Here the camera looks
/// straight down, and a drawn slab that does not collide (so only the map can
/// see it) hangs far above and behind it, on the sun's line through the floor
/// under the camera. Far enough that its bounds leave the camera's view: a
/// slab just overhead has bounds that contain the camera, and is never culled.
#[test]
fn a_roof_out_of_view_shades_the_floor_through_the_map() {
    let scene_with = |d: &Path, roof: bool| {
        let roof = if roof {
            ", (name: \"Roof\", transform: (translation: (12.0, 12.0, 0.0), scale: (6.0, 0.2, 6.0)), \
               matter: Primitive(shape: Cube, color: (0.8, 0.8, 0.8)))"
        } else {
            ""
        };
        std::fs::write(d.join("project.ron"), "(title: Some(\"s\"), entry_scene: Some(\"scenes/first.ron\"), retro: false)").unwrap();
        std::fs::write(
            d.join("scenes/first.ron"),
            format!(
                "(name: \"s\", lighting: (direction: (1.0, 1.0, 0.0), ambient: (0.1, 0.1, 0.1), shadows: true, shadow_softness: 0.2, shadow_map: true), nodes: [\
                   (name: \"Camera\", transform: (translation: (0.0, 2.0, 0.0), rotation: (-0.70710677, 0.0, 0.0, 0.70710677)), matter: Camera(fov_y: 0.8, active: true)), \
                   (name: \"Floor\", transform: (translation: (0.0, -0.1, 0.0), scale: (20.0, 0.1, 20.0)), matter: Primitive(shape: Cube, color: (0.8, 0.8, 0.8))){roof}\
                 ])"
            ),
        )
        .unwrap();
    };
    let (open, under) = (temp("roof-open"), temp("roof-over"));
    scene_with(&open, false);
    scene_with(&under, true);
    let Some(lit) = centre_luma_of(&open) else { return };
    let shaded = centre_luma_of(&under).expect("the roofed shot");
    assert!(lit > 60.0, "the fixture: with no roof the floor is lit: {lit}");
    assert!(shaded < lit * 0.6, "the roof out of view cast nothing through the map: {shaded} against {lit} lit");
    for d in [&open, &under] {
        let _ = std::fs::remove_dir_all(d);
    }
}
