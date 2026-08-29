//! What one windowed `random_point` costs, in milliseconds, at a range of radii.
//!
//! The per-frame "wander somewhere near me" query prices itself by how much of
//! the level its window covers, and a game hitting it a dozen times a frame
//! feels that. This prints absolutes so the curve can be looked at; the GUARD is
//! `tests/scaling.rs`, which asserts a ratio, because a duration on a shared
//! runner is a coin flip.
//!
//! The pillars matter: a flat square merges into ONE polygon and measures
//! nothing at all.
//!
//! Run: `cargo run --release -p floptle-nav --example wander_bench`
use floptle_nav::{bake, NavSettings, Tri};

fn quad(x0: f32, z0: f32, w: f32, d: f32, out: &mut Vec<Tri>) {
    let (x1, z1) = (x0 + w, z0 + d);
    out.push(Tri::new([x0, 0.0, z0], [x1, 0.0, z0], [x0, 0.0, z1]));
    out.push(Tri::new([x1, 0.0, z0], [x1, 0.0, z1], [x0, 0.0, z1]));
}

fn main() {
    let mut tris = Vec::new();
    quad(-100.0, -100.0, 200.0, 200.0, &mut tris);
    // Pillars, so the walkable surface fragments into many polygons — a flat
    // square merges into ONE rect and measures nothing at all.
    for i in 0..40 {
        for j in 0..40 {
            let (x, z) = (-98.0 + i as f32 * 5.0, -98.0 + j as f32 * 5.0);
            for (dx, dz) in [(0.0, 0.0), (0.0, 1.5), (1.5, 0.0), (1.5, 1.5)] {
                let (a, b) = (x + dx, z + dz);
                tris.push(Tri::new([a, 0.0, b], [a + 1.2, 0.0, b], [a, 3.0, b]));
                tris.push(Tri::new([a + 1.2, 0.0, b], [a + 1.2, 3.0, b], [a, 3.0, b]));
            }
        }
    }
    let mesh = bake(&tris, &NavSettings { cell_size: 0.5, ..Default::default() }).unwrap();
    println!("mesh: {} polys", mesh.polys.len());
    for r in [8.0f32, 12.0, 16.0, 25.0, 40.0, 80.0] {
        let n = 2000;
        let t = std::time::Instant::now();
        let mut acc = 0.0f32;
        for i in 0..n {
            let u = (i % 97) as f32 / 97.0;
            let v = (i % 89) as f32 / 89.0;
            if let Some(p) = mesh.random_point(Some(([0.0, 0.0, 0.0], r)), u, v) {
                acc += p[0];
            }
        }
        let per = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
        println!("r={r:>5.0}  {per:.4} ms/call   {}", std::hint::black_box(acc) as i64);
    }
}
