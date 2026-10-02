//! Generic procedural planet fill — the native backend of the Lua
//! `terrain.generatePlanet(id, opts)` API.
//!
//! Deliberately game-agnostic: what to build (solar systems, archetypes,
//! orbits, names) is game-side scripting; this module is only the heavy
//! per-voxel primitive a script can't afford to run itself — a layered,
//! cavernous, cratered sphere written into a sparse [`ChunkField`], with
//! every knob exposed on [`PlanetFill`].
//!
//! The recipe (proven on the solar demo's planetoid): sphere ± fbm-of-direction
//! relief; caves where two noise fields are both near zero (galleries that
//! widen with depth + a larger-scale chamber system), guarded by a crust
//! band, a depth cap and a solid core; impact craters subtracted in the fill
//! (carving after hits the band clamp and terraces); materials by depth layer
//! (surface biome patches → subsoil → strata → deep cave rock) with optional
//! glowing pockets, thin glowing seams and polar ice.

use floptle_core::math::Vec3;
use floptle_core::noise::{Noise, Rng};

use crate::ChunkField;

/// One material layer: a terrain palette slot (1-based) + an RGB tint
/// (`albedo = texture × tint`).
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct LayerPaint {
    pub slot: u8,
    pub color: [f32; 3],
}

/// Sparse glowing pockets (crystal/ore): painted where a dedicated fbm field
/// exceeds `threshold`, below `min_depth`. Use a palette glow slot to make
/// them self-lit; higher thresholds = rarer.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct GlowPockets {
    pub paint: LayerPaint,
    pub threshold: f32,
    pub min_depth: f32,
}

/// Thin vein seams (magma/ore filaments): painted where a seam noise field
/// sits within `width` of `center` — a band, so seams read as veins running
/// through the rock instead of flooding whole walls.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct SeamSpec {
    pub paint: LayerPaint,
    pub min_depth: f32,
    pub center: f32,
    pub width: f32,
}

/// Everything `generate_planet` needs — every field has a workable default,
/// so callers (the Lua table) override only what they care about.
/// Serializable: the RON form is the on-node "genspec" that lets a body
/// generate on-demand when first approached (G2 galaxy streaming) — new
/// fields must keep serde defaults so old genspec strings stay loadable.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PlanetFill {
    pub seed: u32,
    pub radius: f32,
    /// Voxel size (world units) — smaller = finer + heavier.
    pub voxel: f32,
    /// Peak surface displacement.
    pub relief: f32,
    /// Relief noise frequency (dunes ripple high, ice rolls low).
    pub bump_freq: f32,
    /// Cave zone thickness below the surface (0 = solid body).
    pub cave_depth: f32,
    /// Solid core ball radius the caves never touch (0 = none).
    pub core_r: f32,
    /// Molten zone paint around the core (slot 0 = none).
    pub core_paint: LayerPaint,
    /// Impact crater count (0 = none) + dent radii as fractions of `radius`.
    pub craters: u32,
    pub crater_min: f32,
    pub crater_max: f32,
    pub crater_dust: LayerPaint,
    /// Surface biomes: `patch + alt·patch_bias > patch_thr` → `surface_a`.
    pub surface_a: LayerPaint,
    pub surface_b: LayerPaint,
    pub patch_bias: f32,
    pub patch_thr: f32,
    /// Depth layers (dig walls read as geology).
    pub subsoil: LayerPaint,
    pub subsoil_depth: f32,
    pub strata: LayerPaint,
    pub strata_depth: f32,
    /// Deep cave-zone wall rock (below half the cave depth).
    pub deep: LayerPaint,
    pub pockets: Option<GlowPockets>,
    pub seam: Option<SeamSpec>,
    /// Polar caps + frost patches: `|dir.y| > lat` (or patch noise < -0.42).
    pub ice_caps: Option<(f32, LayerPaint)>,
}

impl Default for PlanetFill {
    fn default() -> Self {
        PlanetFill {
            seed: 7,
            radius: 120.0,
            voxel: 1.5,
            relief: 8.0,
            bump_freq: 4.3,
            cave_depth: 40.0,
            core_r: 8.0,
            core_paint: LayerPaint { slot: 0, color: [0.98, 0.82, 0.6] },
            craters: 0,
            crater_min: 0.12,
            crater_max: 0.26,
            crater_dust: LayerPaint { slot: 11, color: [0.78, 0.74, 0.66] },
            surface_a: LayerPaint { slot: 1, color: [0.72, 0.62, 0.58] },
            surface_b: LayerPaint { slot: 2, color: [0.66, 0.6, 0.74] },
            patch_bias: 0.45,
            patch_thr: 0.08,
            subsoil: LayerPaint { slot: 3, color: [0.6, 0.58, 0.68] },
            subsoil_depth: 2.4,
            strata: LayerPaint { slot: 4, color: [0.85, 0.78, 0.66] },
            strata_depth: 9.0,
            deep: LayerPaint { slot: 5, color: [0.78, 0.68, 0.58] },
            pockets: None,
            seam: None,
            ice_caps: None,
        }
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

/// Smooth minimum: `min(a, b)` with the corner rounded over about `k`. Never
/// above `min(a, b)`.
fn smin(a: f32, b: f32, k: f32) -> f32 {
    let h = (0.5 + 0.5 * (b - a) / k).clamp(0.0, 1.0);
    b + (a - b) * h - k * h * (1.0 - h)
}

/// Smooth maximum: `max(a, b)` with the corner rounded over about `k`. Never
/// below `max(a, b)`.
fn smax(a: f32, b: f32, k: f32) -> f32 {
    -smin(-a, -b, k)
}

/// A planet's shape before it is voxelized: the ground, the craters and the
/// caves, as a signed distance (negative is rock).
pub(crate) struct PlanetShape {
    noise: Noise,
    radius: f32,
    relief: f32,
    cave_depth: f32,
    core_r: f32,
    bump_freq: f32,
    voxel: f32,
    craters: Vec<(Vec3, f32)>,
}

impl PlanetShape {
    /// The ground alone: negative under it.
    pub(crate) fn body(&self, p: Vec3) -> f32 {
        let r = p.length();
        let dir = if r > 1e-3 { p / r } else { Vec3::X };
        let bump = self.noise.fbm(dir * self.bump_freq, 5) * self.relief;
        let mut body = r - (self.radius + bump);
        for (c, cr) in &self.craters {
            body = body.max(-((p - *c).length() - cr));
        }
        body
    }

    /// The whole shape, caves included.
    ///
    /// Caves are where two noise fields are both near zero. Their cross-section
    /// is round (the length of the pair, not the larger of the two, which made
    /// a rhombus of four flat walls and four creases), galleries widen with
    /// depth, a larger-scale chamber field opens rooms further down, and a
    /// solid core survives at the centre. Every join is smooth, so tunnels meet
    /// rooms and the ceiling in rounded junctions, and a metre of noise at wall
    /// scale keeps a wall from being a plane over tens of metres. The crust
    /// gate is a smooth maximum, never below the hard one: no cave comes within
    /// three voxels of the surface.
    pub(crate) fn d(&self, p: Vec3) -> f32 {
        let body = self.body(p);
        if self.cave_depth <= 0.0 {
            return body;
        }
        let noise = &self.noise;
        let r = p.length();
        let a = noise.fbm(p * 0.018, 3);
        let b = noise.fbm(p * 0.018 + Vec3::splat(51.7), 3);
        let w = 0.1 + (-body * 0.0006).clamp(0.0, 0.07);
        let tunnel = (a.hypot(b) - w) * 34.0;
        let c1 = noise.fbm(p * 0.008 + Vec3::splat(113.0), 2);
        let c2 = noise.fbm(p * 0.008 + Vec3::splat(7.9), 2);
        let chamber = (c1.hypot(c2) - 0.14) * 70.0;
        let ceiling = body + self.cave_depth * 0.42;
        let detail = noise.fbm(p * 0.07 + Vec3::splat(3.3), 3) * 1.2;
        let cave = smin(tunnel, smax(chamber, ceiling, 6.0), 4.0) + detail;
        let gated = smax(smax(cave, self.voxel * 3.0 + body, 2.0), -(body + self.cave_depth), 4.0)
            .max((self.core_r + self.voxel * 6.0) - r);
        body.max(-gated)
    }
}

/// Fill one planet into a fresh sparse field. Deterministic in `spec`.
pub fn generate_planet(spec: &PlanetFill) -> ChunkField {
    let noise = Noise::new(spec.seed);
    let mut rng = Rng::new(spec.seed.wrapping_add(31));
    let mut field = ChunkField::new(spec.voxel.clamp(0.25, 16.0));
    let ext = spec.radius + spec.relief + 2.0;
    let (radius, relief, cave_depth, core_r, bump_freq) =
        (spec.radius, spec.relief, spec.cave_depth, spec.core_r, spec.bump_freq);
    let spec = spec.clone();

    // Impact craters: spherical dents subtracted in the fill SDF.
    let craters: Vec<(Vec3, f32)> = (0..spec.craters)
        .map(|_| {
            let d = Vec3::new(
                rng.range(-1.0, 1.0) as f32,
                rng.range(-1.0, 1.0) as f32,
                rng.range(-1.0, 1.0) as f32,
            )
            .normalize_or_zero();
            let d = if d == Vec3::ZERO { Vec3::X } else { d };
            let r = rng.range(spec.crater_min as f64, spec.crater_max as f64) as f32 * radius;
            (d * (radius + relief * 0.3), r)
        })
        .collect();
    let craters2 = craters.clone();
    let voxel = field.voxel();
    let shape = PlanetShape { noise, radius, relief, cave_depth, core_r, bump_freq, voxel, craters };

    field.fill_with_rgba(
        Vec3::splat(-ext),
        Vec3::splat(ext),
        move |p| shape.d(p),
        move |p| {
            let r = p.length();
            let dir = if r > 1e-3 { p / r } else { Vec3::X };
            let bump = noise.fbm(dir * bump_freq, 5) * relief;
            let alt = (r - radius) / relief.max(1.0);
            let depth = (radius + bump) - r;
            let vary = noise.fbm(p * 0.05 + Vec3::splat(83.0), 2);
            let patch = noise.fbm(dir * 9.0 + Vec3::splat(37.0), 3);

            // Molten core zone (usually a glow slot): a deep dig reads hot
            // before the core itself appears.
            if spec.core_paint.slot != 0 && cave_depth > 0.0 && r < core_r + core_r.min(10.0) {
                return rgba(tint(spec.core_paint.color, vary), spec.core_paint.slot);
            }
            // Sparse glowing pockets — the reason to dig.
            if let Some(pk) = &spec.pockets {
                let pocket = noise.fbm(p * 0.05 + Vec3::splat(17.3), 3);
                if depth > pk.min_depth && pocket > pk.threshold {
                    return rgba(tint(pk.paint.color, vary), pk.paint.slot);
                }
            }
            // Vein seams: a thin band of the seam field — filaments through
            // the rock, not glowing walls.
            if let Some(sm) = &spec.seam
                && depth > sm.min_depth
                && (noise.fbm(p * 0.035 + Vec3::splat(5.9), 3) - sm.center).abs() < sm.width
            {
                return rgba(tint(sm.paint.color, vary), sm.paint.slot);
            }
            // Depth layers: deep cave walls → strata → subsoil.
            if depth > cave_depth * 0.5 {
                return rgba(tint(spec.deep.color, vary), spec.deep.slot);
            }
            if depth > spec.strata_depth {
                return rgba(tint(spec.strata.color, vary), spec.strata.slot);
            }
            if depth > spec.subsoil_depth {
                return rgba(tint(spec.subsoil.color, vary), spec.subsoil.slot);
            }
            // Crater floors wear dust.
            for (c, cr) in &craters2 {
                if (p - *c).length() < cr + 1.6 {
                    return rgba(tint(spec.crater_dust.color, vary), spec.crater_dust.slot);
                }
            }
            // Polar caps + frost patches.
            if let Some((lat, ice)) = &spec.ice_caps
                && (dir.y.abs() > *lat || patch < -0.42)
            {
                return rgba(tint(ice.color, vary), ice.slot);
            }
            // Surface biomes.
            if patch + alt * spec.patch_bias > spec.patch_thr {
                rgba(tint(spec.surface_a.color, vary), spec.surface_a.slot)
            } else {
                rgba(tint(spec.surface_b.color, vary), spec.surface_b.slot)
            }
        },
    );
    field
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The card's Bryolru-like world: radius 200, relief 13, caves to 0.35 r.
    fn bryolru() -> PlanetShape {
        let radius = 200.19;
        PlanetShape {
            noise: Noise::new(800921623),
            radius,
            relief: 13.11,
            cave_depth: radius * 0.35,
            core_r: radius * 0.07,
            bump_freq: 4.5,
            voxel: 1.5,
            craters: Vec::new(),
        }
    }

    /// Wall points of the shape, cave walls and the outer ground apart: for
    /// each, how far the wall runs before it curves by a radian (its radius of
    /// curvature), and the sharpest normal change to a wall point 1.5 m away.
    fn wall_samples(shape: &PlanetShape, n: usize) -> [Vec<(f32, f32)>; 2] {
        let f = |p: Vec3| shape.d(p);
        let grad = |p: Vec3| {
            let h = 0.25;
            Vec3::new(
                f(p + Vec3::X * h) - f(p - Vec3::X * h),
                f(p + Vec3::Y * h) - f(p - Vec3::Y * h),
                f(p + Vec3::Z * h) - f(p - Vec3::Z * h),
            )
            .normalize_or_zero()
        };
        let mut seed = 7u64;
        let mut rnd = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((seed >> 33) as f32) / (1u64 << 31) as f32
        };
        let mut out: [Vec<(f32, f32)>; 2] = [Vec::new(), Vec::new()];
        let mut tries = 0;
        while out[0].len() + out[1].len() < n && tries < n * 200 {
            tries += 1;
            // A random open point in the cave zone or just above the ground,
            // then the nearest wall along a random ray.
            let d = Vec3::new(rnd() * 2.0 - 1.0, rnd() * 2.0 - 1.0, rnd() * 2.0 - 1.0).normalize_or_zero();
            let r0 = shape.radius - shape.cave_depth + rnd() * (shape.cave_depth + shape.relief + 5.0);
            let p0 = d * r0;
            if d == Vec3::ZERO || f(p0) <= 0.2 {
                continue;
            }
            let rd = Vec3::new(rnd() * 2.0 - 1.0, rnd() * 2.0 - 1.0, rnd() * 2.0 - 1.0).normalize_or_zero();
            let mut t = 0.0;
            let mut hit = None;
            while t < 30.0 {
                let v = f(p0 + rd * t);
                if v < 0.0 {
                    hit = Some(t);
                    break;
                }
                t += (v * 0.3).max(0.05);
            }
            let Some(t) = hit else { continue };
            let p = p0 + rd * t;
            // Cave wall or open ground: is the ground what makes the wall here?
            let outer = shape.body(p) >= f(p) - 1e-3;
            let nrm = grad(p);
            let t1 = nrm.cross(Vec3::new(0.3, 0.8, 0.5)).normalize_or_zero();
            let t2 = nrm.cross(t1);
            let k1 = (grad(p + t1 * 0.75) - grad(p - t1 * 0.75)).length() / 1.5;
            let k2 = (grad(p + t2 * 0.75) - grad(p - t2 * 0.75)).length() / 1.5;
            let mut crease = 0.0f32;
            for step in [t1, t2, -t1, -t2] {
                let mut q = p + step * 1.5;
                for _ in 0..8 {
                    q -= grad(q) * f(q);
                }
                crease = crease.max(nrm.dot(grad(q)).clamp(-1.0, 1.0).acos().to_degrees());
            }
            out[outer as usize].push((1.0 / k1.max(k2).max(1e-4), crease));
        }
        out
    }

    /// **Cave walls are rock, not panels.** On the Bryolru-like spec, no more of
    /// the cave walls is flat over 20 m than twice the outer ground's share (it
    /// was 27% against 6%), and under 2% of them sit within 1.5 m of a crease
    /// sharper than 60°.
    #[test]
    fn cave_walls_curve_like_the_ground_and_have_no_creases() {
        let [caves, ground] = wall_samples(&bryolru(), 2500);
        assert!(caves.len() > 300 && ground.len() > 300, "too few samples: {} caves, {} ground", caves.len(), ground.len());
        let flat = |s: &[(f32, f32)]| s.iter().filter(|x| x.0 > 20.0).count() as f32 / s.len() as f32;
        let sharp = |s: &[(f32, f32)], deg: f32| s.iter().filter(|x| x.1 > deg).count() as f32 / s.len() as f32;
        let (fc, fg) = (flat(&caves), flat(&ground));
        let creased = sharp(&caves, 60.0);
        eprintln!(
            "caves {}: {:.1}% flat over 20 m, {:.1}% by a crease > 30°, {:.1}% > 60°; ground {}: {:.1}% flat",
            caves.len(),
            fc * 100.0,
            sharp(&caves, 30.0) * 100.0,
            creased * 100.0,
            ground.len(),
            fg * 100.0
        );
        assert!(fc <= 2.0 * fg.max(0.01), "{:.1}% of cave walls are flat over 20 m against {:.1}% of the ground", fc * 100.0, fg * 100.0);
        assert!(creased < 0.02, "{:.1}% of cave walls sit by a crease sharper than 60°", creased * 100.0);
    }

    /// **The crust holds.** No cave opens within three voxels of the surface,
    /// however the joins are smoothed.
    #[test]
    fn no_cave_comes_within_three_voxels_of_the_surface() {
        let shape = bryolru();
        let mut seed = 3u64;
        let mut rnd = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((seed >> 33) as f32) / (1u64 << 31) as f32
        };
        let mut checked = 0;
        for _ in 0..20000 {
            let d = Vec3::new(rnd() * 2.0 - 1.0, rnd() * 2.0 - 1.0, rnd() * 2.0 - 1.0).normalize_or_zero();
            if d == Vec3::ZERO {
                continue;
            }
            // Walk in from above the ground to three voxels under it.
            let mut r = shape.radius + shape.relief + 1.0;
            while shape.body(d * r) > 0.0 {
                r -= 0.25;
            }
            // Down through the crust, by the ground's own depth: from just
            // under the surface (which is zero) to three voxels below it.
            loop {
                let p = d * r;
                let depth = -shape.body(p);
                if depth >= shape.voxel * 3.0 {
                    break;
                }
                if depth > 0.01 {
                    assert!(shape.d(p) < 0.0, "open space {depth:.2} m under the surface at {p}");
                    checked += 1;
                }
                r -= 0.25;
            }
        }
        assert!(checked > 100_000);
    }

    /// Same spec → byte-identical field; a different seed diverges.    /// Same spec → byte-identical field; a different seed diverges.
    #[test]
    fn generate_planet_is_deterministic() {
        let spec = PlanetFill { radius: 24.0, voxel: 1.0, cave_depth: 10.0, ..Default::default() };
        let a = generate_planet(&spec).to_bytes();
        let b = generate_planet(&spec).to_bytes();
        assert_eq!(a, b);
        let c = generate_planet(&PlanetFill { seed: 8, ..spec }).to_bytes();
        assert_ne!(a, c);
    }

    /// The seam band paints veins, not walls: a banded seam must color far
    /// fewer voxels than a threshold at the same level would.
    #[test]
    fn seam_band_is_sparse() {
        let base = PlanetFill {
            radius: 22.0,
            voxel: 1.0,
            relief: 3.0,
            cave_depth: 12.0,
            core_r: 3.0,
            ..Default::default()
        };
        let spec = PlanetFill {
            seam: Some(SeamSpec {
                paint: LayerPaint { slot: 6, color: [1.0, 0.8, 0.6] },
                min_depth: 2.0,
                center: 0.3,
                width: 0.04,
            }),
            ..base.clone()
        };
        let field = generate_planet(&spec);
        let (mut seam_px, mut total) = (0u32, 0u32);
        let (Some((lo, hi)),) = (field.bounds(),) else { panic!("empty field") };
        let mut p = lo;
        while p.x < hi.x {
            p.y = lo.y;
            while p.y < hi.y {
                p.z = lo.z;
                while p.z < hi.z {
                    if field.d(p) < 0.0 {
                        total += 1;
                        if field.color(p)[3] == 6 {
                            seam_px += 1;
                        }
                    }
                    p.z += 2.0;
                }
                p.y += 2.0;
            }
            p.x += 2.0;
        }
        assert!(total > 500, "sample too small ({total})");
        let frac = seam_px as f32 / total as f32;
        assert!(frac < 0.15, "seams flood the rock: {frac:.2} of solid voxels glow");
    }
}
