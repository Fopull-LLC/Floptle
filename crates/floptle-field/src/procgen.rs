//! Generic procedural PLANET fill — the native backend of the Lua
//! `terrain.generatePlanet(id, opts)` API.
//!
//! Deliberately game-agnostic: WHAT to build (solar systems, archetypes,
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
/// exceeds `threshold`, below `min_depth`. Use a palette GLOW slot to make
/// them self-lit; higher thresholds = rarer.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct GlowPockets {
    pub paint: LayerPaint,
    pub threshold: f32,
    pub min_depth: f32,
}

/// Thin vein seams (magma/ore filaments): painted where a seam noise field
/// sits within `width` of `center` — a BAND, so seams read as veins running
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
/// Serializable: the RON form IS the on-node "genspec" that lets a body
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
            // down, and a solid core survives at the center.
            let a = noise.fbm(p * 0.018, 3);
            let b = noise.fbm(p * 0.018 + Vec3::splat(51.7), 3);
            let w = 0.1 + (-body * 0.0006).clamp(0.0, 0.07);
            let tunnel = (a.abs().max(b.abs()) - w) * 34.0;
            let c1 = noise.fbm(p * 0.008 + Vec3::splat(113.0), 2);
            let c2 = noise.fbm(p * 0.008 + Vec3::splat(7.9), 2);
            let chamber = ((c1.abs().max(c2.abs())) - 0.14) * 70.0;
            let cave = tunnel.min(chamber.max(body + cave_depth * 0.42));
            let gated = cave
                .max(voxel * 3.0 + body)
                .max(-(body + cave_depth))
                .max((core_r + voxel * 6.0) - r);
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

            // Molten core zone (usually a GLOW slot): a deep dig reads HOT
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
            // Vein seams: a thin BAND of the seam field — filaments through
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

    /// Same spec → byte-identical field; a different seed diverges.
    #[test]
    fn generate_planet_is_deterministic() {
        let spec = PlanetFill { radius: 24.0, voxel: 1.0, cave_depth: 10.0, ..Default::default() };
        let a = generate_planet(&spec).to_bytes();
        let b = generate_planet(&spec).to_bytes();
        assert_eq!(a, b);
        let c = generate_planet(&PlanetFill { seed: 8, ..spec }).to_bytes();
        assert_ne!(a, c);
    }

    /// The seam band paints VEINS, not walls: a banded seam must color far
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
