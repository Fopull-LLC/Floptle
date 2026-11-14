//! What a small craft lying on its side on terrain costs a tick, and whether
//! it falls asleep, while a game burns its fuel off a few grams a tick.
//!
//! Run:
//! ```text
//! cargo run --release -p floptle-physics --example tipped_craft_bench
//! ```
//!
//! A bike of `PARTS` welded parts (14 by default: a frame, a tank, a seat,
//! thin fenders and bars, two wheels) is dropped on its side onto rolling
//! terrain. Each case then runs it for ten seconds and prints, a second at a
//! time, a tick's cost by stage, how fast it moves, how many parts touch the
//! ground, and whether it sleeps:
//!
//! - `still`: nothing is done to it;
//! - `fuel`: its tank loses `DRIFT` kg a tick (0.002 by default), as a game
//!   writing a burning tank's mass every tick does;
//! - `resync`: its parts are re-posed where they already are every tick, as
//!   a game calling `assembly.syncColliders` every tick does;
//! - `shoved`: it is kept sliding and turning at about 1 m/s, as one being
//!   pushed about is, which is what a tick costs while it cannot sleep.
//! - `sunk`: it is put back a metre and a half under the ground every tick,
//!   as a craft jammed into a hillside is, where every part is asked how
//!   far it is from a surface that is nowhere near it.
//!
//! Then what one distance question costs the terrain by how far the point
//! is from the surface, above it and under it.
//!
//! `tests/tipped_craft.rs` holds the `fuel` and `resync` cases asleep and
//! cheap; which samples a slow craft sweeps is held by `part_sweep_tests` in
//! `world.rs`.
use floptle_core::math::{Quat, Vec3};
use floptle_physics::*;

const DT: f32 = 1.0 / 120.0;

fn ground(x: f32, z: f32) -> f32 {
    0.06 * x + 0.4 * (0.21 * x).sin() * (0.17 * z).cos()
}

fn terrain() -> ChunkTerrain {
    let mut field = floptle_field::ChunkField::new(0.75);
    field.fill_with(
        Vec3::new(-40.0, -12.0, -40.0),
        Vec3::new(40.0, 12.0, 40.0),
        |p| (p.y - ground(p.x, p.z)) * 0.95,
        |_| [0.5; 3],
    );
    ChunkTerrain::new(field)
}

fn env(name: &str, default: f32) -> f32 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// The tank's shape id.
const TANK: u64 = 2;

/// A bike authored upright along Z, its wheels welded on.
fn bike(parts: usize) -> Vec<CompoundShape> {
    let part = |geom, offset: [f32; 3], rot, mass, id| CompoundShape { geom, offset: Vec3::from(offset), rot, mass, id };
    let b = |h: [f32; 3]| ShapeGeom::Box { half: Vec3::from(h) };
    let along_x = Quat::from_rotation_z(std::f32::consts::FRAC_PI_2);
    let along_z = Quat::from_rotation_x(std::f32::consts::FRAC_PI_2);
    let mut s = vec![
        part(b([0.12, 0.25, 0.8]), [0.0, 0.7, 0.0], Quat::IDENTITY, 90.0, 1),
        part(b([0.14, 0.15, 0.3]), [0.0, 1.0, 0.2], Quat::IDENTITY, 12.0, TANK),
        part(b([0.15, 0.04, 0.35]), [0.0, 1.0, -0.4], Quat::IDENTITY, 4.0, 3),
        // Wheels: a capsule across the axle each.
        part(ShapeGeom::Capsule { radius: 0.33, half_height: 0.04 }, [0.0, 0.33, 0.85], along_x, 9.0, 4),
        part(ShapeGeom::Capsule { radius: 0.33, half_height: 0.04 }, [0.0, 0.33, -0.85], along_x, 9.0, 5),
        // Thin parts: fenders, bars, a fork, a rack, a plate.
        part(b([0.08, 0.012, 0.3]), [0.0, 0.7, 0.85], Quat::IDENTITY, 0.6, 6),
        part(b([0.08, 0.012, 0.3]), [0.0, 0.7, -0.85], Quat::IDENTITY, 0.6, 7),
        part(ShapeGeom::Capsule { radius: 0.02, half_height: 0.35 }, [0.0, 1.25, 0.6], along_x, 1.0, 8),
        part(ShapeGeom::Capsule { radius: 0.025, half_height: 0.3 }, [0.07, 0.75, 0.75], along_z, 1.5, 9),
        part(ShapeGeom::Capsule { radius: 0.025, half_height: 0.3 }, [-0.07, 0.75, 0.75], along_z, 1.5, 10),
        part(b([0.2, 0.015, 0.25]), [0.0, 0.95, -0.9], Quat::IDENTITY, 1.2, 11),
        part(b([0.01, 0.08, 0.12]), [0.0, 0.45, -1.2], Quat::IDENTITY, 0.2, 12),
        part(ShapeGeom::Sphere { radius: 0.07 }, [0.0, 1.1, 0.78], Quat::IDENTITY, 0.5, 13),
        part(b([0.05, 0.12, 0.2]), [0.18, 0.5, -0.3], Quat::IDENTITY, 3.0, 14),
    ];
    // More parts on request: panniers stacked along the frame.
    let mut id = 100;
    while s.len() < parts {
        let k = (s.len() - 14) as f32;
        let side = if (s.len() % 2) == 0 { 0.22 } else { -0.22 };
        s.push(part(b([0.06, 0.1, 0.1]), [side, 0.6 + 0.02 * k, -0.7 + 0.11 * k], Quat::IDENTITY, 1.0, id));
        id += 1;
    }
    s.truncate(parts.max(1));
    s
}

/// The bike lying on its side at rest on the terrain, awake.
fn scene(parts: usize) -> (PhysicsWorld, usize) {
    let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
    w.set_colliders(vec![AnchoredCollider::world(Box::new(terrain()))]);
    let (x, z) = (4.0, 3.0);
    let on_side = Quat::from_rotation_z(1.45);
    let ci = w.add_compound(Compound::new(Vec3::new(x, ground(x, z) + 1.2, z), on_side, bike(parts)));
    // Let it fall and lie down, kept awake so each case starts the same.
    for _ in 0..360 {
        w.step(DT);
        w.compounds[ci].wake(WakeReason::Script);
    }
    (w, ci)
}

fn run(name: &str, parts: usize, each_tick: impl Fn(&mut PhysicsWorld, usize, usize)) {
    let (mut w, ci) = scene(parts);
    println!("\n{name}: {} parts, thinnest {:.3} m", w.compounds[ci].shapes.len(), w.compounds[ci].thinnest());
    println!("  s   ms/tick  static  sweeps   v m/s  w rad/s  touching  grounded  asleep  wake");
    w.time_steps(true);
    let mut slept_at = None;
    for sec in 0..10 {
        let mut touching = 0usize;
        for t in 0..120 {
            let tick = sec * 120 + t;
            each_tick(&mut w, ci, tick);
            w.step(DT);
            let mut ids: Vec<u64> = w.compound_contacts.iter().filter(|c| c.compound == ci).map(|c| c.shape_id).collect();
            ids.sort_unstable();
            ids.dedup();
            touching = touching.max(ids.len());
            if w.compounds[ci].asleep && slept_at.is_none() {
                slept_at = Some(tick);
            }
        }
        let tm = w.take_timing().unwrap_or_default();
        let n = tm.steps.max(1) as f32;
        let c = &w.compounds[ci];
        println!(
            "{:3}   {:7.3} {:7.3} {:7.3} {:7.2} {:8.2} {:9} {:9} {:7}  {}",
            sec + 1,
            (tm.static_ms + tm.sweeps_ms + tm.pairs_ms + tm.joints_ms + tm.sleep_ms + tm.broadphase_ms) / n,
            tm.static_ms / n,
            tm.sweeps_ms / n,
            c.vel.length(),
            c.ang_vel.length(),
            touching,
            c.grounded,
            c.asleep,
            c.wake.name(),
        );
    }
    match slept_at {
        Some(t) => println!("  fell asleep after {:.2} s", t as f32 * DT),
        None => println!("  never slept"),
    }
}

fn main() {
    let parts = env("PARTS", 14.0) as usize;
    let drift = env("DRIFT", 0.002);
    run("still", parts, |_, _, _| {});
    run("fuel", parts, move |w, ci, tick| {
        let m = 12.0 - drift * (tick + 1) as f32;
        w.compounds[ci].set_shape_masses(&[(TANK, m.max(0.5))]);
    });
    run("resync", parts, |w, ci, _| {
        let c = &w.compounds[ci];
        let lo = c.local_origin;
        let same: Vec<(u64, Vec3, Quat)> = c.shapes.iter().map(|s| (s.id, s.offset - lo, s.rot)).collect();
        w.compounds[ci].resync_shape_geometry(&same);
    });
    run("shoved", parts, |w, ci, tick| {
        let a = tick as f32 * DT * 0.5;
        let c = &mut w.compounds[ci];
        let (v, s) = (c.vel, c.ang_vel);
        c.vel = Vec3::new(a.cos(), v.y, a.sin());
        c.ang_vel = Vec3::new(s.x, 0.9, s.z);
        c.wake(WakeReason::Script);
    });
    let home = std::cell::Cell::new(None);
    run("sunk", parts, move |w, ci, _| {
        let c = &mut w.compounds[ci];
        let at = home.get().unwrap_or((c.pos - Vec3::Y * 1.5, c.orient));
        home.set(Some(at));
        (c.pos, c.orient) = at;
        (c.vel, c.ang_vel) = (Vec3::ZERO, Vec3::ZERO);
        c.wake(WakeReason::Script);
    });
    println!("\none distance question of the terrain, by height over the surface:");
    let t = terrain();
    t.distance(Vec3::ZERO);
    for h in [2.0f32, 0.5, 0.0, -0.5, -1.0, -1.5, -2.0, -2.5] {
        // The best of a few rounds, the first of which meshes the ground:
        // a loaded machine only ever adds.
        let n = 20_000;
        let (mut best, mut sum) = (f64::INFINITY, 0.0);
        for _ in 0..6 {
            sum = 0.0;
            let t0 = std::time::Instant::now();
            for i in 0..n {
                let (x, z) = ((i % 100) as f32 * 0.1 - 5.0, (i / 100) as f32 * 0.05 - 5.0);
                sum += t.distance_within(Vec3::new(x, ground(x, z) + h, z), 0.0);
            }
            best = best.min(t0.elapsed().as_secs_f64() * 1e6 / n as f64);
        }
        println!("  {h:5.1} m: {best:6.2} us (mean distance {:5.2})", sum / n as f32);
    }
}
