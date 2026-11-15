//! A game can never hang the physics step with a bad number. A spin that
//! doubles every tick until it overflows, and NaN or infinity fed straight in
//! through every way a game sets a force, an impulse, a torque, a velocity or
//! a position, each leave a step that returns, promptly, every time.
//!
//! Every case runs on its own thread against a deadline, so a step that never
//! returns fails the test by its timeout instead of hanging the suite.

use std::sync::mpsc;
use std::time::Duration;

use floptle_core::math::{DVec3, Quat, Vec3};
use floptle_core::{BodyKind, Parent, RigidBody, Transform, World};
use floptle_field::ChunkField;
use floptle_physics::{GravityField, Sim, TerrainVolume};

/// Run `f` on its own thread; fail if it has not finished within `secs`.
fn within(secs: u64, what: &str, f: impl FnOnce() + Send + 'static) {
    let (tx, rx) = mpsc::channel();
    let h = std::thread::spawn(move || {
        f();
        let _ = tx.send(());
    });
    match rx.recv_timeout(Duration::from_secs(secs)) {
        Ok(()) => h.join().expect("the case panicked"),
        // A panic drops the sender: report it as the panic it was.
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            if let Err(e) = h.join() {
                std::panic::resume_unwind(e);
            }
        }
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("{what}: the physics step did not return within {secs} s"),
    }
}

/// Terrain, two small one-part assemblies resting just above it side by side
/// (`debris` and `other`), and a turning crate body beside them.
struct Scene {
    sim: Sim,
    debris: u32,
    other: u32,
    crate_body: u32,
    ball: u32,
}

fn assembly(ecs: &mut World, at: DVec3, half: f32) -> u32 {
    let root = ecs.spawn();
    ecs.insert(root, Transform::from_translation(at));
    ecs.insert(root, RigidBody { assembly: true, ..Default::default() });
    let part = ecs.spawn();
    ecs.insert(part, Transform { translation: DVec3::ZERO, rotation: Quat::from_rotation_x(0.2), scale: Vec3::ONE });
    ecs.insert(part, Parent(root));
    ecs.insert(part, RigidBody { kind: BodyKind::Box, half_extents: [half; 3], mass: 0.2, ..Default::default() });
    root.index()
}

fn scene() -> Scene {
    let mut field = ChunkField::new(0.75);
    field.fill_slab(Vec3::new(-48.0, -6.0, -48.0), Vec3::new(48.0, 0.0, 48.0), 0.0, [0.5; 3]);
    let field: &'static ChunkField = Box::leak(Box::new(field));
    let mut ecs = World::default();
    let debris = assembly(&mut ecs, DVec3::new(0.0, 0.3, 0.0), 0.2);
    let other = assembly(&mut ecs, DVec3::new(0.7, 0.3, 0.0), 0.2);
    let crate_e = ecs.spawn();
    ecs.insert(crate_e, Transform::from_translation(DVec3::new(-0.7, 0.3, 0.0)));
    ecs.insert(crate_e, RigidBody { kind: BodyKind::Box, half_extents: [0.2; 3], mass: 0.2, rotates: true, ..Default::default() });
    let ball_e = ecs.spawn();
    ecs.insert(ball_e, Transform::from_translation(DVec3::new(0.0, 0.4, 0.8)));
    ecs.insert(ball_e, RigidBody { kind: BodyKind::Sphere, radius: 0.25, mass: 0.2, ..Default::default() });
    let sim = Sim::build_layered(
        &ecs,
        &[TerrainVolume::new(DVec3::ZERO, field)],
        GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)),
        DVec3::ZERO,
        floptle_core::Layers::default(),
    );
    Scene { sim, debris, other, crate_body: crate_e.index(), ball: ball_e.index() }
}

/// The report: a drag torque left uncapped on a piece of debris. Each tick
/// its torque is what doubles the spin it has, so the spin runs up through
/// every finite value and overflows, while its push does the same to its
/// speed. Holds the debris's push and turn for `ticks` ticks, calling `each`
/// after every one with how long it took.
fn run_away(ticks: usize, mut each: impl FnMut(&mut Scene, usize, Duration)) -> Scene {
    let mut s = scene();
    let dt = 1.0 / 60.0;
    for tick in 0..ticks {
        let c = s.sim.compound_of(s.debris).unwrap();
        let w = if c.ang_vel.length_squared() > 1e-6 { c.ang_vel } else { Vec3::new(7.0, 3.0, 1.0) };
        let v = if c.vel.length_squared() > 1e-6 { c.vel } else { Vec3::new(1.0, 0.0, 0.0) };
        let torque = c.world_inertia() * w / dt;
        let force = v * c.mass / dt;
        s.sim.hold_compound_force(s.debris, force, None, torque);
        let t0 = std::time::Instant::now();
        s.sim.step_tick(dt, None);
        each(&mut s, tick, t0.elapsed());
    }
    s
}

/// Every one of 400 ticks of the run-away returns, each well inside a
/// second (a debug build steps this scene in about a millisecond).
#[test]
fn a_spin_doubling_every_tick_never_hangs_the_step() {
    within(60, "doubling spin", || {
        run_away(400, |_, tick, took| {
            assert!(took < Duration::from_secs(2), "tick {tick} took {took:?}");
        });
    });
}

/// And the debris is stopped where it last was finite and put to sleep,
/// once, with its neighbours untouched; nothing the game reads goes NaN.
#[test]
fn a_run_away_spin_is_stopped_where_it_last_was_finite() {
    within(60, "doubling spin", || {
        let mut s = run_away(400, |s, tick, _| {
            let c = s.sim.compound_of(s.debris).unwrap();
            assert!(
                c.pos.is_finite() && c.orient.is_finite() && c.vel.is_finite() && c.ang_vel.is_finite(),
                "tick {tick}: the debris's state went non-finite: {:?} {:?} {:?}",
                c.pos,
                c.vel,
                c.ang_vel
            );
        });
        let stopped = s.sim.world.sanity.take_quarantined();
        assert!(stopped.contains(&s.debris), "the debris was never stopped: {stopped:?}");
        let said = s.sim.world.sanity.take_reports();
        assert!(said.iter().any(|r| r.node == s.debris), "nothing was said about the debris: {said:?}");
        assert!(said.len() <= 16, "the log was flooded: {} lines", said.len());
        assert!(!stopped.contains(&s.other), "its neighbour was stopped too: {stopped:?}");
        let other = s.sim.compound_of(s.other).unwrap();
        assert!(other.pos.is_finite() && other.vel.is_finite(), "the debris's neighbour went non-finite");
    });
}

/// The same run-away spin and speed on a turning body, written each tick
/// through its spin and velocity until the doubling overflows.
#[test]
fn a_body_spin_doubling_every_tick_never_hangs_the_step() {
    within(60, "doubling body spin", || {
        let mut s = scene();
        let mut spin = 7.0f32;
        let mut v = 1.0f32;
        for _ in 0..400 {
            s.sim.set_body_spin(s.crate_body, spin);
            s.sim.set_body_velocity(s.crate_body, Vec3::new(v, 0.0, 0.0));
            s.sim.step_tick(1.0 / 60.0, None);
            spin *= 2.0;
            v *= 2.0;
        }
        let stopped = s.sim.world.sanity.take_quarantined();
        assert!(stopped.contains(&s.crate_body), "the crate was never stopped: {stopped:?}");
        settle_and_check(&mut s.sim, "after the run-away");
    });
}

const BAD: [f32; 3] = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY];

fn bad_vectors() -> Vec<Vec3> {
    BAD.iter().flat_map(|&x| [Vec3::new(x, 0.0, 0.0), Vec3::new(0.0, x, 0.0), Vec3::splat(x)]).collect()
}

/// The write was counted as refused, and nothing it reached took it: every
/// state is still finite before a single step has run. A step would stop a
/// body that took it anyway, so this is what tells the door from the step.
fn refused_at_the_door(s: &Sim, what: &str) {
    assert_eq!(s.world.sanity.refused(), 1, "{what}: not refused, or refused twice");
    for c in &s.world.compounds {
        assert!(c.pos.is_finite() && c.vel.is_finite() && c.ang_vel.is_finite() && c.orient.is_finite(), "{what}: an assembly took it");
    }
    for b in &s.world.bodies {
        assert!(b.pos.is_finite() && b.vel.is_finite() && b.ang_vel.is_finite() && b.orient.is_finite(), "{what}: a body took it");
    }
}

/// A few ticks of the scene, then every body and assembly must still have a
/// finite state: the bad write was refused, not taken in.
fn settle_and_check(s: &mut Sim, what: &str) {
    for _ in 0..30 {
        s.step_tick(1.0 / 60.0, None);
    }
    for (eid, c) in s.assemblies() {
        assert!(
            c.pos.is_finite() && c.vel.is_finite() && c.ang_vel.is_finite() && c.orient.is_finite(),
            "{what}: assembly {eid} took a non-finite state: pos {:?} vel {:?} spin {:?}",
            c.pos,
            c.vel,
            c.ang_vel
        );
    }
    for (eid, p) in s.body_positions() {
        let snap = s.body_snapshot(eid).unwrap();
        assert!(
            p.is_finite() && snap.vel.is_finite() && snap.ang_vel.is_finite() && snap.orient.is_finite(),
            "{what}: body {eid} took a non-finite state: pos {p:?} vel {:?} spin {:?}",
            snap.vel,
            snap.ang_vel
        );
    }
}

/// NaN and both infinities, in one axis and in all three, through every way
/// a game pushes, turns, speeds or places an assembly: each one is refused,
/// and the step goes on returning.
#[test]
fn every_assembly_write_refuses_nan_and_infinity() {
    within(120, "assembly writes", || {
        type Write = fn(&mut Sim, u32, Vec3);
        let writes: [(&str, Write); 8] = [
            ("held force", |s, e, v| s.hold_compound_force(e, v, None, Vec3::ZERO)),
            ("held force at", |s, e, v| s.hold_compound_force(e, Vec3::Y, Some(v.as_dvec3()), Vec3::ZERO)),
            ("held torque", |s, e, v| s.hold_compound_force(e, Vec3::ZERO, None, v)),
            ("impulse", |s, e, v| s.compound_impulse(e, v, DVec3::new(0.0, 0.3, 0.0))),
            ("impulse at", |s, e, v| s.compound_impulse(e, Vec3::X, v.as_dvec3())),
            ("velocity", |s, e, v| s.set_compound_velocity(e, v)),
            ("angular velocity", |s, e, v| s.set_compound_angular_velocity(e, v)),
            ("origin", |s, e, v| {
                s.set_compound_origin(e, v.as_dvec3());
            }),
        ];
        for (what, write) in writes {
            for v in bad_vectors() {
                let mut s = scene();
                let e = s.debris;
                write(&mut s.sim, e, v);
                refused_at_the_door(&s.sim, &format!("{what} {v:?}"));
                settle_and_check(&mut s.sim, &format!("{what} {v:?}"));
            }
        }
        let more: [(&str, Write); 4] = [
            ("shift", |s, e, v| s.shift_compound(e, v.as_dvec3())),
            ("node velocity", |s, e, v| s.set_body_velocity(e, v)),
            ("node position", |s, e, v| s.set_body_position(e, v.as_dvec3())),
            ("node shift", |s, e, v| s.shift_body(e, v.as_dvec3())),
        ];
        for (what, write) in more {
            for v in bad_vectors() {
                let mut s = scene();
                let e = s.debris;
                write(&mut s.sim, e, v);
                refused_at_the_door(&s.sim, &format!("assembly {what} {v:?}"));
                settle_and_check(&mut s.sim, &format!("assembly {what} {v:?}"));
            }
        }
    });
}

/// The same for a body: its velocity, spin, position, carry and a restored
/// snapshot.
#[test]
fn every_body_write_refuses_nan_and_infinity() {
    within(120, "body writes", || {
        type Write = fn(&mut Sim, u32, Vec3);
        let writes: [(&str, Write); 6] = [
            ("velocity", |s, e, v| s.set_body_velocity(e, v)),
            ("spin", |s, e, v| s.set_body_spin(e, v.x + v.y)),
            ("position", |s, e, v| s.set_body_position(e, v.as_dvec3())),
            ("shift", |s, e, v| s.shift_body(e, v.as_dvec3())),
            ("restored velocity", |s, e, v| {
                let mut snap = s.body_snapshot(e).unwrap();
                snap.vel = v;
                s.restore_body(e, &snap);
            }),
            ("restored position", |s, e, v| {
                let mut snap = s.body_snapshot(e).unwrap();
                snap.pos = v.as_dvec3();
                s.restore_body(e, &snap);
            }),
        ];
        for (what, write) in writes {
            for v in bad_vectors() {
                for pick in [0, 1] {
                    let mut s = scene();
                    let e = if pick == 0 { s.crate_body } else { s.ball };
                    write(&mut s.sim, e, v);
                    refused_at_the_door(&s.sim, &format!("{what} {v:?} on body {e}"));
                    settle_and_check(&mut s.sim, &format!("{what} {v:?} on body {e}"));
                }
            }
        }
    });
}

/// A tick length that is not a number, or is endless, is still one bounded
/// tick.
#[test]
fn a_non_finite_tick_length_still_returns() {
    within(30, "tick length", || {
        for dt in [f32::NAN, f32::INFINITY, 1e30] {
            let mut s = scene();
            s.sim.step_tick(dt, None);
            settle_and_check(&mut s.sim, &format!("tick of {dt}"));
        }
    });
}
