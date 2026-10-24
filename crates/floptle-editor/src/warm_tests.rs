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
    project_sized(tag, warm, 256)
}

/// [`project`] with images `side` pixels square.
fn project_sized(tag: &str, warm: bool, side: u32) -> Option<(crate::Editor, floptle_core::Entity, std::path::PathBuf)> {
    let mut ed = crate::offscreen::test_editor_with_gpu()?;
    let dir = std::env::temp_dir().join(format!("floptle-warm-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("textures")).unwrap();
    std::fs::create_dir_all(dir.join("shaders")).unwrap();
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    // Noise, so each image costs a real decode.
    let mut seed = 0x9e37_79b9u32;
    for i in 0..TEXTURES {
        let mut im = image::RgbaImage::new(side, side);
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

/// The first frame of play: the loader's `start` asks for the warm, and the
/// pump begins it.
fn begin_warm(ed: &mut crate::Editor) {
    ed.toggle_play();
    ed.play_step(1.0 / 60.0, true);
}

/// Pump frames until every warm job is in, drawing each as a loading screen
/// would.
fn finish_warm(ed: &mut crate::Editor, cam: &RenderCamera) {
    for _ in 0..2000 {
        ed.play_step(1.0 / 60.0, true);
        let _ = crate::shot::render_frame_pixels(ed, cam, None, 64, 64, !0, false);
        if !ed.warm.busy() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("the warm never finished");
}

/// **The warm runs on a few workers, not a thread per asset, and builds at
/// most two shaders at a time.** A loading screen warming a hundred and fifty
/// pictures and four shaders started a hundred and fifty-four threads and
/// stalled its own frame a third of a second; with four pipeline builds
/// running at once the game's frame waited on the driver for every one, and
/// with one at a time Solar's four took twice as long.
#[test]
fn warming_runs_on_a_few_workers_and_builds_two_shaders_at_a_time() {
    let cam = camera();
    let Some((mut ed, loader, dir)) = project("pool", true) else { return };
    load(&mut ed, loader, &cam);
    let _ = std::fs::remove_dir_all(&dir);
    let (images, shaders) = (&ed.warm.images, &ed.warm.shaders);
    assert!(
        images.peak() <= 3 && images.started() < TEXTURES / 4,
        "{TEXTURES} images decoded on {} threads, {} at once",
        images.started(),
        images.peak()
    );
    assert_eq!(shaders.peak(), 2, "{} shaders built at once, of the two warmed together", shaders.peak());
    assert_eq!(ed.warm.installed as usize, TEXTURES + SHADERS.len(), "warming put every image and shader in use");
}

/// **A first use of an asset still warming waits for that warm instead of
/// preparing it again.** A level drawn while its images and shaders were
/// still on the workers decoded the images a second time and built the
/// shaders a second time beside the warm, and the frame paid for all of it.
#[test]
fn a_first_use_while_warming_waits_for_the_warm_instead_of_preparing_it_again() {
    let cam = camera();
    // Images large enough that fifty of them are still decoding a frame later.
    let Some((mut ed, _loader, dir)) = project_sized("adopt", true, 768) else { return };
    let built = |ed: &crate::Editor| ed.raster.as_ref().expect("a raster pass").flsl_pipelines_built();
    let before = built(&ed);
    begin_warm(&mut ed);
    assert!(
        ed.warm.shaders_pending() && ed.warm.images_pending() > TEXTURES / 2,
        "the fixture's images and shaders must still be warming when the level draws ({} images)",
        ed.warm.images_pending()
    );
    // Every one of the fifty images, and both shaders, drawn at once.
    build_level(&mut ed);
    ed.play_step(1.0 / 60.0, true);
    let _ = crate::shot::render_frame_pixels(&mut ed, &cam, None, 64, 64, !0, false);
    finish_warm(&mut ed, &cam);
    let _ = std::fs::remove_dir_all(&dir);
    let decodes = ed.warm.decodes.load(std::sync::atomic::Ordering::Relaxed) as usize;
    assert_eq!(decodes, TEXTURES, "{TEXTURES} images decoded {decodes} times");
    assert_eq!(built(&ed) - before, SHADERS.len() as u64, "{} shaders built {} pipelines", SHADERS.len(), built(&ed) - before);
}

/// **Shaders that finish together land one a frame, and the landing says so**
/// (`shaderInstall`). Each landing has every material wearing that shader
/// bound afresh, and a loaded scene that took all of it on one frame spent
/// about a second there under no name at all.
#[test]
fn warmed_shaders_land_one_a_frame_under_a_name() {
    let Some((mut ed, _loader, dir)) = project("land", false) else { return };
    for s in SHADERS {
        ed.request_warm(&format!("shaders/{s}.flsl"), floptle_script::PreloadKind::Shader);
    }
    for _ in 0..12_000 {
        if ed.warm.shaders.idle() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(ed.warm.shaders.idle(), "the shaders never built");
    let profile = ed.script_host.profile().clone();
    profile.borrow_mut().enable(true);
    profile.borrow_mut().end_frame();
    let mut landed = Vec::new();
    for _ in 0..SHADERS.len() {
        let was = ed.warm.installed;
        ed.pump_warm();
        profile.borrow_mut().end_frame();
        let named = profile.borrow().last_parts().iter().any(|(p, _)| *p == "shaderInstall");
        landed.push((ed.warm.installed - was, named));
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(landed, vec![(1, true); SHADERS.len()], "(shaders landed, named) per frame");
}

/// **UI and screen shaders warm through the same list.** `assets.warm`
/// refused a `stage ui` shader, so each HUD shader built on the frame it
/// first drew.
#[test]
fn ui_and_screen_shaders_can_be_warmed() {
    let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
    let dir = std::env::temp_dir().join(format!("floptle-warm-ui-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("shaders")).unwrap();
    let examples = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/shaders/examples");
    std::fs::copy(examples.join("ui/ui_glow.flsl"), dir.join("shaders/glow.flsl")).unwrap();
    std::fs::copy(examples.join("crtScanlines.flsl"), dir.join("shaders/crt.flsl")).unwrap();
    ed.project_root = dir.clone();
    let (ui, post) = ("shaders/glow.flsl", "shaders/crt.flsl");
    ed.request_warm(ui, floptle_script::PreloadKind::Shader);
    ed.request_warm(post, floptle_script::PreloadKind::Shader);
    for _ in 0..12_000 {
        ed.pump_warm();
        if !ed.warm.shaders_pending() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(ed.warm.failed().is_empty(), "{:?}", ed.warm.failed());
    assert_eq!(ed.warm.installed, 2, "both shaders put in use by the warm");
    // A HUD element and the scene's screen pass wearing them build nothing.
    let e = ed.world.spawn();
    ed.world.insert(e, floptle_ui::ElementSpec { shader: ui.into(), ..Default::default() });
    let profile = ed.script_host.profile().clone();
    profile.borrow_mut().enable(true);
    profile.borrow_mut().end_frame();
    ed.ensure_ui_shaders();
    profile.borrow_mut().end_frame();
    let _ = std::fs::remove_dir_all(&dir);
    let work: Vec<_> =
        profile.borrow().last_parts().iter().filter(|(p, _)| FIRST_USE.contains(p)).copied().collect();
    assert!(work.is_empty(), "the warmed ui shader built again on first use: {work:?}");
    assert!(ed.ui_flsl_cache.get(ui).is_some_and(|c| c.compiled.is_some()), "the ui shader is in use");
    assert!(ed.post_flsl_cache.get(post).is_some_and(|c| c.compiled.is_some()), "the screen shader is in use");
}

/// **A full path to a file in the project warms the file a scene names.**
/// `assets.getContents` answered full paths, and a picture warmed by one was
/// decoded and uploaded under that second name while the scene, naming it
/// the short way, decoded it again.
#[test]
fn a_full_path_warms_the_file_the_scene_names() {
    let Some((mut ed, _loader, dir)) = project("fullpath", false) else { return };
    let full = dir.join("textures/t0.png").to_string_lossy().to_string();
    ed.request_warm(&full, floptle_script::PreloadKind::Texture);
    for _ in 0..2000 {
        ed.pump_warm();
        if !ed.warm.busy() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(ed.texture_registry.contains_key("textures/t0.png"), "warmed under {:?}", ed.texture_registry.keys().collect::<Vec<_>>());
    ed.ensure_texture("textures/t0.png");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(ed.warm.decodes.load(std::sync::atomic::Ordering::Relaxed), 1, "the picture was decoded twice");
}


/// **A frame that needs a shader still warming is not made to build it.**
/// Waited for, or built on the frame when no worker had begun it, a game that
/// drew before its warm finished built its ocean shader there: an 880 ms
/// frame. The frame now goes on, the material drawn without its shader, and
/// the shader, moved to the front of the warm, lands on a later frame.
#[test]
fn a_first_use_of_a_shader_still_warming_is_not_built_on_the_frame() {
    let Some((mut ed, _loader, dir)) = project("hurry", false) else { return };
    for s in SHADERS {
        ed.request_warm(&format!("shaders/{s}.flsl"), floptle_script::PreloadKind::Shader);
    }
    let rel = format!("shaders/{}.flsl", SHADERS[1]);
    let t = std::time::Instant::now();
    ed.ensure_flsl_shader(&rel);
    let first_use_ms = t.elapsed().as_secs_f64() * 1e3;
    let built_here = ed.flsl_cache.get(&rel).is_some_and(|e| e.compiled.is_some());
    let still_warming = ed.warm.shaders_pending();
    let until = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while ed.warm.shaders_pending() {
        assert!(std::time::Instant::now() < until, "the shaders never landed");
        std::thread::sleep(std::time::Duration::from_millis(5));
        ed.pump_warm();
    }
    let _ = std::fs::remove_dir_all(&dir);
    floptle_say::say_err!("first use of a shader still warming: {first_use_ms:.2} ms");
    assert!(still_warming && !built_here, "the frame built the shader it needed instead of leaving it to the warm");
    assert!(ed.flsl_cache.get(&rel).is_some_and(|e| e.compiled.is_some()), "the shader never landed from the warm");
}

/// **A HUD shader a scene puts on screen is built off the frame.** Built on
/// the frame the scene loaded (Solar's navball 30 ms, its tape 18 ms), it
/// could only be kept off it by warming it from a scene before. In the game
/// the frame hands it to the warm's workers and draws the element without it;
/// it lands a frame or two later, and the element wears it.
#[test]
fn a_hud_shader_a_scene_puts_on_screen_is_built_off_the_frame() {
    let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
    let dir = std::env::temp_dir().join(format!("floptle-warm-hud-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("shaders")).unwrap();
    let examples = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/shaders/examples");
    std::fs::copy(examples.join("ui/ui_glow.flsl"), dir.join("shaders/glow.flsl")).unwrap();
    ed.project_root = dir.clone();
    let ui = "shaders/glow.flsl";
    let e = ed.world.spawn();
    ed.world.insert(e, floptle_ui::ElementSpec { shader: ui.into(), ..Default::default() });
    ed.toggle_play();
    let profile = ed.script_host.profile().clone();
    profile.borrow_mut().enable(true);
    profile.borrow_mut().end_frame();
    ed.ensure_ui_shaders();
    profile.borrow_mut().end_frame();
    let work: Vec<_> = profile.borrow().last_parts().iter().filter(|(p, _)| FIRST_USE.contains(p)).copied().collect();
    let in_use_at_once = ed.ui_flsl_cache.get(ui).is_some_and(|c| c.compiled.is_some());
    let until = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while ed.warm.shaders_pending() {
        assert!(std::time::Instant::now() < until, "the shader never landed");
        std::thread::sleep(std::time::Duration::from_millis(5));
        ed.pump_warm();
    }
    ed.ensure_ui_shaders();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(work.is_empty() && !in_use_at_once, "the HUD shader was built on the frame its scene drew it: {work:?}");
    assert!(ed.warm.failed().is_empty(), "{:?}", ed.warm.failed());
    assert!(ed.ui_flsl_cache.get(ui).is_some_and(|c| c.compiled.is_some()), "the HUD shader never came into use");
    assert!(ed.ui_flsl_binds.contains_key(&e.index()), "the element does not wear it");
}
