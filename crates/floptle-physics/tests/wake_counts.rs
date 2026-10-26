//! **Every sleeper woken is counted by what woke it** (`perf.counts().wakes`).
//! A craft woken again and again reads `settling` or `moving` between wakes,
//! and only the count says what keeps waking it. Its own binary: the count
//! is the process's.

use floptle_core::math::{Quat, Vec3};
use floptle_physics::{BoxShape, Compound, CompoundShape, GravityField, PhysicsWorld, ShapeGeom, WakeReason};

#[test]
fn a_sleeper_woken_by_a_push_is_counted_as_a_force() {
    let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
    w.add_collider(Box::new(BoxShape::new(Vec3::new(0.0, -1.0, 0.0), Vec3::new(10.0, 1.0, 10.0), Quat::IDENTITY)));
    let part = CompoundShape { geom: ShapeGeom::Box { half: Vec3::splat(0.5) }, offset: Vec3::ZERO, rot: Quat::IDENTITY, mass: 1.0, id: 1 };
    let ci = w.add_compound(Compound::new(Vec3::new(0.0, 0.52, 0.0), Quat::IDENTITY, vec![part]));
    for _ in 0..240 {
        w.step(1.0 / 120.0);
    }
    assert!(w.compounds[ci].asleep, "fixture: the craft should be asleep");
    let _ = floptle_physics::take_wake_counts();
    w.compounds[ci].apply_force(Vec3::new(20.0, 0.0, 0.0));
    w.step(1.0 / 120.0);
    let counts = floptle_physics::take_wake_counts();
    let force = WakeReason::ALL.iter().position(|r| *r == WakeReason::Force).unwrap();
    assert_eq!(counts[force], 1, "one push, one force wake: {counts:?}");
    assert_eq!(counts.iter().sum::<u32>(), 1, "nothing else woke: {counts:?}");
    assert_eq!(floptle_physics::take_wake_counts().iter().sum::<u32>(), 0, "a count taken is gone");
}
