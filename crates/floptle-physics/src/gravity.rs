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
        for s in &self.sources {
            a += match s {
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
