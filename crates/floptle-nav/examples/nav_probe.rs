//! What a bake costs, at sizes a level actually reaches.
//!
//! Run with `cargo run --release --example nav_probe -p floptle-nav`. Prints the
//! three stages separately, because they scale differently and knowing which one
//! is the expensive one is the whole reason to look.

use std::time::Instant;

use floptle_nav::{Heightfield, NavMesh, NavSettings, Tri, WalkableGrid};

/// A floor `size` metres square, with a grid of pillars in it so the bake has
/// edges to erode and regions to trace rather than one clean rectangle.
fn level(size: f32) -> Vec<Tri> {
    let mut tris = Vec::new();
    let quad = |x0: f32, z0: f32, w: f32, d: f32, y: f32, out: &mut Vec<Tri>| {
        out.push(Tri::new([x0, y, z0], [x0 + w, y, z0], [x0, y, z0 + d]));
        out.push(Tri::new([x0 + w, y, z0], [x0 + w, y, z0 + d], [x0, y, z0 + d]));
    };
    quad(0.0, 0.0, size, size, 0.0, &mut tris);
    // A pillar every 8 m, as a low roof the agent cannot stand under.
    let n = (size / 8.0) as i32;
    for i in 0..n {
        for j in 0..n {
            quad(i as f32 * 8.0 + 3.0, j as f32 * 8.0 + 3.0, 2.0, 2.0, 1.0, &mut tris);
        }
    }
    tris
}

fn main() {
    let settings = NavSettings::default();
    println!("cell {} m, radius {} m", settings.cell_size, settings.agent_radius);
    println!("{:>7} {:>10} {:>10} {:>10} {:>10} {:>9} {:>8}", "size", "tris", "field", "walk", "mesh", "cells", "polys");

    for size in [32.0f32, 64.0, 128.0, 256.0] {
        let tris = level(size);

        let t = Instant::now();
        let field = Heightfield::build(&tris, &settings).unwrap();
        let field_ms = t.elapsed().as_secs_f64() * 1000.0;

        let t = Instant::now();
        let grid = WalkableGrid::build(&field, &settings).unwrap();
        let walk_ms = t.elapsed().as_secs_f64() * 1000.0;

        let t = Instant::now();
        let mesh = NavMesh::build(&grid, &settings).unwrap();
        let mesh_ms = t.elapsed().as_secs_f64() * 1000.0;

        println!(
            "{size:>6}m {:>10} {field_ms:>9.1}m {walk_ms:>9.1}m {mesh_ms:>9.1}m {:>9} {:>8}",
            tris.len(),
            grid.cells.len(),
            mesh.polys.len(),
        );

        // And what a query costs once it is baked, which is the number that runs
        // every frame rather than once.
        let from = [1.0, 0.0, 1.0];
        let to = [size - 1.0, 0.0, size - 1.0];
        let t = Instant::now();
        let mut hops = 0;
        for _ in 0..100 {
            hops += mesh.path(from, to).map(|p| p.points.len()).unwrap_or(0);
        }
        let path_ms = t.elapsed().as_secs_f64() * 1000.0 / 100.0;

        // Split out the snap, because a path is a search plus two of these and
        // it is not obvious which half the time is in.
        let t = Instant::now();
        for _ in 0..100 {
            mesh.nearest(from, 2.0);
            mesh.nearest(to, 2.0);
        }
        let snap_ms = t.elapsed().as_secs_f64() * 1000.0 / 100.0;

        println!(
            "         path {path_ms:>6.2} ms each ({snap_ms:.2} of it snapping), {} corners",
            hops / 100
        );
    }
}
