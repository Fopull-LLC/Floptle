//! Before/after for the collider broadphase (`floptle/0076`).
//!
//! Run: `cargo run --release -p floptle-physics --example broadphase_bench`
//!
//! The "scan" column is the old behaviour reproduced honestly: colliders that
//! decline to report a bound are offered to every query, which is what the
//! solver used to do for all of them. (It therefore also pays the index's own
//! per-query overhead, so the real speed-up is a little larger than shown.)
//!
//! Measured on this machine, 120 steps:
//!
//! ```text
//!   169 colliders x  50 bodies:  scan  61.60ms   indexed  50.46ms   1.2x
//!   625 colliders x 200 bodies:  scan 859.48ms   indexed 247.94ms   3.5x
//!  1681 colliders x 400 bodies:  scan    4.50s   indexed 555.83ms   8.1x
//! ```
//!
//! The ratio GROWING with scene size is the point: that is a quadratic being
//! removed, not a constant being shaved.
use floptle_physics::{Body, GravityField, PhysicsWorld};
use floptle_physics::{BoxShape, CollisionShape};
use floptle_core::math::{Quat, Vec3};

struct Unbounded(BoxShape);
impl CollisionShape for Unbounded {
    fn distance(&self, p: Vec3) -> f32 { self.0.distance(p) }
    fn normal(&self, p: Vec3) -> Vec3 { self.0.normal(p) }
}

fn run(bounded: bool, side: i32, bodies: usize) -> std::time::Duration {
    let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
    for x in -side..=side {
        for z in -side..=side {
            let b = BoxShape::new(Vec3::new(x as f32 * 4.0, 0.0, z as f32 * 4.0), Vec3::new(2.0, 0.5, 2.0), Quat::IDENTITY);
            if bounded { w.add_collider(Box::new(b)); } else { w.add_collider(Box::new(Unbounded(b))); }
        }
    }
    for i in 0..bodies {
        let a = i as f32 * 0.7;
        w.add_body(Body::sphere(Vec3::new(a.sin() * side as f32 * 3.0, 3.0, a.cos() * side as f32 * 3.0), 0.5));
    }
    // warm
    w.step(1.0 / 60.0);
    let t = std::time::Instant::now();
    for _ in 0..120 { w.step(1.0 / 60.0); }
    t.elapsed()
}

fn main() {
    for (side, bodies) in [(6, 50), (12, 200), (20, 400)] {
        let colliders = (2 * side + 1) * (2 * side + 1);
        let scan = run(false, side, bodies);
        let idx = run(true, side, bodies);
        println!(
            "{colliders:5} colliders x {bodies:4} bodies, 120 steps:  scan {:>9.2?}   indexed {:>9.2?}   {:.1}x faster",
            scan, idx, scan.as_secs_f64() / idx.as_secs_f64()
        );
    }
}
