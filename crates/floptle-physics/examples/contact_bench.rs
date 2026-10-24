//! What compound contact costs: a wheeled craft rolling on a big static mesh,
//! and a thousand-part assembly released onto terrain.
//!
//! Run:
//! ```text
//! cargo run --release -p floptle-physics --example contact_bench
//! ```
//!
//! Prints the mean and worst milliseconds of physics per 60 Hz frame (two
//! 120 Hz steps) for each case, and where each craft ends up, so a change to
//! the contact pass can be checked for cost and for behaviour in one run; then
//! what twenty chase-camera sweeps past the thousand-part craft cost, with the
//! craft in `ignore` and not.
use floptle_core::math::{Quat, Vec3};
use floptle_physics::*;

/// A `n`×`n` grid of quads `step` apart, gently rolling: `2·n²` triangles.
fn runway(n: usize, step: f32) -> TriMeshCollider {
    let half = n as f32 * step * 0.5;
    let mut verts = Vec::new();
    for z in 0..=n {
        for x in 0..=n {
            let (px, pz) = (x as f32 * step - half, z as f32 * step - half);
            verts.push(Vec3::new(px, 0.05 * (px * 0.11).sin() * (pz * 0.07).cos(), pz));
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

/// Fifteen parts: a fuselage, wings, a tail and three wheels.
fn plane() -> Compound {
    let b = |h: [f32; 3]| ShapeGeom::Box { half: Vec3::from(h) };
    let mut s = vec![
        part(b([0.5, 0.5, 3.0]), Vec3::new(0.0, 1.2, 0.0), 400.0, 1),
        part(b([0.4, 0.4, 1.0]), Vec3::new(0.0, 1.2, 3.8), 100.0, 2),
        part(b([0.3, 0.3, 1.0]), Vec3::new(0.0, 1.3, -3.8), 80.0, 3),
        part(b([3.0, 0.08, 0.8]), Vec3::new(3.5, 1.3, 0.3), 60.0, 4),
        part(b([3.0, 0.08, 0.8]), Vec3::new(-3.5, 1.3, 0.3), 60.0, 5),
        part(b([1.2, 0.06, 0.4]), Vec3::new(1.4, 1.5, -4.4), 20.0, 6),
        part(b([1.2, 0.06, 0.4]), Vec3::new(-1.4, 1.5, -4.4), 20.0, 7),
        part(b([0.06, 0.8, 0.5]), Vec3::new(0.0, 2.2, -4.4), 15.0, 8),
        part(b([0.3, 0.3, 0.6]), Vec3::new(2.0, 1.0, 0.8), 30.0, 9),
        part(b([0.3, 0.3, 0.6]), Vec3::new(-2.0, 1.0, 0.8), 30.0, 10),
        part(b([0.1, 0.4, 0.1]), Vec3::new(0.0, 0.6, 3.5), 10.0, 11),
        part(b([0.1, 0.4, 0.1]), Vec3::new(1.5, 0.6, -0.5), 10.0, 12),
    ];
    s.push(part(ShapeGeom::Sphere { radius: 0.3 }, Vec3::new(0.0, 0.3, 3.5), 15.0, 13));
    s.push(part(ShapeGeom::Sphere { radius: 0.35 }, Vec3::new(1.5, 0.35, -0.5), 15.0, 14));
    s.push(part(ShapeGeom::Sphere { radius: 0.35 }, Vec3::new(-1.5, 0.35, -0.5), 15.0, 15));
    Compound::new(Vec3::new(0.0, 0.02, -150.0), Quat::IDENTITY, s)
}

/// A spine of 40 tanks, each ringed with fins, standing on its bottom tank:
/// 1,002 parts.
fn tower(base_y: f32) -> Compound {
    let mut s = Vec::new();
    let mut id = 0;
    for t in 0..40 {
        let y = base_y + 1.25 + t as f32 * 2.5;
        id += 1;
        s.push(part(ShapeGeom::Capsule { radius: 1.2, half_height: 0.05 }, Vec3::new(0.0, y, 0.0), 50.0, id));
        for f in 0..24 {
            let a = f as f32 / 24.0 * std::f32::consts::TAU;
            let o = Vec3::new(a.cos() * 1.45, y, a.sin() * 1.45);
            id += 1;
            let mut fin = part(ShapeGeom::Box { half: Vec3::new(0.25, 1.0, 0.04) }, o, 1.0, id);
            fin.rot = Quat::from_rotation_y(-a);
            s.push(fin);
        }
    }
    for k in 0..2 {
        id += 1;
        s.push(part(ShapeGeom::Sphere { radius: 0.6 }, Vec3::new(k as f32 * 1.6 - 0.8, base_y + 0.6, 0.0), 20.0, id));
    }
    let mut c = Compound::new(Vec3::ZERO, Quat::IDENTITY, s);
    c.anchored = true;
    c
}

fn frame(w: &mut PhysicsWorld) -> f64 {
    let t = std::time::Instant::now();
    w.step(1.0 / 120.0);
    w.step(1.0 / 120.0);
    t.elapsed().as_secs_f64() * 1e3
}

fn report(name: &str, ms: &[f64], c: &Compound) {
    let mean = ms.iter().sum::<f64>() / ms.len() as f64;
    let mut top: Vec<(usize, f64)> = ms.iter().cloned().enumerate().collect();
    top.sort_by(|a, b| b.1.total_cmp(&a.1));
    let worst: Vec<String> = top.iter().take(3).map(|(i, ms)| format!("{ms:.1} (frame {i})")).collect();
    println!(
        "{name}: mean {mean:.3} ms over {} frames, worst {}; ends at {:?} moving {:?}",
        ms.len(),
        worst.join(", "),
        c.pos,
        c.vel
    );
}

fn main() {
    let g = || GravityField::uniform(Vec3::new(0.0, -9.81, 0.0));

    let mut w = PhysicsWorld::new(g());
    let mesh = runway(100, 4.0);
    w.set_colliders(vec![AnchoredCollider::world(Box::new(mesh))]);
    let ci = w.add_compound(plane());
    for _ in 0..30 {
        frame(&mut w);
    }
    let mut ms = Vec::new();
    for _ in 0..300 {
        let v = w.compounds[ci].vel;
        w.compounds[ci].vel = Vec3::new(v.x, v.y, 40.0);
        ms.push(frame(&mut w));
    }
    report("plane on a 20,000-triangle mesh at 40 m/s", &ms, &w.compounds[ci]);

    for (name, with_mesh) in [("plane on terrain at 40 m/s", false), ("plane on a mesh over terrain at 40 m/s", true)] {
        let mut field = floptle_field::ChunkField::new(0.75);
        field.fill_slab(Vec3::new(-60.0, -6.0, -200.0), Vec3::new(60.0, 0.0, 200.0), 0.0, [0.5; 3]);
        let t = ChunkTerrain::new(field);
        let mut w = PhysicsWorld::new(g());
        let mut cols = vec![AnchoredCollider::world(Box::new(t))];
        if with_mesh {
            cols.push(AnchoredCollider::world(Box::new(runway(100, 4.0))));
        }
        w.set_colliders(cols);
        let mut c = plane();
        c.pos.y += 0.5;
        let ci = w.add_compound(c);
        for _ in 0..30 {
            frame(&mut w);
        }
        let mut ms = Vec::new();
        for _ in 0..300 {
            let v = w.compounds[ci].vel;
            w.compounds[ci].vel = Vec3::new(v.x, v.y, 40.0);
            ms.push(frame(&mut w));
        }
        report(name, &ms, &w.compounds[ci]);
    }

    let mut field = floptle_field::ChunkField::new(0.75);
    field.fill_slab(Vec3::new(-20.0, -6.0, -20.0), Vec3::new(20.0, 0.0, 20.0), 0.0, [0.5; 3]);
    let t = ChunkTerrain::new(field);
    let mut w = PhysicsWorld::new(g());
    let mut cols = vec![AnchoredCollider::world(Box::new(t))];
    // A base round the pad: sheds, towers and walls within reach of the
    // craft's sphere but nowhere near most of its parts. `NO_BASE=1` leaves
    // the pad bare.
    if std::env::var("NO_BASE").is_err() {
        for k in 0..60 {
            let a = k as f32 / 60.0 * std::f32::consts::TAU;
            let r = 8.0 + (k % 5) as f32 * 7.0;
            let h = 1.0 + (k % 7) as f32 * 2.0;
            cols.push(AnchoredCollider::world(Box::new(BoxShape::new(
                Vec3::new(a.cos() * r, h, a.sin() * r),
                Vec3::new(1.5, h, 1.5),
                Quat::IDENTITY,
            ))));
        }
        for k in 0..6 {
            let x = -15.0 + k as f32 * 6.0;
            let verts: Vec<Vec3> = (0..=10).flat_map(|z| (0..=10).map(move |y| Vec3::new(x, y as f32 * 3.0, -12.0 + z as f32))).collect();
            let mut idx = Vec::new();
            for z in 0..10u32 {
                for y in 0..10u32 {
                    let (a, b, c, d) = (z * 11 + y, z * 11 + y + 1, (z + 1) * 11 + y, (z + 1) * 11 + y + 1);
                    idx.extend_from_slice(&[a, c, b, b, c, d]);
                }
            }
            cols.push(AnchoredCollider::world(Box::new(TriMeshCollider::new(&verts, &idx))));
        }
    }
    w.set_colliders(cols);
    let ci = w.add_compound(tower(0.01));
    let mut ms = Vec::new();
    for _ in 0..60 {
        ms.push(frame(&mut w));
    }
    report("1,002 parts anchored on terrain", &ms, &w.compounds[ci]);
    w.compounds[ci].anchored = false;
    let mut ms = Vec::new();
    for _ in 0..120 {
        ms.push(frame(&mut w));
    }
    report("1,002 parts released onto terrain", &ms, &w.compounds[ci]);
    println!("asleep after release: {}", w.compounds[ci].asleep);
    let mut ms = Vec::new();
    for _ in 0..180 {
        ms.push(frame(&mut w));
    }
    report("1,002 parts resting on terrain", &ms, &w.compounds[ci]);
    println!("asleep after resting: {}", w.compounds[ci].asleep);

    // A raft of 1,000 boxes lying on the ground, every part on it: the case
    // a per-part cull cannot skip, since every part is near the terrain.
    // Then the same raft split in two, so the halves also meet each other.
    for split in [false, true] {
        let mut field = floptle_field::ChunkField::new(0.75);
        field.fill_slab(Vec3::new(-40.0, -6.0, -40.0), Vec3::new(40.0, 0.0, 40.0), 0.0, [0.5; 3]);
        let mut w = PhysicsWorld::new(g());
        w.set_colliders(vec![AnchoredCollider::world(Box::new(ChunkTerrain::new(field)))]);
        let shapes: Vec<CompoundShape> = (0..1000u64)
            .map(|k| {
                let o = Vec3::new((k % 32) as f32 * 1.02 - 16.0, 0.26, (k / 32) as f32 * 1.02 - 16.0);
                part(ShapeGeom::Box { half: Vec3::new(0.5, 0.25, 0.5) }, o, 5.0, k + 1)
            })
            .collect();
        let raft = w.add_compound(Compound::new(Vec3::ZERO, Quat::IDENTITY, shapes));
        if split {
            let half: Vec<u64> = (1..=500).collect();
            if let Some(other) = w.compounds[raft].split(&half) {
                w.add_compound(other);
            }
        }
        let mut ms = Vec::new();
        for _ in 0..90 {
            ms.push(frame(&mut w));
        }
        let name = if split { "1,000 boxes on the ground, split in two" } else { "1,000 boxes lying on the ground" };
        report(name, &ms, &w.compounds[raft]);
        println!("  asleep: {:?}", w.compounds.iter().map(|c| c.asleep).collect::<Vec<_>>());
    }

    // A chase camera's casts past the craft: its parts as the hulls queries
    // see, each owned by the craft's root (node 7 here).
    let c = &w.compounds[ci];
    let hulls: Vec<BodyHull> = (0..c.shapes.len())
        .map(|si| {
            let s = &c.shapes[si];
            let orient = (c.orient * s.rot).normalize();
            let (radius, shape) = match s.geom {
                ShapeGeom::Sphere { radius } => (radius, BodyShape::Sphere),
                ShapeGeom::Capsule { radius, half_height } => (radius, BodyShape::Capsule { half_height }),
                ShapeGeom::Box { half } => (half.min_element(), BodyShape::Box { half }),
            };
            BodyHull { eid: 100 + si as u32, owner: 7, pos: c.shape_center(si), radius, shape, up: orient * Vec3::Y, layer: 0, orient }
        })
        .collect();
    for (name, ignore) in [("not ignored", vec![]), ("ignored", vec![7u32])] {
        let mut best = f64::MAX;
        let mut hits = 0;
        for _ in 0..5 {
            let t = std::time::Instant::now();
            hits = 0;
            for k in 0..20 {
                let a = k as f32 * 0.7;
                let from = Vec3::new(a.cos() * 12.0, 3.0 + k as f32 * 4.0, a.sin() * 12.0);
                let to = Vec3::new(0.0, 2.0 + k as f32 * 4.5, 0.0);
                if spherecast(&w.colliders, &hulls, from, to - from, 0.25, 14.0, &ignore, !0, false).is_some() {
                    hits += 1;
                }
            }
            best = best.min(t.elapsed().as_secs_f64() * 1e3);
        }
        println!("20 spherecasts past 1,002 parts, {name}: {best:.3} ms ({hits} hit)");
    }
}
