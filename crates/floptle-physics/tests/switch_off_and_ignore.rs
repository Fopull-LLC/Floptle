//! A switched-off node leaves the simulation with its body, and two named
//! movers can be told to pass through each other.

use floptle_core::math::{DVec3, Vec3};
use floptle_core::{BodyKind, Disabled, Parent, RigidBody, Transform, World};
use floptle_physics::{GravityField, Sim};

const DT: f32 = 1.0 / 60.0;

fn ball(ecs: &mut World, at: DVec3, gravity: bool) -> floptle_core::Entity {
    let e = ecs.spawn();
    ecs.insert(e, Transform::from_translation(at));
    ecs.insert(e, RigidBody { gravity, radius: 0.5, ..Default::default() });
    e
}

/// A one-part assembly: a 2 m crate.
fn crate_assembly(ecs: &mut World, at: DVec3) -> floptle_core::Entity {
    let root = ecs.spawn();
    ecs.insert(root, Transform::from_translation(at));
    ecs.insert(root, RigidBody { assembly: true, gravity: false, mass: 10.0, ..Default::default() });
    let part = ecs.spawn();
    ecs.insert(part, Transform::default());
    ecs.insert(part, Parent(root));
    ecs.insert(
        part,
        RigidBody { kind: BodyKind::Box, half_extents: [1.0, 1.0, 1.0], mass: 10.0, ..Default::default() },
    );
    root
}

fn build(ecs: &World, g: f32) -> Sim {
    Sim::build_layered(
        ecs,
        &[],
        GravityField::uniform(Vec3::new(0.0, -g, 0.0)),
        DVec3::ZERO,
        floptle_core::Layers::default(),
    )
}

fn crate_pos(sim: &Sim, root: floptle_core::Entity) -> DVec3 {
    sim.compound_positions().into_iter().find(|(e, _)| *e == root.index()).expect("the crate's compound").1
}

fn off(ecs: &World) -> impl Fn(u32) -> bool + '_ {
    move |eid| {
        ecs.entity_with::<Transform>(eid).is_some_and(|e| floptle_core::is_disabled(ecs, e))
    }
}

#[test]
fn a_falling_body_switched_off_mid_air_stops_and_resumes_when_switched_on() {
    let mut ecs = World::default();
    let b = ball(&mut ecs, DVec3::new(0.0, 50.0, 0.0), true);
    let mut sim = build(&ecs, 10.0);
    for _ in 0..30 {
        sim.step_tick(DT, None);
    }
    let falling = sim.body_snapshot(b.index()).unwrap();
    assert!(falling.vel.y < -1.0, "the ball should be falling, vel {:?}", falling.vel);

    ecs.insert(b, Disabled);
    assert!(sim.sync_switched_off(off(&ecs)));
    for _ in 0..60 {
        sim.step_tick(DT, None);
    }
    let held = sim.body_snapshot(b.index()).unwrap();
    assert_eq!(held.pos, falling.pos, "a switched-off body moved");

    ecs.remove::<Disabled>(b);
    assert!(sim.sync_switched_off(off(&ecs)));
    for _ in 0..10 {
        sim.step_tick(DT, None);
    }
    let resumed = sim.body_snapshot(b.index()).unwrap();
    assert!(resumed.pos.y < held.pos.y - 0.5, "switched back on, it should fall again: {} -> {}", held.pos.y, resumed.pos.y);
}

#[test]
fn a_mover_passes_through_where_a_switched_off_body_was() {
    // The crate drifts through the ball's spot. With the ball on it is pushed
    // off its line; with the ball switched off it is not.
    let run = |switch_off: bool| {
        let mut ecs = World::default();
        let b = ball(&mut ecs, DVec3::new(0.0, 0.0, 0.0), false);
        let c = crate_assembly(&mut ecs, DVec3::new(-6.0, 0.4, 0.0));
        let mut sim = build(&ecs, 0.0);
        sim.set_compound_velocity(c.index(), Vec3::new(4.0, 0.0, 0.0));
        if switch_off {
            ecs.insert(b, Disabled);
            sim.sync_switched_off(off(&ecs));
        }
        for _ in 0..180 {
            sim.step_tick(DT, None);
        }
        crate_pos(&sim, c)
    };
    let blocked = run(false);
    let through = run(true);
    assert!(through.x > 5.0, "the crate should have crossed the empty spot, x {}", through.x);
    assert!((through.y - 0.4).abs() < 1e-3, "nothing should have nudged it, y {}", through.y);
    assert!(
        (blocked - through).length() > 0.2,
        "the control: an active ball must change the crate's path ({blocked:?} vs {through:?})"
    );
}

#[test]
fn a_body_that_starts_switched_off_is_simulated_once_switched_on() {
    let mut ecs = World::default();
    let b = ball(&mut ecs, DVec3::new(0.0, 50.0, 0.0), true);
    ecs.insert(b, Disabled);
    let mut sim = build(&ecs, 10.0);
    assert!(sim.body_snapshot(b.index()).is_none(), "built switched off, it has no body");
    ecs.remove::<Disabled>(b);
    sim.sync_switched_off(off(&ecs));
    sim.adopt_switched_on(&ecs);
    for _ in 0..30 {
        sim.step_tick(DT, None);
    }
    let s = sim.body_snapshot(b.index()).expect("switched on, it has a body");
    assert!(s.pos.y < 49.0, "and it falls, y {}", s.pos.y);
}

#[test]
fn an_ignored_pair_overlaps_untouched_until_its_time_runs_out() {
    // A ball sitting inside a crate. Ignored, neither moves; once the second
    // runs out, the pair pass pushes them apart.
    let mut ecs = World::default();
    let b = ball(&mut ecs, DVec3::new(0.3, 0.0, 0.0), false);
    let c = crate_assembly(&mut ecs, DVec3::ZERO);
    let mut sim = build(&ecs, 0.0);
    // A part of the assembly stands for the whole of it.
    let part = ecs.query::<Parent>().find(|(_, p)| p.0 == c).unwrap().0;
    assert!(sim.ignore_pair(b.index(), part.index(), Some(1.0)));
    let start = crate_pos(&sim, c);
    let ball_start = sim.body_snapshot(b.index()).unwrap().pos;
    for _ in 0..50 {
        sim.step_tick(DT, None);
    }
    assert_eq!(crate_pos(&sim, c), start, "an ignored pair pushed the crate");
    assert_eq!(sim.body_snapshot(b.index()).unwrap().pos, ball_start, "an ignored pair pushed the ball");
    for _ in 0..30 {
        sim.step_tick(DT, None);
    }
    assert!((crate_pos(&sim, c) - start).length() > 0.05, "after a second the pair should collide again");
}

#[test]
fn ignoring_a_node_with_no_body_changes_nothing_and_says_so() {
    let mut ecs = World::default();
    let b = ball(&mut ecs, DVec3::ZERO, false);
    let plain = ecs.spawn();
    ecs.insert(plain, Transform::default());
    let mut sim = build(&ecs, 0.0);
    assert!(!sim.ignore_pair(b.index(), plain.index(), None));
    assert!(sim.world.ignored_pairs.is_empty());
}
