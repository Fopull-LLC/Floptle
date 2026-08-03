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

/// Which plane the Scene view is locked to, if any.
///
/// Building a 2D game in a 3D editor means fighting the camera: every drag
/// nudges you a little off-axis until "flat" is a thing you keep re-achieving
/// rather than a thing you have. A locked view is square to its plane and STAYS
/// square — mouse-look does nothing, and moving slides you around the plane
/// instead of through it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ViewLock {
    /// Ordinary free-fly.
    #[default]
    Free,
    /// Looking down −Z at the XY plane. The 2D default: X is right, Y is up,
    /// which is how a side-on game is drawn and how sprites are authored.
    Front,
    /// Looking down −X at the ZY plane.
    Side,
    /// Looking down −Y at the XZ plane — the map/blockout view.
    Top,
}

impl ViewLock {
    /// The yaw/pitch this lock holds the camera at.
    fn angles(self) -> (f32, f32) {
        use std::f32::consts::FRAC_PI_2;
        match self {
            // EXACTLY straight down, not the near-miss the free-look clamp
            // uses. A locked view never calls `look`, so there is no gimbal to
            // dodge — and a fraction of a degree off square is the difference
            // between W sliding across the map and W slowly sinking into it.
            ViewLock::Free => (0.0, 0.0),
            ViewLock::Front => (0.0, 0.0),
            ViewLock::Side => (FRAC_PI_2, 0.0),
            ViewLock::Top => (0.0, -FRAC_PI_2),
        }
    }

    pub fn is_locked(self) -> bool {
        self != ViewLock::Free
    }

    /// What to show in a menu.
    pub fn label(self) -> &'static str {
        match self {
            ViewLock::Free => "Free",
            ViewLock::Front => "Front (XY)",
            ViewLock::Side => "Side (ZY)",
            ViewLock::Top => "Top (XZ)",
        }
    }

    pub const ALL: [ViewLock; 4] =
        [ViewLock::Free, ViewLock::Front, ViewLock::Side, ViewLock::Top];
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
    /// Plane lock (Scene view). [`ViewLock::Free`] is the ordinary camera.
    pub lock: ViewLock,
}

impl Default for FlyCamera {
    fn default() -> Self {
        Self {
            position: DVec3::new(0.0, 0.8, 7.0),
            yaw: 0.0,
            pitch: 0.0,
            speed: 4.0,
            sensitivity: 0.0026,
            lock: ViewLock::Free,
        }
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
    ///
    /// A locked view ignores this entirely — that is the point of locking it.
    /// Panning and dollying still work, so you can still get around.
    pub fn look(&mut self, dx: f32, dy: f32) {
        if self.lock.is_locked() {
            return;
        }
        self.yaw -= dx * self.sensitivity;
        let limit = std::f32::consts::FRAC_PI_2 - 0.04;
        self.pitch = (self.pitch - dy * self.sensitivity).clamp(-limit, limit);
    }

    /// Lock the view to a plane (or unlock it), snapping square immediately.
    ///
    /// Position is left alone: locking is a change of heading, not a teleport,
    /// so whatever you were looking at stays roughly in front of you.
    pub fn set_lock(&mut self, lock: ViewLock) {
        self.lock = lock;
        if lock.is_locked() {
            let (yaw, pitch) = lock.angles();
            self.yaw = yaw;
            self.pitch = pitch;
        }
    }

    /// Slide the camera in its own view plane by a screen-drag delta (pixels): the
    /// world tracks the pointer (drag right ⏵ the scene moves right). Speed scales
    /// with the fly speed so it feels consistent as you dial that up/down.
    pub fn pan(&mut self, dx: f32, dy: f32) {
        let rot = self.rotation();
        let right = rot * Vec3::X;
        let up = rot * Vec3::Y;
        let k = self.speed as f32 * 0.01;
        self.position += ((right * -dx) + (up * dy)).as_dvec3() * k as f64;
    }

    /// Dolly along the view direction (mouse wheel): positive `amount` moves forward
    /// (toward what you're looking at). Steps scale with the fly speed.
    pub fn dolly(&mut self, amount: f32) {
        if amount == 0.0 {
            return;
        }
        let forward = self.rotation() * Vec3::NEG_Z;
        self.position += forward.as_dvec3() * (amount * self.speed as f32 * 0.5) as f64;
    }

    /// Integrate movement for `dt` seconds from the held keys.
    pub fn update(&mut self, input: &Input, dt: f32) {
        let rot = self.rotation();
        // Locked: W/S slide UP and DOWN the plane you are looking at rather than
        // flying into it. On a Top view, forward is straight down — pressing W
        // would otherwise bury the camera in the floor, which is the single most
        // annoying thing about using a 3D fly camera as a 2D one.
        let forward = if self.lock.is_locked() { rot * Vec3::Y } else { rot * Vec3::NEG_Z };
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
        // Space/Ctrl keep their world-up meaning when free. Locked, they are the
        // only way to move ALONG the view axis (step a 2D layer forward/back),
        // which is occasionally exactly what you want.
        let axis = if self.lock.is_locked() { rot * Vec3::NEG_Z } else { Vec3::Y };
        if input.up {
            dir += axis;
        }
        if input.down {
            dir -= axis;
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

    /// A locked view stays square. The whole value of the lock is that it does
    /// not drift — one stray drag putting you 2° off-axis is exactly the thing
    /// that makes 2D work in a 3D editor miserable.
    #[test]
    fn a_locked_view_ignores_mouse_look() {
        let mut cam = FlyCamera::default();
        cam.set_lock(ViewLock::Front);
        let (yaw, pitch) = (cam.yaw, cam.pitch);
        cam.look(120.0, -80.0);
        cam.look(-40.0, 25.0);
        assert_eq!((cam.yaw, cam.pitch), (yaw, pitch), "no drag moves a locked view");

        // Unlocking hands the camera straight back.
        cam.set_lock(ViewLock::Free);
        cam.look(120.0, 0.0);
        assert!(cam.yaw != yaw, "a free view still looks around");
    }

    /// Locking is a change of heading, not a teleport: whatever you were looking
    /// at should still be roughly in front of you afterwards.
    #[test]
    fn locking_squares_the_view_without_moving_it() {
        let mut cam = FlyCamera { position: DVec3::new(12.0, 3.0, -7.0), ..Default::default() };
        cam.look(300.0, 120.0);
        let where_it_was = cam.position;
        cam.set_lock(ViewLock::Top);
        assert_eq!(cam.position, where_it_was);
        // Square to the XZ plane: looking straight down.
        let fwd = cam.rotation() * Vec3::NEG_Z;
        assert!(fwd.y < -0.999, "Top looks down, not nearly-down: {fwd:?}");
    }

    /// The one that makes a Top view usable: W must slide you across the map,
    /// not bury the camera in the floor it is pointing at.
    #[test]
    fn moving_in_a_top_view_slides_across_the_plane() {
        let mut cam = FlyCamera { position: DVec3::ZERO, ..Default::default() };
        cam.set_lock(ViewLock::Top);
        let input = Input { forward: true, ..Default::default() };
        cam.update(&input, 1.0);
        assert!(cam.position.y.abs() < 1e-3, "W must not dive: y = {}", cam.position.y);
        assert!(cam.position.z.abs() > 0.5, "W moves across the plane: {:?}", cam.position);

        // Space/Ctrl are the deliberate way THROUGH the plane (step a layer).
        let mut cam = FlyCamera { position: DVec3::ZERO, ..Default::default() };
        cam.set_lock(ViewLock::Top);
        cam.update(&Input { up: true, ..Default::default() }, 1.0);
        assert!(cam.position.y < -0.5, "Space steps along the view axis: {:?}", cam.position);
    }

    /// A Front lock is the 2D one: X right, Y up, looking down −Z.
    #[test]
    fn a_front_lock_is_the_2d_view() {
        let mut cam = FlyCamera { position: DVec3::ZERO, ..Default::default() };
        cam.set_lock(ViewLock::Front);
        let fwd = cam.rotation() * Vec3::NEG_Z;
        assert!(fwd.z < -0.999, "looking down −Z: {fwd:?}");
        // D slides +X, W slides +Y — screen-right and screen-up.
        cam.update(&Input { right: true, ..Default::default() }, 1.0);
        assert!(cam.position.x > 0.5 && cam.position.y.abs() < 1e-3, "{:?}", cam.position);
        let mut cam = FlyCamera { position: DVec3::ZERO, ..Default::default() };
        cam.set_lock(ViewLock::Front);
        cam.update(&Input { forward: true, ..Default::default() }, 1.0);
        assert!(cam.position.y > 0.5 && cam.position.x.abs() < 1e-3, "{:?}", cam.position);
    }
}
