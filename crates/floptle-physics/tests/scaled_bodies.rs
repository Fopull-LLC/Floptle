//! A body's every size and offset is in its node's space: a sphere's radius,
//! a capsule's radius and height, and where a joint holds it, all scale and
//! turn with the node, the way a box's half-extents always did.

use floptle_core::joint::{Joint, JointKind};
use floptle_core::math::{DVec3, Quat, Vec3};
use floptle_core::{BodyKind, BodyMode, RigidBody, Transform, World};
use floptle_physics::{GravityField, Sim, StaticTag};

const DT: f32 = 1.0 / 60.0;

fn build(ecs: &World) -> Sim {
    Sim::build_layered(ecs, &[], GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)), DVec3::ZERO, Default::default())
}

fn floor(sim: &mut Sim) {
    sim.add_static_box(DVec3::new(0.0, -0.5, 0.0), Vec3::new(20.0, 0.5, 20.0), Quat::IDENTITY, StaticTag { layer: 0, eid: 900, sensor: false });
}

fn run(sim: &mut Sim, ecs: &mut World, secs: f32) {
    for _ in 0..(secs / DT).round() as usize {
        sim.sync_dynamic_params(ecs);
        sim.sync_joints(ecs);
        sim.step_tick(DT, None);
    }
}

fn node(ecs: &mut World, at: DVec3, rot: Quat, scale: Vec3, rb: RigidBody) -> floptle_core::Entity {
    let e = ecs.spawn();
    ecs.insert(e, Transform { translation: at, rotation: rot, scale });
    ecs.insert(e, rb);
    e
}

/// A ball scaled 2 rests with its centre at twice its radius; a capsule
/// stretched to 1.5 along Y and thinned to 0.5 across rests a scaled half
/// height up; a sphere scaled unevenly takes its largest axis; and a Static
/// sphere is the size it is scaled to.
#[test]
fn round_bodies_scale_with_their_node() {
    let mut ecs = World::default();
    let ball = node(&mut ecs, DVec3::new(0.0, 3.0, 0.0), Quat::IDENTITY, Vec3::splat(2.0), RigidBody { radius: 0.5, ..Default::default() });
    let capsule = node(
        &mut ecs,
        DVec3::new(4.0, 4.0, 0.0),
        Quat::IDENTITY,
        Vec3::new(0.5, 1.5, 0.5),
        RigidBody { kind: BodyKind::Capsule, radius: 0.5, height: 2.0, ..Default::default() },
    );
    let squashed = node(&mut ecs, DVec3::new(-4.0, 3.0, 0.0), Quat::IDENTITY, Vec3::new(1.0, 0.5, 3.0), RigidBody { radius: 0.5, ..Default::default() });
    // A Static sphere scaled 4 is a dome 2 high a ball lands on.
    node(&mut ecs, DVec3::new(0.0, 0.0, 6.0), Quat::IDENTITY, Vec3::splat(4.0), RigidBody { radius: 0.5, mode: BodyMode::Static, ..Default::default() });
    let on_dome = node(&mut ecs, DVec3::new(0.0, 4.0, 6.0), Quat::IDENTITY, Vec3::ONE, RigidBody { radius: 0.25, lock_pos: [true, false, true], ..Default::default() });
    let mut sim = build(&ecs);
    floor(&mut sim);
    run(&mut sim, &mut ecs, 3.0);
    let y = |e: floptle_core::Entity| sim.body_snapshot(e.index()).unwrap().pos.y as f32;
    assert!((y(ball) - 1.0).abs() < 0.03, "ball centre {:.3}, want 1.0", y(ball));
    assert!((y(capsule) - 1.5).abs() < 0.03, "capsule centre {:.3}, want 1.5", y(capsule));
    assert!((y(squashed) - 1.5).abs() < 0.03, "uneven sphere takes the largest axis: {:.3}, want 1.5", y(squashed));
    assert!((y(on_dome) - 2.25).abs() < 0.03, "ball on the dome at {:.3}, want 2.25", y(on_dome));
}

/// A joint's anchor is where on the node, in the node's space: a ball held
/// by a rod to a hook above it, by an anchor half a unit along its X on a
/// node scaled 2 and turned a quarter turn about Z, hangs from a point one
/// unit straight above its centre, so it stays where it is. Read unscaled
/// and unturned, the rod met the ball half a unit to the side and the ball
/// swung out under the hook. The ball does not turn, so its own frame is
/// not its node's: the anchor has to be turned by the node regardless.
#[test]
fn a_joint_anchor_scales_and_turns_with_its_node() {
    let mut ecs = World::default();
    let hook = ecs.spawn();
    ecs.insert(hook, Transform::from_translation(DVec3::new(0.0, 8.0, 0.0)));
    ecs.insert(hook, floptle_core::Name("Hook".into()));
    let rot = Quat::from_rotation_z(std::f32::consts::FRAC_PI_2);
    let e = node(&mut ecs, DVec3::new(0.0, 5.0, 0.0), rot, Vec3::splat(2.0), RigidBody { radius: 0.2, ..Default::default() });
    ecs.insert(e, Joint { kind: JointKind::Distance, other: "Hook".into(), anchor: [0.5, 0.0, 0.0], length: -1.0, ..Default::default() });
    let mut sim = build(&ecs);
    let mut worst = 0.0f64;
    for _ in 0..180 {
        run(&mut sim, &mut ecs, DT);
        let p = sim.body_snapshot(e.index()).unwrap().pos;
        worst = worst.max(p.x.abs()).max((p.y - 5.0).abs());
    }
    assert!(worst < 0.05, "the ball moved {worst:.3}: its rod did not meet it one unit above its centre");
}

/// A slider's axis is in the other node's space, so it turns with that node:
/// held to an empty turned a quarter turn about Z, an axis along its X runs
/// up and down the world, and a ball on it falls. Read in the world's axes,
/// the slider ran sideways and held the ball up.
#[test]
fn a_joint_axis_turns_with_the_node_it_is_in() {
    let mut ecs = World::default();
    let rail = ecs.spawn();
    ecs.insert(rail, Transform { translation: DVec3::new(0.0, 5.0, 0.0), rotation: Quat::from_rotation_z(std::f32::consts::FRAC_PI_2), scale: Vec3::ONE });
    ecs.insert(rail, floptle_core::Name("Rail".into()));
    let e = node(&mut ecs, DVec3::new(0.0, 5.0, 0.0), Quat::IDENTITY, Vec3::ONE, RigidBody { radius: 0.2, ..Default::default() });
    ecs.insert(e, Joint { kind: JointKind::Slider, other: "Rail".into(), axis: [1.0, 0.0, 0.0], ..Default::default() });
    let mut sim = build(&ecs);
    run(&mut sim, &mut ecs, 0.5);
    let p = sim.body_snapshot(e.index()).unwrap().pos;
    assert!(p.y < 4.0 && p.x.abs() < 0.05, "the ball should fall along the turned rail: {p:?}");
}

/// The other end's anchor is in the other node's space too, scale included:
/// a hook scaled 2 whose anchor is a quarter unit along its X holds a rod
/// half a unit out, straight above the ball hung from it.
#[test]
fn the_other_anchor_scales_with_the_other_node() {
    let mut ecs = World::default();
    let hook = ecs.spawn();
    ecs.insert(hook, Transform { translation: DVec3::new(0.0, 8.0, 0.0), rotation: Quat::IDENTITY, scale: Vec3::splat(2.0) });
    ecs.insert(hook, floptle_core::Name("Hook".into()));
    let e = node(&mut ecs, DVec3::new(0.5, 5.0, 0.0), Quat::IDENTITY, Vec3::ONE, RigidBody { radius: 0.2, ..Default::default() });
    ecs.insert(
        e,
        Joint { kind: JointKind::Distance, other: "Hook".into(), other_anchor: [0.25, 0.0, 0.0], length: -1.0, ..Default::default() },
    );
    let mut sim = build(&ecs);
    let mut worst = 0.0f64;
    for _ in 0..180 {
        run(&mut sim, &mut ecs, DT);
        let p = sim.body_snapshot(e.index()).unwrap().pos;
        worst = worst.max((p.x - 0.5).abs());
    }
    assert!(worst < 0.05, "the ball swung {worst:.3} sideways: the hook's end was not half a unit out");
}
