//! Spatialization: distance attenuation curves + directional stereo panning.
//!
//! Positions are f64 (`DVec3`) because the engine is large-world; everything
//! is computed listener-relative before dropping to f32, so sounds stay
//! correct a million units from the origin.

use glam::DVec3;

use crate::source::{Falloff, SpatialMode};

/// Where the ears are. Fed from the active camera each frame during Play.
#[derive(Debug, Clone, Copy)]
pub struct Listener {
    pub position: DVec3,
    /// Unit forward vector.
    pub forward: DVec3,
    /// Unit right vector (for left/right panning).
    pub right: DVec3,
}

impl Default for Listener {
    fn default() -> Self {
        Self { position: DVec3::ZERO, forward: DVec3::NEG_Z, right: DVec3::X }
    }
}

/// Distance gain in 0..1 for the given curve.
pub fn distance_gain(falloff: Falloff, dist: f32, min_d: f32, max_d: f32) -> f32 {
    let min_d = min_d.max(0.01);
    let max_d = max_d.max(min_d + 0.01);
    if dist <= min_d {
        return 1.0;
    }
    if dist >= max_d {
        return 0.0;
    }
    // All curves get a smooth fade to true zero over the last 20% of range —
    // inverse/exponential never reach silence on their own, and an audible
    // step at max_distance is exactly the artifact people notice.
    let edge0 = min_d + (max_d - min_d) * 0.8;
    let fade = if dist > edge0 {
        let t = (dist - edge0) / (max_d - edge0);
        1.0 - t * t * (3.0 - 2.0 * t)
    } else {
        1.0
    };
    let base = match falloff {
        Falloff::Linear => 1.0 - (dist - min_d) / (max_d - min_d),
        Falloff::Inverse => min_d / dist,
        Falloff::Exponential => (min_d / dist) * (min_d / dist),
    };
    (base * fade).clamp(0.0, 1.0)
}

/// Result of spatializing one emitter against the listener.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spatialized {
    /// Distance gain 0..1.
    pub gain: f32,
    /// Stereo pan -1 (left) .. 1 (right).
    pub pan: f32,
}

/// Compute gain + pan for an emitter. `Flat` ignores positions; `Distance`
/// attenuates without panning; `Spatial` does both.
pub fn spatialize(
    mode: SpatialMode,
    falloff: Falloff,
    min_d: f32,
    max_d: f32,
    manual_pan: f32,
    emitter: DVec3,
    listener: &Listener,
) -> Spatialized {
    if mode == SpatialMode::Flat {
        return Spatialized { gain: 1.0, pan: manual_pan.clamp(-1.0, 1.0) };
    }
    let rel = emitter - listener.position;
    let dist = rel.length() as f32;
    let gain = distance_gain(falloff, dist, min_d, max_d);
    if mode == SpatialMode::Distance {
        return Spatialized { gain, pan: manual_pan.clamp(-1.0, 1.0) };
    }
    // Horizontal-plane azimuth pan. Inside min_distance the image collapses
    // toward center — a sound you're standing on shouldn't hard-flip ears as
    // you spin.
    let pan = if dist > 1e-4 {
        let side = (rel.dot(listener.right) / dist as f64) as f32;
        let proximity = (dist / min_d.max(0.01)).clamp(0.0, 1.0);
        (side * proximity).clamp(-1.0, 1.0)
    } else {
        0.0
    };
    Spatialized { gain, pan }
}

/// Equal-power pan gains for a pan in -1..1: unity at center, each side fades
/// with a cosine so total energy stays constant.
#[inline]
pub fn pan_gains(pan: f32) -> (f32, f32) {
    let p = pan.clamp(-1.0, 1.0);
    // Map to 0..pi/2 and normalize so center = (1, 1).
    let theta = (p + 1.0) * std::f32::consts::FRAC_PI_4;
    let norm = std::f32::consts::SQRT_2;
    (theta.cos() * norm, theta.sin() * norm)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn falloff_endpoints() {
        for f in [Falloff::Linear, Falloff::Inverse, Falloff::Exponential] {
            assert_eq!(distance_gain(f, 1.0, 2.0, 50.0), 1.0, "{f:?} inside min");
            assert_eq!(distance_gain(f, 60.0, 2.0, 50.0), 0.0, "{f:?} past max");
            let mid = distance_gain(f, 20.0, 2.0, 50.0);
            assert!(mid > 0.0 && mid < 1.0, "{f:?} mid-range {mid}");
            // Monotonic decrease.
            let nearer = distance_gain(f, 10.0, 2.0, 50.0);
            assert!(nearer > mid, "{f:?} not monotonic");
        }
    }

    #[test]
    fn spatial_pans_toward_source() {
        let listener = Listener::default(); // facing -Z, right = +X
        let s = spatialize(
            SpatialMode::Spatial,
            Falloff::Inverse,
            1.0,
            50.0,
            0.0,
            DVec3::new(10.0, 0.0, 0.0),
            &listener,
        );
        assert!(s.pan > 0.8, "source to the right should pan right, got {}", s.pan);
        let c = spatialize(
            SpatialMode::Spatial,
            Falloff::Inverse,
            1.0,
            50.0,
            0.0,
            DVec3::new(0.0, 0.0, -10.0),
            &listener,
        );
        assert!(c.pan.abs() < 0.01, "source dead ahead should be centered");
    }

    #[test]
    fn pan_gains_center_is_unity() {
        let (l, r) = pan_gains(0.0);
        assert!((l - 1.0).abs() < 1e-3 && (r - 1.0).abs() < 1e-3);
        let (l, r) = pan_gains(1.0);
        assert!(l < 0.01 && r > 1.3, "hard right should silence left");
    }
}
