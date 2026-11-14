//! A river declared as a ribbon, in the solver: what floats in it rides its
//! eased surface and is carried down the channel at its current.

use std::sync::Arc;

use floptle_core::math::{DVec3, Vec3};
use floptle_core::water::{DeclaredWater, Surface};
use floptle_core::water_ribbon::{Bed, Ribbon, RibbonPoint};
use floptle_core::{RigidBody, Transform, World};
use floptle_physics::{GravityField, GravitySource, Sim, WaterShape, WaterVolume};

/// A river 200 m long running east, falling from 10 m to 6 m, widening from
/// 3 m to 5 m to each bank, 3 m deep, running 1 m/s at its head and 2 m/s at
/// its foot, and one ball over it.
fn river_with(ball: DVec3) -> (Sim, floptle_core::Entity) {
    let mut ecs = World::default();
    let e = ecs.spawn();
    ecs.insert(e, Transform::from_translation(ball));
    // A little lighter than the water, and small, so the water's drag has
    // it at the current's speed within seconds.
    let r = 0.1f32;
    let mass = 900.0 * 4.0 / 3.0 * std::f32::consts::PI * r * r * r;
    ecs.insert(e, RigidBody { radius: r, mass, ..Default::default() });
    let gravity = GravityField { sources: vec![GravitySource::Uniform(Vec3::new(0.0, -9.81, 0.0))] };
    let mut sim = Sim::build(&ecs, &[], gravity, DVec3::ZERO);
    let course = Ribbon::new(
        vec![
            RibbonPoint { pos: DVec3::new(0.0, 10.0, 0.0), half_width: 3.0, depth: 3.0, current: 1.0 },
            RibbonPoint { pos: DVec3::new(200.0, 6.0, 0.0), half_width: 5.0, depth: 3.0, current: 2.0 },
        ],
        true,
        Bed::Flat,
    )
    .unwrap();
    let water = DeclaredWater {
        id: 1,
        anchor: None,
        frame: floptle_core::scatter::Frame::IDENTITY,
        surface: Surface::Ribbon { course: Arc::new(course), lift: 0.0 },
        density: 1000.0,
        drag: 1.0,
        angular_drag: 1.0,
        current: Vec3::ZERO,
        floor: f64::INFINITY,
        look: None,
        simulated: None,
        swell: None,
        time: 0.0,
    };
    sim.world.water.volumes.push(WaterVolume {
        shape: WaterShape::Declared { water: Arc::new(water), origin: DVec3::ZERO },
        density: 1000.0,
        drag: 1.0,
        angular_drag: 1.0,
        frozen: false,
        entity: u32::MAX - 1,
        carry: Default::default(),
        swell: None,
    });
    (sim, e)
}

/// **A ball dropped in a river floats at its surface and drifts downstream
/// at its current.** Dropped from 1 m over the middle of the channel 50 m
/// down a river that falls 4 m in 200, it splashes once; ten seconds later
/// it rides the surface where the river has it by then, lower than where it
/// went in, and moves at the current there, which is between the 1 m/s of
/// the head and the 2 m/s of the foot and faster than where it went in.
#[test]
fn a_ball_dropped_in_a_river_floats_at_its_surface_and_drifts_at_its_current() {
    let (mut sim, ball) = river_with(DVec3::new(50.0, 10.0, 0.0));
    let state = |sim: &Sim| sim.body_states().find(|s| s.entity == ball).unwrap();
    let mut splashes = Vec::new();
    for _ in 0..600 {
        sim.step_tick(1.0 / 60.0, None);
        splashes.extend(sim.take_splashes());
    }
    let s = state(&sim);
    // The river's surface and current where the ball is now: both run
    // straight from the head's to the foot's.
    let (surface, current) = (10.0 - 4.0 * s.pos.x / 200.0, 1.0 + s.pos.x / 200.0);
    assert!(s.pos.x > 58.0 && s.pos.x < 64.0, "in 10 s it drifted from 50 to {}", s.pos.x);
    assert!((s.pos.y - surface).abs() < 0.1, "it floats at {} on a surface at {surface}", s.pos.y);
    assert!(surface < 9.0 - 0.15, "the surface it rides fell with the river: {surface}");
    assert!((s.vel.x as f64 - current).abs() < 0.06, "it moves at {} m/s in a current of {current}", s.vel.x);
    assert!(s.pos.z.abs() < 0.01, "it left the middle of the channel: {}", s.pos.z);
    let entering: Vec<_> = splashes.iter().filter(|e| e.entering).collect();
    assert_eq!(entering.len(), 1, "{splashes:?}");
    assert_eq!(entering[0].water, u32::MAX - 1);
    assert!((entering[0].pos.y - 9.0).abs() < 0.05, "it went in at the surface 50 m down: {:?}", entering[0].pos);
}

/// **Beside the river there is no water.** The same ball dropped 4 m from
/// the middle of a channel 3.5 m to its bank there falls straight past the
/// river's level and keeps falling.
#[test]
fn a_ball_dropped_past_the_bank_falls_through() {
    let (mut sim, ball) = river_with(DVec3::new(50.0, 11.0, 4.0));
    for _ in 0..120 {
        sim.step_tick(1.0 / 60.0, None);
    }
    let s = sim.body_states().find(|s| s.entity == ball).unwrap();
    assert!(s.pos.y < 0.0 && s.vel.y < -15.0, "it stopped at {} moving {}", s.pos.y, s.vel.y);
    assert!(sim.take_splashes().is_empty());
}
