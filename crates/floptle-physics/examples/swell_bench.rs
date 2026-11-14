//! What a swell costs the solver: a thousand crates and one craft of a
//! thousand parts floating in a pool, on still water, on eight rolling
//! sines and on eight steep waves.
//!
//! Run:
//! ```text
//! cargo run --release -p floptle-physics --example swell_bench
//! ```
//!
//! Prints the milliseconds of physics per 60 Hz tick (two 120 Hz steps) for
//! each sea, the best of five runs so a busy machine does not blur it, and
//! what the waves add per floating shape per tick.
//! Still water is the cost every scene paid before waves existed; a sea of
//! sines needs one pass over the waves for each ask, and a steep one a few
//! more to find which point of the still surface has moved over the shape.
use std::sync::Arc;

use floptle_core::math::{DVec3, Quat, Vec3};
use floptle_core::water::{Swell, Wave};
use floptle_core::{RigidBody, Transform, World};
use floptle_physics::*;

fn waves(steepness: f32) -> Vec<Wave> {
    (0..8)
        .map(|i| {
            let a = 0.4 + i as f32 * 0.83;
            Wave {
                amplitude: 0.9 / (1.0 + i as f32),
                wavelength: 60.0 / (1.0 + i as f32 * 0.7),
                direction: [a.cos(), a.sin()],
                steepness,
                speed: None,
                phase: i as f32 * 1.3,
            }
        })
        .collect()
}

fn pool(waves: &[Wave]) -> WaterVolume {
    WaterVolume {
        shape: WaterShape::Box { center: Vec3::new(0.0, -50.0, 0.0), half: Vec3::new(500.0, 50.0, 500.0), rot: Quat::IDENTITY },
        density: 1000.0,
        drag: 1.0,
        angular_drag: 1.0,
        frozen: false,
        entity: 7,
        carry: Default::default(),
        swell: Swell::new(waves, DVec3::Y).map(|swell| VolumeSwell { swell: Arc::new(swell), rot: Quat::IDENTITY, time: 0.0 }),
    }
}

/// The least of five timings.
fn best(mut run: impl FnMut() -> f64) -> f64 {
    (0..5).map(|_| run()).fold(f64::MAX, f64::min)
}

/// Mean ms a tick over `ticks` ticks of `n` floating balls.
fn balls(n: usize, waves: &[Wave], ticks: usize) -> f64 {
    let mut ecs = World::default();
    let side = (n as f64).sqrt().ceil() as usize;
    for i in 0..n {
        let e = ecs.spawn();
        let (x, z) = ((i % side) as f64 * 4.0 - side as f64 * 2.0, (i / side) as f64 * 4.0 - side as f64 * 2.0);
        ecs.insert(e, Transform::from_translation(DVec3::new(x, 0.0, z)));
        // Half as dense as the water: it floats half under.
        ecs.insert(e, RigidBody { radius: 0.5, mass: 262.0, ..Default::default() });
    }
    let gravity = GravityField { sources: vec![GravitySource::Uniform(Vec3::new(0.0, -9.81, 0.0))] };
    let mut sim = Sim::build(&ecs, &[], gravity, DVec3::ZERO);
    sim.world.water.volumes.push(pool(waves));
    time(&mut sim, ticks)
}

/// Mean ms a tick of one craft of `n` box parts, a raft `side` parts wide.
fn raft(n: usize, waves: &[Wave], ticks: usize) -> f64 {
    let side = (n as f64).sqrt().ceil() as usize;
    let shapes: Vec<CompoundShape> = (0..n)
        .map(|i| CompoundShape {
            geom: ShapeGeom::Box { half: Vec3::splat(0.25) },
            offset: Vec3::new((i % side) as f32 * 0.5 - side as f32 * 0.25, 0.0, (i / side) as f32 * 0.5 - side as f32 * 0.25),
            rot: Quat::IDENTITY,
            // A part's bounding sphere, half as dense as the water.
            mass: 170.0,
            id: i as u64 + 1,
        })
        .collect();
    let gravity = GravityField { sources: vec![GravitySource::Uniform(Vec3::new(0.0, -9.81, 0.0))] };
    let mut sim = Sim::build(&World::default(), &[], gravity, DVec3::ZERO);
    sim.world.compounds.push(Compound::new(Vec3::ZERO, Quat::IDENTITY, shapes));
    sim.world.water.volumes.push(pool(waves));
    let started = std::time::Instant::now();
    for _ in 0..ticks {
        sim.world.water.set_time(sim.world.water.volumes[0].swell.as_ref().map_or(0.0, |s| s.time) + 1.0 / 60.0);
        sim.world.step(1.0 / 120.0);
        sim.world.step(1.0 / 120.0);
    }
    started.elapsed().as_secs_f64() * 1000.0 / ticks as f64
}

fn time(sim: &mut Sim, ticks: usize) -> f64 {
    for _ in 0..30 {
        sim.step_tick(1.0 / 60.0, None);
    }
    let started = std::time::Instant::now();
    for _ in 0..ticks {
        sim.step_tick(1.0 / 60.0, None);
    }
    started.elapsed().as_secs_f64() * 1000.0 / ticks as f64
}

fn main() {
    const N: usize = 1000;
    const TICKS: usize = 300;
    let seas = [("still water", Vec::new()), ("8 rolling sines", waves(0.0)), ("8 steep waves", waves(0.7))];
    println!("{N} floating balls, one tick (two 120 Hz steps and the splash pass):");
    let mut still = 0.0;
    for (name, w) in &seas {
        let ms = best(|| balls(N, w, TICKS));
        if w.is_empty() {
            still = ms;
        }
        println!("  {name:16} {ms:7.3} ms a tick   {:+.2} us a ball over still water", (ms - still) * 1000.0 / N as f64);
    }
    println!("one raft of {N} parts, one tick (two 120 Hz steps):");
    for (name, w) in &seas {
        let ms = best(|| raft(N, w, TICKS));
        if w.is_empty() {
            still = ms;
        }
        println!("  {name:16} {ms:7.3} ms a tick   {:+.2} us a part over still water", (ms - still) * 1000.0 / N as f64);
    }
    // The ask itself: the water over a point, no two alike.
    let lay = floptle_core::swell::Lay::Flat { level: 0.0 };
    for (name, steepness) in [("rolling sines", 0.0), ("steep waves", 0.7)] {
        let swell = Swell::new(&waves(steepness), DVec3::Y).unwrap();
        const ASKS: usize = 1_000_000;
        let mut sum = 0.0;
        let ns = best(|| {
            let started = std::time::Instant::now();
            for i in 0..ASKS {
                let p = DVec3::new((i % 977) as f64 * 0.37, -0.2, (i % 613) as f64 * 0.51);
                sum += swell.over(p, lay, i as f64 * 1e-3).above;
            }
            started.elapsed().as_secs_f64() * 1e9 / ASKS as f64
        });
        println!("one ask of 8 {name}: {ns:.0} ns (checksum {sum:.3})");
    }
}
