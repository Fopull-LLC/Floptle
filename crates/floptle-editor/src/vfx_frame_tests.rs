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
        Scripts(vec![ScriptInst { kind: "fx".into(), enabled: true, params: Vec::new(), refs: Vec::new(), strs: Vec::new() }]),
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
