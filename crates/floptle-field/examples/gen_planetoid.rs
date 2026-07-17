//! Generate the Floptle Solar demo's starter planetoid — a seeded, bumpy little
//! planet written straight into the sparse chunk field and saved as the scene's
//! `.cfield`. This is the first D1 (procedural planets) experiment: sphere ± fbm
//! displacement through [`ChunkField::fill_with`], carved caves, colour by
//! altitude + noise patches.
//!
//! Usage:  cargo run --release -p floptle-field --example gen_planetoid [-- <out_dir> [seed]]
//! Default out_dir is `solar/terrain`, seed 7. The scene expects
//! `<out_dir>/planetoid.1.cfield` (scene "planetoid", terrain id 1).

use floptle_core::math::Vec3;
use floptle_core::noise::Noise;
use floptle_field::{Brush, BrushProfile, ChunkField};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let out_dir = args.get(1).map(String::as_str).unwrap_or("solar/terrain");
    let seed: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(7);

    const RADIUS: f32 = 30.0;
    let relief: f32 =
        std::env::var("RELIEF").ok().and_then(|v| v.parse().ok()).unwrap_or(4.5);
    let caves: u32 = std::env::var("CAVES").ok().and_then(|v| v.parse().ok()).unwrap_or(6);
    let voxel = 0.75;
    let noise = Noise::new(seed);

    let mut field = ChunkField::new(voxel);
    let ext = RADIUS + relief + 2.0;

    // The planet body: a sphere displaced by fbm of the DIRECTION (so relief rides
    // the surface, not world axes). Colour by altitude: lowlands mossy, midlands
    // rock, peaks icy — patched up by a second noise so it never looks banded.
    let t0 = std::time::Instant::now();
    field.fill_with(
        Vec3::splat(-ext),
        Vec3::splat(ext),
        |p| {
            let r = p.length();
            let dir = if r > 1e-3 { p / r } else { Vec3::X };
            let bump = noise.fbm(dir * 4.3, 5) * relief;
            let planet = r - (RADIUS + bump);
            if caves == 0 {
                return planet;
            }
            // Caves as part of the SDF, not carved after: tunnels live where two
            // independent noise fields are BOTH near zero (their implicit surfaces
            // intersect along winding curves). Gated to ≥1.2 units below the local
            // surface — you DIG to find them, which is the gameplay. (Carving with
            // brush dabs after the fill exposed band-clamped rock: literal terraces.)
            let a = noise.fbm(p * 0.07, 3);
            let b = noise.fbm(p * 0.07 + Vec3::splat(51.7), 3);
            let tunnel = (a.abs().max(b.abs()) - 0.11) * 9.0;
            // Keep ≥3 units of crust (4 voxels) — a thinner skin over a fat tunnel
            // meshes as pinholes.
            let gated = tunnel.max(3.0 + planet);
            planet.max(-gated)
        },
        |p| {
            // SMOOTH blends, not hard thresholds: each voxel picks its colour
            // independently, so an if/else per voxel renders as blocky square
            // patches (Ty's first playtest: "strange square patterns"). Smooth
            // weights make bands melt into each other at every scale.
            let smooth = |lo: f32, hi: f32, x: f32| {
                let t = ((x - lo) / (hi - lo)).clamp(0.0, 1.0);
                t * t * (3.0 - 2.0 * t)
            };
            let mix = |a: [f32; 3], b: [f32; 3], t: f32| {
                [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
            };
            let r = p.length();
            let dir = if r > 1e-3 { p / r } else { Vec3::X };
            let alt = (r - RADIUS) / relief; // ≈ -1 valleys .. +1 peaks at the surface
            let patch = noise.fbm(dir * 9.0 + Vec3::splat(37.0), 3);
            let rock = [0.45, 0.44, 0.47];
            let dust = [0.52, 0.44, 0.34];
            let moss = [0.27, 0.44, 0.23];
            let ice = [0.86, 0.88, 0.92];
            let deep = [0.24, 0.22, 0.26]; // dug-open interior: dark cave rock
            let mut c = mix(rock, dust, smooth(0.05, 0.5, patch));
            c = mix(c, moss, smooth(0.05, -0.35, alt) * smooth(-0.5, 0.0, patch));
            c = mix(c, ice, smooth(0.2, 0.5, alt));
            // Below the DISPLACED surface, fade to cave rock — dig walls and cave
            // interiors read as their own material instead of smeared surface.
            let bump = noise.fbm(dir * 4.3, 5) * relief;
            let depth = (RADIUS + bump) - r;
            mix(c, deep, smooth(1.2, 3.0, depth))
        },
    );
    let fill_ms = t0.elapsed().as_millis();

    // One starter dig site: a shallow crater at the "north pole" spawn so the first
    // thing you see hints that the ground is diggable. Brush dabs are fine ON the
    // fresh surface (the band is intact there).
    let t0 = std::time::Instant::now();
    field.sculpt(
        Brush::Lower,
        Vec3::new(2.5, RADIUS + noise.fbm(Vec3::Y * 4.3, 5) * relief, 0.0),
        2.0,
        0.5,
        BrushProfile::default(),
    );
    let caves_ms = t0.elapsed().as_millis();

    let (worst, frac) = field.lipschitz_audit();
    let bytes = field.to_bytes();
    std::fs::create_dir_all(out_dir).expect("create out dir");
    let path = format!("{out_dir}/planetoid.1.cfield");
    std::fs::write(&path, &bytes).expect("write cfield");
    println!(
        "planetoid seed {seed}: {} chunks, {:.1} MB resident, {:.1} MB on disk\n\
         fill {fill_ms} ms, caves {caves_ms} ms, |grad| worst {worst:.2} ({:.1}% over)\n\
         wrote {path}",
        field.data_chunks(),
        field.memory_bytes() as f64 / 1e6,
        bytes.len() as f64 / 1e6,
        frac * 100.0,
    );
}
