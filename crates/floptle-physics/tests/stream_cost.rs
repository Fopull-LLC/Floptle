//! A world streaming in costs a tick what the same world costs at rest: the
//! terrain's collision meshes changing a chunk at a time, under bodies resting
//! on it and far from anything, are paid for by the chunks that change, not by
//! everything the level holds. A guard on a ratio of two timings taken in the
//! same run, as `physics_cost.rs` is.
//!
//! `examples/stream_bench.rs` times every way the level can change while it
//! streams, these included, and says which are dear.

use floptle_core::math::{Quat, Vec3};
use floptle_physics::*;

const DT: f32 = 1.0 / 120.0;

fn ground(x: f32, z: f32) -> f32 {
    0.06 * x + 0.4 * (0.21 * x).sin() * (0.17 * z).cos()
}

fn field() -> floptle_field::ChunkField {
    let mut f = floptle_field::ChunkField::new(0.75);
    f.fill_with(Vec3::new(-50.0, -12.0, -50.0), Vec3::new(50.0, 12.0, 50.0), |p| (p.y - ground(p.x, p.z)) * 0.95, |_| [0.5; 3]);
    f
}

fn spot(i: usize) -> (f32, f32) {
    let a = i as f32 * 0.61;
    let r = 3.0 + (i % 5) as f32 * 1.7 + (i / 40) as f32 * 4.5;
    (a.cos() * r, a.sin() * r)
}

const PROPS: usize = 120;

/// Props and a craft resting on the terrain, asleep, and a few bodies
/// moving about.
fn scene() -> PhysicsWorld {
    let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
    w.set_colliders(vec![AnchoredCollider::world(Box::new(ChunkTerrain::new(field())))]);
    for i in 0..PROPS {
        let (x, z) = spot(i);
        let p = Vec3::new(x, ground(x, z) + 1.5, z);
        w.add_body(if i % 2 == 0 { Body::sphere(p, 0.3) } else { Body::capsule(p, 0.3, 1.2) });
    }
    let part = |geom, offset, mass, id| CompoundShape { geom, offset, rot: Quat::IDENTITY, mass, id };
    let wheel = |x: f32, z: f32, id| part(ShapeGeom::Sphere { radius: 0.45 }, Vec3::new(x, 0.45, z), 25.0, id);
    w.add_compound(Compound::new(
        Vec3::new(20.0, ground(20.0, 0.0) + 0.3, 0.0),
        Quat::IDENTITY,
        vec![
            part(ShapeGeom::Box { half: Vec3::new(1.0, 0.25, 2.0) }, Vec3::new(0.0, 0.9, 0.0), 300.0, 1),
            wheel(1.0, 1.4, 2),
            wheel(-1.0, 1.4, 3),
            wheel(1.0, -1.4, 4),
            wheel(-1.0, -1.4, 5),
        ],
    ));
    for _ in 0..1200 {
        w.step(DT);
    }
    // And things moving about, as a world in play has: kept going by
    // `steer`, as a script keeps a rider going.
    for k in 0..MOVERS {
        let (x, z) = mover_at(k, 0);
        w.add_body(Body::sphere(Vec3::new(x, ground(x, z) + 0.5, z), 0.5));
    }
    w
}

const MOVERS: usize = 6;

/// Where mover `k` heads at tick `t`: round a circle of 8 m about (-25, 25).
fn mover_at(k: usize, t: usize) -> (f32, f32) {
    let a = t as f32 * DT * (0.5 + k as f32 * 0.3) + k as f32;
    (-25.0 + a.cos() * 8.0, 25.0 + a.sin() * 8.0)
}

fn steer(w: &mut PhysicsWorld, t: usize) {
    let first = w.bodies.len() - MOVERS;
    for k in 0..MOVERS {
        let (x, z) = mover_at(k, t + 1);
        let b = &mut w.bodies[first + k];
        b.vel = Vec3::new((x - b.pos.x) / DT, b.vel.y, (z - b.pos.z) / DT).clamp_length_max(15.0);
        b.asleep = false;
    }
}

/// Milliseconds `ticks` ticks take, each with `change` made first.
fn ticks(w: &mut PhysicsWorld, n: usize, mut change: impl FnMut(&mut PhysicsWorld, usize)) -> f64 {
    let mut ms = 0.0;
    for k in 0..n {
        steer(w, k);
        let t = std::time::Instant::now();
        change(w, k);
        w.step(DT);
        ms += t.elapsed().as_secs_f64() * 1e3;
    }
    ms
}

fn terrain(w: &mut PhysicsWorld) -> &mut ChunkTerrain {
    w.colliders[0].shape.chunk_terrain_mut().expect("the terrain")
}

/// **Collision meshes landing a chunk a tick, built, cost a tick what the
/// world at rest does.** The collision worker hands each re-meshed chunk
/// over whole, and a world streaming in lands a few a tick, many of them under
/// things resting there. Each landing is the chunk swapped in place: nobody
/// wakes, nothing is re-meshed on the tick, and nothing else is looked at.
/// Far ground put away (a chunk dropped where nothing stands) costs nothing
/// either. Both against the same ticks with nothing changing, interleaved so
/// a busy machine slows both alike; the median round of five.
#[test]
fn a_stream_of_collision_meshes_costs_a_tick_what_rest_does() {
    let mut w = scene();
    let asleep = |w: &PhysicsWorld| w.bodies.iter().take(PROPS).filter(|b| b.asleep).count() + w.compounds.iter().filter(|c| c.asleep).count();
    let before_asleep = asleep(&w);
    let poses: Vec<Vec3> = w.bodies.iter().take(PROPS).map(|b| b.pos).collect();
    assert!(before_asleep >= PROPS * 9 / 10, "only {before_asleep} of {} came to rest: not the resting scene", PROPS + 1);
    // The chunks under the props, built as the worker builds them.
    let f = field();
    let mut under: Vec<[i32; 3]> = (0..PROPS)
        .flat_map(|i| {
            let (x, z) = spot(i);
            let y = ground(x, z);
            f.chunks_in_world_box(Vec3::new(x - 1.0, y - 1.0, z - 1.0), Vec3::new(x + 1.0, y + 1.0, z + 1.0))
        })
        .collect();
    under.sort_unstable();
    under.dedup();
    let built: Vec<CollisionChunk> = under.iter().map(|&c| CollisionChunk::from_mesh(&drawn_chunk(&f, c), f.voxel())).collect();
    const N: usize = 120;
    let (mut rest, mut landing, mut far) = (Vec::new(), Vec::new(), Vec::new());
    for _ in 0..5 {
        rest.push(ticks(&mut w, N, |_, _| {}));
        // Copied before the clock starts: the worker hands each one over
        // made, and the copy is this test's, not the landing's.
        let mut made: Vec<([i32; 3], CollisionChunk)> = (0..N).map(|k| (under[k % under.len()], built[k % under.len()].clone())).collect();
        made.reverse();
        landing.push(ticks(&mut w, N, |w, _| {
            let (c, chunk) = made.pop().expect("one a tick");
            terrain(w).adopt(c, chunk);
        }));
        far.push(ticks(&mut w, N, |w, k| terrain(w).invalidate_meshes(&[[-3 + (k % 2) as i32, 0, -3]])));
    }
    let median = |v: &mut Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    let (rest, landing, far) = (median(&mut rest), median(&mut landing), median(&mut far));
    eprintln!("{N} ticks: at rest {rest:.2} ms, meshes landing {landing:.2} ms, far ground dropped {far:.2} ms");
    assert_eq!(asleep(&w), before_asleep, "a landing mesh woke something resting on it");
    let moved = w.bodies.iter().zip(&poses).map(|(b, p)| b.pos.distance(*p)).fold(0.0f32, f32::max);
    assert!(moved < 1e-4, "a landing mesh moved a resting prop {moved} m");
    assert!(
        landing < rest * 1.5,
        "ticks with a collision mesh landing under the props cost {landing:.2} ms against {rest:.2} ms at rest: \
         landing a built chunk should swap that chunk and nothing more"
    );
    assert!(far < rest * 1.5, "ticks dropping far ground cost {far:.2} ms against {rest:.2} ms at rest");
}
