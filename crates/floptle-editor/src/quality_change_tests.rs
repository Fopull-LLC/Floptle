//! A graphics setting changed while the game runs, as a settings screen or a
//! game's own automatic quality drop changes it, must not stop the game.
//!
//! A drop from one quality level to the next writes many settings in one
//! frame: the texture quality, the render scale, the shadow map's size, how
//! far plants are scattered, the terrain's detail. Each either takes effect
//! in place or is brought in over the frames that follow.

use crate::arrival_tests::{arrival_scene, describe, Frames};
use floptle_core::math::{DVec3, Quat};
use floptle_core::transform::Transform;
use floptle_core::{Material, Matter, Name};

/// Smooth textures the level has loaded, and their side in pixels: enough
/// that reading them all again is a hundred milliseconds and more.
const TEXTURES: usize = 32;
const SIDE: u32 = 1024;

/// What a quality drop writes, by the name the driver script takes.
const CHANGES: [&str; 7] = ["textures", "scale", "shadows", "scatter", "detail", "antialias", "all"];

/// A running game on a generated world: a sun casting shadows, thirty-two
/// boxes each wearing a smooth texture of its own, a scatter field on the
/// ground under the camera, and a driver script that makes the change named
/// in its `change` variable on the frame it is set.
fn quality_scene(tag: &str) -> Option<(crate::Editor, floptle_core::Entity, floptle_core::Entity, std::path::PathBuf)> {
    let radius = 300.0;
    let (mut ed, world, dir) = arrival_scene(tag, radius)?;
    floptle_vfs::create_dir_all(dir.join("textures")).unwrap();
    // Noise, so each costs a real decode; the same picture under every name,
    // which costs each read the same and the test one encode.
    let mut seed = 0x9e37_79b9u32;
    let mut im = image::RgbaImage::new(SIDE, SIDE);
    for p in im.pixels_mut() {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        let b = seed.to_le_bytes();
        *p = image::Rgba([b[0], b[1], b[2], 255]);
    }
    im.save(dir.join("textures/t0.png")).unwrap();
    for i in 0..TEXTURES {
        let name = format!("textures/t{i}.png");
        if i > 0 {
            std::fs::copy(dir.join("textures/t0.png"), dir.join(&name)).unwrap();
        }
        ed.texture_settings.insert(
            name.clone(),
            crate::assets::TexSetting { filter: crate::assets::FilterMode::SmoothMipmaps, ..Default::default() },
        );
        let e = ed.world.spawn();
        let at = DVec3::new((i % 8) as f64 * 3.0 - 10.5, radius + 45.0, (i / 8) as f64 * 3.0 - 4.5);
        ed.world.insert(e, Transform::from_translation(at));
        ed.world.insert(e, Matter::Primitive { shape: floptle_core::Shape::Cube, color: [1.0; 3] });
        ed.world.insert(e, Material { texture: Some(name), ..Default::default() });
    }
    let sun = ed.world.spawn();
    ed.world.insert(sun, Transform { rotation: Quat::from_rotation_x(-0.9), ..Transform::IDENTITY });
    ed.world.insert(sun, Name("Lighting".into()));
    ed.world.insert(sun, Matter::Empty);
    ed.world.insert(sun, floptle_core::Light { intensity: 2.0, shadows: true, ..Default::default() });
    // The driver looks straight down at the boxes and the ground.
    let cam = ed.world.query::<Name>().find(|(_, n)| n.0 == "Camera").map(|(e, _)| e).unwrap();
    if let Some(t) = ed.world.get_mut::<Transform>(cam) {
        t.rotation = Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2);
    }
    let script = format!(
        r#"local field, plant
local function grow(far)
  if field then scatter.destroy(field) end
  field = scatter.create{{ asset = plant, parent = "World", center = {{ x = 0, y = 0, z = 0 }}, radius = {radius},
                          lod = {{ {{ asset = plant, distance = far }} }}, chunk = 16, perChunk = 24, seed = 5 }}
end
local made = {{
  textures = function() app.setTextureQuality("Medium") end,
  scale = function() app.setRenderScale(0.5) end,
  shadows = function()
    local l = find("Lighting"):getcomponent("Light")
    l.shadowMapSize = 1024
    l.shadowDistance = 60
  end,
  scatter = function() grow(60) end,
  detail = function() terrain.setDetail(0.5) end,
  antialias = function() app.setAntialias("Fxaa") end,
}}
function start(node)
  plant = mesh.build{{ positions = {{ -0.2, 0, 0,  0.2, 0, 0,  0, 1.5, 0 }}, indices = {{ 1, 2, 3 }}, scene = true }}
  grow(110)
end
function update(node, dt)
  local c = change
  change = nil
  if c == "all" then
    for _, f in pairs(made) do f() end
  elseif c then
    made[c]()
  end
end
"#
    );
    floptle_vfs::write(dir.join("scripts/driver.lua"), script).unwrap();
    let driver = ed.world.query::<Name>().find(|(_, n)| n.0 == "Driver").map(|(e, _)| e).unwrap();
    Some((ed, world, driver, dir))
}

/// Play until the world has landed and the level has drawn for a while, so
/// what follows measures the change and not the arrival.
fn settle_in(ed: &mut crate::Editor, world: floptle_core::Entity, frames: &Frames) {
    ed.toggle_play();
    assert!(ed.playing, "did not enter play");
    ed.script_host.profile().borrow_mut().enable(true);
    let until = std::time::Instant::now() + std::time::Duration::from_secs(300);
    let mut after = 0;
    while after < 240 {
        assert!(std::time::Instant::now() < until, "the world never landed");
        frames.frame(ed);
        if ed.terrains.contains_key(&world) {
            after += 1;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
}

/// Tell the driver to make `change` in its next `update`.
fn ask(ed: &mut crate::Editor, driver: floptle_core::Entity, change: &str) {
    ed.script_host.instance_env(driver.index(), "driver").expect("the driver runs").set("change", change).unwrap();
}

/// **Probe**: each setting a quality drop writes, changed alone and then all
/// together, with the frame it changed on and the worst frame after it.
/// `FLOPTLE_QUALITY_CHANGE` names one change to make instead of every one.
#[test]
#[ignore = "probe: prints what each runtime quality change costs"]
fn probe_quality_changes() {
    let only = std::env::var("FLOPTLE_QUALITY_CHANGE").ok();
    for change in CHANGES.into_iter().filter(|c| only.as_deref().is_none_or(|o| o == *c)) {
        let Some((mut ed, world, driver, dir)) = quality_scene(&format!("probe_{change}")) else { return };
        let frames = Frames::new(&ed);
        settle_in(&mut ed, world, &frames);
        let quiet = (0..30).map(|_| frames.frame(&mut ed)).fold(0.0f64, f64::max);
        ask(&mut ed, driver, change);
        let mut worst = (0.0f64, 0usize, String::new());
        for f in 0..180 {
            let ms = frames.frame(&mut ed);
            let p = ed.script_host.profile().borrow();
            let r = p.recent_frames(1).next().unwrap();
            if f == 0 {
                floptle_say::say_err!("{change}: quiet worst {quiet:.1} ms; the change frame {ms:.1} ms: {}", describe(r));
            }
            if ms > worst.0 {
                worst = (ms, f, describe(r));
            }
        }
        assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
        floptle_say::say_err!("{change}: worst frame after, {:.1} ms at +{}: {}", worst.0, worst.1, worst.2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// How wide each of the level's textures is on the GPU now.
fn sides(ed: &crate::Editor) -> Vec<u32> {
    let raster = ed.raster.as_ref().unwrap();
    (0..TEXTURES)
        .map(|i| {
            let id = ed.texture_registry.get(&format!("textures/t{i}.png")).copied().expect("the level's textures are loaded");
            raster.texture_size(id).expect("a registered texture")[0] as u32
        })
        .collect()
}

/// What the last frame spent outside drawing, in ms: every bucket but the
/// render and the wait for the GPU, less the time before the frame began.
/// The work a settings change does on the frame lands here, and a slow or
/// shared GPU does not.
fn spent_outside_drawing(ed: &crate::Editor) -> (f64, String) {
    use floptle_core::profile::Bucket;
    let p = ed.script_host.profile().borrow();
    let r = p.recent_frames(1).next().expect("a frame");
    let all: f32 = r.buckets.iter().filter(|(b, _)| !matches!(b, Bucket::Render | Bucket::GpuWait)).map(|(_, ms)| ms).sum();
    let outside = r.parts.iter().find(|(n, _)| *n == "outsideFrame").map_or(0.0, |(_, ms)| *ms);
    (f64::from((all - outside).max(0.0)), describe(r))
}

/// **A quality drop made while the game runs does not stop a frame.** One
/// `update` writes everything a drop from one level to the next writes: the
/// texture quality, the render scale, the shadow map's size and reach, the
/// scatter field declared again nearer, the terrain's detail, antialiasing.
/// Reading every loaded texture again on that frame was seconds on a game
/// with a few hundred of them. Here:
/// - the frame the settings change on brings few of the textures to the new
///   quality itself, and the rest follow over the frames after it;
/// - every texture reaches the new quality, the same size a load at that
///   quality gives it;
/// - no frame of the change spends, outside drawing, a large share of what
///   reading every texture at once costs, timed on this machine in this
///   run, so the machine's speed cancels.
#[test]
fn a_quality_drop_is_spread_over_frames() {
    let Some((mut ed, world, driver, dir)) = quality_scene("guard") else { return };
    let frames = Frames::new(&ed);
    settle_in(&mut ed, world, &frames);
    assert!(sides(&ed).iter().all(|s| *s == SIDE), "the level's textures are not loaded whole: {:?}", sides(&ed));
    ask(&mut ed, driver, "all");
    let mut worst = (0.0f64, String::new());
    let mut frame = |ed: &mut crate::Editor| {
        frames.frame(ed);
        let spent = spent_outside_drawing(ed);
        if spent.0 > worst.0 {
            worst = spent;
        }
    };
    frame(&mut ed);
    assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
    assert_eq!(ed.raster.as_ref().unwrap().texture_quality(), 1, "the script's texture quality did not reach the renderer");
    let cut = sides(&ed).iter().filter(|s| **s != SIDE).count();
    assert!(cut * 2 < TEXTURES, "the frame the quality changed on read {cut} of {TEXTURES} textures again itself");
    let mut taken = 1;
    while sides(&ed).iter().any(|s| *s != SIDE / 2) {
        assert!(taken < 3000, "the textures never reached the new quality: {:?}", sides(&ed));
        frame(&mut ed);
        taken += 1;
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    // The rest of the change (the field declared again settling, the ground
    // meshing at its new detail) plays out too.
    for _ in 0..120 {
        frame(&mut ed);
    }
    // What the same textures cost read again all at once, as a change of
    // quality did before: back to whole, here and now.
    let t = std::time::Instant::now();
    ed.set_texture_quality_now(0);
    let at_once = t.elapsed().as_secs_f64() * 1e3;
    assert!(sides(&ed).iter().all(|s| *s == SIDE), "the textures did not come back whole: {:?}", sides(&ed));
    assert!(
        worst.0 < at_once * 0.5,
        "a frame of the quality change spent {:.1} ms outside drawing, where reading every texture again at once takes \
         {at_once:.1} ms: {}",
        worst.0,
        worst.1
    );
    let _ = std::fs::remove_dir_all(&dir);
}
