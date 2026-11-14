//! What floats: a body's draft does not depend on whether it may rotate.

use floptle_core::math::{DVec3, Vec3};
use floptle_core::{BodyKind, Entity, RigidBody, Transform, World};
use floptle_physics::{GravityField, GravitySource, Sim, WaterShape, WaterVolume};

/// A sea 200 m across and 100 m deep, its surface at y = 0, and one body
/// dropped from 2 m above it.
fn sea_with(body: RigidBody) -> (Sim, Entity) {
    let mut ecs = World::default();
    let e = ecs.spawn();
    ecs.insert(e, Transform::from_translation(DVec3::new(0.0, 2.0, 0.0)));
    ecs.insert(e, body);
    let gravity = GravityField { sources: vec![GravitySource::Uniform(Vec3::new(0.0, -9.81, 0.0))] };
    let mut sim = Sim::build(&ecs, &[], gravity, DVec3::ZERO);
    sim.world.water.volumes.push(WaterVolume {
        shape: WaterShape::Box { center: Vec3::new(0.0, -50.0, 0.0), half: Vec3::new(100.0, 50.0, 100.0), rot: Default::default() },
        density: 1000.0,
        drag: 1.0,
        angular_drag: 1.0,
        frozen: false,
        entity: 7,
        carry: Default::default(),
        swell: None,
    });
    (sim, e)
}

/// Where the body floats: its height over the last two seconds of twenty,
/// the middle of what is left of its bobbing.
fn draft(body: RigidBody) -> f64 {
    let (mut sim, e) = sea_with(body);
    let mut sum = 0.0;
    for tick in 0..1200 {
        sim.step_tick(1.0 / 60.0, None);
        if tick >= 1080 {
            sum += sim.body_states().find(|s| s.entity == e).unwrap().pos.y;
        }
    }
    sum / 120.0
}

/// A wooden crate, one metre a side and 500 kg: half the water's density
/// by its own volume.
fn crate_body(rotates: bool) -> RigidBody {
    RigidBody { kind: BodyKind::Box, half_extents: [0.5, 0.5, 0.5], mass: 500.0, rotates, ..Default::default() }
}

/// **A crate with rotates off floats where the same crate with it on
/// floats.** It sank at full gravity: twenty seconds in, it was two
/// kilometres down.
#[test]
fn a_crate_floats_at_one_draft_whether_or_not_it_rotates() {
    let (still, turning) = (draft(crate_body(false)), draft(crate_body(true)));
    assert!(turning.abs() < 0.87, "fixture: the rotating crate floats at y {turning}, not at the surface");
    assert!((still - turning).abs() < 0.01, "the crate that does not rotate floats at y {still}, the one that does at y {turning}");
}

/// **The play-time density check sizes a crate the way the water does.**
/// Dropped into the sea, the crate's ratio is the same number rotating or not: the
/// water's density against the crate's over its bounding sphere, which is
/// what decides how high it floats.
#[test]
fn the_density_ratio_of_a_crate_is_the_one_it_floats_by() {
    let r = 0.75f32.sqrt();
    let want = 1000.0 * (4.0 / 3.0 * std::f32::consts::PI * r * r * r) / 500.0;
    for rotates in [false, true] {
        let (mut sim, _) = sea_with(crate_body(rotates));
        // Read the first tick its centre is under: the check reports only
        // bodies whose centre is wet, and a crate that floats bobs out again.
        let mut ratios = Vec::new();
        for _ in 0..120 {
            sim.step_tick(1.0 / 60.0, None);
            ratios = sim.water_density_ratios();
            if !ratios.is_empty() {
                break;
            }
        }
        assert_eq!(ratios.len(), 1, "rotates = {rotates}: the crate never went under: {ratios:?}");
        let ratio = ratios[0].4;
        assert!((ratio - want).abs() < 1e-3 * want, "rotates = {rotates}: a ratio of {ratio}, the crate floats by {want}");
    }
}
