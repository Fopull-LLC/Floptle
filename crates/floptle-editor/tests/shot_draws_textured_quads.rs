//! **A trail is a thing in the world, and `shot` photographs it.**
//!
//! Two ways to leave a ribbon behind a moving thing, each guarded on pixels:
//!
//! - `draw.quad` — a ribbon a script draws (a sword trail, a tyre mark, a
//!   decal) takes its picture from an image, keeps the image's alpha, and stays
//!   behind whatever stands in front of it. Each of those is a thing the plain
//!   `draw.tri` layer does not do (colour only, drawn over everything), so each
//!   is asserted on its own pixel: one quad filling the view whose texture is
//!   opaque red on the left and clear on the right, and a green block standing
//!   in front of its left half.
//! - a particle trail that **follows the emitter** — a Local-space particle
//!   sitting still on a node that moves leaves its ribbon along the node's
//!   path in the world, so the ribbon is in the picture where the node HAS
//!   been, not only where it is.

// The `floptle` binary needs the authoring half; see the note at the top of
// `the_json_verbs_emit_only_json.rs`.
#![cfg(feature = "editor-ui")]

use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_floptle")
}

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("flshotquad-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn scaffold(dir: &Path) {
    let out = Command::new(bin())
        .args(["new", &dir.to_string_lossy(), "--template", "platformer"])
        .output()
        .expect("run floptle new");
    assert!(out.status.success(), "scaffold failed: {}", String::from_utf8_lossy(&out.stderr));
}

/// A camera at the origin looking down −Z, a green block 3 units out standing
/// in front of the left of the view, and an empty node carrying the script.
const SCENE: &str = r#"(
    name: "trail",
    nodes: [
        (
            name: "Camera",
            transform: (translation: (0.0, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)),
            matter: Camera(fov_y: 1.0, active: true),
            scripts: [],
        ),
        (
            name: "Block",
            transform: (translation: (-1.0, 0.0, -3.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (0.6, 6.0, 0.2)),
            matter: Primitive(shape: Cube, color: (0.0, 1.0, 0.0)),
            scripts: [],
        ),
        (
            name: "Ribbon",
            transform: (translation: (0.0, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)),
            matter: Empty,
            scripts: [(kind: "ribbon", enabled: true, params: [])],
        ),
    ],
)
"#;

/// One quad 5 units out, wide enough to fill the view, white-tinted so the
/// texture's own colour shows.
const SCRIPT: &str = "function lateUpdate(node, dt)\n\
  draw.quad('textures/halfred.png', -6,-4,-5,  6,-4,-5,  6,4,-5,  -6,4,-5,  1,1,1,1)\n\
end\n";

/// 64×64: opaque red on the left half, fully clear on the right.
fn half_red_texture() -> image::RgbaImage {
    image::RgbaImage::from_fn(64, 64, |x, _| {
        if x < 32 { image::Rgba([255, 0, 0, 255]) } else { image::Rgba([255, 0, 0, 0]) }
    })
}

fn shoot(d: &Path, extra: &[&str]) -> Result<image::RgbaImage, String> {
    let out = d.join(format!("trail{}.png", extra.join("")));
    let r = Command::new(bin())
        .args(["shot", &d.to_string_lossy(), "--scene", "trail", "--size", "160x90", "--out", &out.to_string_lossy()])
        .args(extra)
        .output()
        .expect("run shot");
    if !r.status.success() {
        return Err(String::from_utf8_lossy(&r.stderr).into_owned());
    }
    Ok(image::open(&out).expect("a PNG was written").to_rgba8())
}

fn is_red(p: &image::Rgba<u8>) -> bool {
    p[0] > 200 && p[1] < 60 && p[2] < 60
}

fn is_green(p: &image::Rgba<u8>) -> bool {
    p[1] > 100 && p[1] > p[0] + 40 && p[1] > p[2] + 40
}

/// True when this machine cannot render at all, said out loud — no adapter,
/// or one that cannot build the renderer (CI's is OpenGL). Not a failure of
/// the thing under test.
fn cannot_render(why: &str) -> bool {
    let cannot = why.contains("no GPU") || why.contains("could not build the renderer");
    if cannot {
        eprintln!("skipped — this machine cannot render:\n{why}");
    }
    cannot
}

#[test]
fn a_scripted_textured_quad_is_textured_alpha_blended_and_depth_tested() {
    let d = temp("ribbon");
    scaffold(&d);
    std::fs::write(d.join("scenes/trail.ron"), SCENE).expect("write the scene");
    std::fs::write(d.join("scripts/ribbon.lua"), SCRIPT).expect("write the script");
    std::fs::create_dir_all(d.join("textures")).expect("textures dir");
    half_red_texture().save(d.join("textures/halfred.png")).expect("write the texture");

    // Scripts run only under --after: the unplayed picture has no quad.
    let unplayed = match shoot(&d, &[]) {
        Ok(i) => i,
        Err(why) => {
            assert!(cannot_render(&why), "shot failed for a reason that is not the adapter:\n{why}");
            let _ = std::fs::remove_dir_all(&d);
            return;
        }
    };
    assert!(!is_red(unplayed.get_pixel(20, 45)), "a quad was drawn without the script running");

    let played = shoot(&d, &["--after", "5f"]).expect("the played shot");
    // The texture: the quad's left half is the image's opaque red…
    for (x, y) in [(20, 10), (20, 45), (70, 80)] {
        assert!(
            is_red(played.get_pixel(x, y)),
            "({x},{y}) is {:?} — the quad's textured left half is not in the picture",
            played.get_pixel(x, y)
        );
    }
    // …and its alpha: the right half is clear, so the sky shows through.
    for (x, y) in [(100, 10), (120, 45), (150, 80)] {
        let p = played.get_pixel(x, y);
        assert!(!is_red(p), "({x},{y}) is {p:?} — the image's clear half was drawn opaque");
        assert_eq!(*p, *unplayed.get_pixel(x, y), "({x},{y}) changed where the texture is clear");
    }
    // The depth test: the green block stands 2 units in front of the quad's
    // left half, and the quad does not paint over it.
    let (bx, by) = (54, 45);
    let p = played.get_pixel(bx, by);
    assert!(
        is_green(p),
        "({bx},{by}) is {p:?} — the quad painted over the block standing in front of it \
         (unplayed there: {:?})",
        unplayed.get_pixel(bx, by)
    );

    let _ = std::fs::remove_dir_all(&d);
}

/// A camera at the origin looking down −Z and, 5 units out, a node carrying
/// the effect that a script slides 3 units to the right over the first half
/// second of play. Trail-less, the effect is one still red particle; with the
/// emitter-path trail it drags a red ribbon along the slide.
const SLIDE_SCENE: &str = r#"(
    name: "slide",
    nodes: [
        (
            name: "Camera",
            transform: (translation: (0.0, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)),
            matter: Camera(fov_y: 1.0, active: true),
            scripts: [],
        ),
        (
            name: "Slider",
            transform: (translation: (-1.5, 0.0, -5.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)),
            matter: Empty,
            scripts: [(kind: "slide", enabled: true, params: [])],
            particles: Some((asset: "vfx/Streak", play_on_start: true)),
        ),
    ],
)
"#;

const SLIDE_SCRIPT: &str = "function update(node, dt)\n  node.x = -1.5 + 6.0 * math.min(time, 0.5)\nend\n";

/// One still particle, born at once, living the whole effect; a wide, untapered,
/// untextured ribbon 2 s long so every recorded point is still in the picture.
fn streak_effect(emitter_path: bool) -> String {
    format!(
        r#"(
    name: "Streak",
    lifetime: 5.0,
    playback: Looping,
    tracks: [(
        name: "Dot",
        render: Billboard(texture: None),
        blend: Alpha,
        space: Local,
        clips: [(start: 0.0, end: 5.0, emit: Some(Burst(count: 1, count_jitter: 0.0, pulses: 1, interval: 0.0, interval_jitter: 0.0)))],
        shape: Point,
        velocity: Const(Vec3((0.0, 0.0, 0.0))),
        size: Const(F32(0.4)),
        color: Const(Rgba((1.0, 0.0, 0.0, 1.0))),
        gravity: 0.0,
        trail: Some((time: 2.0, width: 0.6, fade: false, min_distance: 0.02, emitter_path: {emitter_path})),
    )],
)
"#
    )
}

#[test]
fn a_trail_that_follows_the_emitter_is_photographed_along_the_nodes_path() {
    let d = temp("slide");
    scaffold(&d);
    std::fs::write(d.join("scenes/slide.ron"), SLIDE_SCENE).expect("write the scene");
    std::fs::write(d.join("scripts/slide.lua"), SLIDE_SCRIPT).expect("write the script");
    std::fs::create_dir_all(d.join("vfx")).expect("vfx dir");
    let shoot_slide = |d: &Path| -> Result<image::RgbaImage, String> {
        let out = d.join("slide.png");
        let r = Command::new(bin())
            .args(["shot", &d.to_string_lossy(), "--scene", "slide", "--size", "160x90", "--after", "1s"])
            .args(["--out", &out.to_string_lossy()])
            .output()
            .expect("run shot");
        if !r.status.success() {
            return Err(String::from_utf8_lossy(&r.stderr).into_owned());
        }
        Ok(image::open(&out).expect("a PNG was written").to_rgba8())
    };
    // The node ends at x = +1.5, 5 units out: on a 160-wide, fov 1.0 picture a
    // unit there is ~16.5 px, so that is ~25 px right of centre, and its start
    // 25 px left of it (the first point is recorded a tick in). The path's
    // midpoint is the centre pixel.
    let (start_x, mid_x, end_x) = (60u32, 80u32, 105u32);

    std::fs::write(d.join("vfx/Streak.vfx.ron"), streak_effect(false)).expect("write the effect");
    let plain = match shoot_slide(&d) {
        Ok(i) => i,
        Err(why) => {
            assert!(cannot_render(&why), "shot failed for a reason that is not the adapter:\n{why}");
            let _ = std::fs::remove_dir_all(&d);
            return;
        }
    };
    // Without the option the particle has not moved within its emitter, so the
    // ribbon is only the dot at the node's final place.
    assert!(is_red(plain.get_pixel(end_x, 45)), "the particle itself is not in the picture: {:?}", plain.get_pixel(end_x, 45));
    assert!(!is_red(plain.get_pixel(mid_x, 45)), "a trail was drawn along the path without emitter_path");
    assert!(!is_red(plain.get_pixel(start_x, 45)));

    std::fs::write(d.join("vfx/Streak.vfx.ron"), streak_effect(true)).expect("write the effect");
    let followed = shoot_slide(&d).expect("the emitter-path shot");
    for x in [start_x, mid_x, end_x] {
        assert!(
            is_red(followed.get_pixel(x, 45)),
            "({x},45) is {:?} — the ribbon is not along the node's path",
            followed.get_pixel(x, 45)
        );
    }
    // Above and below the ribbon's width, and beyond the path's start: sky.
    for (x, y) in [(mid_x, 10), (mid_x, 80), (20, 45)] {
        assert!(!is_red(followed.get_pixel(x, y)), "({x},{y}) is red off the ribbon");
    }

    let _ = std::fs::remove_dir_all(&d);
}
