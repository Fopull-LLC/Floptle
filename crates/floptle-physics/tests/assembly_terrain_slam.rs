//! A fast assembly hitting terrain comes off it no faster than its
//! restitution allows: it stops at first touch instead of arriving deep
//! inside the ground and being thrown back out.

use floptle_core::math::{DVec3, Quat, Vec3};
use floptle_core::{BodyKind, Parent, RigidBody, Transform, World};
use floptle_field::ChunkField;
use floptle_physics::{GravityField, Sim, TerrainVolume};

/// A 0.1 mass, restitution 0.05 one-part assembly (a 0.55-scale unit box)
/// at `y`, falling at `speed` onto a terrain slab whose top is y = 0.
/// Returns its peak height and peak upward speed after it first touched.
fn slam(speed: f32, y: f64) -> (f32, f32, f32) {
    let mut field = ChunkField::new(0.75);
    field.fill_slab(Vec3::new(-96.0, -6.0, -96.0), Vec3::new(96.0, 0.0, 96.0), 0.0, [0.5; 3]);
    let mut ecs = World::default();
    let root = ecs.spawn();
    ecs.insert(root, Transform::from_translation(DVec3::new(0.3, y, -0.2)));
    ecs.insert(root, RigidBody { assembly: true, restitution: 0.05, ..Default::default() });
    let part = ecs.spawn();
    ecs.insert(part, Transform { translation: DVec3::ZERO, rotation: Quat::from_rotation_x(0.3), scale: Vec3::splat(0.55) });
    ecs.insert(part, Parent(root));
    ecs.insert(part, RigidBody { kind: BodyKind::Box, half_extents: [0.5, 0.5, 0.5], mass: 0.1, ..Default::default() });
    let mut sim = Sim::build_layered(
        &ecs,
        &[TerrainVolume::new(DVec3::ZERO, &field)],
        GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)),
        DVec3::ZERO,
        floptle_core::Layers::default(),
    );
    sim.set_compound_velocity(root.index(), Vec3::new(0.0, -speed, 0.0));
    let mut touched = false;
    let (mut peak_y, mut peak_up, mut lowest) = (f32::NEG_INFINITY, 0.0f32, f32::INFINITY);
    for _ in 0..240 {
        sim.step_tick(1.0 / 60.0, None);
        let c = sim.compound_of(root.index()).unwrap();
        assert!(c.pos.is_finite() && c.vel.is_finite(), "state went non-finite");
        lowest = lowest.min(c.pos.y);
        if !touched && c.vel.y > -speed * 0.5 {
            touched = true;
        }
        if touched {
            peak_y = peak_y.max(c.pos.y);
            peak_up = peak_up.max(c.vel.y);
        }
    }
    assert!(touched, "it never reached the ground");
    (peak_y, peak_up, lowest)
}

/// Dropped at 140 m/s, it comes off at no more than 10 m/s and never climbs
/// more than 2 m off the surface; it never ends up deep inside the slab.
#[test]
fn a_fast_assembly_hitting_terrain_rebounds_within_its_restitution() {
    for (speed, y) in [(140.0, 3.0), (140.0, 2.37), (141.0, 6.11), (90.0, 4.4)] {
        let (peak_y, peak_up, lowest) = slam(speed, y);
        assert!(peak_up <= 10.0, "{speed} m/s from y={y}: came off at {peak_up} m/s");
        assert!(peak_y <= 2.0, "{speed} m/s from y={y}: climbed to {peak_y} m");
        assert!(lowest > -1.0, "{speed} m/s from y={y}: went {lowest} m into the slab");
    }
}
