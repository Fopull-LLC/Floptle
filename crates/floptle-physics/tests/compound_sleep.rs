//! A parked assembly sleeps like a body: it stops costing collider work once
//! it is at rest, and wakes when something acts on it or its ground changes.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use floptle_core::math::{DVec3, Quat, Vec3};
use floptle_physics::{
    Body, BoxShape, CollisionShape, Compound, CompoundShape, GravityField, PhysicsWorld, ShapeGeom,
};

const DT: f32 = 1.0 / 120.0;

/// A box that counts every exact distance query: the work a resting craft
/// costs is these calls, whatever the machine.
struct Counting(BoxShape, Arc<AtomicUsize>);
impl CollisionShape for Counting {
    fn distance(&self, p: Vec3) -> f32 {
        self.1.fetch_add(1, Ordering::Relaxed);
        self.0.distance(p)
    }
    fn normal(&self, p: Vec3) -> Vec3 {
        self.0.normal(p)
    }
    fn bounds(&self) -> Option<(Vec3, f32)> {
        self.0.bounds()
    }
}

/// A 20 m static slab whose top is y = 0, and a 2×2 craft of four 1 m boxes
/// just above it.
fn parked() -> (PhysicsWorld, usize, Arc<AtomicUsize>) {
    let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
    let calls = Arc::new(AtomicUsize::new(0));
    w.add_collider(Box::new(Counting(
        BoxShape::new(Vec3::new(0.0, -1.0, 0.0), Vec3::new(10.0, 1.0, 10.0), Quat::IDENTITY),
        calls.clone(),
    )));
    let part = |x: f32, z: f32, id: u64| CompoundShape {
        geom: ShapeGeom::Box { half: Vec3::splat(0.5) },
        offset: Vec3::new(x, 0.0, z),
        rot: Quat::IDENTITY,
        mass: 1.0,
        id,
    };
    let craft = Compound::new(
        Vec3::new(0.0, 0.52, 0.0),
        Quat::IDENTITY,
        vec![part(-0.5, -0.5, 1), part(0.5, -0.5, 2), part(-0.5, 0.5, 3), part(0.5, 0.5, 4)],
    );
    let ci = w.add_compound(craft);
    (w, ci, calls)
}

fn run(w: &mut PhysicsWorld, secs: f32) {
    for _ in 0..(secs / DT).round() as usize {
        w.step(DT);
    }
}

/// Settled for two seconds, the craft is asleep, and a second of sleep costs
/// one re-check of its 36 samples rather than two passes of them every step;
/// a held force wakes it and it moves.
#[test]
fn a_parked_assembly_falls_asleep_and_a_force_wakes_it() {
    let (mut w, ci, calls) = parked();
    run(&mut w, 2.0);
    assert!(w.compounds[ci].asleep, "a craft at rest for 2 s should sleep");
    let rest = w.compounds[ci].pos;

    calls.store(0, Ordering::Relaxed);
    run(&mut w, 1.0);
    let asleep_calls = calls.load(Ordering::Relaxed);
    assert!(w.compounds[ci].asleep, "nothing happened, it should still sleep");
    assert_eq!(w.compounds[ci].pos, rest, "a sleeping craft moved");
    // An awake second is 120 steps × 2 passes × 36 samples.
    assert!(asleep_calls <= 36, "a second asleep made {asleep_calls} collider queries");
    assert!(
        w.compound_contacts.iter().any(|c| c.compound == ci),
        "asleep, it still reports resting on the slab"
    );

    w.compounds[ci].apply_force(Vec3::new(400.0, 0.0, 0.0));
    w.step(DT);
    assert!(!w.compounds[ci].asleep, "a held force should wake it");
    run(&mut w, 0.2);
    assert!(w.compounds[ci].pos.x > rest.x + 0.01, "woken, it should move: {:?} -> {:?}", rest, w.compounds[ci].pos);
}

/// An impulse and a velocity write wake it; so does a ball dropped on it.
#[test]
fn an_impulse_a_velocity_or_a_falling_ball_wakes_a_sleeping_assembly() {
    let (mut w, ci, _) = parked();
    run(&mut w, 2.0);
    assert!(w.compounds[ci].asleep);
    let at = w.compounds[ci].pos;
    w.compounds[ci].apply_impulse_at(Vec3::new(0.0, 0.0, 2.0), at);
    assert!(!w.compounds[ci].asleep, "an impulse should wake it");

    let (mut w, ci, _) = parked();
    run(&mut w, 2.0);
    w.compounds[ci].vel = Vec3::new(1.0, 0.0, 0.0);
    w.step(DT);
    assert!(!w.compounds[ci].asleep, "a velocity write should wake it");

    let (mut w, ci, _) = parked();
    run(&mut w, 2.0);
    let mut ball = Body::sphere(Vec3::new(0.5, 6.0, 0.5), 0.3);
    ball.mass = 50.0;
    w.add_body(ball);
    let mut woke = false;
    for _ in 0..240 {
        w.step(DT);
        woke |= !w.compounds[ci].asleep;
    }
    assert!(woke, "a heavy ball landing on its deck should wake it");
}

/// Carried with its ground (a planet on rails moves both), it stays asleep
/// and rides along; with its ground gone, it wakes and falls.
#[test]
fn a_sleeping_assembly_rides_its_ground_and_wakes_when_the_ground_goes() {
    let (mut w, ci, _) = parked();
    run(&mut w, 2.0);
    assert!(w.compounds[ci].asleep);
    let d = Vec3::new(3.0, 0.0, -2.0);
    for _ in 0..240 {
        let a = w.colliders()[0].anchor + d.as_dvec3() * (1.0 / 240.0);
        let origin = w.origin;
        w.colliders[0].re_anchor(a, origin);
        w.compounds[ci].pos += d * (1.0 / 240.0);
        w.step(DT);
    }
    assert!(w.compounds[ci].asleep, "carried with its ground, nothing changed under it");

    let y = w.compounds[ci].pos.y;
    w.set_colliders(Vec::new());
    run(&mut w, 0.5);
    assert!(!w.compounds[ci].asleep, "with the slab gone it should wake");
    assert!(w.compounds[ci].pos.y < y - 0.5, "and fall: {y} -> {}", w.compounds[ci].pos.y);
    let _ = DVec3::ZERO;
}

/// A small force held on a parked craft every step, as an idle thruster,
/// wind or buoyancy holds one: the craft falls asleep as it would without
/// it, stays asleep while the force stays the same, and wakes the step the
/// force changes. Each held force used to wake it, so it never slept.
#[test]
fn a_parked_craft_sleeps_under_a_held_force_and_wakes_when_it_changes() {
    let (mut w, ci, _) = parked();
    let hold = Vec3::new(0.3, 0.5, 0.0);
    let mut slept = None;
    for k in 0..240 {
        w.compounds[ci].apply_force(hold);
        w.step(DT);
        if slept.is_none() && w.compounds[ci].asleep {
            slept = Some(k);
        }
    }
    let slept = slept.expect("held by a small force, the craft never fell asleep");
    assert!(slept < 120, "held by a small force, the craft slept only at step {slept}");
    assert!(w.compounds[ci].asleep, "asleep under the same force, it woke");
    w.compounds[ci].apply_force(hold * 1.5);
    w.step(DT);
    assert!(!w.compounds[ci].asleep, "the force changed and it slept on");
}
