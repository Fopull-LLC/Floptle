//! Recentring the floating origin never changes the pull a body feels.

use floptle_core::math::DVec3;
use floptle_core::{RigidBody, Transform, World};
use floptle_physics::{GravityField, GravitySource, Sim};

/// A body hanging over a planet whose pull is an inverse-square source, with
/// the origin recentred on every one of ten ticks: after each, the pull at
/// the body points at the planet's centre to within a tenth of a degree.
#[test]
fn a_recentre_leaves_a_planets_pull_pointing_at_the_planet() {
    let centre = DVec3::new(30_000.0, -2_000.0, 12_000.0);
    let at = centre + DVec3::new(0.0, 320.0, 0.0);
    let mut ecs = World::default();
    let ball = ecs.spawn();
    ecs.insert(ball, Transform::from_translation(at));
    ecs.insert(ball, RigidBody { gravity: false, radius: 0.5, ..Default::default() });
    let origin = at.round();
    let gravity = GravityField {
        sources: vec![GravitySource::InvSq { center: (centre - origin).as_vec3(), mu: 7.0e5, soi: 0.0, body_r: 300.0 }],
    };
    let mut sim = Sim::build(&ecs, &[], gravity, origin);
    sim.set_origin_threshold(50.0);
    let mut worst = 0.0f32;
    for k in 0..10 {
        let before = sim.world.origin;
        // A camera far enough off to make the sim recentre on it.
        let focus = at + DVec3::new(if k % 2 == 0 { 400.0 } else { -400.0 }, 0.0, 0.0);
        sim.step_tick(1.0 / 60.0, Some(focus));
        assert_ne!(sim.world.origin, before, "tick {k} did not recentre");
        let p = sim.body_states().find(|r| r.entity == ball).unwrap().pos;
        let local = (p - sim.world.origin).as_vec3();
        let g = sim.world.gravity.accel_at(local, &sim.world.colliders);
        let want = (centre - p).as_vec3().normalize();
        worst = worst.max(g.normalize().dot(want).clamp(-1.0, 1.0).acos().to_degrees());
    }
    assert!(worst < 0.1, "after a recentre the pull was {worst}° off the planet's centre");
}

/// A sea is where it was after a recentre: the depth at a fixed point in
/// the world reads the same on every tick.
#[test]
fn a_recentre_leaves_a_sea_where_it_was() {
    let centre = DVec3::new(30_000.0, -2_000.0, 12_000.0);
    let probe = centre + DVec3::new(0.0, 299.0, 0.0);
    let mut ecs = World::default();
    let ball = ecs.spawn();
    ecs.insert(ball, Transform::from_translation(probe));
    ecs.insert(ball, RigidBody { gravity: false, radius: 0.5, ..Default::default() });
    let origin = probe.round();
    let mut sim = Sim::build(&ecs, &[], GravityField::default(), origin);
    sim.world.water.volumes.push(floptle_physics::WaterVolume {
        shape: floptle_physics::WaterShape::Sphere { center: (centre - origin).as_vec3(), radius: 300.0 },
        density: 1000.0,
        drag: 0.0,
        angular_drag: 0.0,
        frozen: false,
        entity: 0,
    });
    sim.set_origin_threshold(50.0);
    for k in 0..10 {
        let focus = probe + DVec3::new(if k % 2 == 0 { 400.0 } else { -400.0 }, 0.0, 0.0);
        sim.step_tick(1.0 / 60.0, Some(focus));
        let depth = sim.world.water.sample((probe - sim.world.origin).as_vec3()).depth;
        assert!((depth - 1.0).abs() < 0.01, "tick {k}: the sea reads {depth} m deep where it is 1 m deep");
    }
}
