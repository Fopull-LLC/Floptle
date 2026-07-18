//! Generate the Floptle Solar demo's starter planetoid — a seeded, bumpy little
//! planet written straight into the sparse chunk field and saved as the scene's
//! `.cfield`. This is the D1 (procedural planets) generator: sphere ± fbm
//! displacement through [`ChunkField::fill_with_rgba`], carved caves, and now
//! real MATERIALS — the voxel alpha carries a terrain palette slot, so the
//! surface is textured biomes, digging exposes geological strata, and caves are
//! lined with vein rock, glowing magma seams and crystal pockets (the glowing
//! slots are what make caves readable at all: they bypass lighting + AO).
//!
//! Alongside the `.cfield`s this writes `<out_dir>/planetoid.palette` — the
//! per-scene slot→image list the editor adopts with the terrain (glowing slots
//! carry a `|glow` suffix). Texture paths are written ABSOLUTE: asset paths
//! resolve as-is from the editor's CWD, and the solar project lives outside it.
//!
//! Usage:  cargo run --release -p floptle-field --example gen_planetoid [-- <out_dir> [seed]]
//! Default out_dir is `solar/terrain`, seed 7. The scene expects
//! `<out_dir>/planetoid.1.cfield` (scene "planetoid", terrain id 1).

use floptle_core::math::Vec3;
use floptle_core::noise::Noise;
use floptle_field::{Brush, BrushProfile, ChunkField};

/// The scene's terrain texture palette, in slot order (1-based in voxel alpha).
/// Ordering is GEOLOGICAL on purpose: the shader crossfades fractional slot
/// values at material boundaries, sweeping through every slot in between — so
/// neighbours in this list must be neighbours in the ground (surface → strata →
/// cave), and the planet's run ends before the moon's begins.
const PALETTE: [(&str, bool); 12] = [
    ("planet_rust_rock.png", false),      // 1  planet surface: highlands
    ("planet_purple_dirt.png", false),    // 2  planet surface: lowlands
    ("planet_purple_slate.png", false),   // 3  shallow strata
    ("planet_banded_onyx.png", false),    // 4  deep strata
    ("cave_orange_veinstone.png", false), // 5  cave-zone wall rock
    ("cave_magma_veins.png", true),       // 6  deep cave magma seams (GLOWS)
    ("cave_amethyst.png", true),          // 7  planet crystal pockets (GLOWS)
    ("cave_blue_crystal.png", true),      // 8  moon crystal pockets (GLOWS)
    ("moon_silver_slate.png", false),     // 9  moon subsurface
    ("moon_gray_regolith.png", false),    // 10 moon surface
    ("moon_crater_dust.png", false),      // 11 crater floors
    ("moon_teal_ice.png", false),         // 12 polar / patch ice
];

/// Tint × brightness-variation → voxel RGB bytes. The shader computes
/// `albedo = texture × tint × 1.6`, so ~0.63 here is "texture as authored";
/// the fbm-driven `vary` breaks up tiling without a second texture.
/// Channels are QUANTIZED to 12-step levels: the cfield stores colors RLE'd
/// along x, and a smoothly-drifting tint that changes every voxel turns every
/// run into length 1 (measured: 28 MB → 84 MB on disk). Steps keep the
/// variation but let neighbouring voxels share exact bytes again.
fn tint(base: [f32; 3], vary: f32) -> [u8; 3] {
    let v = 0.85 + 0.25 * vary.clamp(-1.0, 1.0);
    let f = |c: f32| (((c * v).clamp(0.0, 1.0) * 255.0 / 12.0).round() * 12.0) as u8;
    [f(base[0]), f(base[1]), f(base[2])]
}

fn rgba(rgb: [u8; 3], slot: u8) -> [u8; 4] {
    [rgb[0], rgb[1], rgb[2], slot]
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let out_dir = args.get(1).map(String::as_str).unwrap_or("solar/terrain");
    let seed: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(7);

    // Planet-scale world (Ty: "these planets aren't a realistic scale"): radius
    // 300 with µ tuned to g ≈ 9.8 at the surface. Orbital velocity in low orbit
    // ≈ 53 u/s, escape ≈ 76 — you REACH orbit with technique, you don't trip
    // into it. Voxel 2.0: crust chunks only ≈ tens of MB resident (sparse).
    const RADIUS: f32 = 340.0;
    let relief: f32 =
        std::env::var("RELIEF").ok().and_then(|v| v.parse().ok()).unwrap_or(14.0);
    let caves: u32 = std::env::var("CAVES").ok().and_then(|v| v.parse().ok()).unwrap_or(6);
    let voxel = 2.0;
    let noise = Noise::new(seed);

    let mut field = ChunkField::new(voxel);
    let ext = RADIUS + relief + 2.0;

    // The planet body: a sphere displaced by fbm of the DIRECTION (so relief rides
    // the surface, not world axes). Materials by DEPTH below the displaced surface
    // (strata you dig through) + biome patches on the surface itself; crystal and
    // magma pockets live where their own noise fields peak.
    let t0 = std::time::Instant::now();
    field.fill_with_rgba(
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
            // intersect along winding curves). Gated below the local surface —
            // you DIG to find them, which is the gameplay. (Carving with brush
            // dabs after the fill exposed band-clamped rock: literal terraces.)
            let a = noise.fbm(p * 0.018, 3);
            let b = noise.fbm(p * 0.018 + Vec3::splat(51.7), 3);
            let tunnel = (a.abs().max(b.abs()) - 0.1) * 34.0;
            // Keep ≥3 voxels of crust (pinhole guard), and confine caves to the
            // outer 50 units: a cave network running to the CORE keeps every
            // interior chunk resident (measured: 389 MB → this cut it ~4×).
            let gated = tunnel.max(6.0 + planet).max(-(planet + 50.0));
            planet.max(-gated)
        },
        |p| {
            let r = p.length();
            let dir = if r > 1e-3 { p / r } else { Vec3::X };
            let bump = noise.fbm(dir * 4.3, 5) * relief;
            let alt = (r - RADIUS) / relief; // ≈ -1 valleys .. +1 peaks at the surface
            let depth = (RADIUS + bump) - r; // how far below the displaced surface
            let vary = noise.fbm(p * 0.05 + Vec3::splat(83.0), 2);

            // Crystal pockets: sparse fbm peaks anywhere below the topsoil. They
            // glow (palette slot 7), so a dig or a cave wall that cuts one open
            // reads instantly even in the dark.
            let pocket = noise.fbm(p * 0.05 + Vec3::splat(17.3), 3);
            if depth > 8.0 && pocket > 0.38 {
                return rgba(tint([0.72, 0.65, 0.85], vary), 7);
            }
            // Deep cave zone: vein rock walls; magma seams (glowing) where their
            // own noise field peaks — the cave "floor lighting".
            if depth > 30.0 && noise.fbm(p * 0.035 + Vec3::splat(5.9), 3) > 0.12 {
                return rgba(tint([0.95, 0.85, 0.72], vary), 6);
            }
            if depth > 26.0 {
                return rgba(tint([0.78, 0.68, 0.58], vary), 5);
            }
            // Strata: slate under the topsoil, banded onyx beneath that. Exposed
            // in dig walls and shallow cave ceilings — the ground has HISTORY.
            if depth > 10.0 {
                return rgba(tint([0.85, 0.78, 0.66], vary), 4);
            }
            if depth > 2.2 {
                return rgba(tint([0.6, 0.58, 0.68], vary), 3);
            }
            // Surface biomes: rusty highland rock vs purple lowland dirt, patched
            // by fbm of the direction (rides the surface, not world axes) with an
            // altitude bias so peaks trend rocky and basins trend dusty.
            let patch = noise.fbm(dir * 9.0 + Vec3::splat(37.0), 3);
            if patch + alt * 0.45 > 0.08 {
                rgba(tint([0.72, 0.62, 0.58], vary), 1)
            } else {
                rgba(tint([0.66, 0.6, 0.74], vary), 2)
            }
        },
    );
    let fill_ms = t0.elapsed().as_millis();

    // One starter dig site: a shallow crater at the "north pole" spawn so the first
    // thing you see hints that the ground is diggable. Brush dabs are fine ON the
    // fresh surface (the band is intact there).
    let t0 = std::time::Instant::now();
    field.sculpt(
        Brush::Lower,
        Vec3::new(-(RADIUS + noise.fbm(Vec3::NEG_X * 4.3, 5) * relief), 5.0, 0.0),
        4.0,
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

    // ---- Pebble, the moon (terrain id 2): a cratered grey-ice ball. Every
    // celestial body in the demo is REAL terrain — walkable, diggable, its own
    // collider — not a prop sphere. Craters are part of the fill SDF (spherical
    // dents subtracted at seeded surface points; carving after the fill would
    // hit band-clamped rock and terrace).
    let t0 = std::time::Instant::now();
    const MOON_R: f32 = 64.0;
    let mnoise = Noise::new(seed.wrapping_mul(31).wrapping_add(5));
    let mut rng = floptle_core::noise::Rng::new(seed.wrapping_add(99));
    let craters: Vec<(Vec3, f32)> = (0..14)
        .map(|_| {
            let d = Vec3::new(
                rng.range(-1.0, 1.0) as f32,
                rng.range(-1.0, 1.0) as f32,
                rng.range(-1.0, 1.0) as f32,
            )
            .normalize_or_zero();
            let d = if d == Vec3::ZERO { Vec3::X } else { d };
            (d * (MOON_R + 2.0), rng.range(6.0, 14.0) as f32)
        })
        .collect();
    let mut moon = ChunkField::new(1.0);
    let mext = MOON_R + 6.0;
    moon.fill_with_rgba(
        Vec3::splat(-mext),
        Vec3::splat(mext),
        |p| {
            let r = p.length();
            let dir = if r > 1e-3 { p / r } else { Vec3::X };
            let bump = mnoise.fbm(dir * 5.1, 4) * 3.0;
            let mut d = r - (MOON_R + bump);
            for (c, cr) in &craters {
                let dent = (p - *c).length() - cr;
                d = d.max(-dent); // subtract the crater ball
            }
            d
        },
        |p| {
            let r = p.length();
            let dir = if r > 1e-3 { p / r } else { Vec3::X };
            let bump = mnoise.fbm(dir * 5.1, 4) * 3.0;
            let depth = (MOON_R + bump) - r;
            let vary = mnoise.fbm(p * 0.12 + Vec3::splat(29.0), 2);

            // Buried crystal seams (glowing slot 8): the reason to dig the moon.
            let pocket = mnoise.fbm(p * 0.09 + Vec3::splat(7.7), 3);
            if depth > 3.0 && pocket > 0.45 {
                return rgba(tint([0.68, 0.78, 0.95], vary), 8);
            }
            if depth > 2.5 {
                return rgba(tint([0.6, 0.62, 0.68], vary), 9);
            }
            // Crater floors wear dust — the surface inside any dent's ball.
            for (c, cr) in &craters {
                if (p - *c).length() < cr + 1.6 {
                    return rgba(tint([0.78, 0.74, 0.66], vary), 11);
                }
            }
            // Polar caps + scattered frost patches; regolith elsewhere.
            let patch = mnoise.fbm(dir * 7.0 + Vec3::splat(11.0), 3);
            if dir.y.abs() > 0.72 || patch < -0.42 {
                rgba(tint([0.8, 0.88, 0.94], vary), 12)
            } else {
                rgba(tint([0.66, 0.66, 0.68], vary), 10)
            }
        },
    );
    let moon_ms = t0.elapsed().as_millis();
    let (mworst, mfrac) = moon.lipschitz_audit();
    let mbytes = moon.to_bytes();
    let mpath = format!("{out_dir}/planetoid.2.cfield");
    std::fs::write(&mpath, &mbytes).expect("write moon cfield");
    println!(
        "moon      seed {seed}: {} chunks, {:.1} MB on disk, fill {moon_ms} ms, \
         |grad| worst {mworst:.2} ({:.1}% over)\nwrote {mpath}",
        moon.data_chunks(),
        mbytes.len() as f64 / 1e6,
        mfrac * 100.0,
    );

    // ---- The palette sidecar: slot → image path (+ `|glow` for self-lit slots).
    // The editor adopts it with the terrain (adopt_terrain). Absolute paths on
    // purpose — the editor loads them as-is regardless of its CWD.
    let tex_dir = std::path::Path::new(out_dir)
        .parent()
        .map(|p| p.join("textures/terrain"))
        .unwrap_or_else(|| "solar/textures/terrain".into());
    let lines: Vec<String> = PALETTE
        .iter()
        .map(|(name, glow)| {
            let p = tex_dir.join(name);
            let abs = std::fs::canonicalize(&p).unwrap_or(p);
            format!("{}{}", abs.display(), if *glow { "|glow" } else { "" })
        })
        .collect();
    let ppath = format!("{out_dir}/planetoid.palette");
    std::fs::write(&ppath, lines.join("\n")).expect("write palette");
    println!("palette   12 slots (3 glowing) → {ppath}");
}
