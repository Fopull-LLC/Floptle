//! Is carving actually cheaper than rebaking? — the question that decides
//! whether runtime obstacle carving is worth shipping at all.
//!
//! Run with `cargo run --release --example carve_probe -p floptle-nav`.
//!
//! Carving is an **option**, not a replacement: the background whole-level
//! rebake stays underneath as the thing that is always right, and it already
//! handles a building coming down mid-play. What carving has to justify is the
//! narrow case — one crate, on a level far bigger than the crate — and the only
//! honest way to decide that is to put the two side by side at sizes a real
//! level reaches.
//!
//! So this prints, per level size: what a full rebake costs from triangles, and
//! what putting one crate down and picking it up again costs. The last column
//! is the ratio, which is the number the decision actually rests on.

use std::time::Instant;

use floptle_nav::{Heightfield, NavMesh, NavSettings, Tri, WalkableGrid};

/// `nav_probe`'s level, so the two probes are measuring the same thing: a floor
/// with a grid of pillars in it, which gives the bake edges to erode and
/// regions to trace rather than one clean rectangle.
fn level(size: f32) -> Vec<Tri> {
    let mut tris = Vec::new();
    let quad = |x0: f32, z0: f32, w: f32, d: f32, y: f32, out: &mut Vec<Tri>| {
        out.push(Tri::new([x0, y, z0], [x0 + w, y, z0], [x0, y, z0 + d]));
        out.push(Tri::new([x0 + w, y, z0], [x0 + w, y, z0 + d], [x0, y, z0 + d]));
    };
    quad(0.0, 0.0, size, size, 0.0, &mut tris);
    let n = (size / 8.0) as i32;
    for i in 0..n {
        for j in 0..n {
            quad(i as f32 * 8.0 + 3.0, j as f32 * 8.0 + 3.0, 2.0, 2.0, 1.0, &mut tris);
        }
    }
    tris
}

fn bake(tris: &[Tri], s: &NavSettings) -> NavMesh {
    let field = Heightfield::build(tris, s).unwrap();
    let grid = WalkableGrid::build(&field, s).unwrap();
    NavMesh::build(&grid, s).unwrap()
}

fn main() {
    let s = NavSettings::default();
    println!("cell {} m, radius {} m — one 1 m crate, dropped and taken away", s.cell_size, s.agent_radius);
    println!(
        "{:>7} {:>8} {:>11} {:>11} {:>11} {:>9}",
        "size", "polys", "rebake", "carve", "remove", "rebake/carve"
    );

    for size in [32.0f32, 64.0, 128.0, 256.0] {
        let tris = level(size);
        let mut mesh = bake(&tris, &s);
        // Warm the index: a first query builds it, and that is a cost the level
        // pays once whether anything is carved or not. Charging it to the carve
        // would flatter the rebake.
        let _ = mesh.nearest([1.0, 0.0, 1.0], 1.0);

        // A full rebake, from the triangles — what happens today when something
        // moves and the editor's background rebake fires.
        let reps = 3;
        let t = Instant::now();
        for _ in 0..reps {
            std::hint::black_box(bake(&tris, &s));
        }
        let rebake_ms = t.elapsed().as_secs_f64() * 1000.0 / reps as f64;

        // One crate, somewhere in the middle, put down and picked up again.
        // Both halves are timed: a removal is a rebuild from the bake too, so
        // pretending only the carve costs anything would be a lie by omission.
        let reps = 50;
        let mid = size * 0.5;
        let mut carve_ms = 0.0;
        let mut remove_ms = 0.0;
        for k in 0..reps {
            // Nudged each time so the compiler cannot hoist the work out.
            let at = [mid + k as f32 * 0.01, 0.5, mid];
            let t = Instant::now();
            let id = mesh.carve(at, [1.0, 2.0, 1.0]);
            carve_ms += t.elapsed().as_secs_f64() * 1000.0;
            let t = Instant::now();
            mesh.remove_obstacle(id);
            remove_ms += t.elapsed().as_secs_f64() * 1000.0;
        }
        carve_ms /= reps as f64;
        remove_ms /= reps as f64;

        println!(
            "{size:>6}m {:>8} {rebake_ms:>10.2}m {carve_ms:>10.3}m {remove_ms:>10.3}m {:>11.0}x",
            mesh.polys.len(),
            rebake_ms / carve_ms.max(1e-9),
        );
    }

    println!();
    println!("A carve is a copy of the polygon list plus a local relink, so it grows with the");
    println!("LEVEL's polygon count and not with the crate — but only linearly, while a rebake");
    println!("re-voxelises every triangle. The gap is the point: if it ever closes, the honest");
    println!("answer is to drop carving and let the background rebake do the work.");
}
