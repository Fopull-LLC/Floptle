//! Spatialization: distance attenuation curves + directional stereo panning.
//!
//! Positions are f64 (`DVec3`) because the engine is large-world; everything
//! is computed listener-relative before dropping to f32, so sounds stay
//! correct a million units from the origin.

use glam::DVec3;

use crate::source::{Falloff, LowpassFalloff, SpatialMode};

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

/// The top of the band a distance low-pass starts from: air absorption's
/// cutoff right at a sound's `min_distance`.
pub const AIR_OPEN_HZ: f32 = 20_000.0;

/// The low-pass cutoff, in Hz, that distance puts on a sound; 0 = none.
///
/// `air` is air absorption: past `min_d` the cutoff falls as
/// `AIR_OPEN_HZ / (1 + air × metres)`. `curve` closes from its `near` cutoff
/// at `min_d` to its `far` one at `max_d`, evenly in pitch (each metre takes
/// off the same number of octaves), which is how the ear hears a sound dull.
/// With both, the lower cutoff wins.
pub fn distance_cutoff(air: f32, curve: Option<LowpassFalloff>, dist: f32, min_d: f32, max_d: f32) -> f32 {
    let min_d = min_d.max(0.01);
    let max_d = max_d.max(min_d + 0.01);
    let mut hz = 0.0f32;
    let beyond = (dist - min_d).max(0.0);
    if air > 0.0 && beyond > 0.0 {
        hz = AIR_OPEN_HZ / (1.0 + air * beyond);
    }
    if let Some(LowpassFalloff { near, far }) = curve
        && near > 0.0
        && far > 0.0
    {
        let t = (beyond / (max_d - min_d)).clamp(0.0, 1.0);
        let c = near * (far / near).powf(t);
        if hz == 0.0 || c < hz {
            hz = c;
        }
    }
    hz
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

    /// **Air absorption closes with distance and is open up close.** No
    /// cutoff inside `minDistance`; past it the cutoff falls as the formula
    /// says, monotonically, and a larger `k` dulls faster.
    #[test]
    fn air_absorption_closes_with_distance() {
        assert_eq!(distance_cutoff(0.1, None, 1.5, 2.0, 50.0), 0.0, "inside minDistance");
        assert_eq!(distance_cutoff(0.0, None, 40.0, 2.0, 50.0), 0.0, "k = 0 is off");
        let at = |k: f32, d: f32| distance_cutoff(k, None, d, 2.0, 50.0);
        assert!((at(0.1, 12.0) - AIR_OPEN_HZ / 2.0).abs() < 1.0, "10 m past min at k=0.1 halves it: {}", at(0.1, 12.0));
        assert!(at(0.1, 30.0) < at(0.1, 12.0) && at(0.1, 60.0) < at(0.1, 30.0), "not monotonic");
        assert!(at(0.5, 12.0) < at(0.1, 12.0), "a larger k dulls faster");
    }

    /// **A low-pass falloff runs from `near` to `far` across the sound's
    /// range, evenly in pitch, and the lower of two cutoffs wins.**
    #[test]
    fn a_lowpass_falloff_runs_near_to_far_in_octaves() {
        let curve = Some(LowpassFalloff { near: 8000.0, far: 500.0 });
        let at = |d: f32| distance_cutoff(0.0, curve, d, 2.0, 50.0);
        assert_eq!(at(1.0), 8000.0);
        assert_eq!(at(2.0), 8000.0);
        assert!((at(50.0) - 500.0).abs() < 0.5 && (at(90.0) - 500.0).abs() < 0.5, "{} {}", at(50.0), at(90.0));
        // Halfway along the range is halfway in octaves: the geometric mean.
        assert!((at(26.0) - 2000.0).abs() < 1.0, "halfway is {}", at(26.0));
        let both = distance_cutoff(0.5, curve, 26.0, 2.0, 50.0);
        assert!((both - AIR_OPEN_HZ / 13.0).abs() < 1.0, "the lower cutoff (air, {both}) wins");
        assert_eq!(distance_cutoff(0.0, Some(LowpassFalloff { near: 0.0, far: 500.0 }), 26.0, 2.0, 50.0), 0.0);
    }

    #[test]
    fn pan_gains_center_is_unity() {
        let (l, r) = pan_gains(0.0);
        assert!((l - 1.0).abs() < 1e-3 && (r - 1.0).abs() < 1e-3);
        let (l, r) = pan_gains(1.0);
        assert!(l < 0.01 && r > 1.3, "hard right should silence left");
    }
}
