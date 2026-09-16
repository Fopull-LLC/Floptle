//! Drop capsule bodies onto a real project's terrain and measure how far each
//! settled body's feet sit from the drawn surface — the surface-nets triangles
//! at stride 1, transformed exactly as the renderer places them.
//!
//! Run:
//! ```text
//! cargo run --release -p floptle-physics --example terrain_rest_probe -- \
//!     "<project>/terrain/<scene>.<id>.cfield" <scale> <anchor_y> [radius] [height]
//! ```
//!
//! Two numbers per body, both in world units:
//! - `contact`: distance from the bottom sphere's centre to the closest drawn
//!   point, minus the radius. Zero means the capsule touches the picture.
//! - `feet`: height of the capsule's lowest point above the drawn ground
//!   straight below it (vertical ray vs triangles). This is what the player
//!   SEES: a model whose feet sit at the capsule's bottom hovers by this much
//!   (or clips by this much when negative).
use floptle_core::math::{DVec3, Quat, Vec3};
use floptle_physics::*;

struct Tri([Vec3; 3]);

fn ray_tri(o: Vec3, d: Vec3, t: &Tri) -> Option<f32> {
    let [a, b, c] = t.0;
    let e1 = b - a;
    let e2 = c - a;
    let p = d.cross(e2);
    let det = e1.dot(p);
    if det.abs() < 1e-9 {
        return None;
    }
    let inv = 1.0 / det;
    let s = o - a;
    let u = s.dot(p) * inv;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = s.cross(e1);
    let v = d.dot(q) * inv;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let t = e2.dot(q) * inv;
    (t > 0.0).then_some(t)
}

fn closest_on_tri(p: Vec3, t: &Tri) -> Vec3 {
    let [a, b, c] = t.0;
    // Barycentric clamp via projection onto the plane, then edges.
    let n = (b - a).cross(c - a);
    if n.length_squared() <= 1e-12 {
        return Vec3::splat(f32::MAX / 4.0); // zero-area: nothing to be closest to
    }
    let n = n.normalize();
    let q = p - n * (p - a).dot(n);
    let edge = |x: Vec3, y: Vec3| {
        let d = y - x;
        let s = ((p - x).dot(d) / d.length_squared().max(1e-12)).clamp(0.0, 1.0);
        x + d * s
    };
    // Inside test.
    let inside = |q: Vec3| {
        let c0 = (b - a).cross(q - a).dot(n);
        let c1 = (c - b).cross(q - b).dot(n);
        let c2 = (a - c).cross(q - c).dot(n);
        c0 >= 0.0 && c1 >= 0.0 && c2 >= 0.0
    };
    if inside(q) {
        return q;
    }
    [edge(a, b), edge(b, c), edge(c, a)]
        .into_iter()
        .min_by(|x, y| (*x - p).length().partial_cmp(&(*y - p).length()).unwrap())
        .unwrap()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("path to a .cfield");
    let scale: f32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1.0);
    let anchor_y: f64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let radius: f32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(0.35);
    let height: f32 = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(2.4);
    let bytes = std::fs::read(path).expect("read cfield");
    let field = floptle_field::ChunkField::from_bytes(&bytes).expect("parse cfield");
    println!("voxel {} band {} chunks {}", field.voxel(), field.band(), field.chunk_coords().len());
    let anchor = DVec3::new(0.0, anchor_y, 0.0);
    let rot = Quat::IDENTITY;
    let place = |p: Vec3| anchor.as_vec3() + rot * (p * scale);

    // The drawn surface, in world space.
    let mut tris: Vec<Tri> = Vec::new();
    for c in field.chunk_coords() {
        let m = floptle_field::mesh_chunk(&field, c, 1, false);
        let o = Vec3::from(m.origin);
        for t in m.indices.as_chunks::<3>().0 {
            let at = |i: u32| place(o + Vec3::from(m.positions[i as usize]));
            tris.push(Tri([at(t[0]), at(t[1]), at(t[2])]));
        }
    }
    let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
    for t in &tris {
        for p in t.0 {
            lo = lo.min(p);
            hi = hi.max(p);
        }
    }
    println!("drawn tris {} bounds {lo:?} .. {hi:?}", tris.len());

    let ground_under = |x: f32, z: f32| -> Option<(f32, Vec3)> {
        let o = Vec3::new(x, hi.y + 10.0, z);
        let d = Vec3::NEG_Y;
        let mut best: Option<(f32, Vec3)> = None;
        for t in &tris {
            if let Some(tt) = ray_tri(o, d, t) {
                let n = (t.0[1] - t.0[0]).cross(t.0[2] - t.0[0]).normalize_or_zero();
                if best.is_none_or(|(b, _)| tt < b) {
                    best = Some((tt, n));
                }
            }
        }
        best.map(|(t, n)| (o.y - t, n))
    };

    // Optional: the drawn ground height under one world (x, z) — e.g. a spawn.
    if let (Some(x), Some(z)) = (
        args.get(6).and_then(|s| s.parse::<f32>().ok()),
        args.get(7).and_then(|s| s.parse::<f32>().ok()),
    ) {
        match ground_under(x, z) {
            Some((y, n)) => println!("ground under ({x}, {z}): y = {y:.4}, normal {n:?}"),
            None => println!("ground under ({x}, {z}): none"),
        }
    }

    // `--log <file> --half <h>`: a position log from a real Play session
    // (`POS x y z grounded` per tick, world units, capsule centre). For every
    // logged tick, how far the capsule's lowest point sits from the drawn
    // ground under it, and from the field's own zero crossing under it —
    // which of the two surfaces the body actually rests on.
    if let Some(i) = args.iter().position(|a| a == "--log") {
        let file = args.get(i + 1).expect("--log <file>");
        let half: f32 = args
            .iter()
            .position(|a| a == "--half")
            .and_then(|j| args.get(j + 1))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.85);
        let to_local = |p: Vec3| (p - anchor.as_vec3()) / scale;
        // The field's surface straight under (x, z), near height `y0`:
        // march down from above until the field reads inside, then bisect.
        let field_under = |x: f32, z: f32, y0: f32| -> Option<f32> {
            let mut y_hi = y0 + 4.0;
            if field.d(to_local(Vec3::new(x, y_hi, z))) <= 0.0 {
                return None; // inside at the top: no clean crossing here
            }
            let mut y = y_hi;
            let mut y_lo = None;
            for _ in 0..400 {
                y -= 0.05;
                if field.d(to_local(Vec3::new(x, y, z))) <= 0.0 {
                    y_lo = Some(y);
                    break;
                }
                y_hi = y;
            }
            let mut y_lo = y_lo?;
            for _ in 0..30 {
                let m = 0.5 * (y_lo + y_hi);
                if field.d(to_local(Vec3::new(x, m, z))) <= 0.0 {
                    y_lo = m;
                } else {
                    y_hi = m;
                }
            }
            Some(0.5 * (y_lo + y_hi))
        };
        let text = std::fs::read_to_string(file).expect("read log");
        let mut rows: Vec<(f32, f32, f32, f32, f32, bool)> = Vec::new(); // x z feet mesh_gap field_gap grounded
        for line in text.lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 5 || f[0] != "POS" {
                continue;
            }
            let (x, y, z): (f32, f32, f32) =
                (f[1].parse().unwrap(), f[2].parse().unwrap(), f[3].parse().unwrap());
            let grounded = f[4] == "1";
            if y < -60.0 {
                continue; // fell off the world
            }
            let feet = y - (half + radius);
            let Some((gm, _)) = ground_under(x, z) else { continue };
            let Some(gf) = field_under(x, z, feet) else { continue };
            rows.push((x, z, feet, feet - gm, feet - gf, grounded));
        }
        let g: Vec<_> = rows.iter().filter(|r| r.5).collect();
        let pct = |v: &mut Vec<f32>, q: f32| {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v[((v.len() - 1) as f32 * q) as usize]
        };
        let mut am: Vec<f32> = g.iter().map(|r| r.3.abs()).collect();
        let mut af: Vec<f32> = g.iter().map(|r| r.4.abs()).collect();
        let closer_to_field = g.iter().filter(|r| r.4.abs() < r.3.abs()).count();
        println!(
            "log {file}: {} ticks on the terrain, {} grounded",
            rows.len(),
            g.len()
        );
        if !am.is_empty() {
            println!(
                "  |feet - DRAWN ground|  median {:.3}  p90 {:.3}  p99 {:.3}  max {:.3}",
                pct(&mut am, 0.5), pct(&mut am, 0.9), pct(&mut am, 0.99), pct(&mut am, 1.0)
            );
            println!(
                "  |feet - FIELD surface| median {:.3}  p90 {:.3}  p99 {:.3}  max {:.3}",
                pct(&mut af, 0.5), pct(&mut af, 0.9), pct(&mut af, 0.99), pct(&mut af, 1.0)
            );
            println!(
                "  ticks nearer the field than the drawn ground: {} of {}",
                closer_to_field,
                g.len()
            );
            let hover = g.iter().filter(|r| r.3 > 0.05).count();
            let clip = g.iter().filter(|r| r.3 < -0.05).count();
            println!("  vs drawn ground: hovering > 5 cm on {hover} ticks, sunk > 5 cm on {clip} ticks");
            let mut worst: Vec<_> = g.iter().collect();
            worst.sort_by(|a, b| b.3.abs().partial_cmp(&a.3.abs()).unwrap());
            for r in worst.iter().take(5) {
                println!("    worst: at ({:.2}, {:.2}) feet {:.3}: drawn gap {:+.3}, field gap {:+.3}", r.0, r.1, r.2, r.3, r.4);
            }
        }
        return;
    }

    let mut world = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -30.0, 0.0)));
    world.add_collider_tagged(
        anchor,
        Box::new(ChunkTerrain::posed(field.clone(), rot, scale)),
        0,
        None,
        false,
    );

    // Drops across the drawn surface, oversampling the STEEP spots: the flat
    // ones all read the same and the question is what a round bottom does on
    // a slope and in a crease.
    let n_target = 400;
    let mut seed = 0x9E3779B97F4A7C15u64;
    let mut rnd = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 11) as f32 / (1u64 << 53) as f32
    };
    let half = (height.max(2.0 * radius) * 0.5 - radius).max(0.0);
    // (slope°, contact, feet gap, toe clip, grounded)
    type Row = (f32, f32, f32, f32, bool);
    let mut rows: Vec<Row> = Vec::new();
    let mut tries = 0;
    while rows.len() < n_target && tries < n_target * 20 {
        tries += 1;
        let x = lo.x + (hi.x - lo.x) * (0.1 + 0.8 * rnd());
        let z = lo.z + (hi.z - lo.z) * (0.1 + 0.8 * rnd());
        let Some((gy, gn)) = ground_under(x, z) else { continue };
        let slope = gn.y.clamp(-1.0, 1.0).acos().to_degrees();
        // Keep every steep spot, one flat spot in eight.
        if slope < 15.0 && rnd() > 0.125 {
            continue;
        }
        let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -30.0, 0.0)));
        w.add_collider_tagged(
            anchor,
            Box::new(ChunkTerrain::posed(field.clone(), rot, scale)),
            0,
            None,
            false,
        );
        let b = Body::capsule(Vec3::new(x, gy + half + radius + 2.0, z), radius, height);
        w.add_body(b);
        for _ in 0..(120 * 4) {
            w.step(1.0 / 120.0);
        }
        let b = &w.bodies[0];
        let bottom_c = b.pos - b.up * half;
        let feet = bottom_c - b.up * radius;
        let mut best = f32::MAX;
        for t in &tris {
            let q = closest_on_tri(bottom_c, t);
            best = best.min((q - bottom_c).length());
        }
        let contact = best - radius;
        let under = ground_under(b.pos.x, b.pos.z).map(|(y, _)| y).unwrap_or(f32::NAN);
        let feet_gap = feet.y - under;
        // Toe clip: the drawn ground rising above the sole anywhere on a small
        // foot-sized disc around the centre line (0.22 = the knight's shoe).
        let mut toe = f32::MIN;
        for k in 0..8 {
            let a = k as f32 * std::f32::consts::TAU / 8.0;
            let (dx, dz) = (0.22 * a.cos(), 0.22 * a.sin());
            if let Some((gy, _)) = ground_under(b.pos.x + dx, b.pos.z + dz) {
                toe = toe.max(gy - feet.y);
            }
        }
        // The slope where it actually came to rest, not where it was dropped.
        let slope = ground_under(b.pos.x, b.pos.z)
            .map(|(_, n)| n.y.clamp(-1.0, 1.0).acos().to_degrees())
            .unwrap_or(slope);
        rows.push((slope, contact, feet_gap, toe.max(0.0), b.grounded));
    }
    println!("{:>7} {:>9} {:>9} {:>9} {:>8}", "slope°", "contact", "feet", "toeclip", "grounded");
    let bins = [(0.0, 10.0), (10.0, 20.0), (20.0, 30.0), (30.0, 40.0), (40.0, 90.0)];
    for (a, z) in bins {
        let sel: Vec<_> = rows.iter().filter(|r| r.0 >= a && r.0 < z).collect();
        if sel.is_empty() {
            continue;
        }
        let n = sel.len() as f32;
        let mean = |f: &dyn Fn(&Row) -> f32| sel.iter().map(|r| f(r)).sum::<f32>() / n;
        let max = |f: &dyn Fn(&Row) -> f32| sel.iter().map(|r| f(r)).fold(0.0f32, f32::max);
        println!(
            "slope {a:>2.0}-{z:<2.0}°  n={:<4} feet hover mean {:.3} max {:.3} | toe clip mean {:.3} max {:.3} | contact |max| {:.4} | grounded {}/{}",
            sel.len(),
            mean(&|r| r.2), max(&|r| r.2),
            mean(&|r| r.3), max(&|r| r.3),
            max(&|r| r.1.abs()),
            sel.iter().filter(|r| r.4).count(), sel.len()
        );
    }
    let _ = world;

}
