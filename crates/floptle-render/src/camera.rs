//! A free-fly debug camera and the raw input it reads — the shared way the runtime
//! and editor move through the world and look at the scene.
//!
//! The camera holds an `f64` world position (large-world-safe, ADR-0015) plus
//! yaw/pitch in `f32`. WASD moves on the camera's own axes, Space/Ctrl go
//! world-up/down, and holding the right mouse button enables mouse-look. It hands
//! the renderer a [`RenderCamera`]; the world is offset to *it*, never the reverse.

use floptle_core::math::{DVec3, Quat, Vec3};

use crate::{Projection, RenderCamera};

/// Input snapshot: which movement keys are held this frame and whether mouse-look
/// is active. The host writes it from winit events.
#[derive(Default)]
pub struct Input {
    pub forward: bool,
    pub back: bool,
    pub left: bool,
    pub right: bool,
    pub up: bool,
    pub down: bool,
    pub boost: bool,
    /// Right mouse button held — mouse motion steers the camera while true.
    pub looking: bool,
}

/// A WASD + mouse-look fly camera.
pub struct FlyCamera {
    pub position: DVec3,
    /// Yaw about world-up (radians); positive turns left.
    pub yaw: f32,
    /// Pitch about the camera's right axis (radians); clamped near ±90°.
    pub pitch: f32,
    pub speed: f64,
    pub sensitivity: f32,
}

impl Default for FlyCamera {
    fn default() -> Self {
        Self { position: DVec3::new(0.0, 0.8, 7.0), yaw: 0.0, pitch: 0.0, speed: 4.0, sensitivity: 0.0026 }
    }
}

impl FlyCamera {
    /// Orientation as a quaternion: yaw about world-Y, then pitch about local-X.
    pub fn rotation(&self) -> Quat {
        Quat::from_rotation_y(self.yaw) * Quat::from_rotation_x(self.pitch)
    }

    /// Frame `target`: keep the current view direction but reposition so the target
    /// sits `distance` straight ahead (centered in view).
    pub fn focus(&mut self, target: DVec3, distance: f64) {
        let forward = (self.rotation() * Vec3::NEG_Z).as_dvec3();
        self.position = target - forward * distance;
    }

    /// Apply a mouse-motion delta (pixels) to yaw/pitch.
    pub fn look(&mut self, dx: f32, dy: f32) {
        self.yaw -= dx * self.sensitivity;
        let limit = std::f32::consts::FRAC_PI_2 - 0.04;
        self.pitch = (self.pitch - dy * self.sensitivity).clamp(-limit, limit);
    }

    /// Integrate movement for `dt` seconds from the held keys.
    pub fn update(&mut self, input: &Input, dt: f32) {
        let rot = self.rotation();
        let forward = rot * Vec3::NEG_Z;
        let right = rot * Vec3::X;
        let mut dir = Vec3::ZERO;
        if input.forward {
            dir += forward;
        }
        if input.back {
            dir -= forward;
        }
        if input.right {
            dir += right;
        }
        if input.left {
            dir -= right;
        }
        if input.up {
            dir += Vec3::Y;
        }
        if input.down {
            dir -= Vec3::Y;
        }
        if dir.length_squared() > 0.0 {
            let speed = if input.boost { self.speed * 4.0 } else { self.speed };
            self.position += dir.normalize().as_dvec3() * speed * dt as f64;
        }
    }

    /// The renderer-facing camera for this frame.
    pub fn render_camera(&self) -> RenderCamera {
        RenderCamera::new(
            self.position,
            self.rotation(),
            Projection::Perspective { fov_y: 60f32.to_radians(), near: 0.1, far: 2000.0 },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_looks_down_neg_z() {
        let cam = FlyCamera::default();
        let fwd = cam.rotation() * Vec3::NEG_Z;
        assert!((fwd - Vec3::NEG_Z).length() < 1e-5, "forward should be -Z, got {fwd:?}");
    }

    #[test]
    fn forward_moves_toward_target() {
        let mut cam = FlyCamera::default();
        let start = cam.position;
        let input = Input { forward: true, ..Default::default() };
        cam.update(&input, 1.0);
        assert!((cam.position.z - (start.z - cam.speed)).abs() < 1e-9);
        assert!((cam.position.x - start.x).abs() < 1e-9);
    }

    #[test]
    fn pitch_clamps_at_the_poles() {
        let mut cam = FlyCamera::default();
        cam.look(0.0, 1.0e6);
        assert!(cam.pitch >= -std::f32::consts::FRAC_PI_2);
        assert!(cam.pitch < -1.5);
    }
}
