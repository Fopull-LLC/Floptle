//! Shape casts that start against something let off only what they start
//! against.

use floptle_core::math::{DVec3, Quat, Vec3};
use floptle_core::{BodyKind, BodyMode, RigidBody, Transform, World};
use floptle_physics::{BodyHull, BodyShape, GravityField, Sim};

/// A floor whose top is y = 0, as the sim's static colliders.
fn floor() -> Sim {
    let mut ecs = World::default();
    let f = ecs.spawn();
    ecs.insert(f, Transform::from_translation(DVec3::new(0.0, -1.0, 0.0)));
    ecs.insert(f, RigidBody { kind: BodyKind::Box, mode: BodyMode::Static, half_extents: [20.0, 1.0, 20.0], ..Default::default() });
    Sim::build(&ecs, &[], GravityField::default(), DVec3::ZERO)
}

fn ball(eid: u32, at: Vec3, radius: f32) -> BodyHull {
    BodyHull { eid, owner: eid, pos: at, radius, shape: BodyShape::Sphere, up: Vec3::Y, layer: 0, orient: Quat::IDENTITY }
}

/// A ball cast down from 5 m with a body 0.2 m off to its side: with
/// `ignoreStart` the body is let off and the floor is met at the full
/// radius, so casts of 0.2, 0.6 and 1.5 stop at 5 m less their radius, the
/// place an overlap of that radius would just touch the floor. And a cast
/// that starts inside a body passes out of it and meets the floor.
#[test]
fn ignore_start_lets_off_only_what_the_cast_starts_against() {
    let sim = floor();
    let cols = &sim.world.colliders;
    let origin = Vec3::new(0.0, 5.0, 0.0);
    let beside = [ball(1, Vec3::new(0.35, 5.0, 0.0), 0.15)];
    for r in [0.2f32, 0.6, 1.5] {
        let hit = floptle_physics::spherecast(cols, &beside, origin, Vec3::NEG_Y, r, 20.0, &[], !0, true).expect("it hits the floor");
        assert!(hit.eid != Some(1), "radius {r}: it hit the body it started against");
        assert!((hit.distance - (5.0 - r)).abs() < 0.05, "radius {r} stopped after {} m, not {}", hit.distance, 5.0 - r);
        // Where it stopped, an overlap of that radius just touches the floor.
        let at = origin + Vec3::NEG_Y * hit.distance;
        let over = floptle_physics::overlap_sphere_colliders(cols, at, r + 0.05, !0);
        assert!(!over.is_empty() && over.iter().all(|o| o.distance < 0.1), "radius {r}: the overlap there reads {over:?}");
    }
    let around = [ball(2, origin, 1.0)];
    let hit = floptle_physics::spherecast(cols, &around, origin, Vec3::NEG_Y, 0.3, 20.0, &[], !0, true).expect("it hits the floor");
    assert!(hit.eid != Some(2) && (hit.distance - 4.7).abs() < 0.05, "from inside a body it met {:?} after {} m", hit.eid, hit.distance);
    // The capsule and the box the same way.
    let hit = floptle_physics::capsulecast(cols, &beside, origin, Vec3::NEG_Y, 0.6, 0.6, Vec3::Y, 20.0, &[], !0, true).unwrap();
    assert!((hit.distance - 4.4).abs() < 0.05, "the capsule stopped after {} m", hit.distance);
    let hit = floptle_physics::boxcast(cols, &around, origin, Vec3::NEG_Y, [0.5, 0.5], 20.0, &[], !0, true).unwrap();
    assert!(hit.eid != Some(2) && (hit.distance - 4.5).abs() < 0.1, "the box met {:?} after {} m", hit.eid, hit.distance);
}
