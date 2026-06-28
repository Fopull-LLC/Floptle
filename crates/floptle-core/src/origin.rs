//! Floating origin (ADR-0015): keep the active simulation near `(0,0,0)`.
//!
//! Even with `f64` world transforms, physics finite-differences (`∇f`, penetration
//! depth) are *differences of large nearly-equal numbers* where precision dies. So
//! when the camera drifts past a threshold, the whole world is **rebased** by a
//! whole-number offset between fixed steps: every position shifts, the camera
//! returns toward the origin, and velocities/forces (translation-invariant) are
//! untouched. The render path already uploads camera-relative, so a rebase is
//! invisible on screen — it only protects the *simulation* numbers.
//!
//! This type computes *when* and *by how much* to rebase; applying the shift to
//! all `Transform`s is the caller's job (it owns the ECS) and lands with the ECS
//! integration. Kept deliberately tiny and side-effect-free so it's trivial to
//! test and to call from the fixed-step "quiet point".

use crate::math::DVec3;

/// Decides rebases. Default-on; the developer never configures it.
#[derive(Debug, Clone, Copy)]
pub struct FloatingOrigin {
    /// Rebase once the camera is at least this far from the current origin.
    pub threshold: f64,
    /// Total accumulated shift applied to the world so far (so absolute world
    /// coordinates can always be reconstructed: `absolute = local - total_shift`).
    pub total_shift: DVec3,
}

impl Default for FloatingOrigin {
    fn default() -> Self {
        // 4 km: comfortably inside f32's clean range, rare enough to be cheap.
        Self { threshold: 4096.0, total_shift: DVec3::ZERO }
    }
}

impl FloatingOrigin {
    pub fn new(threshold: f64) -> Self {
        Self { threshold, ..Default::default() }
    }

    /// Given the camera's current (local) position, return the shift to apply to
    /// every entity this step, or `None` if no rebase is needed. The returned
    /// vector is what you *add* to all positions (and to the camera) to recenter.
    pub fn rebase(&mut self, camera_local: DVec3) -> Option<DVec3> {
        if camera_local.length_squared() < self.threshold * self.threshold {
            return None;
        }
        let shift = -camera_local; // pull the camera back to the origin
        self.total_shift += shift;
        Some(shift)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_rebase_when_close() {
        let mut fo = FloatingOrigin::new(1000.0);
        assert!(fo.rebase(DVec3::new(10.0, 0.0, 0.0)).is_none());
        assert_eq!(fo.total_shift, DVec3::ZERO);
    }

    #[test]
    fn rebase_recenters_and_accumulates() {
        let mut fo = FloatingOrigin::new(1000.0);
        let cam = DVec3::new(5000.0, 0.0, 0.0);
        let shift = fo.rebase(cam).expect("should rebase past threshold");
        // applying the shift puts the camera back at the origin
        assert!((cam + shift).length() < 1e-9);
        assert_eq!(fo.total_shift, shift);
    }
}
