//! Thin re-exports over `glam` so the rest of the engine speaks one math
//! vocabulary, plus a few helpers. The split that matters for large-world space
//! (ADR-0015): **world translations are `f64` (`DVec3`)**, while rotation, scale,
//! and everything uploaded to the GPU are `f32` (GPUs are `f32`-native).

pub use glam::{
    Affine3A, DMat4, DQuat, DVec2, DVec3, DVec4, EulerRot, Mat3, Mat4, Quat, Vec2, Vec3, Vec3A,
    Vec4,
};

/// Linear interpolation `a + (b - a) * t`.
#[inline]
pub fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Frame-rate-independent exponential smoothing factor for `lerp(current, target, k)`.
/// `rate` is the approach speed in 1/seconds; larger = snappier. Stable for any `dt`.
#[inline]
pub fn smoothing(rate: f32, dt: f32) -> f32 {
    1.0 - (-rate * dt).exp()
}

/// Map `x` from `[in_min, in_max]` onto `[out_min, out_max]` (no clamping).
#[inline]
pub fn remap(x: f32, in_min: f32, in_max: f32, out_min: f32, out_max: f32) -> f32 {
    out_min + (x - in_min) / (in_max - in_min) * (out_max - out_min)
}
