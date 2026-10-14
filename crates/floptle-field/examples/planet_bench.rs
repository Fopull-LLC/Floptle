//! How long a generated planet takes to fill, and what it keeps resident.
//!
//! `cargo run --release -p floptle-field --example planet_bench -- <radius> [voxel] [cave_max]`
//!
//! The planet is shaped like a Solar world of that radius: relief 6.5% of the
//! radius, caves to 35% of it (capped at `cave_max`, default 80 m), a solid
//! core, a few craters.

use std::time::Instant;

use floptle_field::procgen::{generate_planet, PlanetFill};

fn main() {
    let args: Vec<f32> = std::env::args().skip(1).filter_map(|a| a.parse().ok()).collect();
    let radius = args.first().copied().unwrap_or(207.0);
    let voxel = args.get(1).copied().unwrap_or(1.5);
    let cave_max = args.get(2).copied().unwrap_or(80.0);
    let spec = PlanetFill {
        seed: 800921623,
        radius,
        voxel,
        relief: radius * 0.065,
        bump_freq: 4.5,
        cave_depth: (radius * 0.35).min(cave_max),
        core_r: (radius * 0.07).max(5.0),
        craters: 6,
        ..PlanetFill::default()
    };
    let t = Instant::now();
    let field = generate_planet(&spec);
    let secs = t.elapsed().as_secs_f64();
    println!(
        "r {radius} m, voxel {voxel} m, caves {:.0} m: {secs:.2} s, {} data chunks, {:.0} MB",
        spec.cave_depth,
        field.data_chunks(),
        field.memory_bytes() as f64 / 1e6,
    );
    if std::env::var("MESH").is_ok() {
        for stride in [1, 2, 4, 8] {
            let coords = field.chunk_coords();
            let t = Instant::now();
            for c in &coords {
                std::hint::black_box(floptle_field::mesh_chunk(&field, *c, stride, true));
            }
            let mesh = t.elapsed().as_secs_f64() * 1e3 / coords.len() as f64;
            let t = Instant::now();
            for c in &coords {
                std::hint::black_box(floptle_field::scratch_for_chunk(&field, *c, stride));
            }
            let gather = t.elapsed().as_secs_f64() * 1e3 / coords.len() as f64;
            println!("stride {stride}: {mesh:.3} ms a chunk, {gather:.3} of it gathering");
        }
    }
}
