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
    // LAYERS=1: the alpine-and-canyon mix of relief layers on the same world.
    let spec = if std::env::var("LAYERS").is_ok() {
        use floptle_field::procgen::*;
        PlanetFill {
            mountains: Some(Mountains { height: radius * 0.2, ..Mountains::default() }),
            warp: Some(Warp { strength: 0.15, scale: 2.5 }),
            terrace: Some(Terrace { step: 5.0, sharpness: 0.8 }),
            overhangs: Some(Overhangs::default()),
            ravines: Some(Ravines::default()),
            rivers: Some(Rivers::default()),
            biomes: vec![Biome { paint: LayerPaint { slot: 12, color: [1.0; 3] }, slope: Some([35.0, 90.0]), ..Biome::default() }],
            ..spec
        }
    } else {
        spec
    };
    // DEFER=1: leave the buried cave zone for later.
    let spec = PlanetFill { defer_caves: std::env::var("DEFER").is_ok(), ..spec };
    let t = Instant::now();
    let field = generate_planet(&spec);
    let secs = t.elapsed().as_secs_f64();
    println!(
        "r {radius} m, voxel {voxel} m, caves {:.0} m: {secs:.2} s, {} data chunks, {} pending, {:.0} MB",
        spec.cave_depth,
        field.data_chunks(),
        field.pending_count(),
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
    if std::env::var("DIG").is_ok() {
        use floptle_field::{Brush, BrushProfile};
        let mut field = field;
        let mut mirror = field.clone();
        let mut shadow = field.to_dense(96).expect("a planet has bounds");
        let (mut sculpt, mut copy, mut dense, mut gather, mut n) = (0.0, 0.0, 0.0, 0.0, 0);
        let mut chunks = 0usize;
        for k in 0..50 {
            let a = k as f32 * 0.37;
            let dir = floptle_core::math::Vec3::new(a.cos(), 0.3, a.sin()).normalize();
            let Some(hit) = field.raycast(dir * (radius * 1.5), -dir, radius) else { continue };
            let t = Instant::now();
            let (touched, _) = field.sculpt_measured(Brush::Lower, hit, 2.4, 0.85, BrushProfile::default());
            sculpt += t.elapsed().as_secs_f64();
            let mut region = Vec::new();
            for c in &touched {
                for dz in -1..=1 { for dy in -1..=1 { for dx in -1..=1 { region.push([c[0] + dx, c[1] + dy, c[2] + dz]); } } }
            }
            region.sort_unstable();
            region.dedup();
            let t = Instant::now();
            mirror.copy_chunks_from(&field, &region);
            copy += t.elapsed().as_secs_f64();
            let pad = field.band() + field.voxel();
            let t = Instant::now();
            field.refresh_dense_region(&mut shadow, hit - floptle_core::math::Vec3::splat(2.4 + pad), hit + floptle_core::math::Vec3::splat(2.4 + pad));
            dense += t.elapsed().as_secs_f64();
            let t = Instant::now();
            for c in &touched {
                std::hint::black_box(floptle_field::scratch_for_chunk(&field, *c, 1));
            }
            gather += t.elapsed().as_secs_f64();
            chunks += touched.len();
            n += 1;
        }
        let ms = |v: f64| v * 1e3 / n as f64;
        println!("{n} digs, {:.1} chunks each: sculpt {:.3} ms, collider copy {:.3} ms ({} chunks), shadow {:.3} ms, remesh gathers {:.3} ms",
            chunks as f64 / n as f64, ms(sculpt), ms(copy), 27, ms(dense), ms(gather));
    }
}
