//! **A screen shader reads a picture and a list, and knows when the camera is
//! under water, whether or not the water paints the engine's own look.**
//!
//! Shot through the real `floptle shot`, so every seam is in the picture: the
//! `.flsl` texture slot and its default image, the scene's own override, the
//! uniform array a script fills with `node:setShaderParam`, a slot a script
//! re-points with `node:setShaderTexture`, and `sceneWaterDepth()` from a
//! Water Volume with `underwater: false`.
//!
//! Set `FLOPTLE_KEEP_SHOTS=<dir>` to keep the PNGs and look at them.

// The `floptle` binary needs the authoring half; see the note at the top of
// `the_json_verbs_emit_only_json.rs`.
#![cfg(feature = "editor-ui")]

use std::path::{Path, PathBuf};
use std::process::Command;

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("flscreentex-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    for sub in ["scenes", "scripts", "shaders", "textures"] {
        std::fs::create_dir_all(d.join(sub)).unwrap();
    }
    std::fs::write(d.join("project.ron"), "(title: Some(\"p\"), entry_scene: Some(\"scenes/first.ron\"))").unwrap();
    d
}

/// Shoot the project; `None` when this machine has nothing to render on.
fn shoot(d: &Path, keep_as: &str) -> Option<image::RgbImage> {
    let out = d.join("shot.png");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_floptle"));
    cmd.args(["shot", &d.to_string_lossy(), "--size", "160x90", "--out", &out.to_string_lossy(), "--after", "0.2"]);
    let run = cmd.output().expect("run floptle shot");
    if !run.status.success() {
        let why = String::from_utf8_lossy(&run.stderr).into_owned();
        let cannot = why.contains("no GPU") || why.contains("could not build the renderer");
        assert!(cannot, "shot failed for a reason that is not the adapter:\n{why}");
        eprintln!("skipped — this machine cannot render:\n{why}");
        return None;
    }
    if let Ok(keep) = std::env::var("FLOPTLE_KEEP_SHOTS") {
        std::fs::create_dir_all(&keep).ok();
        std::fs::copy(&out, Path::new(&keep).join(format!("{keep_as}.png"))).ok();
    }
    Some(image::open(&out).expect("the shot is a PNG").to_rgb8())
}

/// A 2×2 picture: one flat colour a quarter, so a quarter's middle is one
/// texel's middle whatever the filter.
fn quad_png(path: &Path, texels: [[u8; 3]; 4]) {
    let mut img = image::RgbImage::new(2, 2);
    for (i, t) in texels.iter().enumerate() {
        img.put_pixel(i as u32 % 2, i as u32 / 2, image::Rgb(*t));
    }
    img.save(path).unwrap();
}

const RED: [u8; 3] = [255, 0, 0];
const GREEN: [u8; 3] = [0, 255, 0];
const BLUE: [u8; 3] = [0, 0, 255];
const WHITE: [u8; 3] = [255, 255, 255];

fn is(p: &image::Rgb<u8>, want: [u8; 3]) -> bool {
    (0..3).all(|c| (p.0[c] as i32 - want[c] as i32).abs() < 40)
}

/// The pass shows its slot's picture over the whole frame, with a black disc
/// wherever an element of `marks` puts one.
const PROBE: &str = "shader probe {\n  stage post\n  texture pattern = \"textures/quad.png\"\n  \
    uniform marks: vec4[4]\n  \
    let a = step(length(uv - marks[0].xy), marks[0].z)\n  \
    let b = step(length(uv - marks[3].xy), marks[3].z)\n  \
    output color = sample(pattern, uv).rgb * (1 - saturate(a + b)) + sceneColor().rgb * 0\n}\n";

fn scene(d: &Path, pass: &str, script: &str) {
    std::fs::write(
        d.join("scenes/first.ron"),
        format!(
            "(name: \"s\", nodes: [\
             (name: \"Cam\", matter: Camera(fov_y: 1.0, active: true)),\
             (name: \"Post\", {script} matter: PostProcess(screen_shaders: [{pass}]))])"
        ),
    )
    .unwrap();
}

/// The quarter middles of a 160×90 shot, in the picture's texel order.
const QUARTERS: [(u32, u32); 4] = [(40, 22), (120, 22), (40, 67), (120, 67)];

/// A project with the probe shader and two pictures for its slot.
fn slot_project(name: &str) -> PathBuf {
    let d = temp(name);
    quad_png(&d.join("textures/quad.png"), [RED, GREEN, BLUE, WHITE]);
    quad_png(&d.join("textures/other.png"), [WHITE, BLUE, GREEN, RED]);
    std::fs::write(d.join("shaders/probe.flsl"), PROBE).unwrap();
    d
}

/// The image the shader itself declares is what the slot shows.
#[test]
fn a_screen_shader_samples_the_image_its_slot_declares() {
    let d = slot_project("slot");
    scene(&d, "(shader: \"shaders/probe.flsl\")", "");
    let Some(img) = shoot(&d, "slot_default") else { return };
    for ((x, y), want) in QUARTERS.iter().zip([RED, GREEN, BLUE, WHITE]) {
        let p = img.get_pixel(*x, *y);
        assert!(is(p, want), "the slot's own picture is not on screen at ({x}, {y}): {p:?}, wanted {want:?}");
    }

    let _ = std::fs::remove_dir_all(&d);
}

/// The scene's pass points the slot somewhere else.
#[test]
fn a_scene_points_a_screen_shaders_slot_at_another_image() {
    let d = slot_project("slot-scene");
    scene(&d, "(shader: \"shaders/probe.flsl\", textures: {\"pattern\": \"textures/other.png\"})", "");
    let Some(img) = shoot(&d, "slot_scene_override") else { return };
    for ((x, y), want) in QUARTERS.iter().zip([WHITE, BLUE, GREEN, RED]) {
        let p = img.get_pixel(*x, *y);
        assert!(is(p, want), "the pass's own image did not replace the shader's at ({x}, {y}): {p:?}");
    }

    let _ = std::fs::remove_dir_all(&d);
}

/// A script does, by the pass's name.
#[test]
fn a_script_points_a_screen_shaders_slot_at_another_image() {
    let d = slot_project("slot-script");
    std::fs::write(
        d.join("scripts/swap.lua"),
        "function start(node)\n  node:setShaderTexture(\"probe.pattern\", \"textures/other.png\")\nend\n",
    )
    .unwrap();
    scene(&d, "(shader: \"shaders/probe.flsl\")", "scripts: [(kind: \"swap\")],");
    let Some(img) = shoot(&d, "slot_script_override") else { return };
    let p = img.get_pixel(40, 22);
    assert!(is(p, WHITE), "node:setShaderTexture did not reach the screen shader's slot: {p:?}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_script_fills_a_screen_shaders_uniform_array() {
    let d = temp("array");
    quad_png(&d.join("textures/quad.png"), [WHITE, WHITE, WHITE, WHITE]);
    std::fs::write(d.join("shaders/probe.flsl"), PROBE).unwrap();
    // Element 0 puts a disc in the top-left quarter and element 3 one in the
    // bottom-right; the two between are left out of the shader on purpose.
    std::fs::write(
        d.join("scripts/marks.lua"),
        "function update(node)\n  node:setShaderParam(\"probe.marks\", { {0.25, 0.25, 0.1}, {0, 0, 0}, {0, 0, 0}, {0.75, 0.75, 0.1} })\nend\n",
    )
    .unwrap();
    scene(&d, "(shader: \"shaders/probe.flsl\")", "scripts: [(kind: \"marks\")],");
    let Some(img) = shoot(&d, "array_marks") else { return };
    let black = [0, 0, 0];
    assert!(is(img.get_pixel(40, 22), black), "element 0's disc is missing: {:?}", img.get_pixel(40, 22));
    assert!(is(img.get_pixel(120, 67), black), "element 3's disc is missing: {:?}", img.get_pixel(120, 67));
    assert!(is(img.get_pixel(120, 22), WHITE), "a disc nobody asked for: {:?}", img.get_pixel(120, 22));
    assert!(is(img.get_pixel(40, 67), WHITE), "a disc nobody asked for: {:?}", img.get_pixel(40, 67));

    // With no script the array is zeros: discs of no size, a clean frame.
    scene(&d, "(shader: \"shaders/probe.flsl\")", "");
    let img = shoot(&d, "array_empty").expect("it rendered a moment ago");
    assert!(is(img.get_pixel(40, 22), WHITE), "an unfilled array drew something: {:?}", img.get_pixel(40, 22));
    let _ = std::fs::remove_dir_all(&d);
}

/// A camera 3 m under the surface of a pool, looking at a white lit-less wall
/// two metres off. With the pool's look on the wall is lost in the pool's red
/// fog; with it off the wall is white, and a screen shader still reads the
/// 3 m and can draw its own.
#[test]
fn a_water_volume_with_its_look_off_leaves_the_view_alone_and_still_reports_depth() {
    let d = temp("water");
    // Green where the camera is 2.5 to 3.5 m under, blue where it is in air.
    std::fs::write(
        d.join("shaders/depth.flsl"),
        "shader depth {\n  stage post\n  let w = sceneWaterDepth()\n  \
         let under = step(2.5, w) * step(w, 3.5)\n  \
         output color = mix(mix(sceneColor().rgb, vec3(0, 1, 0), under), vec3(0, 0, 1), step(w, 0.001))\n}\n",
    )
    .unwrap();
    let scene = |cam_y: f32, underwater: &str, post: &str| {
        std::fs::write(
            d.join("scenes/first.ron"),
            format!(
                "(name: \"s\", nodes: [\
                 (name: \"Cam\", transform: (translation: (0.0, {cam_y}, 0.0)), matter: Camera(fov_y: 1.0, active: true)),\
                 (name: \"Wall\", transform: (translation: (0.0, {cam_y}, -2.0), scale: (8.0, 8.0, 0.2)), \
                  matter: Primitive(shape: Cube, color: (1.0, 1.0, 1.0)), material: Some((color: (1.0, 1.0, 1.0), unlit: true))),\
                 (name: \"Pool\", transform: (translation: (0.0, 0.0, 0.0)), \
                  matter: WaterVolume(pool: true, half_extents: (6.0, 5.0, 6.0), tint: (0.9, 0.0, 0.0), visibility: 1.0{underwater})){post}])"
            ),
        )
        .unwrap();
    };
    let centre = |img: &image::RgbImage| *img.get_pixel(80, 45);

    scene(2.0, "", "");
    let Some(on) = shoot(&d, "water_look_on") else { return };
    let c = centre(&on).0;
    assert!(c[0] > 150 && c[1] < 80 && c[2] < 80, "an older scene's pool lost its underwater fog: {c:?}");

    scene(2.0, ", underwater: false", "");
    let off = shoot(&d, "water_look_off").expect("it rendered a moment ago");
    let c = centre(&off).0;
    assert!(c[0] > 200 && c[1] > 200 && c[2] > 200, "a pool with its look off still fogged the view: {c:?}");

    let post = ", (name: \"Post\", matter: PostProcess(screen_shaders: [(shader: \"shaders/depth.flsl\")]))";
    scene(2.0, ", underwater: false", post);
    let own = shoot(&d, "water_look_off_own_pass").expect("it rendered a moment ago");
    let c = centre(&own).0;
    assert!(c[1] > 200 && c[0] < 60 && c[2] < 60, "sceneWaterDepth() is not 3 m under a look-off pool: {c:?}");

    // Above the surface the same pass reads air.
    scene(7.0, ", underwater: false", post);
    let air = shoot(&d, "water_in_air").expect("it rendered a moment ago");
    let c = centre(&air).0;
    assert!(c[2] > 200 && c[0] < 60 && c[1] < 60, "sceneWaterDepth() is not 0 in air: {c:?}");
    let _ = std::fs::remove_dir_all(&d);
}
