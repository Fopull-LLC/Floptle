//! Joints reach assemblies: a craft built from sub-assemblies articulates, a
//! rover rolls on wheels hung from an assembly chassis, and a craft grips the
//! ground it lands on.

use floptle_core::joint::{Joint, JointKind};
use floptle_core::math::{DVec3, Quat, Vec3};
use floptle_core::{BodyKind, BodyMode, Name, Parent, RigidBody, Transform, World};
use floptle_physics::{GravityField, Sim};

const DT: f32 = 1.0 / 60.0;

fn build(ecs: &World) -> Sim {
    Sim::build_layered(
        ecs,
        &[],
        GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)),
        DVec3::ZERO,
        floptle_core::Layers::default(),
    )
}

/// An assembly root named `name` at `at` with one box part.
fn segment(ecs: &mut World, name: &str, at: DVec3, half: [f32; 3], mass: f32) -> floptle_core::Entity {
    let root = ecs.spawn();
    ecs.insert(root, Transform::from_translation(at));
    ecs.insert(root, Name(name.into()));
    ecs.insert(root, RigidBody { assembly: true, ..Default::default() });
    let part = ecs.spawn();
    ecs.insert(part, Transform::default());
    ecs.insert(part, Parent(root));
    ecs.insert(part, RigidBody { kind: BodyKind::Box, half_extents: half, mass, ..Default::default() });
    root
}

fn com(sim: &Sim, e: floptle_core::Entity) -> Vec3 {
    sim.compound_of(e.index()).expect("compound").pos
}

fn run(sim: &mut Sim, ecs: &mut World, secs: f32) {
    for _ in 0..(secs / DT).round() as usize {
        sim.sync_joints(ecs);
        sim.step_tick(DT, None);
    }
}

/// A three-segment arm of sub-assemblies, hinged to the world and to each
/// other with limits and braked motors, holding a heavy load: it sags onto
/// its limits, comes to rest there and stays without jitter, and every hinge
/// reads its angle.
#[test]
fn a_three_segment_arm_holds_a_load_at_its_limits_without_jitter() {
    let mut ecs = World::default();
    let mut prev: Option<String> = None;
    let mut segs = Vec::new();
    for i in 0..3 {
        let name = format!("Seg{i}");
        let mass = if i == 2 { 20.0 } else { 2.0 };
        let s = segment(&mut ecs, &name, DVec3::new(1.0 + 2.0 * i as f64, 5.0, 0.0), [1.0, 0.1, 0.1], mass);
        // Hinged at its left end to the previous segment's right end (the
        // world for the first), about Z, within ±20°.
        ecs.insert(
            s,
            Joint {
                kind: JointKind::Hinge,
                other: prev.clone().unwrap_or_default(),
                anchor: [-1.0, 0.0, 0.0],
                limit: true,
                min: -20.0,
                max: 20.0,
                // A held motor is the joint's friction: a robotic arm's
                // brake, too weak to hold the load up, strong enough to
                // stop the swinging.
                motor: true,
                motor_speed: 0.0,
                max_force: 100.0,
                ..Default::default()
            },
        );
        prev = Some(name);
        segs.push(s);
    }
    let mut sim = build(&ecs);
    run(&mut sim, &mut ecs, 8.0);
    let rest: Vec<Vec3> = segs.iter().map(|&s| com(&sim, s)).collect();
    run(&mut sim, &mut ecs, 1.0);
    for (k, &s) in segs.iter().enumerate() {
        let moved = (com(&sim, s) - rest[k]).length();
        assert!(moved < 2e-3, "segment {k} still moving {moved} m a second after settling");
        let a = ecs.get::<Joint>(s).unwrap().position;
        assert!(a.abs() <= 21.0, "segment {k} hinge at {a}°, past its ±20° limit");
    }
    // Under the load the arm sags: each hinge sits at its lower limit.
    let a0 = ecs.get::<Joint>(segs[0]).unwrap().position;
    assert!((a0 + 20.0).abs() < 1.5, "the root hinge should hang at its -20° limit, reads {a0}°");
    // The load hangs below where the arm started: it is really held by the joints.
    assert!(com(&sim, segs[2]).y < 5.0 && com(&sim, segs[2]).y > 2.0, "load at {:?}", com(&sim, segs[2]));
}

/// A servo hinge between two sub-assemblies of one craft drives to its goal
/// and holds it; a piston (slider servo) extends to its goal.
#[test]
fn a_servo_and_a_piston_between_sub_assemblies_reach_their_goals() {
    let mut ecs = World::default();
    let base = segment(&mut ecs, "Base", DVec3::new(0.0, 0.0, 0.0), [0.5, 0.5, 0.5], 50.0);
    ecs.insert(base, Joint { kind: JointKind::Fixed, ..Default::default() });
    let arm = segment(&mut ecs, "Arm", DVec3::new(1.5, 0.0, 0.0), [1.0, 0.1, 0.1], 1.0);
    ecs.insert(
        arm,
        Joint {
            kind: JointKind::Hinge,
            other: "Base".into(),
            anchor: [-1.0, 0.0, 0.0],
            motor: true,
            servo: true,
            goal: 30.0,
            motor_speed: 90.0,
            max_force: 500.0,
            ..Default::default()
        },
    );
    let ram = segment(&mut ecs, "Ram", DVec3::new(-1.5, 0.0, 0.0), [0.4, 0.2, 0.2], 1.0);
    ecs.insert(
        ram,
        Joint {
            kind: JointKind::Slider,
            other: "Base".into(),
            axis: [1.0, 0.0, 0.0],
            motor: true,
            servo: true,
            goal: 0.75,
            motor_speed: 2.0,
            max_force: 500.0,
            ..Default::default()
        },
    );
    let mut sim = build(&ecs);
    run(&mut sim, &mut ecs, 3.0);
    let a = ecs.get::<Joint>(arm).unwrap().position;
    assert!((a - 30.0).abs() < 1.0, "servo at {a}°, goal 30°");
    let s = ecs.get::<Joint>(ram).unwrap().position;
    assert!((s - 0.75).abs() < 0.02, "piston at {s}, goal 0.75");
    // A new goal, written the way a script writes it, with no rebuild.
    ecs.get_mut::<Joint>(arm).unwrap().goal = -10.0;
    run(&mut sim, &mut ecs, 2.0);
    let a = ecs.get::<Joint>(arm).unwrap().position;
    assert!((a + 10.0).abs() < 1.0, "servo at {a}°, new goal -10°");
}

/// A four-wheeled rover: an assembly chassis with sphere wheels hung from it
/// on wheel joints (suspension along the chassis's up, a drive motor). It
/// drives up a 20° slope, and with the motor held at zero it parks there
/// without creeping.
#[test]
fn a_four_wheel_rover_climbs_a_20_degree_slope_and_parks_without_creeping() {
    let mut ecs = World::default();
    let tilt = Quat::from_rotation_z(20f32.to_radians());
    // A long 20° ramp rising toward +X, its top face through the origin.
    let ramp = ecs.spawn();
    ecs.insert(
        ramp,
        Transform { translation: (tilt * Vec3::new(0.0, -1.0, 0.0)).as_dvec3(), rotation: tilt, scale: Vec3::ONE },
    );
    ecs.insert(ramp, RigidBody { kind: BodyKind::Box, mode: BodyMode::Static, half_extents: [60.0, 1.0, 10.0], friction: 1.0, ..Default::default() });
    // Chassis, wheels and all, sat on the ramp and turned with it.
    let up = tilt * Vec3::Y;
    let along = tilt * Vec3::X;
    let centre = up * 0.9;
    let chassis = segment(&mut ecs, "Chassis", centre.as_dvec3(), [1.2, 0.2, 0.7], 40.0);
    ecs.get_mut::<Transform>(chassis).unwrap().rotation = tilt;
    let mut wheels = Vec::new();
    for (dx, dz) in [(-1.0f32, -0.9f32), (1.0, -0.9), (-1.0, 0.9), (1.0, 0.9)] {
        let w = ecs.spawn();
        let at = centre + along * dx + tilt * Vec3::new(0.0, -0.45, dz);
        ecs.insert(w, Transform { translation: at.as_dvec3(), rotation: tilt, scale: Vec3::ONE });
        ecs.insert(w, RigidBody { radius: 0.45, mass: 5.0, friction: 1.0, rotates: true, ..Default::default() });
        ecs.insert(
            w,
            Joint {
                kind: JointKind::Wheel,
                other: "Chassis".into(),
                axis: [0.0, 1.0, 0.0],
                stiffness: 3.0,
                damping: 0.8,
                motor: true,
                motor_speed: -120.0,
                max_force: 400.0,
                ..Default::default()
            },
        );
        wheels.push(w);
    }
    let mut sim = build(&ecs);
    let start = com(&sim, chassis);
    run(&mut sim, &mut ecs, 4.0);
    let climbed = (com(&sim, chassis) - start).dot(along);
    assert!(climbed > 2.0, "the rover should drive up the ramp: it went {climbed} m along it");
    for &w in &wheels {
        ecs.get_mut::<Joint>(w).unwrap().motor_speed = 0.0;
    }
    // Braking from a climb takes a moment; then it must hold still.
    run(&mut sim, &mut ecs, 3.0);
    let parked = com(&sim, chassis);
    run(&mut sim, &mut ecs, 3.0);
    let creep = (com(&sim, chassis) - parked).length();
    assert!(creep < 0.02, "parked on the slope it crept {creep} m in 3 s");
}

/// A craft gripped to the ground it stands on holds there while the ground
/// is carried (a planet on its rails moving it and its crust together), and
/// falls once released.
#[test]
fn a_gripped_lander_rides_its_ground_and_falls_when_released() {
    let mut ecs = World::default();
    let pad = ecs.spawn();
    ecs.insert(pad, Transform::from_translation(DVec3::new(0.0, -1.0, 0.0)));
    ecs.insert(pad, RigidBody { kind: BodyKind::Box, mode: BodyMode::Static, half_extents: [5.0, 1.0, 5.0], ..Default::default() });
    // Hung under the pad's edge, so gravity pulls it off unless it grips.
    let lander = segment(&mut ecs, "Lander", DVec3::new(5.4, -0.5, 0.0), [0.4, 0.4, 0.4], 1.0);
    let mut sim = build(&ecs);
    assert!(sim.grip(lander.index(), DVec3::new(5.0, -0.5, 0.0), None, 0.0));
    assert!(sim.gripped(lander.index()));
    let rel0 = com(&sim, lander);
    let carry = Vec3::new(0.05, 0.0, 0.02);
    for _ in 0..180 {
        let origin = sim.world.origin;
        for c in sim.world.colliders.iter_mut() {
            let a = c.anchor + carry.as_dvec3();
            c.re_anchor(a, origin);
        }
        sim.shift_compound(lander.index(), carry.as_dvec3());
        sim.sync_joints(&mut ecs);
        sim.step_tick(DT, None);
    }
    let pad_moved = carry * 180.0;
    let off = (com(&sim, lander) - pad_moved - rel0).length();
    assert!(off < 0.05, "gripped, it should hold its place on the moving pad: {off} m off");
    sim.release_grips(lander.index());
    assert!(!sim.gripped(lander.index()));
    run(&mut sim, &mut ecs, 1.0);
    assert!(com(&sim, lander).y < rel0.y - 1.0, "released, it should fall: {:?}", com(&sim, lander));
}
