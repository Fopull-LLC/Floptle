//! # floptle-physics
//!
//! Floptle's distinctive physics need — letting players drive, roll, and roam on
//! **fractals that are actively morphing** — is exactly the case off-the-shelf
//! rigid-body engines are *worst* at (they assume explicit, mostly-static
//! collision geometry). So the collision core is custom and **SDF-first**: we
//! collide against the same signed-distance function the renderer draws, which
//! is cheaper AND more robust than re-meshing a morphing surface every frame.
//! See `docs/subsystems/physics.md` + ADR-0012.
//!
//! Layered design (own the novel parts, borrow only the boring parts):
//! - `world`     : the collision world — a set of colliders queried each step.
//! - `sdf`       : SDF colliders (fractals + analytic primitives); point/sphere/
//!                 capsule/ray queries via `f(p,t)`, normals from its gradient,
//!                 surface velocity from ∂f/∂t (so riders inherit the morph).
//! - `field`     : baked sparse SDF/voxel grid — decouples physics cost from the
//!                 expensive analytic fractal (analytic near, baked far).
//! - `mesh`      : triangle-BVH colliders for static/imported (Blender) meshes.
//! - `character` : kinematic capsule controller (the "cool movement system");
//!                 samples `gravity` and aligns orientation to `-g`, so you can
//!                 run on a fractal and up its swirling walls (ADR-0014).
//! - `vehicle`   : raycast-vehicle model (drive a car across the fractal).
//! - `gravity`   : gravity as a composable vector field `g(p)` — global, analytic
//!                 sources (planets), SDF-surface (`-∇f`), and calculated
//!                 density-field (Poisson `∇²Φ=4πGρ`, Barnes-Hut/FFT) (ADR-0014).
//! - `dynamics`  : OPTIONAL lightweight impulse solver for object-vs-object.

/// A signed-distance query result: distance to surface + the outward normal.
/// Negative distance means penetration (depth = `-distance`).
#[derive(Debug, Clone, Copy)]
pub struct SdfHit {
    pub distance: f32,
    pub normal: [f32; 3],
}

/// Composable contributions to the gravity field `g(p)`, cheapest → heaviest.
/// A body sums the enabled tiers and treats the result as "down" (ADR-0014).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GravityTier {
    /// Constant vector or authored gravity volume. (Most games.)
    Global,
    /// Analytic point/sphere/line sources — planets: `g = -G·M·(p-c)/|p-c|³`.
    Source,
    /// SDF-surface pull `g ∝ -∇f(p)` — grounds you on fractal surfaces/walls.
    SdfSurface,
    /// Calculated from the density field via a Poisson solve. (Later/research.)
    DensityField,
}
