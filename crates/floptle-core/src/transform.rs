//! World transform — large-world-safe by default (ADR-0015).
//!
//! The translation is `f64` (`DVec3`) so a node twelve light-years out is still
//! placed to sub-millimeter precision; rotation and scale stay `f32` (they don't
//! suffer the magnitude problem, and GPUs are `f32`). The developer always reads/
//! writes ordinary world space — `transform.translation` — and the engine derives
//! a **camera-relative `f32` matrix** at upload time so the GPU never sees a large
//! coordinate. That derivation is the whole trick; it lives here.

use crate::math::{DVec3, Mat4, Quat, Vec3};

/// A node's placement in the world. SRT (scale, then rotate, then translate).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform {
    /// World position, **double precision** (large-world-safe).
    pub translation: DVec3,
    pub rotation: Quat,
    pub scale: Vec3,
}

impl Default for Transform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Transform {
    pub const IDENTITY: Self =
        Self { translation: DVec3::ZERO, rotation: Quat::IDENTITY, scale: Vec3::ONE };

    pub fn from_translation(translation: DVec3) -> Self {
        Self { translation, ..Self::IDENTITY }
    }

    /// The full-precision model matrix in absolute world space. Useful for
    /// `f64` math (parenting, physics); **not** for direct GPU upload far from
    /// the origin — use [`Self::render_matrix`] for that.
    pub fn world_matrix(&self) -> glam::DMat4 {
        glam::DMat4::from_scale_rotation_translation(
            self.scale.as_dvec3(),
            self.rotation.as_dquat(),
            self.translation,
        )
    }

    /// **Camera-relative** model matrix for GPU upload: the translation is taken
    /// relative to `camera_world` in `f64`, and only the small residual is cast
    /// to `f32`. So vertices near the camera keep full precision no matter how far
    /// the absolute coordinates are — no jitter, no developer effort (ADR-0015).
    pub fn render_matrix(&self, camera_world: DVec3) -> Mat4 {
        let rel = (self.translation - camera_world).as_vec3();
        Mat4::from_scale_rotation_translation(self.scale, self.rotation, rel)
    }

    /// Decompose a full model matrix back into a TRS transform (the inverse of
    /// [`Self::world_matrix`]). Any shear (from non-uniform scale composed with
    /// rotation up a chain) is lost — TRS can't represent it; fine for the
    /// uniform-scale bones an attachment rides (see `BoneAttach`).
    pub fn from_matrix(m: glam::DMat4) -> Transform {
        let (scale, rotation, translation) = m.to_scale_rotation_translation();
        Transform { translation, rotation: rotation.as_quat(), scale: scale.as_vec3() }
    }

    /// Compose `self` onto a parent transform (child-in-parent -> child-in-world).
    pub fn mul_transform(&self, child: &Transform) -> Transform {
        Transform {
            translation: self.translation + self.rotation.as_dquat() * (self.scale.as_dvec3() * child.translation),
            rotation: self.rotation * child.rotation,
            scale: self.scale * child.scale,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camera_relative_kills_large_coords() {
        // A node 1e8 units out would jitter as raw f32; camera-relative keeps the
        // residual tiny and exact.
        let far = DVec3::new(1.0e8, 0.0, 0.0);
        let t = Transform::from_translation(far + DVec3::new(0.5, 0.0, 0.0));
        let m = t.render_matrix(far); // camera sits at `far`
        let col = m.w_axis; // translation column
        assert!((col.x - 0.5).abs() < 1e-4, "residual should be ~0.5, got {}", col.x);
    }

    #[test]
    fn identity_renders_at_camera_offset() {
        let t = Transform::IDENTITY;
        let m = t.render_matrix(DVec3::ZERO);
        assert_eq!(m, Mat4::IDENTITY);
    }
}
