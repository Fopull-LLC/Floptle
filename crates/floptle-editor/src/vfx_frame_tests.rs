//! An effect fired on a moving world plays as it would on a still one.
//!
//! A planet on rails moves at orbital speed, 1.45 m a tick at 87 m/s, and a
//! frame runs 0, 1 or several ticks. Particles that kept still in world space
//! streamed off the ground at that speed, and jittered against it frame to
//! frame because they moved on the frame clock while the planet moved on the
//! tick clock.

use floptle_core::math::{DVec3, Vec3};
use floptle_core::{CelestialBody, Matter, Name, ScriptInst, Scripts, Transform};
use floptle_scene::{VfxPlaybackDoc, VfxPropDoc, VfxShapeDoc, VfxSpaceDoc, VfxValueDoc};

/// A still, six-second World-space puff: a burst of particles that never move
/// on their own, so any motion they show is the frame's.
fn still_puff(name: &str, shape: VfxShapeDoc) -> floptle_scene::VfxEffectDoc {
    let mut doc = crate::vfx::starter_effect_doc(name);
    doc.lifetime = 6.0;
    doc.playback = VfxPlaybackDoc::OneShot;
    let t = &mut doc.tracks[0];
    t.space = VfxSpaceDoc::World;
    t.shape = shape;
    t.velocity = VfxPropDoc::Const(VfxValueDoc::Vec3([0.0, 0.0, 0.0]));
    t.gravity = 0.0;
    t.clips = vec![floptle_scene::VfxClipDoc {
        start: 0.0,
        end: 6.0,
        lifetime_jitter: 0.0,
        emit: Some(floptle_scene::VfxEmitDoc::Burst {
            count: 8,
            count_jitter: 0.0,
            pulses: 1,
            interval: 0.0,
            interval_jitter: 0.0,
        }),
    }];
    doc
}

/// A sun and a planet orbiting it at 87 m/s, and a node running `script`.
fn moving_world(tag: &str, script: &str) -> (crate::Editor, floptle_core::Entity, std::path::PathBuf) {
    moving_world_with(tag, script, |_, _| {})
}

/// [`moving_world`], with `setup` adding to the scene before Play.
fn moving_world_with(
    tag: &str,
    script: &str,
    setup: impl FnOnce(&mut crate::Editor, floptle_core::Entity),
) -> (crate::Editor, floptle_core::Entity, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("floptle-vfx-frame-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    std::fs::write(dir.join("scripts/fx.lua"), script).unwrap();
    for (name, shape) in [
        ("Puff", VfxShapeDoc::Point),
        ("Ring", VfxShapeDoc::Ring { radius: 3.0 }),
    ] {
        floptle_scene::save_vfx_effect(&still_puff(name, shape), &dir.join("vfx").join(format!("{name}.vfx.ron")))
            .unwrap();
    }
    let mut ed = crate::Editor { project_root: dir.clone(), ..Default::default() };
    ed.vfx.rescan(&dir);
    let celestial = |ed: &mut crate::Editor, name: &str, at: DVec3, body: CelestialBody| {
        let e = ed.world.spawn();
        ed.world.insert(e, Name(name.into()));
        ed.world.insert(e, Transform { translation: at, ..Transform::IDENTITY });
        ed.world.insert(e, body);
        e
    };
    // 87 m/s: sqrt(mu / a).
    celestial(&mut ed, "Sun", DVec3::ZERO, CelestialBody { mu: 3.78e7, ..Default::default() });
    let home = CelestialBody { parent: "Sun".into(), a: 5000.0, mu: 1.0e4, soi: 500.0, ..Default::default() };
    let home = celestial(&mut ed, "Home", DVec3::new(5000.0, 0.0, 0.0), home);
    let fx = ed.world.spawn();
    ed.world.insert(fx, Transform::IDENTITY);
    ed.world.insert(fx, Name("Fx".into()));
    ed.world.insert(fx, Matter::Empty);
    ed.world.insert(
        fx,
        Scripts(vec![ScriptInst { kind: "fx".into(), enabled: true, params: Vec::new(), refs: Vec::new(), strs: Vec::new(), pinned_bools: None }]),
    );
    setup(&mut ed, home);
    ed.toggle_play();
    assert!(ed.playing, "did not enter play");
    (ed, home, dir)
}

/// Every live World-track particle of every one-shot, in world space.
fn particles(ed: &crate::Editor) -> Vec<Vec<DVec3>> {
    ed.vfx
        .detached
        .iter()
        .map(|d| {
            let p = d.inst.track_particles(0);
            (0..p.count).map(|i| d.inst.anchor() + p.pos_age[i].truncate().as_dvec3()).collect()
        })
        .collect()
}

/// A frame clock that runs 0, 1, 2 and 6 ticks a frame.
const JITTERY: [f32; 10] = [0.005, 0.016, 0.03, 0.008, 0.1, 0.0167, 0.004, 0.02, 0.033, 0.011];

/// **A puff fired on a moving planet stays over its spot.** One with
/// `frame = planet`, and one with no frame (it rides the planet whose sphere
/// of influence it is in). Over six seconds and a frame clock running 0 to 6
/// ticks a frame, neither moves against the planet, frame to frame or in
/// all.
#[test]
fn a_puff_fired_on_a_moving_planet_stays_over_its_spot_at_any_frame_rate() {
    let (mut ed, home, dir) = moving_world(
        "puff",
        "local fired = false\n\
         function update(node, dt)\n\
           if fired then return end\n\
           fired = true\n\
           local home = find('Home')\n\
           spawnEffect('vfx/Puff', vec3(home.x, home.y + 60, home.z), { frame = home })\n\
           spawnEffect('vfx/Puff', vec3(home.x + 10, home.y + 60, home.z))\n\
         end\n",
    );
    let at = |ed: &crate::Editor| floptle_core::world_transform(&ed.world, home).translation;
    // The frame the script fires in.
    ed.play_step(1.0 / 60.0, true);
    assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
    assert_eq!(ed.vfx.detached.len(), 2, "both puffs should be playing");
    let first: Vec<Vec<DVec3>> = particles(&ed).into_iter().map(|ps| ps.iter().map(|p| *p - at(&ed)).collect()).collect();
    assert!(first.iter().all(|ps| !ps.is_empty()), "each puff should have particles");
    let expect = [DVec3::new(0.0, 60.0, 0.0), DVec3::new(10.0, 60.0, 0.0)];
    for (ps, want) in first.iter().zip(expect) {
        assert!((ps[0] - want).length() < 0.05, "a puff began {} m from where it was fired", (ps[0] - want).length());
    }
    let mut prev = first.clone();
    let mut moved = 0.0f64;
    let mut played = 0.0f32;
    let mut k = 0;
    while played < 5.5 {
        let dt = JITTERY[k % JITTERY.len()];
        k += 1;
        played += dt;
        let before = at(&ed);
        ed.play_step(dt, true);
        moved = moved.max((at(&ed) - before).length());
        let now: Vec<Vec<DVec3>> = particles(&ed).into_iter().map(|ps| ps.iter().map(|p| *p - at(&ed)).collect()).collect();
        assert_eq!(now.len(), 2, "a puff ended early");
        for (which, (a, b)) in now.iter().zip(&prev).enumerate() {
            let jump = (a[0] - b[0]).length();
            assert!(jump < 0.01, "puff {which} jumped {jump} m against the planet on a {dt} s frame");
        }
        prev = now;
    }
    for (which, (a, b)) in prev.iter().zip(&first).enumerate() {
        let drift = (a[0] - b[0]).length();
        assert!(drift < 0.5, "puff {which} drifted {drift} m off its spot in {played} s");
    }
    assert!(moved > 5.0, "the fixture needs frames that run several ticks; the planet moved at most {moved} m");
    let _ = std::fs::remove_dir_all(&dir);
}

/// **A ring fired with a tilted normal lies across it.**
#[test]
fn a_ring_fired_with_a_normal_lies_across_it() {
    let (mut ed, _home, dir) = moving_world(
        "ring",
        "local fired = false\n\
         function update(node, dt)\n\
           if fired then return end\n\
           fired = true\n\
           local home = find('Home')\n\
           spawnEffect('vfx/Ring', vec3(home.x, home.y + 60, home.z), { normal = vec3(1, 1, 0) })\n\
         end\n",
    );
    ed.play_step(1.0 / 60.0, true);
    assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
    let d = &ed.vfx.detached[0];
    let normal = Vec3::new(1.0, 1.0, 0.0).normalize();
    let p = d.inst.track_particles(0);
    assert!(p.count >= 4, "the ring should have particles");
    for i in 0..p.count {
        let off = p.pos_age[i].truncate() + (d.inst.anchor() - d.pos).as_vec3();
        assert!(off.length() > 2.5, "a ring particle sits {} m from the centre", off.length());
        assert!(off.normalize().dot(normal).abs() < 0.02, "a ring particle is out of the plane: {off}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// **A node's own effect rides the planet it hangs under.** Its World-track
/// particles used to be left in space while the node went on with its world.
#[test]
fn a_nodes_world_track_particles_ride_the_planet_it_is_on() {
    let (mut ed, home, dir) = moving_world("node", "function update(node, dt) end\n");
    let fire = ed.world.spawn();
    ed.world.insert(fire, Transform { translation: DVec3::new(0.0, 60.0, 0.0), ..Transform::IDENTITY });
    ed.world.insert(fire, floptle_core::Parent(home));
    ed.world.insert(fire, floptle_core::ParticleSystem { asset: "vfx/Puff".into(), play_on_start: true });
    let at = |ed: &crate::Editor| floptle_core::world_transform(&ed.world, home).translation;
    let first = |ed: &crate::Editor| {
        let (_, inst) = ed.vfx.instances.get(&fire).expect("the node's effect is playing");
        let p = inst.track_particles(0);
        assert!(p.count > 0, "the node's effect has particles");
        inst.anchor() + p.pos_age[0].truncate().as_dvec3() - at(ed)
    };
    ed.play_step(1.0 / 60.0, true);
    ed.play_step(1.0 / 60.0, true);
    let start = first(&ed);
    for dt in JITTERY.repeat(4) {
        ed.play_step(dt, true);
        let drift = (first(&ed) - start).length();
        assert!(drift < 0.01, "the node's particles moved {drift} m against the planet on a {dt} s frame");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// **A mark laid on a moving planet stays on the rock it was put on.** It
/// rides the planet as the planet moves, rather than staying in space where
/// the ground was when it was laid.
#[test]
fn a_decal_laid_on_a_moving_planet_rides_it() {
    use floptle_core::{BodyKind, BodyMode, Parent, RigidBody};
    use floptle_script::decal_api::DecalFrame;
    let (mut ed, home, dir) = moving_world_with(
        "decal",
        "local laid = false\n\
         function update(node, dt)\n\
           if laid then return end\n\
           local home = find('Home')\n\
           if decals.add({ texture = 'scorch.png', pos = vec3(home.x, home.y + 59, home.z), normal = vec3(0, 1, 0), size = 2 }) then\n\
             laid = true\n\
           end\n\
         end\n",
        |ed, home| {
            // The ground: a slab on the planet, top face 59 m above its centre.
            let floor = ed.world.spawn();
            ed.world.insert(floor, Transform { translation: DVec3::new(0.0, 58.0, 0.0), ..Transform::IDENTITY });
            ed.world.insert(floor, Parent(home));
            let slab =
                RigidBody { kind: BodyKind::Box, mode: BodyMode::Static, half_extents: [20.0, 1.0, 20.0], ..Default::default() };
            ed.world.insert(floor, slab);
        },
    );
    let at = |ed: &crate::Editor| floptle_core::world_transform(&ed.world, home).translation;
    for _ in 0..3 {
        ed.play_step(1.0 / 60.0, true);
    }
    assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
    let store = ed.script_host.decals().clone();
    let laid = store.borrow().get(1).map(|d| d.frame.clone()).expect("the mark was laid");
    assert!(matches!(laid, DecalFrame::On(f, _) if f == home.index()), "the mark should ride the planet, rides {laid:?}");
    let drawn_at = |ed: &crate::Editor| {
        let (anchor, _) = crate::decals::pack(&store.borrow(), "scorch.png", Some(home.index())).expect("a batch on the planet");
        floptle_core::world_transform(&ed.world, home).mul_transform(&Transform::from_translation(anchor)).translation
    };
    let start = drawn_at(&ed) - at(&ed);
    assert!((start - DVec3::new(0.0, 59.0, 0.0)).length() < 0.05, "laid at {start} from the planet's centre");
    for dt in JITTERY.repeat(3) {
        ed.play_step(dt, true);
        let drift = (drawn_at(&ed) - at(&ed) - start).length();
        assert!(drift < 0.01, "the mark moved {drift} m against the planet on a {dt} s frame");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// **A lit cloud beside a lamp takes the lamp's light.** Two identical lit
/// puffs in a dim scene, one inside a warm point light's range and one out of
/// it, drawn through the real particle pass: the near one comes out brighter
/// and warmer. Lit particles used to take the ambient and the key light only,
/// so both came out the same.
#[test]
#[cfg(feature = "editor-ui")]
fn a_lit_cloud_beside_a_lamp_takes_its_light() {
    use floptle_core::math::Quat;
    let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
    let dir = std::env::temp_dir().join(format!("floptle-lit-dust-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut doc = still_puff("Dust", VfxShapeDoc::Point);
    doc.tracks[0].lit = true;
    // Big enough to measure: the starter puff is a dot at this distance.
    doc.tracks[0].size = VfxPropDoc::Const(VfxValueDoc::F32(0.8));
    // Mid grey and opaque, so a lamp has headroom above it before white.
    doc.tracks[0].color = VfxPropDoc::Const(VfxValueDoc::Rgba([0.2, 0.2, 0.2, 1.0]));
    floptle_scene::save_vfx_effect(&doc, &dir.join("vfx/Dust.vfx.ron")).unwrap();
    ed.project_root = dir.clone();
    ed.vfx.rescan(&dir);
    // A dim key light, so a lamp has something to add to.
    let sun = ed.world.spawn();
    ed.world.insert(sun, Transform::IDENTITY);
    ed.world.insert(sun, floptle_core::Light { intensity: 0.1, ..Default::default() });
    let cam = ed.camera.render_camera();
    let ahead = |x: f32| cam.world_position + (cam.rotation * Vec3::new(x, 0.0, -6.0)).as_dvec3();
    let (near, far) = (ahead(-1.5), ahead(1.5));
    let lamp = ed.world.spawn();
    ed.world.insert(lamp, Transform { translation: near, ..Transform::IDENTITY });
    ed.world.insert(lamp, Matter::PointLight {
        color: [1.0, 0.45, 0.1],
        intensity: 1.0,
        range: 2.0,
        shadows: Default::default(),
        shape: Default::default(),
        spot_angle: floptle_core::OMNI_ANGLE,
        spot_softness: 0.25,
    });
    ed.toggle_play();
    for at in [near, far] {
        ed.vfx.spawn_detached_with(
            "vfx/Dust",
            crate::vfx::DetachedSpawn {
                emitter: Transform { translation: at, rotation: Quat::IDENTITY, ..Transform::IDENTITY },
                vel: Vec3::ZERO,
                frame: None,
                intensity: None,
                volume: None,
                tint: None,
                up: None,
            },
        );
    }
    for _ in 0..3 {
        ed.play_step(1.0 / 60.0, true);
    }
    let (w, h) = (160u32, 120u32);
    let cam = ed.camera.render_camera();
    let px = crate::shot::render_frame_pixels(&mut ed, &cam, None, w, h, !0, false).expect("a frame");
    let vp = cam.view_proj(w as f32 / h as f32);
    let sample = |p: DVec3| {
        let s = crate::viz::project(p, cam.world_position, vp, w as f32, h as f32).expect("on screen");
        let (x, y) = (s.x.round() as i64, s.y.round() as i64);
        let mut sum = [0.0f64; 3];
        let mut n = 0.0;
        for dy in -2..=2 {
            for dx in -2..=2 {
                let (sx, sy) = ((x + dx).clamp(0, w as i64 - 1) as u32, (y + dy).clamp(0, h as i64 - 1) as u32);
                let i = ((sy * w + sx) * 4) as usize;
                for c in 0..3 {
                    sum[c] += px[i + c] as f64;
                }
                n += 1.0;
            }
        }
        sum.map(|v| v / n)
    };
    let (a, b) = (sample(near), sample(far));
    let _ = std::fs::write(dir.join("probe.txt"), format!("near {a:?} far {b:?}"));
    let lum = |c: [f64; 3]| c[0] + c[1] + c[2];
    assert!(lum(a) > lum(b) + 30.0, "the cloud by the lamp is brighter: near {a:?}, far {b:?}");
    assert!(a[0] - a[2] > b[0] - b[2] + 20.0, "and warmer: near {a:?}, far {b:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A sky shader seen through an orthographic camera looks as it does through
/// the same camera in perspective. It used to come out one flat colour: every
/// pixel of an orthographic view looks the same way, so the shader was asked
/// about one direction for the whole screen.
#[test]
#[cfg(feature = "editor-ui")]
fn a_sky_shader_draws_the_same_through_an_orthographic_camera() {
    use floptle_render::{Projection, RenderCamera};
    let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
    let dir = std::env::temp_dir().join(format!("floptle-ortho-sky-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("shaders")).unwrap();
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/shaders/examples/sunsetStreaks.flsl");
    std::fs::copy(&src, dir.join("shaders/sunsetStreaks.flsl")).unwrap();
    ed.project_root = dir.clone();
    let sky = ed.world.spawn();
    ed.world.insert(sky, Transform::IDENTITY);
    ed.world.insert(
        sky,
        Matter::Skybox {
            color: [0.5; 3],
            size: 500.0,
            texture: None,
            tint: [1.0; 3],
            shader: Some("shaders/sunsetStreaks.flsl".into()),
            shader_params: Default::default(),
        },
    );
    let (w, h) = (96u32, 54u32);
    let shoot = |ed: &mut crate::Editor, ortho: bool| {
        let proj = Projection::of_camera(1.0, ortho, 6.0, 0.05, 300_000.0);
        let cam = RenderCamera::new(DVec3::new(0.0, 0.0, 20.0), Default::default(), proj);
        crate::shot::render_frame_pixels(ed, &cam, None, w, h, !0, false).expect("a frame")
    };
    let persp = shoot(&mut ed, false);
    let ortho = shoot(&mut ed, true);
    let _ = std::fs::remove_dir_all(&dir);
    let spread = |px: &[u8]| {
        let lum: Vec<i32> = px.chunks(4).map(|p| p[0] as i32 + p[1] as i32 + p[2] as i32).collect();
        lum.iter().max().unwrap() - lum.iter().min().unwrap()
    };
    assert!(spread(&persp) > 60, "the sky shader drew nothing to compare against");
    assert!(spread(&ortho) > 60, "the orthographic sky is one flat colour");
    let worst = persp.iter().zip(&ortho).map(|(a, b)| (*a as i32 - *b as i32).abs()).max().unwrap();
    assert!(worst <= 4, "the orthographic sky differs from the perspective one by up to {worst}");
}

/// A World-space burst that drops onto the level under gravity and collides
/// with it, spread over a ring so the particles land apart.
fn falling_debris(friction: f32) -> floptle_scene::VfxEffectDoc {
    let mut doc = still_puff("Debris", VfxShapeDoc::Ring { radius: 1.5 });
    let t = &mut doc.tracks[0];
    t.gravity = 1.0;
    t.collide = Some(floptle_scene::VfxCollideDoc { bounce: 0.0, friction, die: false });
    doc
}

/// Play a scene with `floor` (a static box) under a `falling_debris` burst
/// fired at `at`, with the camera — and so the sim's origin — beside it, and
/// hand back each frame's particle positions in metres from `at`, and how
/// many rest on the level at the end.
fn drop_debris(
    tag: &str,
    at: DVec3,
    floor: (DVec3, floptle_core::math::Quat),
    friction: f32,
    frames: usize,
) -> (Vec<Vec<Vec3>>, usize) {
    use floptle_core::{BodyKind, BodyMode, RigidBody};
    let dir = std::env::temp_dir().join(format!("floptle-debris-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    floptle_scene::save_vfx_effect(&falling_debris(friction), &dir.join("vfx/Debris.vfx.ron")).unwrap();
    let mut ed = crate::Editor { project_root: dir.clone(), ..Default::default() };
    ed.vfx.rescan(&dir);
    let slab = ed.world.spawn();
    ed.world.insert(slab, Transform { translation: floor.0, rotation: floor.1, ..Transform::IDENTITY });
    let body = RigidBody { kind: BodyKind::Box, mode: BodyMode::Static, half_extents: [20.0, 1.0, 20.0], ..Default::default() };
    ed.world.insert(slab, body);
    let cam = ed.world.spawn();
    ed.world.insert(cam, Transform { translation: at + DVec3::new(0.0, 3.0, 12.0), ..Transform::IDENTITY });
    ed.world.insert(
        cam,
        Matter::Camera {
            fov_y: 1.0,
            active: true,
            target: String::new(),
            cull_mask: u32::MAX,
            target_w: 0,
            target_h: 0,
            target_hz: 0.0,
            ortho: false,
            ortho_height: 10.0,
            stack: Vec::new(),
        },
    );
    let fx = ed.world.spawn();
    ed.world.insert(fx, Transform { translation: at, ..Transform::IDENTITY });
    ed.world.insert(fx, floptle_core::ParticleSystem { asset: "vfx/Debris".into(), play_on_start: true });
    ed.toggle_play();
    let mut out = Vec::new();
    for _ in 0..frames {
        ed.play_step(1.0 / 60.0, true);
        let mut now = Vec::new();
        if let Some((_, inst)) = ed.vfx.instances.get(&fx) {
            let anchor = inst.anchor();
            inst.sample_track(0, |p| now.push(((anchor - at).as_vec3()) + p.pos));
        }
        out.push(now);
    }
    let resting = ed.vfx.instances.get(&fx).map_or(0, |(_, inst)| inst.resting());
    let _ = std::fs::remove_dir_all(&dir);
    (out, resting)
}

/// **Landed debris lies still megametres from the origin, as it does at it.**
/// A bounce 0, friction 1 drop onto a flat floor. The hit point used to come
/// back as f32 world coordinates, which 4.9 Mm out snap to a half-metre grid:
/// every hit moved the particle onto the grid and it fell back, hopping about
/// a tenth of a metre a frame. At rest it now does not move at all, and sits
/// within a skin of the floor.
#[test]
fn landed_debris_lies_still_far_from_the_origin() {
    // The floor's top sits off the f32 grid out there (0.5 m), as ground does.
    for y in [0.3f64, 4.9e6 + 0.3] {
        let at = DVec3::new(0.0, y + 2.0, 0.0);
        let floor = (DVec3::new(0.0, y - 1.0, 0.0), Default::default());
        let (frames, resting) = drop_debris(&format!("far{y}"), at, floor, 1.0, 150);
        let (a, b) = (&frames[120], &frames[149]);
        assert_eq!(a.len(), 8, "y = {y}: the burst is not all there");
        assert_eq!(resting, 8, "y = {y}: landed debris should be at rest, asking the level nothing");
        let mut worst = 0.0f32;
        for w in frames[120..].windows(2) {
            for (p, q) in w[0].iter().zip(&w[1]) {
                worst = worst.max(p.distance(*q));
            }
        }
        assert!(worst < 1e-5, "y = {y}: landed particles moved {worst} m between frames");
        for p in b {
            // Two metres up from the emitter is the floor's top.
            let above = p.y + 2.0;
            assert!((0.0..=0.015).contains(&above), "y = {y}: a particle rests {above} m off the floor");
        }
    }
}

/// **A landed particle with little friction slides down a slope**, held
/// within a skin of it. Each hit put it back where it touched down, and a
/// particle a centimetre off a surface counted as touching it at the start of
/// every step, so it could not slide at all and hovered.
#[test]
fn a_landed_particle_slides_down_a_slope() {
    use floptle_core::math::Quat;
    let tilt = Quat::from_rotation_z(20f32.to_radians());
    let at = DVec3::new(0.0, 3.0, 0.0);
    let (frames, _) = drop_debris("slope", at, (DVec3::ZERO, tilt), 0.1, 120);
    let n = tilt * Vec3::Y;
    // The slab's top face passes through the centre plus its normal.
    let off = |p: Vec3| (p + at.as_vec3() - n).dot(n);
    let (a, b) = (&frames[60], &frames[119]);
    assert_eq!(a.len(), 8, "the burst is not all there");
    for (p, q) in a.iter().zip(b) {
        assert!(off(*p) < 0.1, "it never reached the slope: {} m off", off(*p));
        assert!(p.distance(*q) > 0.2, "a particle at friction 0.1 on a 20° slope moved {} m in a second", p.distance(*q));
    }
    for f in &frames[60..] {
        for p in f {
            let d = off(*p);
            assert!((-0.002..=0.015).contains(&d), "a sliding particle rides {d} m off the slope");
        }
    }
}

/// **`perf.counts().particles` answers on a frame nobody drew.** The count
/// was filled in only by the draw gather, so `floptle run` — which draws no
/// frame at all — and the undrawn frames of a `shot --after` read 0 while the
/// particles were live.
#[test]
fn particle_counts_are_filled_in_without_a_draw() {
    let dir = std::env::temp_dir().join(format!("floptle-undrawn-counts-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    floptle_scene::save_vfx_effect(&still_puff("Puff", VfxShapeDoc::Point), &dir.join("vfx/Puff.vfx.ron")).unwrap();
    let mut ed = crate::Editor { project_root: dir.clone(), ..Default::default() };
    ed.vfx.rescan(&dir);
    let fx = ed.world.spawn();
    ed.world.insert(fx, Transform::IDENTITY);
    ed.world.insert(fx, floptle_core::ParticleSystem { asset: "vfx/Puff".into(), play_on_start: true });
    ed.script_host.profile().borrow_mut().enable(true);
    ed.toggle_play();
    for _ in 0..5 {
        ed.play_step(1.0 / 60.0, true);
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(ed.vfx.live_particles(), 8, "the fixture: the puff is live");
    let counted = ed.script_host.profile().borrow().counts().particles;
    assert_eq!(counted, 8, "perf.counts().particles on an undrawn frame");
}
