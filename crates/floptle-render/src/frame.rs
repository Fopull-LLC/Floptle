//! The camera and the **camera-relative** view/projection — the render side of
//! large-world space (ADR-0015).
//!
//! The camera holds an `f64` world position, but it is treated as the **render-
//! space origin**: every object is uploaded at `world - camera_world` (see
//! `floptle_core::Transform::render_matrix`), so the GPU only ever sees small
//! coordinates and never jitters. Consequently the *view* matrix carries **no
//! translation** — it's purely the inverse camera rotation. That asymmetry (world
//! is `f64`, the GPU sees `f32` residuals) is the whole large-world trick, and it
//! lives at this seam. Projection/view math is real here; binding it to GPU
//! uniforms lands with `device` + `graph`.

use floptle_core::math::{DVec3, Mat4, Quat};

/// How the camera maps view space to clip space. Depth is wgpu/Metal/DX
/// convention (`0..1`), so we build right-handed `*_rh` (not the GL `-1..1`).
#[derive(Debug, Clone, Copy)]
pub enum Projection {
    Perspective { fov_y: f32, near: f32, far: f32 },
    Orthographic { height: f32, near: f32, far: f32 },
}

impl Projection {
    pub fn matrix(&self, aspect: f32) -> Mat4 {
        match *self {
            Projection::Perspective { fov_y, near, far } => {
                Mat4::perspective_rh(fov_y, aspect, near, far)
            }
            Projection::Orthographic { height, near, far } => {
                let w = height * aspect;
                Mat4::orthographic_rh(-w * 0.5, w * 0.5, -height * 0.5, height * 0.5, near, far)
            }
        }
    }
}

/// The active camera. `world_position` is full-precision; the render path keeps it
/// at the origin and offsets the world to it.
#[derive(Debug, Clone, Copy)]
pub struct RenderCamera {
    pub world_position: DVec3,
    pub rotation: Quat,
    pub projection: Projection,
}

impl RenderCamera {
    pub fn new(world_position: DVec3, rotation: Quat, projection: Projection) -> Self {
        Self { world_position, rotation, projection }
    }

    /// View matrix in **camera-relative** render space: no translation (the camera
    /// is the origin), just the inverse of the camera's orientation.
    pub fn view_matrix(&self) -> Mat4 {
        Mat4::from_quat(self.rotation.conjugate())
    }

    pub fn proj_matrix(&self, aspect: f32) -> Mat4 {
        self.projection.matrix(aspect)
    }

    /// The combined view-projection an object's `render_matrix(world_position)`
    /// feeds into. Upload this per frame; objects upload their camera-relative model.
    pub fn view_proj(&self, aspect: f32) -> Mat4 {
        self.proj_matrix(aspect) * self.view_matrix()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_has_no_translation() {
        // Even a camera a million units out contributes no translation to the
        // view matrix — the world is offset to it instead (large-world).
        let cam = RenderCamera::new(
            DVec3::new(1.0e6, 0.0, 0.0),
            Quat::IDENTITY,
            Projection::Perspective { fov_y: 1.0, near: 0.1, far: 1000.0 },
        );
        let v = cam.view_matrix();
        assert_eq!(v.w_axis.truncate(), glam::Vec3::ZERO);
    }
}
