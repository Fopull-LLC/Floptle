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

/// A segment on a ball joint, hung from the world by one end and started
/// leaning out both ways: it swings round in three dimensions (a hinge
/// would keep it in one plane), its end never leaves the pivot, and with a
/// 25° cone about where it started it never leans past 25°, reading its lean
/// back as `position`.
fn ball_swing(start_lean: f32, cone: Option<f32>) -> (f32, f32, f32, f32) {
    let mut ecs = World::default();
    let s = segment(&mut ecs, "Arm", DVec3::new(0.0, 4.0, 0.0), [0.1, 1.0, 0.1], 2.0);
    // Leaning 35° from straight down, about a tilted axis, and pushed
    // sideways so it goes round rather than back and forth.
    let tilt = Quat::from_axis_angle(Vec3::new(1.0, 0.0, 1.0).normalize(), start_lean.to_radians());
    let pivot = Vec3::new(0.0, 5.0, 0.0);
    let centre = pivot + tilt * Vec3::new(0.0, -1.0, 0.0);
    ecs.get_mut::<Transform>(s).unwrap().translation = centre.as_dvec3();
    ecs.get_mut::<Transform>(s).unwrap().rotation = tilt;
    ecs.insert(
        s,
        Joint {
            kind: JointKind::Ball,
            anchor: [0.0, 1.0, 0.0],
            axis: [0.0, 1.0, 0.0],
            limit: cone.is_some(),
            max: cone.unwrap_or(0.0),
            ..Default::default()
        },
    );
    let mut sim = build(&ecs);
    sim.set_compound_velocity(s.index(), Vec3::new(-1.5, 0.0, 1.5 + 2.0 * (start_lean == 0.0) as i32 as f32));
    let (mut gap, mut lean, mut read) = (0.0f32, 0.0f32, 0.0f32);
    let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
    for _ in 0..240 {
        sim.sync_joints(&mut ecs);
        sim.step_tick(DT, None);
        let c = sim.compound_of(s.index()).unwrap();
        let top = c.pos + c.orient * Vec3::Y;
        gap = gap.max((top - pivot).length());
        // How far the segment leans from where it would hang at rest.
        lean = lean.max((c.orient * Vec3::Y).dot(Vec3::Y).clamp(-1.0, 1.0).acos().to_degrees());
        read = read.max(ecs.get::<Joint>(s).unwrap().position);
        lo = lo.min(c.pos);
        hi = hi.max(c.pos);
    }
    let spread = (hi - lo).x.min((hi - lo).z);
    (gap, lean, read, spread)
}

#[test]
fn a_ball_joint_swings_every_way_about_its_pivot_and_holds_its_cone() {
    let (gap, lean, _, spread) = ball_swing(35.0, None);
    assert!(gap < 0.02, "the free ball joint let its end drift {gap} m off the pivot");
    assert!(spread > 0.5, "it swung in one plane only ({spread} m across the other)");
    assert!(lean > 30.0, "a free ball joint should swing out as far as it started: {lean}°");
    let (gap, lean, read, _) = ball_swing(0.0, Some(25.0));
    assert!(gap < 0.02, "the coned ball joint let its end drift {gap} m off the pivot");
    assert!(lean > 20.0 && lean < 27.0, "the 25° cone let it lean {lean}°");
    assert!((read - lean).abs() < 1.5 && read > 20.0, "position reads {read}°, it leant {lean}°");
}

/// A body that is not last in the list is destroyed while two others are
/// jointed: the joint follows its body to the slot the removal moved it to,
/// and rebuilding the joints in the same frame neither panics nor lets go.
#[test]
fn destroying_a_body_beside_a_joint_keeps_the_joint_on_its_bodies() {
    let mut ecs = World::default();
    let body = |ecs: &mut World, name: &str, at: Vec3| {
        let e = ecs.spawn();
        ecs.insert(e, Transform::from_translation(at.as_dvec3()));
        ecs.insert(e, Name(name.into()));
        ecs.insert(e, RigidBody { radius: 0.3, ..Default::default() });
        e
    };
    let doomed = body(&mut ecs, "Doomed", Vec3::new(10.0, 5.0, 0.0));
    let hook = body(&mut ecs, "Hook", Vec3::new(0.0, 5.0, 0.0));
    ecs.get_mut::<RigidBody>(hook).unwrap().mode = BodyMode::Kinematic;
    let bob = body(&mut ecs, "Bob", Vec3::new(0.0, 3.0, 0.0));
    ecs.insert(bob, Joint { kind: JointKind::Rope, other: "Hook".into(), length: 2.0, ..Default::default() });
    let mut sim = build(&ecs);
    run(&mut sim, &mut ecs, 0.5);
    sim.remove_body(doomed.index());
    ecs.despawn(doomed);
    run(&mut sim, &mut ecs, 2.0);
    let b = sim.body_states().find(|r| r.entity == bob).unwrap().pos;
    assert!((b - DVec3::new(0.0, 5.0, 0.0)).length() < 2.05, "the rope let go: the bob is at {b:?}");
}

/// A hinge made while its nodes are drawn a carry behind their bodies (a
/// planet on its rails moved them both this tick) pivots where it was asked
/// to, on the bodies, not where the nodes were last drawn.
#[test]
fn a_joint_made_on_a_carried_craft_pivots_where_it_was_asked_to() {
    let mut ecs = World::default();
    let base = segment(&mut ecs, "Base", DVec3::new(0.0, 0.0, 0.0), [0.8, 0.15, 0.5], 2.0);
    let arm = segment(&mut ecs, "Arm", DVec3::new(1.8, 0.0, 0.0), [1.0, 0.08, 0.08], 0.3);
    let mut sim = Sim::build_layered(&ecs, &[], GravityField::uniform(Vec3::ZERO), DVec3::ZERO, floptle_core::Layers::default());
    sim.set_compound_anchored(base.index(), true);
    // This tick's carry: the bodies move on, the nodes are still where they
    // were drawn.
    let carry = DVec3::new(0.0, 1.5, 0.0);
    sim.shift_compound(base.index(), carry);
    sim.shift_compound(arm.index(), carry);
    ecs.insert(arm, Joint { kind: JointKind::Hinge, other: "Base".into(), anchor: [-1.0, 0.0, 0.0], ..Default::default() });
    let mut worst = 0.0f32;
    for tick in 0..60 {
        sim.sync_joints(&mut ecs);
        if tick == 0 {
            // Swing it: a hinge about the wrong point swings the arm's end
            // off the chassis.
            sim.set_compound_angular_velocity(arm.index(), Vec3::new(0.0, 0.0, 2.0));
        }
        sim.step_tick(DT, None);
        let b = sim.compound_of(base.index()).unwrap();
        let a = sim.compound_of(arm.index()).unwrap();
        let pivot = b.origin() + b.orient * Vec3::new(0.8, 0.0, 0.0);
        let end = a.origin() + a.orient * Vec3::new(-1.0, 0.0, 0.0);
        worst = worst.max((end - pivot).length());
    }
    assert!(worst < 0.01, "the hinge pivots {worst} m from where it was asked to");
}

/// A rover's build for [`park_creep`]: chassis mass and half extents,
/// wheel mass, radius and tyre friction, where the wheels hang (x along
/// the chassis, z across), the brake's torque, the suspension's stiffness.
struct Rover {
    chassis: f32,
    half: [f32; 3],
    wheel: f32,
    radius: f32,
    friction: f32,
    mount: (f32, f32),
    brake: f32,
    stiffness: f32,
}

/// How fast a braked rover creeps, m/s, parked on a `slope`° bank of voxel
/// terrain under gravity `g`, the bank carried a step at a time the way a
/// planet's rails carry its ground and everything on it.
fn park_creep(r: &Rover, slope: f32, g: f32) -> f32 {
    let tilt = Quat::from_rotation_z(slope.to_radians());
    let up = tilt * Vec3::Y;
    let along = tilt * Vec3::X;
    let mut field = floptle_field::ChunkField::new(0.75);
    field.fill_with(
        Vec3::new(-30.0, -16.0, -10.0),
        Vec3::new(30.0, 16.0, 10.0),
        |p| p.dot(up).max(-p.dot(up) - 4.0),
        |_| [0.5; 3],
    );
    let mut ecs = World::default();
    let centre = up * (2.0 * r.radius);
    let chassis = segment(&mut ecs, "Chassis", centre.as_dvec3(), r.half, r.chassis);
    ecs.get_mut::<Transform>(chassis).unwrap().rotation = tilt;
    let mut wheels = Vec::new();
    for (sx, sz) in [(-1.0f32, -1.0f32), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
        let w = ecs.spawn();
        let at = centre + along * (sx * r.mount.0) + tilt * Vec3::new(0.0, -r.radius, sz * r.mount.1);
        ecs.insert(w, Transform { translation: at.as_dvec3(), rotation: tilt, scale: Vec3::ONE });
        ecs.insert(w, RigidBody { radius: r.radius, mass: r.wheel, friction: r.friction, rotates: true, ..Default::default() });
        ecs.insert(
            w,
            Joint {
                kind: JointKind::Wheel,
                other: "Chassis".into(),
                axis: [0.0, 1.0, 0.0],
                stiffness: r.stiffness,
                damping: 0.8,
                motor: true,
                motor_speed: 0.0,
                max_force: r.brake,
                ..Default::default()
            },
        );
        wheels.push(w);
    }
    let mut sim = Sim::build_layered(
        &ecs,
        &[floptle_physics::TerrainVolume { eid: Some(9999), ..floptle_physics::TerrainVolume::new(DVec3::ZERO, &field) }],
        GravityField::uniform(Vec3::new(0.0, -g, 0.0)),
        DVec3::ZERO,
        floptle_core::Layers::default(),
    );
    // A planet's orbital speed, a tick at a time, with the world origin
    // following the rover as a camera on it would.
    let carry = DVec3::new(-1.47, 0.0, 3.75);
    let mut moved = DVec3::ZERO;
    let tick = |sim: &mut Sim, ecs: &mut World, moved: &mut DVec3| {
        *moved += carry;
        sim.set_collider_anchor(9999, *moved);
        sim.shift_compound(chassis.index(), carry);
        for &w in &wheels {
            sim.shift_body(w.index(), carry);
        }
        sim.sync_joints(ecs);
        sim.step_tick(DT, Some(*moved));
    };
    for _ in 0..180 {
        tick(&mut sim, &mut ecs, &mut moved);
    }
    let parked = com(&sim, chassis).as_dvec3() - moved;
    for _ in 0..360 {
        tick(&mut sim, &mut ecs, &mut moved);
    }
    ((com(&sim, chassis).as_dvec3() - moved - parked).length() / 6.0) as f32
}

/// Braked on a bank of voxel terrain carried on its rails, a rover holds
/// its place to under a millimetre a second: a heavy-wheeled one with tyres
/// that grip far more than the slope asks, one whose grip is close to the
/// slope's limit (where it used to let go and re-park a hair downhill every
/// few steps, creeping 25 mm a second), and a light one whose chassis
/// outweighs its tyres six to one. Past the limit it slides, as it should.
#[test]
fn a_braked_rover_holds_still_on_a_voxel_slope() {
    let heavy = Rover { chassis: 40.0, half: [1.2, 0.2, 0.7], wheel: 5.0, radius: 0.45, friction: 1.0, mount: (1.0, 0.9), brake: 400.0, stiffness: 3.0 };
    let creep = park_creep(&heavy, 20.0, 9.81);
    assert!(creep < 0.001, "the heavy rover crept {} mm a second", creep * 1000.0);
    let near_limit = Rover { friction: 0.5, ..heavy };
    let creep = park_creep(&near_limit, 20.0, 9.81);
    assert!(creep < 0.001, "on a slope close to its grip it crept {} mm a second", creep * 1000.0);
    let light = Rover { chassis: 0.6, half: [0.8, 0.15, 0.5], wheel: 0.1, radius: 0.35, friction: 1.0, mount: (0.7, 0.65), brake: 0.6, stiffness: 5.0 };
    let creep = park_creep(&light, 21.0, 6.86);
    assert!(creep < 0.001, "the light rover crept {} mm a second", creep * 1000.0);
    let too_steep = Rover { friction: 0.3, ..heavy };
    let creep = park_creep(&too_steep, 20.0, 9.81);
    assert!(creep > 0.02, "tyres that cannot hold a 20° slope held it (creep {} mm a second)", creep * 1000.0);
}

/// A rover whose front wheels are steered turns as it drives, and one whose
/// wheels are straight keeps its heading.
fn rover_heading_after_driving(steer: f32) -> f32 {
    let mut ecs = World::default();
    let floor = ecs.spawn();
    ecs.insert(floor, Transform::from_translation(DVec3::new(0.0, -1.0, 0.0)));
    ecs.insert(floor, RigidBody { kind: BodyKind::Box, mode: BodyMode::Static, half_extents: [80.0, 1.0, 80.0], friction: 1.0, ..Default::default() });
    let chassis = segment(&mut ecs, "Chassis", DVec3::new(0.0, 0.9, 0.0), [1.2, 0.2, 0.7], 40.0);
    for (dx, dz) in [(-1.0f64, -0.9f64), (1.0, -0.9), (-1.0, 0.9), (1.0, 0.9)] {
        let w = ecs.spawn();
        ecs.insert(w, Transform::from_translation(DVec3::new(dx, 0.45, dz)));
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
                motor_speed: -180.0,
                max_force: 400.0,
                // The front pair steers.
                steer: if dx > 0.0 { steer } else { 0.0 },
                ..Default::default()
            },
        );
    }
    let mut sim = build(&ecs);
    run(&mut sim, &mut ecs, 4.0);
    let fwd = sim.compound_of(chassis.index()).unwrap().orient * Vec3::X;
    fwd.z.atan2(fwd.x).to_degrees().abs()
}

#[test]
fn a_rover_with_steered_front_wheels_turns() {
    let straight = rover_heading_after_driving(0.0);
    assert!(straight < 2.0, "with straight wheels it turned {straight}°");
    let turned = rover_heading_after_driving(25.0);
    assert!(turned > 20.0, "with the front wheels steered 25° it turned only {turned}°");
}

/// A lander dropped onto a 19° bank of voxel terrain carried on its rails,
/// gripping the ground the tick it touches down (as a landing claw does),
/// with a weld that lets go past `hold`. Optionally one of its parts is
/// re-posed every tick (a claw's moving fingers). Returns whether the grip
/// held to the end, how far the lander drifted along the ground over the
/// 60 s after it took hold, and its largest speed change in one tick.
fn claw_landing(hold: f32, repose: bool) -> (bool, f32, f32) {
    let tilt = Quat::from_rotation_z(19f32.to_radians());
    let up = tilt * Vec3::Y;
    let mut field = floptle_field::ChunkField::new(0.75);
    field.fill_with(
        Vec3::new(-20.0, -12.0, -10.0),
        Vec3::new(20.0, 12.0, 10.0),
        |p| p.dot(up).max(-p.dot(up) - 4.0),
        |_| [0.5; 3],
    );
    let mut ecs = World::default();
    let lander = segment(&mut ecs, "Lander", (up * 1.2).as_dvec3(), [0.5, 0.3, 0.5], 0.8);
    ecs.get_mut::<Transform>(lander).unwrap().rotation = tilt;
    let claw = ecs.spawn();
    ecs.insert(claw, Transform::from_translation(DVec3::new(0.0, -0.45, 0.0)));
    ecs.insert(claw, Parent(lander));
    ecs.insert(claw, RigidBody { kind: BodyKind::Box, half_extents: [0.1, 0.15, 0.1], mass: 0.04, ..Default::default() });
    let mut sim = Sim::build_layered(
        &ecs,
        &[floptle_physics::TerrainVolume { eid: Some(9999), ..floptle_physics::TerrainVolume::new(DVec3::ZERO, &field) }],
        GravityField::uniform(Vec3::new(0.0, -6.5, 0.0)),
        DVec3::ZERO,
        floptle_core::Layers::default(),
    );
    let carry = DVec3::new(-1.47, 0.0, 3.75);
    let mut moved = DVec3::ZERO;
    let (mut gripped_at, mut rest, mut worst_kick) = (None, DVec3::ZERO, 0.0f32);
    let mut last_v = Vec3::ZERO;
    for tick in 0..(4 * 60 + 60 * 60) {
        moved += carry;
        sim.set_collider_anchor(9999, moved);
        sim.shift_compound(lander.index(), carry);
        if repose {
            // The fingers open and close: the part node moves under its root.
            let s = (tick as f32 * 0.2).sin() * 0.03;
            ecs.get_mut::<Transform>(claw).unwrap().translation = DVec3::new(s as f64, -0.45, 0.0);
            sim.resync_compound_shapes(lander.index(), &ecs);
        }
        sim.sync_joints(&mut ecs);
        sim.step_tick(DT, Some(moved));
        let (cpos, cvel) = {
            let c = sim.compound_of(lander.index()).unwrap();
            (c.pos, c.vel)
        };
        let world = cpos.as_dvec3() + sim.world.origin - moved;
        if let Some(at) = gripped_at {
            worst_kick = worst_kick.max((cvel - last_v).length());
            if tick == at + 30 {
                rest = world;
            }
        } else {
            let touching = sim.compound_impacts().into_iter().find(|i| i.0 == lander.index());
            if let Some(hit) = touching {
                // Grip the ground where it touched, as a claw's ray finds it.
                assert!(sim.grip(lander.index(), hit.5, None, hold));
                gripped_at = Some(tick);
                rest = world;
            }
        }
        last_v = cvel;
    }
    let c = sim.compound_of(lander.index()).unwrap();
    let drift = (c.pos.as_dvec3() + sim.world.origin - moved - rest).length() as f32;
    (sim.gripped(lander.index()) && gripped_at.is_some(), drift, worst_kick)
}

/// On a 19° slope of a planet moving on its rails, a lander that grips the
/// ground as it touches down holds there: the grip is not torn loose, it
/// drifts less than 5 cm in a minute, and a grip strong enough never to
/// break takes hold without a kick. The same with a part re-posed every tick.
#[test]
fn a_lander_gripping_a_carried_slope_at_touchdown_holds() {
    for repose in [false, true] {
        let (held, drift, _) = claw_landing(400.0, repose);
        assert!(held, "the grip let go (re-posed part: {repose})");
        assert!(drift < 0.05, "it drifted {drift} m on the slope in 60 s (re-posed part: {repose})");
        let (held, drift, kick) = claw_landing(1.0e9, repose);
        assert!(held && drift < 0.05, "an unbreakable grip held {held}, drifted {drift} m (re-posed part: {repose})");
        assert!(kick < 1.0, "taking hold kicked it {kick} m/s in a tick (re-posed part: {repose})");
    }
}

/// A grip is taken where the craft is: at its foot it holds, and a point
/// 50 m clear of it (worked out from somewhere the craft is not) is refused
/// rather than welded about a 50 m lever.
#[test]
fn a_grip_out_of_the_crafts_reach_is_refused() {
    let mut ecs = World::default();
    let pad = ecs.spawn();
    ecs.insert(pad, Transform::from_translation(DVec3::new(0.0, -1.0, 0.0)));
    ecs.insert(pad, RigidBody { kind: BodyKind::Box, mode: BodyMode::Static, half_extents: [100.0, 1.0, 100.0], ..Default::default() });
    let lander = segment(&mut ecs, "Lander", DVec3::new(0.0, 0.4, 0.0), [0.4, 0.4, 0.4], 1.0);
    let mut sim = build(&ecs);
    assert!(!sim.grip(lander.index(), DVec3::new(50.0, 0.0, 0.0), None, 0.0), "a grip 50 m off was taken");
    assert!(!sim.gripped(lander.index()));
    assert!(sim.grip(lander.index(), DVec3::new(0.0, 0.0, 0.0), None, 0.0), "a grip at its foot was refused");
}

/// Where each wheel of a rover parked on a 20° slope sits on its mount, in
/// the chassis's frame, after 13 s. Its wheel joints name no anchor on the
/// chassis, so each was made where its wheel stood. With `visitors`, another
/// craft arrives far away and leaves again, ten times, and each arrival and
/// departure rebuilds the joints.
fn parked_wheel_offsets(visitors: bool) -> Vec<Vec3> {
    let mut ecs = World::default();
    let tilt = Quat::from_rotation_z(20f32.to_radians());
    let ramp = ecs.spawn();
    ecs.insert(ramp, Transform { translation: (tilt * Vec3::new(0.0, -1.0, 0.0)).as_dvec3(), rotation: tilt, scale: Vec3::ONE });
    ecs.insert(ramp, RigidBody { kind: BodyKind::Box, mode: BodyMode::Static, half_extents: [60.0, 1.0, 10.0], friction: 1.0, ..Default::default() });
    let (up, along) = (tilt * Vec3::Y, tilt * Vec3::X);
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
                stiffness: 1.5,
                damping: 0.8,
                motor: true,
                motor_speed: 0.0,
                max_force: 400.0,
                ..Default::default()
            },
        );
        wheels.push(w);
    }
    let mut sim = build(&ecs);
    run(&mut sim, &mut ecs, 3.0);
    for k in 0..10 {
        if visitors {
            let visitor = segment(&mut ecs, "Visitor", DVec3::new(300.0 + k as f64 * 5.0, 50.0, 0.0), [0.3, 0.3, 0.3], 1.0);
            assert!(sim.add_compound_for(visitor, &ecs));
            run(&mut sim, &mut ecs, 0.5);
            sim.remove_compound(visitor.index());
            ecs.despawn(visitor);
            run(&mut sim, &mut ecs, 0.5);
        } else {
            run(&mut sim, &mut ecs, 1.0);
        }
    }
    let c = sim.compound_of(chassis.index()).unwrap();
    wheels
        .iter()
        .map(|w| {
            let p = sim.body_states().find(|r| r.entity == *w).unwrap().pos.as_vec3();
            c.orient.inverse() * (p - c.pos)
        })
        .collect()
}

/// Craft coming and going far from a parked rover leave its wheels where
/// they sit with nobody about, to a centimetre, rather than each rebuild of
/// the joints taking the suspension's sag of the moment as the new rest.
#[test]
fn a_joints_rest_survives_the_joints_being_rebuilt() {
    let (still, visited) = (parked_wheel_offsets(false), parked_wheel_offsets(true));
    for (k, (a, b)) in still.iter().zip(&visited).enumerate() {
        assert!((*a - *b).length() < 0.01, "wheel {k} is {} m from where it sits with no visitors: {a:?} -> {b:?}", (*a - *b).length());
    }
}

/// A four-wheeled rover parked on a floor and teleported a kilometre onto
/// another one arrives whole: the tick after, every tyre is within a
/// centimetre of where it sat on its mount, and neither a tyre nor the
/// chassis has been kicked, against the same rover woken where it stood.
/// (Parked for two seconds, the rover is asleep, and a sleeper's speed reads
/// nothing; its first tick awake is the yardstick, not its last asleep.)
#[test]
fn a_teleported_rover_takes_its_wheels_with_it() {
    let parked = || {
        let mut ecs = World::default();
        // The same floor here and a kilometre off.
        for x in [0.0, 1000.0] {
            let floor = ecs.spawn();
            ecs.insert(floor, Transform::from_translation(DVec3::new(x, -1.0, 0.0)));
            ecs.insert(floor, RigidBody { kind: BodyKind::Box, mode: BodyMode::Static, half_extents: [50.0, 1.0, 50.0], friction: 1.0, ..Default::default() });
        }
        let chassis = segment(&mut ecs, "Chassis", DVec3::new(0.0, 0.9, 0.0), [1.2, 0.2, 0.7], 40.0);
        let mut wheels = Vec::new();
        for (dx, dz) in [(-1.0f64, -0.9f64), (1.0, -0.9), (-1.0, 0.9), (1.0, 0.9)] {
            let w = ecs.spawn();
            ecs.insert(w, Transform::from_translation(DVec3::new(dx, 0.45, dz)));
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
                    motor_speed: 0.0,
                    max_force: 400.0,
                    ..Default::default()
                },
            );
            wheels.push(w);
        }
        let mut sim = build(&ecs);
        run(&mut sim, &mut ecs, 2.0);
        (sim, ecs, chassis, wheels)
    };
    let state = |sim: &Sim, chassis: floptle_core::Entity, wheels: &[floptle_core::Entity]| -> (Vec<Vec3>, Vec<Vec3>, Vec3) {
        let c = sim.compound_of(chassis.index()).unwrap();
        let (mut at, mut vel) = (Vec::new(), Vec::new());
        for w in wheels {
            let r = sim.body_states().find(|r| r.entity == *w).unwrap();
            at.push(c.orient.inverse() * (r.pos.as_vec3() - c.pos));
            vel.push(r.vel);
        }
        (at, vel, c.vel)
    };
    // Woken where it stands, for a tick.
    let (mut still, mut still_ecs, chassis, wheels) = parked();
    still.set_body_asleep(chassis.index(), false);
    for w in &wheels {
        still.set_body_asleep(w.index(), false);
    }
    still.sync_joints(&mut still_ecs);
    still.step_tick(DT, None);
    let (_, vel0, cv0) = state(&still, chassis, &wheels);
    // Set down a kilometre off, standing on the far floor as it stood here.
    let (mut sim, mut ecs, chassis, wheels) = parked();
    let (at0, _, _) = state(&sim, chassis, &wheels);
    let origin = sim.world.origin;
    let to = sim.compound_of(chassis.index()).unwrap().origin().as_dvec3() + origin + DVec3::new(1000.0, 0.0, 0.0);
    let moved = sim.set_compound_origin(chassis.index(), to);
    assert_eq!(moved.len(), 5, "the chassis and its four wheels should all move: {moved:?}");
    sim.sync_joints(&mut ecs);
    sim.step_tick(DT, None);
    let (at1, vel1, cv1) = state(&sim, chassis, &wheels);
    // Parked, it should not have moved at all.
    let fall = 0.05;
    for k in 0..4 {
        assert!((at1[k] - at0[k]).length() < 0.01, "wheel {k} is {} m off its mount", (at1[k] - at0[k]).length());
        assert!((vel1[k] - vel0[k]).length() < fall, "wheel {k} was kicked to {:?}, against {:?} woken in place", vel1[k], vel0[k]);
    }
    assert!((cv1 - cv0).length() < fall, "the chassis was kicked to {cv1:?}, against {cv0:?} woken in place");
}

/// How far past its goal a servo arm swings, in degrees: a 2 m arm with a
/// load at its end hinged to an anchored base under gravity, driven from 0°
/// to `goal` at 45° a second with `force` of torque.
fn servo_overshoot(goal: f32, force: f32) -> f32 {
    let mut ecs = World::default();
    let base = segment(&mut ecs, "Base", DVec3::ZERO, [0.5, 0.5, 0.5], 50.0);
    let arm = segment(&mut ecs, "Arm", DVec3::new(1.5, 0.0, 0.0), [1.0, 0.08, 0.08], 0.3);
    ecs.insert(
        arm,
        Joint {
            kind: JointKind::Hinge,
            other: "Base".into(),
            anchor: [-1.0, 0.0, 0.0],
            motor: true,
            servo: true,
            goal,
            motor_speed: 45.0,
            max_force: force,
            ..Default::default()
        },
    );
    let mut sim = Sim::build_layered(&ecs, &[], GravityField::uniform(Vec3::new(0.0, -6.5, 0.0)), DVec3::ZERO, floptle_core::Layers::default());
    sim.set_compound_anchored(base.index(), true);
    let mut past = 0.0f32;
    for _ in 0..(6.0 / DT) as usize {
        sim.sync_joints(&mut ecs);
        sim.step_tick(DT, None);
        let p = ecs.get::<Joint>(arm).unwrap().position;
        past = past.max((p - goal) * goal.signum());
    }
    past
}

/// A servo arm driven to its goal arrives without swinging more than a
/// degree past it, down with gravity or up against it.
#[test]
fn a_servo_reaches_its_goal_without_overshoot() {
    for (goal, force) in [(-45.0, 5.0), (45.0, 5.0), (-45.0, 50.0), (90.0, 5.0)] {
        let past = servo_overshoot(goal, force);
        assert!(past < 1.0, "driven to {goal}° with {force} of torque, it swung {past}° past");
    }
}

/// How fast a rover on four driven wheels is going after a second of full
/// throttle on flat ground: chassis `mass`, wheels of radius 0.4, each
/// motor's torque capped at `torque`.
fn drive_speed_after_a_second(mass: f32, torque: f32) -> f32 {
    let mut ecs = World::default();
    let floor = ecs.spawn();
    ecs.insert(floor, Transform::from_translation(DVec3::new(0.0, -1.0, 0.0)));
    ecs.insert(floor, RigidBody { kind: BodyKind::Box, mode: BodyMode::Static, half_extents: [200.0, 1.0, 50.0], friction: 1.0, ..Default::default() });
    let chassis = segment(&mut ecs, "Chassis", DVec3::new(0.0, 0.8, 0.0), [1.2, 0.2, 0.7], mass);
    let mut wheels = Vec::new();
    for (dx, dz) in [(-1.0f64, -0.9f64), (1.0, -0.9), (-1.0, 0.9), (1.0, 0.9)] {
        let w = ecs.spawn();
        ecs.insert(w, Transform::from_translation(DVec3::new(dx, 0.4, dz)));
        ecs.insert(w, RigidBody { radius: 0.4, mass: 7.5, friction: 1.0, rotates: true, ..Default::default() });
        ecs.insert(
            w,
            Joint {
                kind: JointKind::Wheel,
                other: "Chassis".into(),
                axis: [0.0, 1.0, 0.0],
                // Stiff enough on a 7.5 kg tyre to hold its quarter of 800 kg
                // up with 2 cm of sag, not let the chassis sit on its belly.
                stiffness: 18.0,
                damping: 0.8,
                motor: true,
                motor_speed: 0.0,
                max_force: torque,
                ..Default::default()
            },
        );
        wheels.push(w);
    }
    let mut sim = build(&ecs);
    run(&mut sim, &mut ecs, 1.0);
    // Full throttle: far more rim speed than it can reach in a second.
    for &w in &wheels {
        ecs.get_mut::<Joint>(w).unwrap().motor_speed = -3000.0;
    }
    run(&mut sim, &mut ecs, 1.0);
    sim.compound_of(chassis.index()).unwrap().vel.length()
}

/// A wheel's drive pushes the craft no harder than its motor's torque
/// allows, through the tyre's grip: after a second, a 0.83-tonne rover on
/// four wheels of torque F at radius r is going no faster than 4F/(r·m) per
/// second gives, within 10%, rather than spinning light tyres up and having
/// the grip haul the chassis to the rim's speed.
#[test]
fn a_wheels_drive_is_limited_by_its_motors_torque() {
    let (mass, torque) = (800.0f32, 100.0f32);
    let total = mass + 4.0 * 7.5;
    let most = 4.0 * torque / (0.4 * total);
    let v = drive_speed_after_a_second(mass, torque);
    assert!(v <= most * 1.1, "after a second it is going {v} m/s; its motors can give {most}");
    assert!(v > most * 0.5, "after a second it is going only {v} m/s; its motors should give near {most}");
}

/// How fast a 1.8 t craft on three free (undriven, unbraked) wheels is going
/// after `secs` of a constant 14.7 kN push along a long flat runway,
/// starting `x0` along it. (A tyre much lighter than 0.05 against this hull
/// needs a stiffer spring than 4 Hz, or the hull sits down on its belly.)
fn ground_roll_speed(x0: f64, tyre_mass: f32, secs: f32) -> f32 {
    let mut ecs = World::default();
    let floor = ecs.spawn();
    ecs.insert(floor, Transform::from_translation(DVec3::new(200.0, -1.0, 0.0)));
    ecs.insert(floor, RigidBody { kind: BodyKind::Box, mode: BodyMode::Static, half_extents: [400.0, 1.0, 20.0], friction: 1.0, ..Default::default() });
    let hull = segment(&mut ecs, "Hull", DVec3::new(x0, 0.9, 0.0), [2.0, 0.3, 0.6], 1.8);
    for (dx, dz) in [(1.5f64, 0.0f64), (-0.8, -1.0), (-0.8, 1.0)] {
        let w = ecs.spawn();
        ecs.insert(w, Transform::from_translation(DVec3::new(x0 + dx, 0.25, dz)));
        ecs.insert(w, RigidBody { radius: 0.25, mass: tyre_mass, friction: 1.0, rotates: true, ..Default::default() });
        ecs.insert(
            w,
            Joint {
                kind: JointKind::Wheel,
                other: "Hull".into(),
                axis: [0.0, 1.0, 0.0],
                stiffness: 4.0,
                damping: 0.8,
                ..Default::default()
            },
        );
    }
    let mut sim = build(&ecs);
    run(&mut sim, &mut ecs, 1.0);
    for _ in 0..(secs / DT) as usize {
        sim.hold_compound_force(hull.index(), Vec3::new(14.7, 0.0, 0.0), None, Vec3::ZERO);
        sim.sync_joints(&mut ecs);
        sim.step_tick(DT, None);
    }
    sim.compound_of(hull.index()).unwrap().vel.x
}

/// A plane's ground roll: a 1.8 t craft on three free wheels, neither
/// driven nor braked, pushed by a constant 14.7 kN, is within 5% of the
/// speed it would make with no friction at all after 5 s (the wheels' spin
/// takes a little), from ten places along the runway. Its free wheels used
/// to be gripped as if locked to it, and it crawled at a skid's pace.
#[test]
fn a_craft_on_free_wheels_rolls() {
    let free = 14.7 / (1.8 + 3.0 * 0.05) * 5.0;
    for k in 0..10 {
        let v = ground_roll_speed(k as f64 * 0.73, 0.05, 5.0);
        assert!(v > free * 0.95 && v <= free * 1.001, "run {k}: {v} m/s after 5 s; with no friction it would be {free}");
    }
}

/// How fast a rover with its four suspension axes raked `rake` radians
/// along the way it drives is going after a second of full throttle, `dir`
/// forwards (+1) or backwards (-1), under gravity `g`. Its motors are far
/// stronger than its tyres' grip, so what it reaches is the grip.
fn raked_drive_speed(rake: f32, dir: f32, g: Vec3) -> f32 {
    drive_speed_with(rake, dir, g, false)
}

/// [`raked_drive_speed`], with each wheel's friction its tyre grip's alone
/// when `single` (see `PhysicsWorld::single_wheel_friction`).
fn drive_speed_with(rake: f32, dir: f32, g: Vec3, single: bool) -> f32 {
    let mut ecs = World::default();
    let floor = ecs.spawn();
    ecs.insert(floor, Transform::from_translation(DVec3::new(0.0, -1.0, 0.0)));
    ecs.insert(floor, RigidBody { kind: BodyKind::Box, mode: BodyMode::Static, half_extents: [200.0, 1.0, 50.0], friction: 0.6, ..Default::default() });
    let chassis = segment(&mut ecs, "Chassis", DVec3::new(0.0, 0.8, 0.0), [1.2, 0.2, 0.7], 400.0);
    let mut wheels = Vec::new();
    for (dx, dz) in [(-1.0f64, -0.9f64), (1.0, -0.9), (-1.0, 0.9), (1.0, 0.9)] {
        let w = ecs.spawn();
        ecs.insert(w, Transform::from_translation(DVec3::new(dx, 0.4, dz)));
        ecs.insert(w, RigidBody { radius: 0.4, mass: 7.5, friction: 0.6, rotates: true, ..Default::default() });
        ecs.insert(
            w,
            Joint {
                kind: JointKind::Wheel,
                other: "Chassis".into(),
                axis: [rake.sin(), rake.cos(), 0.0],
                stiffness: 18.0,
                damping: 0.8,
                motor: true,
                motor_speed: 0.0,
                max_force: 5000.0,
                ..Default::default()
            },
        );
        wheels.push(w);
    }
    let mut sim = Sim::build_layered(&ecs, &[], GravityField::uniform(g), DVec3::ZERO, floptle_core::Layers::default());
    sim.world.single_wheel_friction = single;
    run(&mut sim, &mut ecs, 1.0);
    let x0 = sim.compound_of(chassis.index()).unwrap().vel.x;
    for &w in &wheels {
        ecs.get_mut::<Joint>(w).unwrap().motor_speed = -3000.0 * dir;
    }
    run(&mut sim, &mut ecs, 1.0);
    (sim.compound_of(chassis.index()).unwrap().vel.x - x0) * dir
}

/// **A raked suspension grips the same both ways.** A tyre's grip is
/// friction times how hard it is pressed into the ground, which does not
/// depend on which way it drives. It was sized from the spring's push
/// along the suspension's own axis instead, and a slanted axis takes part
/// of the drive along it: one way the drive unloaded the spring and the
/// grip shrank, the other way it loaded it. A raked bike crawled forwards
/// and was a beast in reverse. With the axis raked 35°, on the flat and on
/// a 25% grade, it now gains the same speed either way within 5%, and as
/// much as with the axis upright.
#[test]
fn a_raked_suspension_grips_the_same_forwards_and_backwards() {
    let flat = Vec3::new(0.0, -9.81, 0.0);
    // A 25% grade, uphill along +x: gravity tilted back by atan(0.25).
    let a = 0.25f32.atan();
    let slope = Vec3::new(-a.sin(), -a.cos(), 0.0) * 9.81;
    for (name, g) in [("flat", flat), ("a 25% grade", slope)] {
        let up = raked_drive_speed(0.0, 1.0, g);
        let fwd = raked_drive_speed(35f32.to_radians(), 1.0, g);
        let back = raked_drive_speed(35f32.to_radians(), -1.0, g);
        assert!(up > 0.5, "on {name} the upright rover gained only {up} m/s: the test is not traction-limited");
        if name == "flat" {
            assert!((fwd - back).abs() <= 0.05 * fwd.max(back), "on {name}, raked 35°: {fwd} m/s forwards against {back} backwards");
            assert!((fwd - up).abs() <= 0.1 * up, "on {name}, raked it gained {fwd} m/s; upright {up}");
        } else {
            // Uphill and downhill differ by gravity; what must match is the
            // raked rover and the upright one, each way.
            let up_back = raked_drive_speed(0.0, -1.0, g);
            assert!((fwd - up).abs() <= 0.1 * up, "on {name} uphill, raked {fwd} m/s against upright {up}");
            assert!((back - up_back).abs() <= 0.1 * up_back, "on {name} downhill, raked {back} m/s against upright {up_back}");
        }
    }
}

/// **A wheel grips once.** A wheel joint's wheel took friction from its
/// contact with the ground and again from its tyre grip, both sized from the
/// same load, so a driven rover accelerated at about twice its friction
/// times gravity. With one budget (the project setting new projects start
/// with) a grip-limited rover on tyres of friction 0.6 gains within 10% of
/// 0.6 g in a second; without it, a project from before keeps its grip.
#[test]
fn a_wheel_grips_once_with_one_friction_budget() {
    let g = Vec3::new(0.0, -9.81, 0.0);
    let most = 0.6 * 9.81;
    let single = drive_speed_with(0.0, 1.0, g, true);
    let classic = drive_speed_with(0.0, 1.0, g, false);
    assert!((single - most).abs() <= 0.1 * most, "with one budget it gained {single} m/s; 0.6 g for a second is {most}");
    assert!(classic > 1.3 * most, "without the setting it gained {classic} m/s: the old grip must stay as it was");
}
