//! `assets.warm` takes a level's first-use work off its first frame.
//!
//! A frame that first draws an image decodes and uploads it there, and one
//! that first draws a material shader compiles it and builds its pipeline
//! there: a world load that met three shaders at once stalled one frame for
//! over a second. Warmed behind a loading screen, that frame does none of it.

use floptle_core::math::{DVec3, Quat};
use floptle_core::{Material, Matter, Name, ScriptInst, Scripts, Transform};
use floptle_render::{Projection, RenderCamera};

const TEXTURES: usize = 50;
const MATERIALS: usize = 20;
const SHADERS: [&str; 2] = ["toonPrint", "lavaFlow"];

/// The work a first use does, by the names `perf.parts()` gives it.
const FIRST_USE: [&str; 4] = ["assetDecode", "assetUpload", "shaderCompile", "pipelineCompile"];

/// A project with fifty noisy images and two material shaders, and a loader
/// script that warms the lot when `warm` is true.
fn project(tag: &str, warm: bool) -> Option<(crate::Editor, floptle_core::Entity, std::path::PathBuf)> {
    let mut ed = crate::offscreen::test_editor_with_gpu()?;
    let dir = std::env::temp_dir().join(format!("floptle-warm-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("textures")).unwrap();
    std::fs::create_dir_all(dir.join("shaders")).unwrap();
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    // Noise, so each image costs a real decode.
    let mut seed = 0x9e37_79b9u32;
    for i in 0..TEXTURES {
        let mut im = image::RgbaImage::new(256, 256);
        for p in im.pixels_mut() {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let b = seed.to_le_bytes();
            *p = image::Rgba([b[0], b[1], b[2], 255]);
        }
        im.save(dir.join(format!("textures/t{i}.png"))).unwrap();
    }
    for s in SHADERS {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../assets/shaders/examples/{s}.flsl"));
        std::fs::copy(&src, dir.join(format!("shaders/{s}.flsl"))).unwrap();
    }
    let mut list: Vec<String> = (0..TEXTURES).map(|i| format!("\"textures/t{i}.png\"")).collect();
    list.extend(SHADERS.iter().map(|s| format!("\"shaders/{s}.flsl\"")));
    let script = format!(
        "warmed = false\nfunction start(node)\n  if {warm} then\n    assets.warm({{ {} }}, function(failed) warmed = true; failures = #failed end)\n  else\n    warmed = true\n  end\nend\n",
        list.join(", ")
    );
    std::fs::write(dir.join("scripts/loader.lua"), script).unwrap();
    ed.project_root = dir.clone();
    let loader = ed.world.spawn();
    ed.world.insert(loader, Transform::IDENTITY);
    ed.world.insert(loader, Name("Loader".into()));
    ed.world.insert(loader, Matter::Empty);
    ed.world.insert(
        loader,
        Scripts(vec![ScriptInst {
            kind: "loader".into(),
            enabled: true,
            params: Vec::new(),
            refs: Vec::new(),
            strs: Vec::new(),
            pinned_bools: None,
        }]),
    );
    let sun = ed.world.spawn();
    ed.world.insert(sun, Transform { rotation: Quat::from_rotation_x(-0.7), ..Transform::IDENTITY });
    ed.world.insert(sun, floptle_core::Light { intensity: 2.0, ..Default::default() });
    Some((ed, loader, dir))
}

/// The level: twenty boxes, each wearing three of the fifty images, ten of
/// them through a material shader.
fn build_level(ed: &mut crate::Editor) {
    for i in 0..MATERIALS {
        let e = ed.world.spawn();
        let at = DVec3::new((i % 5) as f64 * 2.5 - 5.0, (i / 5) as f64 * 2.5 - 3.75, 0.0);
        ed.world.insert(e, Transform { translation: at, ..Transform::IDENTITY });
        ed.world.insert(e, Matter::Primitive { shape: floptle_core::Shape::Cube, color: [1.0; 3] });
        let tex = |k: usize| Some(format!("textures/t{}.png", k % TEXTURES));
        let mut m = Material {
            texture: tex(i),
            normal_map: tex(i + MATERIALS),
            roughness_map: tex(i + 2 * MATERIALS),
            shading: floptle_core::Shading::Physical,
            ..Default::default()
        };
        if i.is_multiple_of(2) {
            m.shader = Some(format!("shaders/{}.flsl", SHADERS[(i / 2) % 2]));
        }
        ed.world.insert(e, m);
    }
}

/// Play until the loader says it is ready, drawing a frame each time as a
/// loading screen would. Frames taken.
fn load(ed: &mut crate::Editor, loader: floptle_core::Entity, cam: &RenderCamera) -> usize {
    ed.toggle_play();
    for frame in 0..600 {
        ed.play_step(1.0 / 60.0, true);
        let _ = crate::shot::render_frame_pixels(ed, cam, None, 64, 64, !0, false);
        let env = ed.script_host.instance_env(loader.index(), "loader").expect("the loader runs");
        if env.get::<bool>("warmed").unwrap_or(false) {
            assert_eq!(env.get::<Option<i64>>("failures").unwrap().unwrap_or(0), 0, "something failed to warm");
            return frame + 1;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("assets.warm never called back");
}

/// The first frame of the level: the first-use work it did, by part, and the
/// milliseconds of it.
fn first_frame(ed: &mut crate::Editor, cam: &RenderCamera, tag: &str) -> (Vec<(&'static str, f32)>, f32) {
    let profile = ed.script_host.profile().clone();
    profile.borrow_mut().enable(true);
    profile.borrow_mut().end_frame();
    build_level(ed);
    ed.play_step(1.0 / 60.0, true);
    let px = crate::shot::render_frame_pixels(ed, cam, None, 128, 128, !0, false).expect("a frame");
    if let Ok(out) = std::env::var("WARM_PNG") {
        let _ = image::RgbaImage::from_raw(128, 128, px).map(|im| im.save(format!("{out}_{tag}.png")));
    }
    profile.borrow_mut().end_frame();
    let parts: Vec<(&'static str, f32)> =
        profile.borrow().last_parts().iter().filter(|(p, _)| FIRST_USE.contains(p)).copied().collect();
    let ms = parts.iter().map(|(_, ms)| ms).sum();
    (parts, ms)
}

fn camera() -> RenderCamera {
    RenderCamera::new(DVec3::new(0.0, 0.0, 14.0), Quat::IDENTITY, Projection::of_camera(1.0, false, 6.0, 0.05, 1000.0))
}

/// **After `assets.warm` calls back, the level's first frame decodes,
/// uploads, compiles and builds nothing**, where the same level unwarmed does
/// all of it on that frame.
#[test]
fn a_warmed_level_does_no_first_use_work_on_its_first_frame() {
    let cam = camera();
    // The control: the level met cold.
    let Some((mut ed, loader, dir)) = project("cold", false) else { return };
    load(&mut ed, loader, &cam);
    let (cold, cold_ms) = first_frame(&mut ed, &cam, "cold");
    let _ = std::fs::remove_dir_all(&dir);
    let count = |parts: &[(&str, f32)], what: &str| parts.iter().filter(|(p, _)| *p == what).count();
    assert!(
        count(&cold, "assetDecode") == 1 && count(&cold, "pipelineCompile") == 1,
        "the control must do the first-use work it exists to show: {cold:?}"
    );

    let Some((mut ed, loader, dir)) = project("warm", true) else { return };
    let frames = load(&mut ed, loader, &cam);
    let installed = ed.warm.installed as usize;
    let (warm, warm_ms) = first_frame(&mut ed, &cam, "warm");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        warm.is_empty(),
        "the warmed level's first frame still did first-use work: {warm:?} ({warm_ms:.1} ms; cold {cold_ms:.1} ms, warmed in {frames} frames)"
    );
    assert_eq!(installed, TEXTURES + SHADERS.len(), "warming put every image and shader in use");
}

/// **A material shader builds the one pipeline it draws with.** Its skinned
/// variant cost as much again and most shaders never meet a skinned part, so
/// it waits until one draws.
#[test]
fn a_material_shader_builds_only_the_pipeline_it_draws_with() {
    let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
    let dir = std::env::temp_dir().join(format!("floptle-warm-lazy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("shaders")).unwrap();
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/shaders/examples/toonPrint.flsl");
    std::fs::copy(&src, dir.join("shaders/toonPrint.flsl")).unwrap();
    ed.project_root = dir.clone();
    let built = |ed: &crate::Editor| ed.raster.as_ref().expect("a raster pass").flsl_pipelines_built();
    let before = built(&ed);
    ed.ensure_flsl_shader("shaders/toonPrint.flsl");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(ed.flsl_cache.get("shaders/toonPrint.flsl").is_some_and(|e| e.compiled.is_some()), "the fixture shader compiles");
    assert_eq!(built(&ed) - before, 1, "compiling one material shader built {} pipelines", built(&ed) - before);
}
