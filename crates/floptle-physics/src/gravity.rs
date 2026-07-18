//! Gravity as a composable vector field `g(p)`: a global vector plus
//! point/radial sources, sampled by bodies and the character (ADR-0014).

use floptle_core::math::Vec3;

use crate::world::AnchoredCollider;

/// One contribution to the composable gravity field (ADR-0014). A body sums the
/// enabled sources at its position and treats the result as "down".
pub enum GravitySource {
    /// Constant acceleration — most games, e.g. `(0, -9.81, 0)`.
    Uniform(Vec3),
    /// Constant-magnitude radial pull toward a point — a planet (Mario Galaxy):
    /// stand anywhere on a sphere world and "down" is toward its center. `radius`
    /// bounds the gravity well (≤ 0 = unbounded), so multiple planets don't fight.
    Point { center: Vec3, strength: f32, radius: f32 },
    /// Pull onto a collider's surface along `-∇f` — grounds you on fractal walls.
    SdfSurface { collider: usize, strength: f32 },
    /// Real inverse-square gravity µ/r² toward a celestial body (solar demo S2).
    /// PATCHED CONICS: of all `InvSq` sources whose SOI contains the point, only
    /// the DEEPEST (smallest SOI — the moon inside the planet inside the sun)
    /// pulls; the rest contribute nothing. `soi ≤ 0` = infinite (the system
    /// root). Non-InvSq sources still add on top as usual. INSIDE `body_r`
    /// (the physical surface radius; 0 = point mass) the pull falls off
    /// linearly to zero at the center — the uniform-density interior solution —
    /// so digging to a planet's core never meets the 1/r² singularity.
    InvSq { center: Vec3, mu: f32, soi: f32, body_r: f32 },
}

/// Gravity as a sum of composable sources `g(p)`.
#[derive(Default)]
pub struct GravityField {
    pub sources: Vec<GravitySource>,
}

impl GravityField {
    /// A single uniform gravity vector (the common case).
    pub fn uniform(g: Vec3) -> Self {
        Self { sources: vec![GravitySource::Uniform(g)] }
    }

    /// The summed acceleration at sim-frame point `p` (colliders are needed for the
    /// `SdfSurface` tier). `Point` centers are sim-frame too — the caller converts
    /// world positions when it builds the field.
    pub fn accel_at(&self, p: Vec3, colliders: &[AnchoredCollider]) -> Vec3 {
        let mut a = Vec3::ZERO;
        // Patched conics: exactly one dominant InvSq body pulls (see `InvSq`).
        let mut dominant: Option<(f32, Vec3, f32, f32)> = None; // (soi, center, mu, body_r)
        for s in &self.sources {
            if let GravitySource::InvSq { center, mu, soi, body_r } = s {
                let soi_eff = if *soi <= 0.0 { f32::INFINITY } else { *soi };
                if (p - *center).length() <= soi_eff
                    && dominant.is_none_or(|(cur, ..)| soi_eff < cur)
                {
                    dominant = Some((soi_eff, *center, *mu, *body_r));
                }
            }
        }
        if let Some((_, center, mu, body_r)) = dominant {
            let to = center - p;
            let r2 = to.length_squared().max(1e-6);
            let r = r2.sqrt();
            // Uniform-density interior below the surface: g = µ·r/R³, reaching
            // ZERO at the exact center (instead of the 1/r² blow-up that
            // slingshotted anyone who dug to the core).
            let g = if body_r > 0.0 && r < body_r {
                mu * r / (body_r * body_r * body_r)
            } else {
                mu / r2
            };
            a += to / r * g;
        }
        for s in &self.sources {
            a += match s {
                GravitySource::InvSq { .. } => Vec3::ZERO, // handled above
                GravitySource::Uniform(g) => *g,
                GravitySource::Point { center, strength, radius } => {
                    let to = *center - p;
                    if *radius > 0.0 && to.length() > *radius {
                        Vec3::ZERO
                    } else {
                        to.try_normalize().unwrap_or(Vec3::ZERO) * *strength
                    }
                }
                GravitySource::SdfSurface { collider, strength } => colliders
                    .get(*collider)
                    .map(|c| -c.normal(p) * *strength)
                    .unwrap_or(Vec3::ZERO),
            };
        }
        a
    }
}
