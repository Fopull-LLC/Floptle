//! How long a whole-planet water step takes.
//!
//! `cargo run --release -p floptle-field --example water_bench -- <radius> [cell]`
use std::time::Instant;

use floptle_field::water::{lake_levels, ShallowWater, WaterGrid};

fn main() {
    let args: Vec<f32> = std::env::args().skip(1).filter_map(|a| a.parse().ok()).collect();
    let radius = args.first().copied().unwrap_or(400.0);
    let cell = args.get(1).copied().unwrap_or(2.0);
    let n = ((std::f32::consts::FRAC_PI_2 * radius) / cell).ceil() as usize;
    let noise = floptle_core::noise::Noise::new(7);
    let t = Instant::now();
    let grid = WaterGrid::sphere(n, radius, |d| noise.fbm(d * 4.5, 5) * radius * 0.065);
    let build = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let level = lake_levels(&grid, 0.0);
    let lakes = t.elapsed().as_secs_f64();
    let mut w = ShallowWater::at_rest(&grid, &level);
    let dt = w.stable_dt(&grid);
    let time = |w: &mut ShallowWater, grid: &WaterGrid, steps: usize| {
        let t = Instant::now();
        let mut awake = 0;
        for _ in 0..steps {
            w.step(grid, dt);
            awake += w.awake_count();
        }
        (t.elapsed().as_secs_f64() * 1e3 / steps as f64, awake / steps)
    };
    let (settled, _) = time(&mut w, &grid, 20);
    // A dig at the shore of the deepest water: a ten-cell-wide trench woken.
    let deepest = (0..grid.len()).max_by(|&a, &b| w.depth[a].total_cmp(&w.depth[b])).unwrap_or(0);
    let mut grid = grid;
    let mut ring = vec![deepest];
    for _ in 0..5 {
        let next: Vec<usize> = ring.iter().flat_map(|&c| grid.nbr[c]).filter(|&o| o != floptle_field::water::WALL).map(|o| o as usize).collect();
        ring.extend(next);
        ring.sort_unstable();
        ring.dedup();
    }
    for &c in &ring {
        grid.ground[c] -= 2.0;
        w.wake(c);
    }
    let (dig, awake) = time(&mut w, &grid, 200);
    println!(
        "r {radius} m, {n}² cells a face ({} cells): grid {build:.2} s, lakes {lakes:.2} s; a settled step {settled:.3} ms; after a dig {dig:.3} ms a step, {awake} cells awake (dt {dt:.3} s)",
        grid.len()
    );
}
