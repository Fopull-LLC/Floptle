//! What a tick costs while the level streams in, against the same scene once
//! it has.
//!
//! Run:
//! ```text
//! cargo run --release -p floptle-physics --example stream_bench
//! ```
//!
//! One scene, built afresh for each case: rolling terrain, forty props
//! (`PROPS=n` for more) and a seven-part craft resting on it (asleep), and a
//! few bodies and a second craft kept moving (awake), one of them running
//! across ground nothing has stood on. Each case streams something into the
//! level for `STREAM` ticks, one change a tick (a world streaming in at about
//! 25 chunks a second, at the 8 ticks a frame a slow frame catches up with),
//! and prints a tick's cost, by stage, before, during and after:
//!
//! - `colliders`: a terrain-chunk-sized static mesh is added to the level
//!   each tick through `set_colliders`, as a streamed level piece arrives;
//! - `removed`: one is taken away each tick, as a far piece unloads;
//! - `invalidate`: the collision mesh of a terrain chunk under a resting prop
//!   is dropped each tick, as a stand-in made real does, and the next query
//!   there meshes it again on the spot;
//! - `far invalidate`: a terrain chunk nowhere near anything is dropped each
//!   tick, as far ground put away does;
//! - `adopt`: the same chunk's mesh is handed over built, as the collision
//!   worker lands it;
//! - `replace`: a level piece under a resting prop is put back in place by
//!   `replace_collider_of`, as a terrain node's baked mesh is swapped.
//!
//! "during / steady" is the number the stream must hold near 1. The
//! `adopt` and `far invalidate` cases, the ways a terrain's collision meshes
//! change while its world streams in, are held there by
//! `tests/stream_cost.rs`.
use floptle_core::math::{Quat, Vec3};
use floptle_physics::*;
use std::time::Instant;

const STREAM: usize = 400;
const STEADY: usize = 240;
const DT: f32 = 1.0 / 120.0;

fn ground(x: f32, z: f32) -> f32 {
    0.06 * x + 0.4 * (0.21 * x).sin() * (0.17 * z).cos()
}

fn terrain() -> ChunkTerrain {
    let mut field = floptle_field::ChunkField::new(0.75);
    field.fill_with(
        Vec3::new(-60.0, -12.0, -60.0),
        Vec3::new(60.0, 12.0, 60.0),
        |p| (p.y - ground(p.x, p.z)) * 0.95,
        |_| [0.5; 3],
    );
    ChunkTerrain::new(field)
}

/// A level piece the size of a terrain chunk (24 m, 32×32 quads, 2,048
/// triangles) with its corner at `(x, z)`, following the ground.
fn piece(x0: f32, z0: f32, lift: f32) -> TriMeshCollider {
    let n = 32;
    let step = 0.75;
    let mut verts = Vec::new();
    for z in 0..=n {
        for x in 0..=n {
            let (px, pz) = (x0 + x as f32 * step, z0 + z as f32 * step);
            verts.push(Vec3::new(px, ground(px, pz) + lift, pz));
        }
    }
    let mut idx = Vec::new();
    let w = (n + 1) as u32;
    for z in 0..n as u32 {
        for x in 0..n as u32 {
            let (a, b, c, d) = (z * w + x, z * w + x + 1, (z + 1) * w + x, (z + 1) * w + x + 1);
            idx.extend_from_slice(&[a, c, b, b, c, d]);
        }
    }
    TriMeshCollider::new(&verts, &idx)
}

fn part(geom: ShapeGeom, offset: Vec3, mass: f32, id: u64) -> CompoundShape {
    CompoundShape { geom, offset, rot: Quat::IDENTITY, mass, id }
}

/// A small wheeled craft: a chassis, a cab, a rack and four wheels.
fn craft(at: Vec3) -> Compound {
    let b = |h: [f32; 3]| ShapeGeom::Box { half: Vec3::from(h) };
    let s = vec![
        part(b([1.0, 0.25, 2.0]), Vec3::new(0.0, 0.9, 0.0), 300.0, 1),
        part(b([0.8, 0.5, 0.6]), Vec3::new(0.0, 1.6, 0.9), 80.0, 2),
        part(b([0.9, 0.05, 0.9]), Vec3::new(0.0, 1.2, -1.1), 20.0, 3),
        part(ShapeGeom::Sphere { radius: 0.45 }, Vec3::new(1.0, 0.45, 1.4), 25.0, 4),
        part(ShapeGeom::Sphere { radius: 0.45 }, Vec3::new(-1.0, 0.45, 1.4), 25.0, 5),
        part(ShapeGeom::Sphere { radius: 0.45 }, Vec3::new(1.0, 0.45, -1.4), 25.0, 6),
        part(ShapeGeom::Sphere { radius: 0.45 }, Vec3::new(-1.0, 0.45, -1.4), 25.0, 7),
    ];
    Compound::new(at, Quat::IDENTITY, s)
}

/// How many props rest in the scene (`PROPS`, 40 by default).
fn props() -> usize {
    std::env::var("PROPS").ok().and_then(|v| v.parse().ok()).unwrap_or(40)
}

/// Where the props stand: a spiral round the origin, the first forty on the
/// level piece.
fn prop_spot(i: usize) -> (f32, f32) {
    let a = i as f32 * 0.61;
    let r = 3.0 + (i % 5) as f32 * 1.7 + (i / 40) as f32 * 4.5;
    (a.cos() * r, a.sin() * r)
}

/// The scene, settled: everything at rest that will rest.
fn scene() -> PhysicsWorld {
    let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
    let mut cols = vec![AnchoredCollider::world(Box::new(terrain()))];
    // A level piece under the props, a hair over the terrain, which the
    // `replace` case swaps.
    let mut under = AnchoredCollider::world(Box::new(piece(-12.0, -12.0, 0.05)));
    under.eid = Some(900);
    cols.push(under);
    w.set_colliders(cols);
    for i in 0..props() {
        let (x, z) = prop_spot(i);
        let y = ground(x, z) + 1.5;
        let b = if i % 2 == 0 { Body::sphere(Vec3::new(x, y, z), 0.3) } else { Body::capsule(Vec3::new(x, y, z), 0.3, 1.2) };
        w.add_body(b);
    }
    w.add_compound(craft(Vec3::new(20.0, ground(20.0, 0.0) + 0.3, 0.0)));
    for _ in 0..1200 {
        w.step(DT);
    }
    // Movers, steered round a circle every tick (see `steer`): awake
    // throughout, one fast enough to be swept.
    for k in 0..MOVERS {
        let (x, z) = mover_at(k, 0);
        let b = Body::sphere(Vec3::new(x, ground(x, z) + 0.5, z), 0.5);
        w.add_body(b);
    }
    // A driven craft, the same as the parked one.
    w.add_compound(craft(Vec3::new(-20.0, ground(-20.0, -20.0) + 0.3, -20.0)));
    w
}

const MOVERS: usize = 4;

/// Where mover `k` heads at tick `t`: round a circle of 8 m about (-25, 25).
fn mover_at(k: usize, t: usize) -> (f32, f32) {
    let a = t as f32 * DT * (0.5 + k as f32 * 0.4) + k as f32;
    (-25.0 + a.cos() * 8.0, 25.0 + a.sin() * 8.0)
}

/// Which lane the runner crosses: a fresh one each run, so it always meets
/// ground no query has meshed, as a rider heading out does.
static LANE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Keep the movers and the driven craft going, as a script steering them.
/// The last mover runs straight across the terrain at 12 m/s.
fn steer(w: &mut PhysicsWorld, t: usize) {
    let first = w.bodies.len() - MOVERS;
    let lane = LANE.load(std::sync::atomic::Ordering::Relaxed) % 4;
    {
        let b = &mut w.bodies[first + MOVERS - 1];
        let (x, z) = (-50.0 + 12.0 * t as f32 * DT, -45.0 + lane as f32 * 25.0);
        if t == 0 {
            b.pos = Vec3::new(x, ground(x, z) + 0.5, z);
        }
        b.vel = Vec3::new(12.0, b.vel.y, (z - b.pos.z) / DT);
        b.asleep = false;
    }
    for k in 0..MOVERS - 1 {
        let (x, z) = mover_at(k, t + 1);
        let b = &mut w.bodies[first + k];
        let to = Vec3::new(x, b.pos.y, z) - b.pos;
        b.vel = Vec3::new(to.x / DT, b.vel.y, to.z / DT).clamp_length_max(15.0);
        b.asleep = false;
    }
    let ci = w.compounds.len() - 1;
    let c = &mut w.compounds[ci];
    let m = c.mass;
    let a = t as f32 * DT * 0.3;
    c.apply_force(Vec3::new(a.cos(), 0.0, a.sin()) * m * 3.0);
}

#[derive(Default, Clone, Copy)]
struct Cost {
    ticks: usize,
    total: f64,
    change: f64,
    static_ms: f64,
    sweeps: f64,
    sleep: f64,
    broadphase: f64,
}

impl Cost {
    fn per(&self) -> String {
        let n = self.ticks.max(1) as f64;
        format!(
            "{:.3} ms a tick (change {:.3}, broadphase {:.3}, static {:.3}, sweeps {:.3}, sleep {:.3})",
            self.total / n,
            self.change / n,
            self.broadphase / n,
            self.static_ms / n,
            self.sweeps / n,
            self.sleep / n
        )
    }
    fn mean(&self) -> f64 {
        self.total / self.ticks.max(1) as f64
    }
}

/// `ticks` ticks, each with `change` made to the world first.
fn run(w: &mut PhysicsWorld, ticks: usize, mut change: impl FnMut(&mut PhysicsWorld, usize)) -> Cost {
    let mut c = Cost::default();
    LANE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    w.time_steps(true);
    let _ = w.take_timing();
    for k in 0..ticks {
        steer(w, k);
        let t0 = Instant::now();
        change(w, k);
        let t1 = Instant::now();
        w.step(DT);
        let t2 = Instant::now();
        c.change += (t1 - t0).as_secs_f64() * 1e3;
        c.total += (t2 - t0).as_secs_f64() * 1e3;
        c.ticks += 1;
    }
    let t = w.take_timing().unwrap_or_default();
    c.static_ms = t.static_ms as f64;
    c.sweeps = t.sweeps_ms as f64;
    c.sleep = t.sleep_ms as f64;
    c.broadphase = t.broadphase_ms as f64;
    c
}

fn asleep(w: &PhysicsWorld) -> (usize, usize) {
    (w.bodies.iter().filter(|b| b.asleep).count() + w.compounds.iter().filter(|c| c.asleep).count(), w.bodies.len() + w.compounds.len())
}

/// Where everything stands, to show a case moved nothing it should not.
fn poses(w: &PhysicsWorld) -> Vec<Vec3> {
    w.bodies.iter().take(props()).map(|b| b.pos).chain(w.compounds.iter().take(1).map(|c| c.pos)).collect()
}

fn case(name: &str, change: impl FnMut(&mut PhysicsWorld, usize)) {
    case_from(name, |_| {}, change);
}

/// [`case`], with `setup` made to the settled scene before anything is timed.
fn case_from(name: &str, setup: impl FnOnce(&mut PhysicsWorld), mut change: impl FnMut(&mut PhysicsWorld, usize)) {
    let mut w = scene();
    setup(&mut w);
    LANE.store(0, std::sync::atomic::Ordering::Relaxed);
    for t in 0..120 {
        steer(&mut w, t);
        w.step(DT);
    }
    let before = poses(&w);
    let steady = run(&mut w, STEADY, |_, _| {});
    let (s0, n) = asleep(&w);
    let during = run(&mut w, STREAM, &mut change);
    let (s1, _) = asleep(&w);
    let after = run(&mut w, STEADY, |_, _| {});
    let moved = poses(&w).iter().zip(&before).map(|(a, b)| a.distance(*b)).fold(0.0f32, f32::max);
    println!("{name}:");
    println!("  steady  {}", steady.per());
    println!("  during  {}", during.per());
    println!("  after   {}", after.per());
    println!(
        "  during / steady {:.2}; asleep {s0}/{n} before, {s1}/{n} after the stream; the props and the craft moved {moved:.4} m at most",
        during.mean() / steady.mean()
    );
}

fn main() {
    // A streamed level: a piece a tick, in a ring well clear of everything.
    // Built before the timing starts: what is timed is the level taking
    // them, not the mesh being made.
    let far = |k: usize| {
        let a = k as f32 * 0.37;
        let r = 200.0 + (k % 7) as f32 * 30.0;
        AnchoredCollider::world(Box::new(piece(a.cos() * r, a.sin() * r, 0.0)))
    };
    let mut pieces: Vec<AnchoredCollider> = (0..STREAM).map(far).collect();
    case("colliders (a piece added a tick)", move |w, _| {
        let mut cols = std::mem::take(&mut w.colliders);
        cols.extend(pieces.pop());
        w.set_colliders(cols);
    });
    case_from(
        "removed (a piece let go a tick)",
        |w| {
            let mut cols = std::mem::take(&mut w.colliders);
            cols.extend((0..STREAM).map(far));
            w.set_colliders(cols);
        },
        |w, _| {
            let mut cols = std::mem::take(&mut w.colliders);
            if cols.len() > 2 {
                cols.pop();
            }
            w.set_colliders(cols);
        },
    );
    // The terrain chunks under the props, which a stand-in made real drops.
    let under: Vec<[i32; 3]> = {
        let t = terrain();
        let mut v: Vec<[i32; 3]> = (0..props())
            .flat_map(|i| {
                let (x, z) = prop_spot(i);
                t.field.chunks_in_world_box(Vec3::new(x - 1.0, ground(x, z) - 1.0, z - 1.0), Vec3::new(x + 1.0, ground(x, z) + 1.0, z + 1.0))
            })
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    println!("(the props stand on {} terrain chunks)", under.len());
    let u = under.clone();
    case("invalidate (a chunk under the props dropped a tick)", move |w, k| {
        let c = u[k % u.len()];
        if let Some(t) = w.colliders[0].shape.chunk_terrain_mut() {
            t.invalidate_meshes(&[c]);
        }
    });
    // Far ground put away or made again: a chunk at the edge of the world,
    // nowhere near anything that moves, dropped each tick.
    case("far invalidate (a far chunk dropped a tick)", |w, k| {
        if let Some(t) = w.colliders[0].shape.chunk_terrain_mut() {
            t.invalidate_meshes(&[[-3 + (k % 2) as i32, 0, -3]]);
        }
    });
    let u = under.clone();
    let built: Vec<_> = {
        let t = terrain();
        u.iter().map(|&c| CollisionChunk::from_mesh(&drawn_chunk(&t.field, c), t.field.voxel())).collect()
    };
    case("adopt (a chunk under the props landed built a tick)", move |w, k| {
        let i = k % u.len();
        if let Some(t) = w.colliders[0].shape.chunk_terrain_mut() {
            t.adopt(u[i], built[i].clone());
        }
    });
    let mut swaps: Vec<TriMeshCollider> = (0..STREAM).map(|_| piece(-12.0, -12.0, 0.05)).collect();
    case("replace (the piece under the props swapped a tick)", move |w, _| {
        if let Some(p) = swaps.pop() {
            w.replace_collider_of(900, Default::default(), Box::new(p), 0);
        }
    });
}
