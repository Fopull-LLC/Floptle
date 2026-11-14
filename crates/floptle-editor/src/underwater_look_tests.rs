//! A Water Volume's own underwater look can be switched off, and that is all
//! it switches off: the water still holds a body up, and a screen shader is
//! still told how deep the camera is.

use floptle_core::math::DVec3;
use floptle_core::{GravityMode, Matter, Name, RigidBody, Transform, WaterKind};

fn pool(underwater: bool) -> Matter {
    tinted(underwater, [0.9, 0.1, 0.1])
}

fn tinted(underwater: bool, tint: [f32; 3]) -> Matter {
    Matter::WaterVolume {
        kind: WaterKind::Pool,
        radius: 0.0,
        half_extents: [5.0, 5.0, 5.0],
        density: 1000.0,
        drag: 1.0,
        angular_drag: 1.0,
        frozen: false,
        tint,
        visibility: 4.0,
        underwater,
        waves: Vec::new(),
    }
}

fn world_with(pools: &[(f64, bool)]) -> floptle_core::World {
    let mut world = floptle_core::World::new();
    for (y, underwater) in pools {
        let e = world.spawn();
        world.insert(e, Name("Pool".into()));
        world.insert(e, Transform { translation: [0.0, *y, 0.0].into(), ..Transform::IDENTITY });
        world.insert(e, pool(*underwater));
    }
    world
}

/// **Look off: no fog from inside, and the depth is still known.** The same
/// pool with its look on paints its tint; a camera above either reads air.
#[test]
fn a_volume_with_its_look_off_paints_no_fog_and_still_reports_its_depth() {
    let open = |_: DVec3, _: DVec3, _: f64| false;
    let inside = DVec3::new(0.0, 2.0, 0.0);

    let on = crate::shading::water_at_open(&world_with(&[(0.0, true)]), &[], inside, &open);
    assert_eq!(on.look, Some(([0.9, 0.1, 0.1], 4.0)));
    assert!((on.depth - 3.0).abs() < 1e-4, "3 m under a surface at y = 5, got {}", on.depth);

    let world = world_with(&[(0.0, false)]);
    let off = crate::shading::water_at_open(&world, &[], inside, &open);
    assert_eq!(off.look, None, "a volume with its look off painted the engine's underwater fog");
    assert!((off.depth - 3.0).abs() < 1e-4, "the depth went with the look: {}", off.depth);

    // The fog the draw uses is the scene's own, as in air.
    let light = floptle_core::Light::default();
    let (lanes, _) = crate::shading::fog_uniforms_and_particles_at(&light, &world, &[], &open, inside);
    assert_eq!(lanes, crate::shading::fog_uniforms(&light), "the scene's fog was replaced under look-off water");

    let above = crate::shading::water_at_open(&world, &[], DVec3::new(0.0, 6.0, 0.0), &open);
    assert_eq!(above, crate::shading::WaterAt::default(), "above the surface is air");
}

/// Two bodies of water at once: the depth is the deepest water's, the look
/// the deepest water's that has one. And a cave sealed under the surface is
/// neither.
#[test]
fn depth_and_look_can_come_from_different_water() {
    let open = |_: DVec3, _: DVec3, _: f64| false;
    // A look-off pool whose surface is at y = 9, and a look-on one at y = 5.
    let mut world = world_with(&[(0.0, true)]);
    let deep = world.spawn();
    world.insert(deep, Name("Deep".into()));
    world.insert(deep, Transform { translation: [0.0, 4.0, 0.0].into(), ..Transform::IDENTITY });
    world.insert(deep, tinted(false, [0.1, 0.1, 0.9]));
    let at = crate::shading::water_at_open(&world, &[], DVec3::new(0.0, 2.0, 0.0), &open);
    assert!((at.depth - 7.0).abs() < 1e-4, "the deeper water is 7 m overhead, got {}", at.depth);
    assert_eq!(at.look, Some(([0.9, 0.1, 0.1], 4.0)), "the pool that has a look still paints it");

    let roofed = |_: DVec3, _: DVec3, _: f64| true;
    let sealed = crate::shading::water_at_open(&world, &[], DVec3::new(0.0, 2.0, 0.0), &roofed);
    assert_eq!(sealed, crate::shading::WaterAt::default(), "a sealed cave is not under water");
}

/// **Look off still floats a crate.** A body falling into a pool whose look
/// is off is pushed up by it on the next tick, exactly as by any pool.
#[test]
fn a_volume_with_its_look_off_still_floats_a_body() {
    let mut ed = crate::Editor::default();
    let g = ed.world.spawn();
    ed.world.insert(g, Name("Gravity".into()));
    ed.world.insert(g, Transform::IDENTITY);
    ed.world.insert(g, Matter::GravityVolume { mode: GravityMode::Down, strength: 15.0, radius: 0.0 });
    let crate_ = ed.world.spawn();
    ed.world.insert(crate_, Name("Crate".into()));
    ed.world.insert(crate_, Transform { translation: [0.0, 2.0, 0.0].into(), ..Transform::IDENTITY });
    ed.world.insert(crate_, RigidBody::default());
    let p = ed.world.spawn();
    ed.world.insert(p, Name("Pool".into()));
    ed.world.insert(p, Transform::IDENTITY);
    ed.world.insert(p, pool(false));

    ed.toggle_play();
    assert!(ed.playing, "the session must start");
    for _ in 0..3 {
        ed.play_step(1.0 / 60.0, true);
    }
    let sim = ed.sim.as_ref().unwrap();
    assert_eq!(sim.world.water.volumes.len(), 1, "water with its look off left the solver");
    let vel = sim.body_states().find(|r| r.entity == crate_).unwrap().vel.y;
    assert!(vel > 0.0, "a body inside look-off water kept falling at {vel}: it lost its buoyancy with its look");
}
