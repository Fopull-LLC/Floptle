//! **A billboard draws its texture, alpha and all.**
//!
//! A billboard whose texture never reached the GPU draws the white 1x1 in its
//! place: a tinted square with hard edges where a soft dot was meant. These
//! render a round, soft-edged dot on a dark wall, fired from a script and
//! placed in a scene, and look at the pixels.

use floptle_core::math::{DVec3, Vec3};
use floptle_core::{Matter, Name, ParticleSystem, ScriptInst, Scripts, Transform};
use floptle_scene::{VfxBlendDoc, VfxPlaybackDoc, VfxPropDoc, VfxRenderDoc, VfxShapeDoc, VfxSpaceDoc, VfxValueDoc};

const W: u32 = 160;
const H: u32 = 120;
/// How big the dot is, in metres, and how far in front of the camera.
const SIZE: f32 = 1.6;
const AHEAD: f32 = 5.0;

/// A white disc, opaque inside half the radius and fading to nothing at
/// 0.8 of it: alpha 0 at every corner and along every border.
fn write_dot(path: &std::path::Path) {
    let n = 64u32;
    let img = image::RgbaImage::from_fn(n, n, |x, y| {
        let c = (n as f32 - 1.0) / 2.0;
        let r = ((x as f32 - c).hypot(y as f32 - c)) / c;
        let a = ((0.8 - r) / 0.3).clamp(0.0, 1.0);
        image::Rgba([255, 255, 255, (a * 255.0).round() as u8])
    });
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    img.save(path).unwrap();
}

/// One still, opaque white billboard wearing `texture`, alive for ten seconds.
fn dot_effect(name: &str, texture: &str) -> floptle_scene::VfxEffectDoc {
    let mut doc = crate::vfx::starter_effect_doc(name);
    doc.lifetime = 10.0;
    doc.playback = VfxPlaybackDoc::OneShot;
    let t = &mut doc.tracks[0];
    t.name = "Drop".into();
    t.render = VfxRenderDoc::Billboard { texture: Some(texture.into()) };
    t.blend = VfxBlendDoc::Alpha;
    t.space = VfxSpaceDoc::World;
    t.shape = VfxShapeDoc::Point;
    t.soft = 0.0;
    t.velocity = VfxPropDoc::Const(VfxValueDoc::Vec3([0.0, 0.0, 0.0]));
    t.size = VfxPropDoc::Const(VfxValueDoc::F32(SIZE));
    t.color = VfxPropDoc::Const(VfxValueDoc::Rgba([1.0, 1.0, 1.0, 1.0]));
    t.gravity = 0.0;
    t.clips = vec![floptle_scene::VfxClipDoc {
        start: 0.0,
        end: 10.0,
        lifetime_jitter: 0.0,
        emit: Some(floptle_scene::VfxEmitDoc::Burst {
            count: 1,
            count_jitter: 0.0,
            pulses: 1,
            interval: 0.0,
            interval_jitter: 0.0,
        }),
    }];
    doc
}

/// A project with the dot texture and its effect, an editor on the GPU
/// pointed at it, a black wall behind where the dot will be, and the point
/// the dot goes. `None` where this machine cannot render.
fn stage(tag: &str, texture_on_disk: bool) -> Option<(crate::Editor, std::path::PathBuf, DVec3)> {
    let mut ed = crate::offscreen::test_editor_with_gpu()?;
    let dir = std::env::temp_dir().join(format!("floptle-billboard-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    if texture_on_disk {
        write_dot(&dir.join("textures/fx/dot.png"));
    }
    floptle_scene::save_vfx_effect(&dot_effect("Dot", "textures/fx/dot.png"), &dir.join("vfx/Dot.vfx.ron")).unwrap();
    ed.project_root = dir.clone();
    // Smooth with mipmaps, as a soft particle sprite is usually imported.
    ed.texture_settings.insert(
        "textures/fx/dot.png".into(),
        crate::assets::TexSetting { filter: crate::assets::FilterMode::SmoothMipmaps, ..Default::default() },
    );
    ed.vfx.rescan(&dir);
    let cam = ed.camera.render_camera();
    let ahead = |d: f32| cam.world_position + (cam.rotation * Vec3::new(0.0, 0.0, -d)).as_dvec3();
    let wall = ed.world.spawn();
    ed.world.insert(
        wall,
        Transform { translation: ahead(AHEAD + 4.0), rotation: cam.rotation, scale: Vec3::new(40.0, 40.0, 0.2) },
    );
    ed.world.insert(wall, Matter::Primitive { shape: floptle_core::Shape::Cube, color: [0.0; 3] });
    Some((ed, dir, ahead(AHEAD)))
}

/// A node at `at` running `script`.
fn scripted(ed: &mut crate::Editor, at: DVec3, script: &str) {
    std::fs::write(ed.project_root.join("scripts/fx.lua"), script).unwrap();
    let e = ed.world.spawn();
    ed.world.insert(e, Transform { translation: at, ..Transform::IDENTITY });
    ed.world.insert(e, Name("Fx".into()));
    ed.world.insert(e, Matter::Empty);
    ed.world.insert(e, Scripts(vec![ScriptInst::new("fx")]));
}

/// The frame through the editor camera, the way `camera.capture` and `shot`
/// draw one, saved for a look as `floptle-billboard-<project>.png` in the
/// temp folder (the project itself is removed after).
fn capture(ed: &mut crate::Editor, dir: &std::path::Path) -> Vec<u8> {
    let cam = ed.camera.render_camera();
    let px = crate::shot::render_frame_pixels(ed, &cam, None, W, H, !0, false).expect("a frame");
    let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let look = std::env::temp_dir().join(format!("{name}.png"));
    let _ = image::RgbaImage::from_raw(W, H, px.clone()).map(|i| i.save(look));
    px
}

/// The brightness (sum of RGB) at a point on the dot's quad, `(u, v)` from
/// its centre in units of its side, averaged over a 3x3 patch.
fn lum_at(ed: &crate::Editor, px: &[u8], at: DVec3, u: f32, v: f32) -> f64 {
    let cam = ed.camera.render_camera();
    let p = at + (cam.rotation * Vec3::new(u * SIZE, v * SIZE, 0.0)).as_dvec3();
    let vp = cam.view_proj(W as f32 / H as f32);
    let s = crate::viz::project(p, cam.world_position, vp, W as f32, H as f32).expect("on screen");
    let (x, y) = (s.x.round() as i64, s.y.round() as i64);
    let (mut sum, mut n) = (0.0, 0.0);
    for dy in -1..=1 {
        for dx in -1..=1 {
            let (sx, sy) = ((x + dx).clamp(0, W as i64 - 1) as u32, (y + dy).clamp(0, H as i64 - 1) as u32);
            let i = ((sy * W + sx) * 4) as usize;
            sum += px[i] as f64 + px[i + 1] as f64 + px[i + 2] as f64;
            n += 1.0;
        }
    }
    sum / n
}

/// The dot is round: lit at its centre, and as dark as the wall at all four
/// corners of its quad, where the texture's alpha is 0.
fn assert_round(ed: &crate::Editor, px: &[u8], at: DVec3, what: &str) {
    let centre = lum_at(ed, px, at, 0.0, 0.0);
    let wall = lum_at(ed, px, at, 1.5, 0.0);
    assert!(centre > 500.0, "{what}: the dot's centre is lit (got {centre:.0} of 765, wall {wall:.0})");
    for (u, v) in [(-0.42, -0.42), (0.42, -0.42), (-0.42, 0.42), (0.42, 0.42)] {
        let corner = lum_at(ed, px, at, u, v);
        assert!(
            corner < wall + 30.0,
            "{what}: the quad's corner ({u}, {v}) is as dark as the wall behind — the texture's alpha is \
             respected (corner {corner:.0}, wall {wall:.0}, centre {centre:.0})"
        );
    }
}

/// **A billboard fired with `spawnEffect` draws its texture's alpha.** A
/// script fires a soft round dot in front of the camera; the frame shows a
/// round dot, not a white square.
#[test]
fn a_billboard_fired_from_a_script_draws_its_textures_alpha() {
    let Some((mut ed, dir, at)) = stage("script", true) else { return };
    scripted(&mut ed, at, "function start(node)\n  spawnEffect('vfx/Dot', node.pos)\nend\n");
    ed.toggle_play();
    assert!(ed.playing, "did not enter play");
    for _ in 0..3 {
        ed.play_step(1.0 / 60.0, true);
    }
    assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
    assert_eq!(ed.vfx.detached.len(), 1, "the script fired the dot");
    let px = capture(&mut ed, &dir);
    assert_round(&ed, &px, at, "fired from a script");
    let _ = std::fs::remove_dir_all(&dir);
}

/// **A billboard placed in the scene draws its texture's alpha.** The same
/// dot, from a particle system node that plays on start.
#[test]
fn a_billboard_placed_in_the_scene_draws_its_textures_alpha() {
    let Some((mut ed, dir, at)) = stage("scene", true) else { return };
    let e = ed.world.spawn();
    ed.world.insert(e, Transform { translation: at, ..Transform::IDENTITY });
    ed.world.insert(e, ParticleSystem { asset: "vfx/Dot".into(), play_on_start: true });
    ed.toggle_play();
    assert!(ed.playing, "did not enter play");
    for _ in 0..3 {
        ed.play_step(1.0 / 60.0, true);
    }
    let px = capture(&mut ed, &dir);
    assert_round(&ed, &px, at, "placed in the scene");
    let _ = std::fs::remove_dir_all(&dir);
}

/// **A billboard whose texture will not load says so, once.** The effect
/// names a file that is not there: the Console gets one warning naming the
/// effect, the track and the path, however many frames draw it.
#[test]
fn a_billboard_texture_that_will_not_load_is_reported_once() {
    let Some((mut ed, dir, at)) = stage("missing", false) else { return };
    scripted(&mut ed, at, "function start(node)\n  spawnEffect('vfx/Dot', node.pos)\nend\n");
    ed.toggle_play();
    assert!(ed.playing, "did not enter play");
    for _ in 0..3 {
        ed.play_step(1.0 / 60.0, true);
        ed.ensure_vfx_assets();
    }
    capture(&mut ed, &dir);
    capture(&mut ed, &dir);
    // The Console folds a line repeated back to back into one row with a
    // count, so the count is what says how many times it was pushed.
    let rows: Vec<_> = ed.console.entries.iter().filter(|e| e.msg.contains("textures/fx/dot.png")).collect();
    let said: Vec<&str> = rows.iter().map(|e| e.msg.as_str()).collect();
    let pushed: u32 = rows.iter().map(|e| e.count).sum();
    assert_eq!((said.len(), pushed), (1, 1), "one warning for the missing texture, said once, got {said:?} x{pushed}");
    assert!(
        said[0].contains("vfx/Dot") && said[0].contains("Drop"),
        "the warning names the effect and the track: {}",
        said[0]
    );
    let _ = std::fs::remove_dir_all(&dir);
}
