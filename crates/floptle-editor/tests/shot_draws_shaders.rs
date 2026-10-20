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

/// **A surface shader keeps a Physical material's normal map.** `litPbr` is the
/// built-in Physical lighting on the shader's albedo, with the Material's own
/// normal and ORM maps: so the same wall, its normal map on, draws its ridges
/// under the shader as it does with no shader at all, and differs plainly from
/// the wall with the map taken off. `litSurface` was Blinn-Phong over the mesh
/// normal, which is why a damage shader on a normal-mapped hull flattened it.
#[test]
fn a_pbr_surface_shader_shows_the_materials_normal_map() {
    let d = temp("pbrnormal");
    for sub in ["scenes", "shaders", "textures"] {
        std::fs::create_dir_all(d.join(sub)).unwrap();
    }
    std::fs::write(d.join("project.ron"), "(title: Some(\"p\"), entry_scene: Some(\"scenes/pbr.ron\"))").unwrap();
    std::fs::write(
        d.join("shaders/pbr.flsl"),
        "shader pbr {\n  stage fragment\n  output color = vec4(litPbr(vec3(0.8)), 1)\n}\n",
    )
    .unwrap();
    // Ridges: the normal leans hard one way, then the other, eight times across.
    let mut n = image::RgbaImage::new(64, 64);
    for (x, _, p) in n.enumerate_pixels_mut() {
        *p = if (x / 4) % 2 == 0 { image::Rgba([30, 128, 200, 255]) } else { image::Rgba([225, 128, 200, 255]) };
    }
    n.save(d.join("textures/ridges.png")).unwrap();
    let shoot = |tag: &str, material: &str| -> Option<image::RgbaImage> {
        let scene = format!(
            "(name: \"pbr\", lighting: (direction: (0.8, 0.3, 0.6), ambient: (0.1, 0.1, 0.1)), nodes: [\
             (name: \"Camera\", transform: (translation: (0.0, 0.0, 0.0)), matter: Camera(fov_y: 1.0, active: true)),\
             (name: \"Wall\", transform: (translation: (0.0, 0.0, -6.0), scale: (8.0, 8.0, 1.0)), \
              matter: Primitive(shape: Cube, color: (1.0, 1.0, 1.0)), material: Some(({material})))])"
        );
        std::fs::write(d.join("scenes/pbr.ron"), scene).unwrap();
        let out = d.join(format!("{tag}.png"));
        let r = Command::new(bin())
            .args(["shot", &d.to_string_lossy(), "--scene", "pbr", "--size", "160x90", "--no-ui", "--out", &out.to_string_lossy()])
            .output()
            .expect("run shot");
        if !r.status.success() {
            let why = String::from_utf8_lossy(&r.stderr);
            assert!(cannot_render(&why), "shot failed for a reason that is not the adapter:\n{why}");
            return None;
        }
        Some(image::open(&out).expect("a PNG was written").to_rgba8())
    };
    let surface = "color: (0.8, 0.8, 0.8), shading: Physical, metallic: 0.0, roughness: 0.5";
    let Some(mapped) = shoot("mapped", &format!("{surface}, normal_map: Some(\"textures/ridges.png\"), shader: Some(\"shaders/pbr.flsl\")")) else { return };
    let Some(flat) = shoot("flat", &format!("{surface}, shader: Some(\"shaders/pbr.flsl\")")) else { return };
    let Some(builtin) = shoot("builtin", &format!("{surface}, normal_map: Some(\"textures/ridges.png\")")) else { return };
    let diff = |a: &image::RgbaImage, b: &image::RgbaImage| {
        // The middle of the wall, away from its edges.
        let mut sum = 0u64;
        for y in 25..65 {
            for x in 40..120 {
                let (p, q) = (a.get_pixel(x, y), b.get_pixel(x, y));
                sum += (0..3).map(|c| (p[c] as i32 - q[c] as i32).unsigned_abs() as u64).sum::<u64>();
            }
        }
        sum as f64 / (40.0 * 80.0 * 3.0)
    };
    let ridges = diff(&mapped, &flat);
    let same = diff(&mapped, &builtin);
    let _ = std::fs::remove_dir_all(&d);
    assert!(ridges > 8.0, "the shader drew the wall the same with and without its normal map ({ridges:.2} apart)");
    assert!(same < 2.0, "litPbr differs from the built-in Physical look by {same:.2} a channel");
}

/// Run `shot` on `scene` in `d`; the picture, or `None` on a machine that
/// cannot render.
fn shoot(d: &Path, scene: &str) -> Option<image::RgbaImage> {
    let out = d.join(format!("{scene}.png"));
    let r = Command::new(bin())
        .args(["shot", &d.to_string_lossy(), "--scene", scene, "--size", "160x90", "--out", &out.to_string_lossy()])
        .output()
        .expect("run shot");
    if !r.status.success() {
        let why = String::from_utf8_lossy(&r.stderr);
        assert!(cannot_render(&why), "shot failed for a reason that is not the adapter:\n{why}");
        return None;
    }
    Some(image::open(&out).expect("a PNG was written").to_rgba8())
}

/// A wall of `size` at `z` wearing `shader`, as a scene node.
fn shaded_wall(name: &str, z: f32, size: f32, shader: &str) -> String {
    format!(
        "(name: \"{name}\", transform: (translation: (0.0, 0.0, {z}), rotation: (0.0, 0.0, 0.0, 1.0), \
         scale: ({size}, {size}, 0.2)), matter: Primitive(shape: Cube, color: (1.0, 1.0, 1.0)), scripts: [], \
         material: Some((shader: Some(\"shaders/{shader}.flsl\"))))"
    )
}

fn camera_scene(name: &str, walls: &[String]) -> String {
    format!(
        "(name: \"{name}\", nodes: [(name: \"Camera\", transform: (translation: (0.0, 0.0, 0.0), \
         rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)), matter: Camera(fov_y: 1.0, active: true), \
         scripts: []), {}])",
        walls.join(", ")
    )
}

/// **A material shader can use all eight of its texture slots.** The engine
/// binds sixteen textures of its own to every surface, and the device kept
/// wgpu's default limit of sixteen, so even one `texture` slot failed pipeline
/// validation and the shot stopped. Eight slots beside surfaceGap and
/// fieldDistance now draw: the first slot's green is what shows.
#[test]
fn a_shot_draws_a_material_with_eight_texture_slots() {
    let d = temp("slots");
    scaffold(&d);
    std::fs::create_dir_all(d.join("shaders")).unwrap();
    std::fs::create_dir_all(d.join("art")).unwrap();
    image::RgbaImage::from_pixel(4, 4, image::Rgba([0, 255, 0, 255])).save(d.join("art/green.png")).unwrap();
    image::RgbaImage::from_pixel(4, 4, image::Rgba([0, 0, 0, 255])).save(d.join("art/black.png")).unwrap();
    let names = ["a", "b", "c", "d", "e", "f", "h", "k"];
    let decl: String = names
        .iter()
        .map(|n| format!("  texture {n} = \"art/{}.png\"\n", if *n == "a" { "green" } else { "black" }))
        .collect();
    let sum: Vec<String> = names.iter().map(|n| format!("sample({n}, uv).rgb")).collect();
    std::fs::write(
        d.join("shaders/eight.flsl"),
        format!(
            "shader eight {{\n  stage fragment\n{decl}  let gap = surfaceGap(worldPos) + fieldDistance(worldPos)\n  \
             output color = vec4({} + vec3(gap * 0.0), 1)\n}}\n",
            sum.join(" + ")
        ),
    )
    .unwrap();
    std::fs::write(d.join("scenes/eight.ron"), camera_scene("eight", &[shaded_wall("Wall", -6.0, 8.0, "eight")])).unwrap();
    let Some(img) = shoot(&d, "eight") else { return };
    let p = img.get_pixel(80, 45);
    assert!(p[1] > 180 && p[0] < 60 && p[2] < 60, "the wall is {p:?}, not its first texture's green");
    let _ = std::fs::remove_dir_all(&d);
}

/// **A surface shader sees the scene behind it.** `sceneColor()` on a surface
/// is the scene captured before the surface is drawn, as glass gets: a pane in
/// front of a green wall that shows `sceneColor` with its red and blue taken
/// out shows the wall's green, where drawing it with the rest of the scene
/// would show black, or the pane itself.
#[test]
fn a_surface_shader_shows_the_scene_behind_it() {
    let d = temp("behind");
    scaffold(&d);
    std::fs::create_dir_all(d.join("shaders")).unwrap();
    std::fs::write(d.join("shaders/green.flsl"), "shader green {\n  stage fragment\n  output color = vec4(0.1, 0.9, 0.1, 1)\n}\n").unwrap();
    std::fs::write(
        d.join("shaders/pane.flsl"),
        "shader pane {\n  stage fragment\n  let behind = sceneColor()\n  output color = vec4(behind.g, behind.g * 0.5, 0, 1)\n}\n",
    )
    .unwrap();
    std::fs::write(
        d.join("scenes/behind.ron"),
        camera_scene("behind", &[shaded_wall("Wall", -8.0, 12.0, "green"), shaded_wall("Pane", -4.0, 1.5, "pane")]),
    )
    .unwrap();
    let Some(img) = shoot(&d, "behind") else { return };
    let (pane, wall) = (img.get_pixel(80, 45), img.get_pixel(10, 10));
    assert!(wall[1] > 150, "the wall is {wall:?}");
    // The pane turned the green behind it into orange: red from its green.
    assert!(pane[0] > 120 && pane[1] > 40 && pane[2] < 40, "the pane is {pane:?}: it did not show the wall behind it");
    let _ = std::fs::remove_dir_all(&d);
}
