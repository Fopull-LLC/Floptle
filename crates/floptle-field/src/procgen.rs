//! Randomized solar-system generation — the solar demo's S7 generator grown
//! into a library, so the editor's "🪐 New Solar System" can build a whole
//! star system from one seed in a background thread (and the `gen_solar`
//! example stays a thin CLI over the same code).
//!
//! One master seed decides EVERYTHING: the star's class and brightness, how
//! many planets orbit it, each planet's archetype (canyon / dune / ice / lava
//! / crystal), radius, gravity, relief, atmosphere, cloud cover, cave network,
//! molten core, moons, names — so every generated system is a different place.
//!
//! Body variety rides per-voxel TINTS over one shared 12-slot texture palette
//! (`albedo = texture × tint`): the per-scene palette is a hard engine
//! constraint, so uniqueness comes from randomized hues, reliefs, noise
//! frequencies and layer thresholds, not from new textures.
//!
//! `generate_system` writes, for scene `<scene>`:
//!   `<out_dir>/<scene>.<id>.cfield`   (terrain ids 1..=N)
//!   `<out_dir>/<scene>.palette`       (shared palette, `|glow` suffixes)
//!   `<scenes_dir>/<scene>.ron`        (playable: bodies + cores + ship + HUD)

use floptle_core::math::Vec3;
use floptle_core::noise::{Noise, Rng};

use crate::ChunkField;

/// The shared 12-slot terrain palette every generated body draws from
/// (file name in the project's `textures/terrain/`, `true` = glowing slot).
pub const PALETTE: [(&str, bool); 12] = [
    ("planet_rust_rock.png", false),      // 1  rocky highlands
    ("planet_purple_dirt.png", false),    // 2  soft lowlands
    ("planet_purple_slate.png", false),   // 3  shallow strata
    ("planet_banded_onyx.png", false),    // 4  deep strata
    ("cave_orange_veinstone.png", false), // 5  cave-zone wall rock
    ("cave_magma_veins.png", true),       // 6  magma seams (GLOWS)
    ("cave_amethyst.png", true),          // 7  amethyst pockets (GLOWS)
    ("cave_blue_crystal.png", true),      // 8  blue crystal pockets (GLOWS)
    ("moon_silver_slate.png", false),     // 9  pale subsurface
    ("moon_gray_regolith.png", false),    // 10 regolith surface
    ("moon_crater_dust.png", false),      // 11 crater floors
    ("moon_teal_ice.png", false),         // 12 ice caps / frost
];

/// HSV → RGB, all channels 0..1 (`h` wraps).
fn hsv(h: f32, s: f32, v: f32) -> [f32; 3] {
    let h = (h.rem_euclid(1.0)) * 6.0;
    let i = h.floor();
    let f = h - i;
    let (p, q, t) = (v * (1.0 - s), v * (1.0 - s * f), v * (1.0 - s * (1.0 - f)));
    match i as u32 % 6 {
        0 => [v, t, p],
        1 => [q, v, p],
        2 => [p, v, t],
        3 => [p, q, v],
        4 => [t, p, v],
        _ => [v, p, q],
    }
}

fn tint(base: [f32; 3], vary: f32) -> [u8; 3] {
    let v = 0.85 + 0.25 * vary.clamp(-1.0, 1.0);
    // 12-step quantization: smooth per-voxel tints destroy the cfield's
    // color RLE (measured 28 → 84 MB); steps keep neighbours byte-equal.
    let f = |c: f32| (((c * v).clamp(0.0, 1.0) * 255.0 / 12.0).round() * 12.0) as u8;
    [f(base[0]), f(base[1]), f(base[2])]
}

fn rgba(rgb: [u8; 3], slot: u8) -> [u8; 4] {
    [rgb[0], rgb[1], rgb[2], slot]
}

/// A planet/moon "character class": picks tint hues, noise character and
/// layer rules. Uniqueness within an archetype comes from seeded jitter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Archetype {
    Canyon,
    Dune,
    Ice,
    Lava,
    Crystal,
    Barren,
    Frost,
}

/// The look of one body: which palette slots it wears, with which tints.
/// All derived from the seed by [`BodySpec::roll`].
#[derive(Clone)]
pub struct Look {
    pub surf_a: ([f32; 3], u8),
    pub surf_b: ([f32; 3], u8),
    /// Biome patch decision: `patch + alt * bias > thr` → surf_a.
    pub patch_bias: f32,
    pub patch_thr: f32,
    pub sub: ([f32; 3], u8),
    pub strata: ([f32; 3], u8),
    pub deep: ([f32; 3], u8),
    /// Glowing pocket crystal (or magma) and its fbm threshold.
    pub glow: ([f32; 3], u8),
    pub pocket_thr: f32,
    /// Magma seams in the deep cave zone: (tint, min depth, fbm threshold).
    pub seam: Option<([f32; 3], f32, f32)>,
    /// Polar ice: `|dir.y| > lat` (or frost patches where `patch < -0.42`).
    pub ice_caps: Option<f32>,
}

/// Everything needed to fill one body's terrain field.
#[derive(Clone)]
pub struct BodySpec {
    pub name: String,
    /// Terrain id in the scene (1-based).
    pub id: u32,
    pub radius: f32,
    pub voxel: f32,
    pub relief: f32,
    pub bump_freq: f32,
    /// Cave zone thickness below the surface (0 = solid body).
    pub cave_depth: f32,
    /// Solid core ball radius the caves never touch (0 = none).
    pub core_r: f32,
    /// Impact craters (airless bodies wear their history).
    pub craters: u32,
    pub archetype: Archetype,
    pub look: Look,
    pub seed: u32,
}

impl BodySpec {
    /// Roll a body of `archetype` at `radius` from the rng — every knob
    /// (tints, noise frequencies, cave depth, craters) is jittered.
    pub fn roll(rng: &mut Rng, name: String, id: u32, radius: f32, archetype: Archetype) -> Self {
        let rf = |rng: &mut Rng, lo: f32, hi: f32| rng.range(lo as f64, hi as f64) as f32;
        let (bump_freq, relief_k) = match archetype {
            Archetype::Dune => (rf(rng, 6.5, 8.5), rf(rng, 0.06, 0.1)),
            Archetype::Ice | Archetype::Frost => (rf(rng, 2.6, 3.6), rf(rng, 0.04, 0.07)),
            Archetype::Lava => (rf(rng, 4.0, 5.5), rf(rng, 0.05, 0.09)),
            _ => (rf(rng, 3.8, 5.2), rf(rng, 0.05, 0.08)),
        };
        let relief = (radius * relief_k).clamp(3.0, 22.0);
        let cave_k = match archetype {
            Archetype::Crystal => 0.4,
            Archetype::Canyon | Archetype::Lava => 0.35,
            Archetype::Ice => 0.3,
            _ => 0.25,
        };
        let cave_depth = radius * cave_k;
        let core_r = (radius * 0.07).max(5.0);
        let airless = matches!(archetype, Archetype::Barren | Archetype::Frost);
        let craters = if airless { 8 + (rng.next_u32() % 10) } else { 0 };
        // Voxel size scales with radius so big planets stay affordable.
        let voxel = if radius > 170.0 {
            2.0
        } else if radius > 90.0 {
            1.5
        } else {
            1.0
        };

        let j = rf(rng, -0.03, 0.03); // hue jitter shared by the body's family
        let look = match archetype {
            Archetype::Canyon => {
                let warm = rf(rng, 0.01, 0.09) + j;
                let cool = rf(rng, 0.68, 0.85) + j;
                Look {
                    surf_a: (hsv(warm, rf(rng, 0.25, 0.45), rf(rng, 0.6, 0.75)), 1),
                    surf_b: (hsv(cool, rf(rng, 0.15, 0.3), rf(rng, 0.55, 0.7)), 2),
                    patch_bias: 0.45,
                    patch_thr: rf(rng, -0.05, 0.15),
                    sub: (hsv(cool, 0.18, 0.62), 3),
                    strata: (hsv(warm, 0.25, 0.75), 4),
                    deep: (hsv(warm, 0.3, 0.68), 5),
                    glow: (hsv(rf(rng, 0.7, 0.82), 0.28, 0.82), 7),
                    pocket_thr: rf(rng, 0.36, 0.42),
                    seam: Some((hsv(warm, 0.25, 0.9), cave_depth * 0.55, 0.12)),
                    ice_caps: None,
                }
            }
            Archetype::Dune => {
                let gold = rf(rng, 0.08, 0.15) + j;
                Look {
                    surf_a: (hsv(gold, rf(rng, 0.4, 0.55), rf(rng, 0.85, 0.98)), 2),
                    surf_b: (hsv(gold - 0.03, rf(rng, 0.4, 0.55), rf(rng, 0.65, 0.8)), 1),
                    patch_bias: 0.6,
                    patch_thr: -0.05,
                    sub: (hsv(gold - 0.02, 0.5, 0.85), 4),
                    strata: (hsv(gold - 0.04, 0.5, 0.7), 5),
                    deep: (hsv(gold - 0.05, 0.45, 0.62), 5),
                    glow: (hsv(gold, 0.4, 0.95), 6),
                    pocket_thr: rf(rng, 0.4, 0.48),
                    seam: Some((hsv(gold, 0.4, 1.0), cave_depth * 0.6, 0.18)),
                    ice_caps: None,
                }
            }
            Archetype::Ice => {
                let blue = rf(rng, 0.5, 0.62) + j;
                Look {
                    surf_a: (hsv(blue, rf(rng, 0.1, 0.25), rf(rng, 0.85, 0.98)), 12),
                    surf_b: (hsv(blue, rf(rng, 0.15, 0.3), rf(rng, 0.7, 0.82)), 9),
                    patch_bias: 0.2,
                    patch_thr: 0.25,
                    sub: (hsv(blue, 0.2, 0.72), 9),
                    strata: (hsv(blue + 0.04, 0.25, 0.6), 3),
                    deep: (hsv(blue, 0.25, 0.55), 9),
                    glow: (hsv(blue + rf(rng, -0.05, 0.05), 0.3, 0.9), 8),
                    pocket_thr: rf(rng, 0.3, 0.38),
                    seam: None,
                    ice_caps: Some(0.75),
                }
            }
            Archetype::Lava => {
                let ash = rf(rng, 0.0, 0.06) + j;
                Look {
                    surf_a: (hsv(ash, rf(rng, 0.15, 0.3), rf(rng, 0.28, 0.4)), 1),
                    surf_b: (hsv(ash, rf(rng, 0.2, 0.35), rf(rng, 0.4, 0.52)), 4),
                    patch_bias: 0.4,
                    patch_thr: 0.05,
                    sub: (hsv(ash, 0.25, 0.35), 4),
                    strata: (hsv(ash, 0.3, 0.45), 5),
                    deep: (hsv(ash, 0.3, 0.5), 5),
                    glow: (hsv(ash + 0.02, 0.5, 1.0), 6),
                    pocket_thr: rf(rng, 0.28, 0.34),
                    // Shallow, aggressive seams: the surface cracks glow.
                    seam: Some((hsv(ash + 0.02, 0.5, 1.0), 4.0, 0.16)),
                    ice_caps: None,
                }
            }
            Archetype::Crystal => {
                let vio = if rng.next_u32().is_multiple_of(2) {
                    rf(rng, 0.68, 0.8)
                } else {
                    rf(rng, 0.45, 0.55)
                } + j;
                Look {
                    surf_a: (hsv(vio, rf(rng, 0.2, 0.35), rf(rng, 0.55, 0.7)), 2),
                    surf_b: (hsv(vio + 0.06, rf(rng, 0.15, 0.3), rf(rng, 0.45, 0.6)), 3),
                    patch_bias: 0.35,
                    patch_thr: 0.0,
                    sub: (hsv(vio, 0.2, 0.55), 3),
                    strata: (hsv(vio, 0.25, 0.5), 4),
                    deep: (hsv(vio, 0.28, 0.6), 5),
                    glow: (hsv(vio, 0.32, 0.95), if vio > 0.6 { 7 } else { 8 }),
                    pocket_thr: rf(rng, 0.24, 0.3), // crystal EVERYWHERE
                    seam: None,
                    ice_caps: None,
                }
            }
            Archetype::Barren => {
                let g = rf(rng, 0.5, 0.75);
                Look {
                    surf_a: (hsv(rf(rng, 0.0, 1.0), 0.04, g), 10),
                    surf_b: (hsv(0.1, 0.08, g + 0.1), 11),
                    patch_bias: 0.0,
                    patch_thr: -0.42,
                    sub: ([0.6, 0.62, 0.68], 9),
                    strata: ([0.55, 0.56, 0.6], 9),
                    deep: ([0.5, 0.5, 0.55], 9),
                    glow: (hsv(rf(rng, 0.5, 0.8), 0.3, 0.9), 8),
                    pocket_thr: rf(rng, 0.42, 0.5),
                    seam: None,
                    ice_caps: None,
                }
            }
            Archetype::Frost => {
                let blue = rf(rng, 0.5, 0.6) + j;
                Look {
                    surf_a: (hsv(blue, 0.12, 0.92), 12),
                    surf_b: (hsv(blue, 0.08, 0.72), 10),
                    patch_bias: 0.0,
                    patch_thr: -0.2,
                    sub: (hsv(blue, 0.15, 0.7), 9),
                    strata: (hsv(blue, 0.18, 0.6), 9),
                    deep: (hsv(blue, 0.2, 0.55), 9),
                    glow: (hsv(blue, 0.3, 0.9), 8),
                    pocket_thr: rf(rng, 0.36, 0.44),
                    seam: None,
                    ice_caps: Some(0.6),
                }
            }
        };
        BodySpec {
            name,
            id,
            radius,
            voxel,
            relief,
            bump_freq,
            cave_depth,
            core_r,
            craters,
            archetype,
            look,
            seed: rng.next_u32(),
        }
    }
}

/// One body's place in the sky: Kepler elements + gravity (see CelestialBody).
#[derive(Clone)]
pub struct OrbitSpec {
    pub parent: String,
    pub a: f64,
    pub e: f64,
    pub i: f64,
    pub m0: f64,
    pub mu: f64,
    /// Physical surface radius (gravity interior falloff + impostor size).
    pub body_radius: f64,
    /// World position at t=0 (editor preview; rails own it during Play).
    pub pos: [f64; 3],
}

/// Atmosphere shell for the scene's CelestialBody.
#[derive(Clone, Copy)]
pub struct AtmoSpec {
    pub color: [f32; 3],
    pub height: f64,
    pub density: f32,
    pub clouds: f32,
}

#[derive(Clone)]
pub struct GenBody {
    pub body: BodySpec,
    pub orbit: OrbitSpec,
    pub atmo: Option<AtmoSpec>,
}

#[derive(Clone)]
pub struct StarSpec {
    pub name: String,
    pub color: [f32; 3],
    pub tint: [f32; 3],
    pub luminosity: f32,
    pub mu: f64,
    pub visual_radius: f64,
}

/// A whole rolled system, ready to generate.
#[derive(Clone)]
pub struct SystemSpec {
    pub seed: u32,
    pub star: StarSpec,
    pub bodies: Vec<GenBody>,
}

/// Name generator: alien but pronounceable.
fn roll_name(rng: &mut Rng, taken: &mut Vec<String>) -> String {
    const A: [&str; 14] =
        ["Ka", "Ve", "Zor", "Ath", "Or", "Ta", "Dra", "Pel", "Na", "Gol", "Bry", "Um", "Sol", "Ry"];
    const B: [&str; 12] =
        ["ru", "il", "un", "mi", "quo", "os", "ver", "eth", "and", "ol", "ia", "ex"];
    loop {
        let mut n = A[(rng.next_u32() as usize) % A.len()].to_string();
        n.push_str(B[(rng.next_u32() as usize) % B.len()]);
        if rng.next_u32().is_multiple_of(2) {
            n.push_str(B[(rng.next_u32() as usize) % B.len()]);
        }
        if !taken.contains(&n) {
            taken.push(n.clone());
            return n;
        }
    }
}

impl SystemSpec {
    /// Roll a whole system from one master seed. `planets` clamps 2..=4
    /// (None = seeded random) — each may carry up to two moons.
    pub fn random(seed: u32, planets: Option<u32>) -> Self {
        let mut rng = Rng::new(seed.wrapping_mul(747_796_405).wrapping_add(11));
        let rf = |rng: &mut Rng, lo: f64, hi: f64| rng.range(lo, hi);
        let mut taken = Vec::new();

        // The star: class decides color + brightness; µ sets the year lengths.
        let classes: [([f32; 3], f32); 5] = [
            ([0.72, 0.82, 1.0], 90.0), // blue-white
            ([1.0, 0.97, 0.9], 55.0),  // white
            ([1.0, 0.92, 0.6], 40.0),  // yellow
            ([1.0, 0.72, 0.42], 26.0), // orange
            ([1.0, 0.5, 0.34], 15.0),  // red dwarf
        ];
        let (scol, lum) = classes[(rng.next_u32() as usize) % classes.len()];
        let star = StarSpec {
            name: roll_name(&mut rng, &mut taken),
            color: scol,
            tint: scol,
            luminosity: (lum as f64 * rf(&mut rng, 0.8, 1.3)) as f32,
            mu: rf(&mut rng, 4.0e7, 1.1e8),
            visual_radius: rf(&mut rng, 650.0, 1000.0),
        };

        let n_planets = planets.map(|n| n.clamp(2, 4)).unwrap_or(2 + rng.next_u32() % 3);
        let planet_types =
            [Archetype::Canyon, Archetype::Dune, Archetype::Ice, Archetype::Lava, Archetype::Crystal];
        let moon_types = [Archetype::Barren, Archetype::Frost, Archetype::Crystal, Archetype::Ice];

        let mut bodies: Vec<GenBody> = Vec::new();
        let mut id = 1u32;
        let mut a = rf(&mut rng, 4500.0, 6500.0);
        // First planet leans hospitable (it's the spawn world); later ones roll free.
        let mut last_type: Option<Archetype> = None;
        for pi in 0..n_planets {
            let archetype = loop {
                let t = if pi == 0 {
                    [Archetype::Canyon, Archetype::Dune, Archetype::Ice]
                        [(rng.next_u32() as usize) % 3]
                } else {
                    planet_types[(rng.next_u32() as usize) % planet_types.len()]
                };
                if Some(t) != last_type {
                    break t;
                }
            };
            last_type = Some(archetype);
            let radius = rf(&mut rng, 100.0, 230.0) as f32;
            let g = rf(&mut rng, 4.5, 10.0);
            let mu = g * (radius as f64) * (radius as f64);
            let name = roll_name(&mut rng, &mut taken);
            let body = BodySpec::roll(&mut rng, name.clone(), id, radius, archetype);
            id += 1;
            let m0 = rf(&mut rng, 0.0, std::f64::consts::TAU);
            let i_incl = rf(&mut rng, 0.0, 0.14);
            let e = rf(&mut rng, 0.0, 0.06);
            let pos = [a * m0.cos(), 0.0, a * m0.sin()];
            let atmo = if matches!(archetype, Archetype::Crystal) && rng.next_u32().is_multiple_of(2) {
                None // some crystal worlds are airless vacuum gardens
            } else {
                let (h, s, v) = match archetype {
                    Archetype::Canyon => (rf(&mut rng, 0.02, 0.1), 0.5, 0.8),
                    Archetype::Dune => (rf(&mut rng, 0.08, 0.13), 0.55, 0.9),
                    Archetype::Ice => (rf(&mut rng, 0.5, 0.6), 0.45, 0.85),
                    Archetype::Lava => (rf(&mut rng, 0.0, 0.04), 0.6, 0.55),
                    _ => (rf(&mut rng, 0.55, 0.8), 0.4, 0.8),
                };
                Some(AtmoSpec {
                    color: hsv(h as f32, s as f32, v as f32),
                    height: (radius as f64) * rf(&mut rng, 0.25, 0.45),
                    density: rf(&mut rng, 0.5, 0.95) as f32,
                    clouds: (rf(&mut rng, -0.2, 0.8).max(0.0)) as f32,
                })
            };
            let soi = a * (mu / star.mu).powf(0.4);
            bodies.push(GenBody {
                body,
                orbit: OrbitSpec {
                    parent: star.name.clone(),
                    a,
                    e,
                    i: i_incl,
                    m0,
                    mu,
                    body_radius: radius as f64 + body_radius_pad(radius),
                    pos,
                },
                atmo,
            });

            // Moons: up to two, orbits inside half the planet's SOI.
            let n_moons = match rng.next_u32() % 4 {
                0 => 0,
                1 | 2 => 1,
                _ => 2,
            };
            let mut ma = (radius as f64) * rf(&mut rng, 5.0, 7.0);
            for _ in 0..n_moons {
                if ma > soi * 0.45 {
                    break; // SOI too small for this (or another) moon
                }
                let mr = rf(&mut rng, 26.0, 58.0).min(radius as f64 * 0.35) as f32;
                let mg = rf(&mut rng, 2.0, 5.0);
                let mut mmu = mg * (mr as f64) * (mr as f64);
                // The moon's own SOI must clear its terrain, or landing there
                // would hand gravity back to the planet at the surface.
                let msoi = |mmu: f64| ma * (mmu / mu).powf(0.4);
                while msoi(mmu) < (mr as f64) * 2.5 {
                    mmu *= 1.6;
                }
                let mtype = moon_types[(rng.next_u32() as usize) % moon_types.len()];
                let mname = roll_name(&mut rng, &mut taken);
                let mbody = BodySpec::roll(&mut rng, mname, id, mr, mtype);
                id += 1;
                let mm0 = rf(&mut rng, 0.0, std::f64::consts::TAU);
                let mpos =
                    [pos[0] + ma * mm0.cos(), 0.0, pos[2] + ma * mm0.sin()];
                bodies.push(GenBody {
                    body: mbody,
                    orbit: OrbitSpec {
                        parent: name.clone(),
                        a: ma,
                        e: rf(&mut rng, 0.0, 0.04),
                        i: rf(&mut rng, 0.0, 0.25),
                        m0: mm0,
                        mu: mmu,
                        body_radius: mr as f64 + body_radius_pad(mr),
                        pos: mpos,
                    },
                    atmo: None,
                });
                ma *= rf(&mut rng, 1.8, 2.4);
            }

            // Next orbit: geometric spacing keeps SOIs far apart.
            a *= rf(&mut rng, 1.85, 2.55);
        }

        SystemSpec { seed, star, bodies }
    }
}

/// body_radius = terrain radius + a relief-ish pad (impostor + gravity surface).
fn body_radius_pad(radius: f32) -> f64 {
    (radius as f64 * 0.035).max(4.0)
}

/// Fill one body's terrain field (sphere ± fbm, caves, chambers, core,
/// craters, layered materials — all from the spec).
pub fn generate_body(spec: &BodySpec) -> ChunkField {
    let noise = Noise::new(spec.seed);
    let mut rng = Rng::new(spec.seed.wrapping_add(31));
    let mut field = ChunkField::new(spec.voxel);
    let ext = spec.radius + spec.relief + 2.0;
    let (radius, relief, cave_depth, core_r, bump_freq) =
        (spec.radius, spec.relief, spec.cave_depth, spec.core_r, spec.bump_freq);
    let look = spec.look.clone();

    // Impact craters: spherical dents subtracted in the fill SDF (carving
    // after the fill hits band-clamped rock and terraces).
    let craters: Vec<(Vec3, f32)> = (0..spec.craters)
        .map(|_| {
            let d = Vec3::new(
                rng.range(-1.0, 1.0) as f32,
                rng.range(-1.0, 1.0) as f32,
                rng.range(-1.0, 1.0) as f32,
            )
            .normalize_or_zero();
            let d = if d == Vec3::ZERO { Vec3::X } else { d };
            (d * (radius + relief * 0.3), rng.range(0.12, 0.26) as f32 * radius)
        })
        .collect();
    let craters2 = craters.clone();

    field.fill_with_rgba(
        Vec3::splat(-ext),
        Vec3::splat(ext),
        move |p| {
            let r = p.length();
            let dir = if r > 1e-3 { p / r } else { Vec3::X };
            let bump = noise.fbm(dir * bump_freq, 5) * relief;
            let mut body = r - (radius + bump);
            for (c, cr) in &craters {
                body = body.max(-((p - *c).length() - cr));
            }
            if cave_depth <= 0.0 {
                return body;
            }
            // Caves where two noise fields are both near zero; galleries widen
            // with depth, a larger-scale chamber field opens rooms further
            // down, and a solid core survives at the center (see gen_planetoid
            // — same recipe, scaled to the body).
            let a = noise.fbm(p * 0.018, 3);
            let b = noise.fbm(p * 0.018 + Vec3::splat(51.7), 3);
            let w = 0.1 + (-body * 0.0006).clamp(0.0, 0.07);
            let tunnel = (a.abs().max(b.abs()) - w) * 34.0;
            let c1 = noise.fbm(p * 0.008 + Vec3::splat(113.0), 2);
            let c2 = noise.fbm(p * 0.008 + Vec3::splat(7.9), 2);
            let chamber = ((c1.abs().max(c2.abs())) - 0.14) * 70.0;
            let cave = tunnel.min(chamber.max(body + cave_depth * 0.42));
            let gated = cave
                .max(spec.voxel * 3.0 + body)
                .max(-(body + cave_depth))
                .max((core_r + spec.voxel * 6.0) - r);
            body.max(-gated)
        },
        move |p| {
            let r = p.length();
            let dir = if r > 1e-3 { p / r } else { Vec3::X };
            let bump = noise.fbm(dir * bump_freq, 5) * relief;
            let alt = (r - radius) / relief.max(1.0);
            let depth = (radius + bump) - r;
            let vary = noise.fbm(p * 0.05 + Vec3::splat(83.0), 2);
            let patch = noise.fbm(dir * 9.0 + Vec3::splat(37.0), 3);
            let pocket = noise.fbm(p * 0.05 + Vec3::splat(17.3), 3);

            // Molten core zone: glows all the way through (slot 6) so a deep
            // dig reads HOT before the core ball itself appears.
            if cave_depth > 0.0 && r < core_r + core_r.min(10.0) {
                return rgba(tint([0.98, 0.82, 0.6], vary), 6);
            }
            // Glowing pockets — the reason to dig.
            if depth > 6.0 && pocket > look.pocket_thr {
                return rgba(tint(look.glow.0, vary), look.glow.1);
            }
            // Magma seams in the deep cave zone (lava worlds: near-surface).
            if let Some((seam_tint, seam_min, seam_thr)) = look.seam
                && depth > seam_min
                && noise.fbm(p * 0.035 + Vec3::splat(5.9), 3) > seam_thr
            {
                return rgba(tint(seam_tint, vary), 6);
            }
            // Depth layers: deep cave walls → strata → subsoil.
            if depth > cave_depth * 0.5 {
                return rgba(tint(look.deep.0, vary), look.deep.1);
            }
            if depth > 9.0 {
                return rgba(tint(look.strata.0, vary), look.strata.1);
            }
            if depth > 2.4 {
                return rgba(tint(look.sub.0, vary), look.sub.1);
            }
            // Crater floors wear dust.
            for (c, cr) in &craters2 {
                if (p - *c).length() < cr + 1.6 {
                    return rgba(tint([0.78, 0.74, 0.66], vary), 11);
                }
            }
            // Polar caps + frost patches.
            if let Some(lat) = look.ice_caps
                && (dir.y.abs() > lat || patch < -0.42)
            {
                return rgba(tint([0.85, 0.92, 0.98], vary), 12);
            }
            // Surface biomes.
            if patch + alt * look.patch_bias > look.patch_thr {
                rgba(tint(look.surf_a.0, vary), look.surf_a.1)
            } else {
                rgba(tint(look.surf_b.0, vary), look.surf_b.1)
            }
        },
    );
    field
}

/// A finished generation: where the scene landed + a printable summary.
pub struct GeneratedSystem {
    pub scene_path: std::path::PathBuf,
    pub summary: String,
}

/// Generate every body's field + palette sidecar + the playable scene.
/// `progress` is called with one line per completed step (thread-friendly).
pub fn generate_system(
    spec: &SystemSpec,
    scene: &str,
    out_dir: &std::path::Path,
    scenes_dir: &std::path::Path,
    tex_dir: &std::path::Path,
    progress: &mut dyn FnMut(String),
) -> std::io::Result<GeneratedSystem> {
    std::fs::create_dir_all(out_dir)?;
    std::fs::create_dir_all(scenes_dir)?;
    let mut summary = format!(
        "system \"{}\" (seed {}): star {} lum {:.0}, {} bodies\n",
        scene,
        spec.seed,
        spec.star.name,
        spec.star.luminosity,
        spec.bodies.len()
    );
    for gb in &spec.bodies {
        let t0 = std::time::Instant::now();
        let field = generate_body(&gb.body);
        let bytes = field.to_bytes();
        let path = out_dir.join(format!("{scene}.{}.cfield", gb.body.id));
        std::fs::write(&path, &bytes)?;
        let line = format!(
            "{:10} {:8?} r {:5.0}  a {:6.0} ({})  {:5.1} MB  {} ms",
            gb.body.name,
            gb.body.archetype,
            gb.body.radius,
            gb.orbit.a,
            gb.orbit.parent,
            bytes.len() as f64 / 1e6,
            t0.elapsed().as_millis(),
        );
        progress(line.clone());
        summary.push_str(&line);
        summary.push('\n');
    }

    // Palette sidecar — ABSOLUTE texture paths (assets resolve as-is from the
    // editor's CWD; the project usually lives outside it).
    let lines: Vec<String> = PALETTE
        .iter()
        .map(|(name, glow)| {
            let p = tex_dir.join(name);
            let abs = std::fs::canonicalize(&p).unwrap_or(p);
            format!("{}{}", abs.display(), if *glow { "|glow" } else { "" })
        })
        .collect();
    std::fs::write(out_dir.join(format!("{scene}.palette")), lines.join("\n"))?;

    let scene_str = build_scene(spec, scene);
    let scene_path = scenes_dir.join(format!("{scene}.ron"));
    std::fs::write(&scene_path, &scene_str)?;
    progress(format!("scene → {}", scene_path.display()));
    Ok(GeneratedSystem { scene_path, summary })
}

/// The playable scene: star + bodies + cores on rails, then the standard solar
/// kit (skybox, post, astronaut+camera, ship+flame+legs, HUD instruments) with
/// the spawn on the FIRST planet's north pole.
fn build_scene(spec: &SystemSpec, scene: &str) -> String {
    let mut nodes: Vec<String> = Vec::new();
    let s = &spec.star;

    // 0 — the star: gravity root + emissive sphere + the system's light.
    nodes.push(format!(
        r#"        (
            name: "{name}",
            transform: (
                translation: (0.0, 0.0, 0.0),
                rotation: (0.0, 0.0, 0.0, 1.0),
                scale: ({vr:.1}, {vr:.1}, {vr:.1}),
            ),
            matter: Primitive(
                shape: Sphere,
                color: ({r:.2}, {g:.2}, {b:.2}),
            ),
            scripts: [],
            material: Some((
                color: ({r:.2}, {g:.2}, {b:.2}),
                emissive: ({r:.2}, {g:.2}, {b:.2}),
                emissive_strength: 2.0,
                unlit: true,
            )),
            celestial: Some((
                mu: {mu:.1},
                body_radius: {br:.1},
                soi: 0.0,
                a: 0.0, e: 0.0, i: 0.0, lan: 0.0, arg_pe: 0.0, m0: 0.0,
                luminosity: {lum:.1},
                star_color: ({r:.2}, {g:.2}, {b:.2}),
            )),
        ),
"#,
        name = s.name,
        vr = s.visual_radius,
        r = s.color[0],
        g = s.color[1],
        b = s.color[2],
        mu = s.mu,
        br = s.visual_radius * 0.85,
        lum = s.luminosity,
    ));

    // 1..=B — the terrain bodies.
    let mut body_indices = Vec::new();
    for gb in &spec.bodies {
        let o = &gb.orbit;
        let atmo_lines = match &gb.atmo {
            Some(a) => format!(
                "\n                atmo_color: ({:.2}, {:.2}, {:.2}),\n                atmo_height: {:.1},\n                atmo_density: {:.2},\n                clouds: {:.2},",
                a.color[0], a.color[1], a.color[2], a.height, a.density, a.clouds
            ),
            None => String::new(),
        };
        body_indices.push(nodes.len());
        nodes.push(format!(
            r#"        (
            name: "{name}",
            transform: (
                translation: ({x:.1}, {y:.1}, {z:.1}),
                rotation: (0.0, 0.0, 0.0, 1.0),
                scale: (1.0, 1.0, 1.0),
            ),
            matter: Terrain(
                id: {id},
            ),
            scripts: [],
            celestial: Some((
                mu: {mu:.1},
                body_radius: {br:.1},
                soi: 0.0,
                parent: "{parent}",
                a: {a:.1}, e: {e:.3}, i: {i:.3}, lan: 0.0, arg_pe: 0.0, m0: {m0:.3},{atmo_lines}
            )),
        ),
"#,
            name = gb.body.name,
            x = o.pos[0],
            y = o.pos[1],
            z = o.pos[2],
            id = gb.body.id,
            mu = o.mu,
            br = o.body_radius,
            parent = o.parent,
            a = o.a,
            e = o.e,
            i = o.i,
            m0 = o.m0,
        ));
    }

    // Core kernels: an emissive ball inside each body's guarded solid core
    // (dig to the center and FIND something). Parented → rides the rails.
    for (gb, bidx) in spec.bodies.iter().zip(body_indices.iter()) {
        let hot = gb.body.look.glow.1 == 6 || gb.body.look.seam.is_some();
        let c: [f32; 3] = if hot { [1.0, 0.55, 0.25] } else { [0.55, 0.8, 1.0] };
        let e: [f32; 3] = if hot { [1.0, 0.45, 0.15] } else { [0.4, 0.7, 1.0] };
        let cs = (gb.body.core_r * 0.8).max(4.0);
        nodes.push(format!(
            r#"        (
            name: "{name} Core",
            transform: (
                translation: (0.0, 0.0, 0.0),
                rotation: (0.0, 0.0, 0.0, 1.0),
                scale: ({cs:.1}, {cs:.1}, {cs:.1}),
            ),
            matter: Primitive(
                shape: Sphere,
                color: ({cr:.2}, {cg:.2}, {cb:.2}),
            ),
            scripts: [],
            material: Some((
                color: ({cr:.2}, {cg:.2}, {cb:.2}),
                emissive: ({er:.2}, {eg:.2}, {eb:.2}),
                emissive_strength: 2.5,
                unlit: true,
            )),
            parent: Some({bidx}),
        ),
"#,
            name = gb.body.name,
            cr = c[0],
            cg = c[1],
            cb = c[2],
            er = e[0],
            eg = e[1],
            eb = e[2],
        ));
    }

    // The standard solar kit, spawn at the first planet's north pole.
    let p0 = &spec.bodies[0];
    let (sx, sz) = (p0.orbit.pos[0], p0.orbit.pos[2]);
    let sy = (p0.body.radius + p0.body.relief) as f64 + 4.0;
    nodes.push(format!(
        r#"        (
            name: "Skybox",
            transform: (
                translation: (0.0, 0.0, 0.0),
                rotation: (0.0, 0.0, 0.0, 1.0),
                scale: (1.0, 1.0, 1.0),
            ),
            matter: Skybox(
                color: (1.0, 1.0, 1.0),
                size: 250000.0,
                tint: (0.004, 0.005, 0.012),
            ),
            scripts: [],
        ),
        (
            name: "Post Processing",
            transform: (
                translation: (0.0, 0.0, 0.0),
                rotation: (0.0, 0.0, 0.0, 1.0),
                scale: (1.0, 1.0, 1.0),
            ),
            matter: PostProcess(
                enabled: true,
                bloom: true,
                bloom_threshold: 0.3,
                bloom_intensity: 0.3,
                vignette: true,
                vignette_strength: 0.4,
                vignette_radius: 0.5,
                ao: Off,
                ao_strength: 1.0,
                ao_radius: 0.7,
                posterize_bands: 0,
                posterize_dither: false,
            ),
            scripts: [],
        ),
        (
            name: "Astronaut",
            transform: (
                translation: ({ax:.1}, {ay:.1}, {az:.1}),
                rotation: (0.0, 0.0, 0.0, 1.0),
                scale: (1.0, 1.0, 1.0),
            ),
            matter: Primitive(
                shape: Capsule,
                color: (0.85, 0.6, 0.25),
            ),
            scripts: [
                (
                    kind: "planet_walker",
                    enabled: true,
                    params: [
                        ("double_tap", 0.3),
                        ("ground_ray", 1.6),
                        ("jump", 10.4),
                        ("run", 9.4),
                        ("walk", 4.5),
                    ],
                ),
                (
                    kind: "dig_tool",
                    enabled: true,
                    params: [
                        ("radius", 3.0),
                        ("range", 45.0),
                        ("rate", 0.12),
                        ("spacing", 0.45),
                        ("strength", 0.6),
                    ],
                ),
            ],
            rigidbody: Some((
                capsule: true,
                radius: 0.5,
                height: 2.0,
                half_extents: (0.5, 0.5, 0.5),
                restitution: 0.0,
                friction: 0.3,
                gravity: true,
                lock_pos: (false, false, false),
                lock_rot: (false, false, false),
                align_up: true,
            )),
        ),
        (
            name: "Camera 1",
            transform: (
                translation: ({ax:.1}, {cy:.1}, {cz:.1}),
                rotation: (0.0, 0.0, 0.0, 1.0),
                scale: (1.0, 1.0, 1.0),
            ),
            matter: Camera(
                fov_y: 1.1519173,
                active: true,
            ),
            scripts: [
                (
                    kind: "planet_camera",
                    enabled: true,
                    params: [
                        ("distance", 7.0),
                        ("height", 1.2),
                        ("max_distance", 14.0),
                        ("min_distance", 1.5),
                        ("sensitivity", 0.3),
                        ("start_pitch", -0.3),
                        ("zoom_speed", 1.0),
                    ],
                ),
            ],
        ),
"#,
        ax = sx,
        ay = sy + p0.orbit.pos[1],
        az = sz,
        cy = sy + 5.0,
        cz = sz + 8.0,
    ));

    let ship_idx = nodes.len();
    nodes.push(format!(
        r#"        (
            name: "Ship",
            transform: (
                translation: ({shx:.1}, {shy:.1}, {shz:.1}),
                rotation: (0.0, 0.0, 0.0, 1.0),
                scale: (1.6, 2.4, 1.6),
            ),
            matter: Primitive(
                shape: Capsule,
                color: (0.85, 0.87, 0.92),
            ),
            scripts: [
                (
                    kind: "ship_controller",
                    enabled: true,
                    params: [],
                ),
            ],
            rigidbody: Some((
                capsule: false,
                radius: 2.0,
                height: 4.0,
                half_extents: (0.5, 0.5, 0.5),
                restitution: 0.0,
                friction: 0.05,
                gravity: true,
                lock_pos: (false, false, false),
                lock_rot: (false, false, false),
            )),
        ),
        (
            name: "Ship Flame",
            transform: (
                translation: (0.0, -1.0, 0.0),
                rotation: (1.0, 0.0, 0.0, 0.0),
                scale: (0.625, 0.41667, 0.625),
            ),
            matter: PointLight(
                color: (1.0, 0.62, 0.25),
                intensity: 0.0,
                range: 16.0,
            ),
            scripts: [],
            particles: Some((
                asset: "vfx/Flame",
                play_on_start: false,
            )),
            parent: Some({ship_idx}),
        ),
        (
            name: "Landing Cam",
            transform: (
                translation: (0.0, -1.1, 0.0),
                rotation: (-0.7071068, 0.0, 0.0, 0.7071068),
                scale: (1.0, 1.0, 1.0),
            ),
            matter: Camera(
                fov_y: 1.4,
                active: false,
                target: "landing",
            ),
            scripts: [],
            parent: Some({ship_idx}),
        ),
"#,
        shx = sx + 20.0,
        shy = sy - 4.0,
        shz = sz,
    ));

    let hud_idx = nodes.len();
    nodes.push(String::from(
        r#"        (
            name: "Ship HUD",
            transform: (
                translation: (0.0, 0.0, 0.0),
                rotation: (0.0, 0.0, 0.0, 1.0),
                scale: (1.0, 1.0, 1.0),
            ),
            matter: Empty,
            scripts: [],
            ui_layer: Some((
                design_height: 720.0,
                z: 120,
                enabled: true,
                space: Screen,
                canvas_scale: 0.009,
            )),
        ),
"#,
    ));
    let pin_el = |name: &str, anchor: &str, off: (f32, f32), size: (f32, f32), body: &str| {
        format!(
            r#"        (
            name: "{name}",
            transform: (
                translation: (0.0, 0.0, 0.0),
                rotation: (0.0, 0.0, 0.0, 1.0),
                scale: (1.0, 1.0, 1.0),
            ),
            matter: Empty,
            scripts: [],
            parent: Some({hud_idx}),
            ui: Some((
                place: Pin(
                    anchor: {anchor},
                    offset: ({ox:.1}, {oy:.1}),
                ),
                size: (Fixed({w:.1}), Fixed({h:.1})),
{body}
                visible: false,
                opacity: 0.95,
            )),
        ),
"#,
            ox = off.0,
            oy = off.1,
            w = size.0,
            h = size.1,
        )
    };
    nodes.push(pin_el(
        "Ship HUD Text",
        "TopLeft",
        (16.0, 12.0),
        (640.0, 200.0),
        "                text: Some((\n                    text: \"\",\n                    size: 19.0,\n                    color: (0.7, 1.0, 0.78, 1.0),\n                    align: Start,\n                    valign: Start,\n                    fit: false,\n                )),",
    ));
    nodes.push(pin_el(
        "Navball",
        "Bottom",
        (0.0, -12.0),
        (170.0, 170.0),
        "                shader: \"shaders/navball.flsl\",",
    ));
    nodes.push(pin_el(
        "Speed Tape",
        "Bottom",
        (-128.0, -22.0),
        (52.0, 180.0),
        "                shader: \"shaders/tape.flsl\",\n                shader_params: {\n                    \"side\": (0.0, 0.0, 0.0, 0.0),\n                    \"accent\": (0.45, 0.95, 0.6, 1.0),\n                },",
    ));
    nodes.push(pin_el(
        "Alt Tape",
        "Bottom",
        (128.0, -22.0),
        (52.0, 180.0),
        "                shader: \"shaders/tape.flsl\",\n                shader_params: {\n                    \"side\": (1.0, 0.0, 0.0, 0.0),\n                    \"accent\": (1.0, 0.75, 0.3, 1.0),\n                },",
    ));
    nodes.push(pin_el(
        "Speed Readout",
        "Bottom",
        (-185.0, -104.0),
        (58.0, 24.0),
        "                text: Some((\n                    text: \"0\",\n                    size: 17.0,\n                    color: (0.45, 0.95, 0.6, 1.0),\n                    align: End,\n                    valign: Center,\n                    fit: false,\n                )),",
    ));
    nodes.push(pin_el(
        "Alt Readout",
        "Bottom",
        (185.0, -104.0),
        (58.0, 24.0),
        "                text: Some((\n                    text: \"0\",\n                    size: 17.0,\n                    color: (1.0, 0.75, 0.3, 1.0),\n                    align: Start,\n                    valign: Center,\n                    fit: false,\n                )),",
    ));
    nodes.push(pin_el(
        "Landing Screen",
        "BottomRight",
        (-14.0, -14.0),
        (206.0, 116.0),
        "                image: Some((\n                    texture: \"rt:landing\",\n                    tint: (1.0, 1.0, 1.0, 1.0),\n                )),",
    ));
    nodes.push(pin_el(
        "Heading Readout",
        "Bottom",
        (0.0, -188.0),
        (120.0, 22.0),
        "                text: Some((\n                    text: \"HDG 000°\",\n                    size: 16.0,\n                    color: (0.85, 0.95, 1.0, 1.0),\n                    align: Center,\n                    valign: Center,\n                    fit: false,\n                )),",
    ));

    // Landing legs.
    for (i, (x, z)) in
        [(0.52, 0.52), (-0.52, 0.52), (0.52, -0.52), (-0.52, -0.52)].iter().enumerate()
    {
        nodes.push(format!(
            r#"        (
            name: "Leg {}",
            transform: (
                translation: ({x}, -0.6, {z}),
                rotation: (0.0, 0.0, 0.0, 1.0),
                scale: (0.1, 0.5, 0.1),
            ),
            matter: Primitive(
                shape: Cube,
                color: (0.38, 0.4, 0.46),
            ),
            scripts: [],
            parent: Some({ship_idx}),
        ),
"#,
            ["A", "B", "C", "D"][i],
        ));
    }

    format!(
        r#"(
    name: "{scene}",
    lighting: (
        direction: (0.4, 0.9, 0.45),
        stars: true,
        color: ({lr:.2}, {lg:.2}, {lb:.2}),
        ambient: (0.012, 0.012, 0.018),
        intensity: 1.0,
        shadows: true,
        shadow_softness: 0.35,
        shadow_strength: 1.0,
        shadow_tint: (0.0, 0.0, 0.0),
        shadow_quantize: 0,
        shadow_dither: false,
        shadow_distance: 400.0,
    ),
    nodes: [
{nodes}    ],
)
"#,
        lr = spec.star.color[0],
        lg = spec.star.color[1],
        lb = spec.star.color[2],
        nodes = nodes.concat(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same seed → identical spec; different seeds → different systems.
    #[test]
    fn system_roll_is_deterministic_and_varied() {
        let a = SystemSpec::random(1234, None);
        let b = SystemSpec::random(1234, None);
        assert_eq!(a.bodies.len(), b.bodies.len());
        assert_eq!(a.star.name, b.star.name);
        for (x, y) in a.bodies.iter().zip(&b.bodies) {
            assert_eq!(x.body.name, y.body.name);
            assert_eq!(x.orbit.a, y.orbit.a);
        }
        let c = SystemSpec::random(99, None);
        assert!(
            a.star.name != c.star.name || a.bodies[0].body.name != c.bodies[0].body.name,
            "different seeds should not collide on every name"
        );
    }

    /// Orbits must nest: every moon inside its planet's SOI, every moon's own
    /// SOI clear of its terrain, planet SOIs far smaller than orbit gaps.
    #[test]
    fn rolled_orbits_nest_safely() {
        for seed in [1, 7, 42, 1000, 123456] {
            let s = SystemSpec::random(seed, None);
            let planets: Vec<&GenBody> =
                s.bodies.iter().filter(|b| b.orbit.parent == s.star.name).collect();
            assert!(planets.len() >= 2);
            for p in &planets {
                let soi = p.orbit.a * (p.orbit.mu / s.star.mu).powf(0.4);
                assert!(soi > p.orbit.body_radius * 2.0, "seed {seed}: SOI hugs the terrain");
                for m in s.bodies.iter().filter(|b| b.orbit.parent == p.body.name) {
                    assert!(m.orbit.a < soi * 0.5, "seed {seed}: moon outside half the SOI");
                    let msoi = m.orbit.a * (m.orbit.mu / p.orbit.mu).powf(0.4);
                    assert!(msoi > m.orbit.body_radius * 1.5, "seed {seed}: moon SOI too tight");
                }
            }
            for w in planets.windows(2) {
                let soi0 = w[0].orbit.a * (w[0].orbit.mu / s.star.mu).powf(0.4);
                assert!(
                    w[1].orbit.a - w[0].orbit.a > soi0 * 2.0,
                    "seed {seed}: adjacent planet orbits too close"
                );
            }
        }
    }

    /// The generated scene must be parseable RON with sane structure (the
    /// scene crate does full validation; here we guard the string builder).
    #[test]
    fn built_scene_is_balanced() {
        let s = SystemSpec::random(777, Some(3));
        let scene = build_scene(&s, "system");
        assert!(scene.matches("name: \"").count() >= 10);
        let open = scene.matches('(').count();
        let close = scene.matches(')').count();
        assert_eq!(open, close, "unbalanced parens in generated scene");
        assert!(scene.contains("stars: true"));
        assert!(scene.contains("ship_controller"));
    }
}
