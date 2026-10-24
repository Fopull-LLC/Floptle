//! A capsule touches along its whole length, not only at its two end
//! spheres: a post narrower than the gap between them stops it at its side.

use floptle_core::math::{DVec3, Vec3};
use floptle_core::{BodyKind, BodyMode, RigidBody, Transform, World};
use floptle_physics::{GravityField, Sim};

/// A crew capsule (r 0.33, 1.8 tall) on a floor, steered at 2 m/s into a
/// post of radius 0.5 whose centre is level with the crew's middle. Returns
/// the closest the crew's axis came to the post's.
fn closest_to_post(mode: BodyMode, kind: BodyKind, turns: bool) -> f64 {
    let mut ecs = World::default();
    let floor = ecs.spawn();
    ecs.insert(floor, Transform::from_translation(DVec3::new(0.0, -0.5, 0.0)));
    ecs.insert(floor, RigidBody { kind: BodyKind::Box, mode: BodyMode::Static, half_extents: [50.0, 0.5, 50.0], ..Default::default() });
    let crew = ecs.spawn();
    ecs.insert(crew, Transform::from_translation(DVec3::new(0.0, 0.9, 0.0)));
    ecs.insert(
        crew,
        RigidBody { kind: BodyKind::Capsule, radius: 0.33, height: 1.8, rotates: turns, lock_rot: [turns; 3], ..Default::default() },
    );
    let post = ecs.spawn();
    ecs.insert(post, Transform::from_translation(DVec3::new(3.0, 0.9, 0.0)));
    ecs.insert(post, RigidBody { kind, mode, radius: 0.5, height: 2.0, gravity: false, ..Default::default() });
    let mut sim = Sim::build(&ecs, &[], GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)), DVec3::ZERO);
    let mut closest = f64::MAX;
    for _ in 0..300 {
        sim.sync_dynamic_params(&ecs);
        let v = sim.body_states().find(|r| r.entity == crew).unwrap().vel;
        sim.set_body_velocity(crew.index(), Vec3::new(2.0, v.y, 0.0));
        sim.step_tick(1.0 / 60.0, None);
        let p = sim.body_states().find(|r| r.entity == crew).unwrap().pos;
        closest = closest.min(((p.x - 3.0).powi(2) + p.z.powi(2)).sqrt());
    }
    closest
}

/// The crew stops at the post's radius plus its own (0.83 m), whether the
/// post is kinematic or static, a sphere or a capsule, and whether the crew
/// turns or not. A sphere at its middle went between its end spheres, to
/// 0.60 m, when those were all it tested against a kinematic body, or, for a
/// crew that turns, against anything.
#[test]
fn a_capsule_walking_into_a_narrow_post_stops_at_its_side() {
    let mut missed = Vec::new();
    for mode in [BodyMode::Kinematic, BodyMode::Static] {
        for kind in [BodyKind::Sphere, BodyKind::Capsule] {
            for turns in [false, true] {
                let c = closest_to_post(mode, kind, turns);
                if c < 0.8 {
                    missed.push(format!("{mode:?} {kind:?} post, crew turns {turns}: {c:.3} m"));
                }
            }
        }
    }
    assert!(missed.is_empty(), "the crew went into the post: {missed:?}");
}
