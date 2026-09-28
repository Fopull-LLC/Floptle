//! **`floptle shot` draws a `.flsl` material and a UI shader.**
//!
//! `shot` is the tool a project is looked at with from a terminal, and it
//! photographed every shader-wearing mesh in its plain fallback look: the
//! frame loops compile and bind `.flsl` materials, UI shaders and post
//! shaders each frame, and the one-shot path never did. A distance-faded
//! barrier came out as a solid crosshatch; a shader-drawn button came out as a
//! white slab. The picture was confidently wrong, which is worse than none.
//!
//! The guard: one cube whose material names a shader that paints it pure
//! green, and one screen-space box whose UI shader paints it pure blue.
//! Neither colour is anything the fallback look can produce.

// The `floptle` binary needs the authoring half; see the note at the top of
// `the_json_verbs_emit_only_json.rs`.
#![cfg(feature = "editor-ui")]

use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_floptle")
}

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("flshotsh-{name}-{}", std::process::id()));
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

const GREEN: &str = "shader flatGreen {\n  stage fragment\n  output color = vec4(0, 1, 0, 1)\n}\n";
const BLUE: &str = "shader flatBlue {\n  stage ui\n  output color = vec4(0, 0, 1, 1)\n}\n";

/// A camera at the origin, a big cube filling the middle of its view, and a
/// 40×40 UI box in the top-left corner of a layer where one design unit is one
/// pixel at 160×90.
const SCENE: &str = r#"(
    name: "shaded",
    nodes: [
        (
            name: "Camera",
            transform: (translation: (0.0, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)),
            matter: Camera(fov_y: 1.0, active: true),
            scripts: [],
        ),
        (
            name: "Wall",
            transform: (translation: (0.0, 0.0, -6.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (8.0, 8.0, 1.0)),
            matter: Primitive(shape: Cube, color: (1.0, 1.0, 1.0)),
            scripts: [],
            material: Some((shader: Some("shaders/flatGreen.flsl"))),
        ),
        (
            name: "Hud",
            transform: (translation: (0.0, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)),
            matter: Empty,
            scripts: [],
            ui_layer: Some((design_height: 90.0, reference_width: 160.0, scale_mode: MatchHeight, match_wh: 0.5, z: 0, enabled: true, space: Screen, canvas_scale: 0.01, nav_wrap: true, tooltip_delay: 0.35)),
        ),
        (
            name: "Gauge",
            transform: (translation: (0.0, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)),
            matter: Empty,
            scripts: [],
            parent: Some(2),
            ui: Some((place: Free(pos: (0.0, 0.0)), size: (Fixed(40.0), Fixed(40.0)), shader: "shaders/flatBlue.flsl")),
        ),
    ],
)
"#;

fn cannot_render(why: &str) -> bool {
    let cannot = why.contains("no GPU") || why.contains("could not build the renderer");
    if cannot {
        eprintln!("skipped — this machine cannot render:\n{why}");
    }
    cannot
}

#[test]
fn a_shot_draws_a_flsl_material_and_a_ui_shader() {
    let d = temp("both");
    scaffold(&d);
    std::fs::create_dir_all(d.join("shaders")).unwrap();
    std::fs::write(d.join("shaders/flatGreen.flsl"), GREEN).unwrap();
    std::fs::write(d.join("shaders/flatBlue.flsl"), BLUE).unwrap();
    std::fs::write(d.join("scenes/shaded.ron"), SCENE).unwrap();

    let out = d.join("shaded.png");
    let r = Command::new(bin())
        .args(["shot", &d.to_string_lossy(), "--scene", "shaded", "--size", "160x90", "--out", &out.to_string_lossy()])
        .output()
        .expect("run shot");
    if !r.status.success() {
        let why = String::from_utf8_lossy(&r.stderr);
        assert!(cannot_render(&why), "shot failed for a reason that is not the adapter:\n{why}");
        let _ = std::fs::remove_dir_all(&d);
        return;
    }
    let img = image::open(&out).expect("a PNG was written").to_rgba8();

    let wall = img.get_pixel(110, 60);
    assert!(
        wall[1] > 180 && wall[0] < 60 && wall[2] < 60,
        "the wall is {wall:?}, not its shader's green: the .flsl material was drawn with its fallback look"
    );
    let gauge = img.get_pixel(20, 20);
    assert!(
        gauge[2] > 180 && gauge[0] < 60 && gauge[1] < 60,
        "the gauge is {gauge:?}, not its shader's blue: the UI shader was never compiled for the shot"
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// **A surface shader lights a normal of its own.** `litSurface(albedo, n)`
/// runs the engine's key light, shadow and point lights over `n` instead of
/// the mesh's normal, and `lightDir` says where that light is: so a normal
/// bent toward the light is lit and one bent away is not, on the same flat
/// face. Waves and surface detail need exactly this, and could only fake it.
#[test]
fn a_surface_shader_lights_its_own_normal_toward_the_key_light() {
    let d = temp("litnormal");
    scaffold(&d);
    std::fs::create_dir_all(d.join("shaders")).unwrap();
    let shader = |name: &str, n: &str| {
        format!("shader {name} {{\n  stage fragment\n  let lit = litSurface(vec3(0.8), {n}) * lightColor\n  output color = vec4(lit, 1)\n}}\n")
    };
    std::fs::write(d.join("shaders/toward.flsl"), shader("toward", "lightDir")).unwrap();
    std::fs::write(d.join("shaders/away.flsl"), shader("away", "-lightDir")).unwrap();
    let wall = |name: &str, x: f32, sh: &str| {
        format!(
            "(name: \"{name}\", transform: (translation: ({x}, 0.0, -6.0), rotation: (0.0, 0.0, 0.0, 1.0), \
             scale: (3.0, 3.0, 1.0)), matter: Primitive(shape: Cube, color: (1.0, 1.0, 1.0)), scripts: [], \
             material: Some((shader: Some(\"shaders/{sh}.flsl\"))))"
        )
    };
    let scene = format!(
        "(name: \"lit\", nodes: [(name: \"Camera\", transform: (translation: (0.0, 0.0, 0.0), \
         rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)), matter: Camera(fov_y: 1.0, active: true), \
         scripts: []), {}, {}])",
        wall("Toward", -2.0, "toward"),
        wall("Away", 2.0, "away")
    );
    std::fs::write(d.join("scenes/lit.ron"), scene).unwrap();

    let out = d.join("lit.png");
    let r = Command::new(bin())
        .args(["shot", &d.to_string_lossy(), "--scene", "lit", "--size", "160x90", "--out", &out.to_string_lossy()])
        .output()
        .expect("run shot");
    if !r.status.success() {
        let why = String::from_utf8_lossy(&r.stderr);
        assert!(cannot_render(&why), "shot failed for a reason that is not the adapter:\n{why}");
        let _ = std::fs::remove_dir_all(&d);
        return;
    }
    let img = image::open(&out).expect("a PNG was written").to_rgba8();
    let lum = |x, y| {
        let p = img.get_pixel(x, y);
        u32::from(p[0]) + u32::from(p[1]) + u32::from(p[2])
    };
    let (toward, away) = (lum(55, 45), lum(105, 45));
    assert!(toward > away + 150, "lit toward the light {toward}, away from it {away}: the shader's normal was not lit");
    let _ = std::fs::remove_dir_all(&d);
}
