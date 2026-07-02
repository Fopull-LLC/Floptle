//! # floptle-field
//!
//! The shared **implicit geometry + spatial fields** layer — the single
//! representation that lets "everything is malleable matter" be true. A surface
//! is a signed-distance function `f(p, t)`; the renderer raymarches it, physics
//! collides against it, and matter deforms it. Because geometry is a *field*,
//! combining shapes is just algebra: blend/mix/reject become CSG operators with a
//! blend radius. The same machinery carries other spatial fields — density `ρ(p)`
//! and gravity (potential `Φ(p)`, acceleration `g(p)`) — so the engine can treat
//! space, matter, gravity, and density uniformly.
//! See `docs/subsystems/deformable-matter.md` + `docs/subsystems/gravity-and-density.md`
//! (ADR-0013, ADR-0014).
//!
//! Planned modules:
//! - `sdf`      : SDF trait + analytic primitives (box/sphere/capsule/…/fractals).
//! - `ops`      : CSG/blend operators — union/intersect/subtract and their
//!                **smooth** variants `smin`/`smax(k)` (clean merging = "soup"),
//!                plus per-pair blend *rules* (merge / mix / reject).
//! - `mesh2sdf` : robust mesh -> SDF (fast/generalized winding number + BVH) so
//!                developer meshes join the field world.
//! - `sdf2mesh` : field -> mesh (surface nets / dual contouring / marching cubes)
//!                when an actual triangle mesh is needed.
//! - `field`    : baked sparse distance field (brickmap) — cheap shared cache.
//! - `scalar`   : general scalar/vector spatial fields sampled on the brickmap —
//!                density `ρ(p)`, gravity potential `Φ(p)`, acceleration `g(p)`
//!                (ADR-0014).

pub mod mesh2sdf;
pub mod terrain;

pub use mesh2sdf::{bake, bake_occluder, BakedSdf, TexRef};
pub use terrain::{Brush, Terrain};

/// Polynomial smooth-minimum: blends two distances over radius `k` (k→0 = hard
/// union). The core "geometry blends together cleanly" operator.
#[inline]
pub fn smin(a: f32, b: f32, k: f32) -> f32 {
    if k <= 0.0 {
        return a.min(b);
    }
    let h = (0.5 + 0.5 * (b - a) / k).clamp(0.0, 1.0);
    b * (1.0 - h) + a * h - k * h * (1.0 - h)
}

/// Smooth maximum (smooth intersection / "reject"), the dual of [`smin`].
#[inline]
pub fn smax(a: f32, b: f32, k: f32) -> f32 {
    -smin(-a, -b, k)
}

/// How two pieces of matter combine when they meet (selected per material pair).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlendRule {
    /// Hard boolean union — they stay distinct solids touching.
    Union,
    /// Smooth-min merge with a blend radius — they fuse like soup.
    Merge,
    /// Smooth subtraction/intersection — they carve / refuse each other.
    Reject,
}

/// Mass density (kg/m³) of the matter occupying a region — the source term for
/// both inertia (`m = ρ·V`) and gravity (`∇²Φ = 4πGρ`). See ADR-0014.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Density(pub f32);

