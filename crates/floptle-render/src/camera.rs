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
    /// Orthographic Scene view: the world-space height the view covers, or
    /// `None` for the ordinary perspective camera.
    ///
    /// This is the other half of [`ViewLock`]. A locked view is square to its
    /// plane, but under perspective it is still a *cone* — so a tilemap at
    /// z = 0 and one at z = -2 are drawn at different scales and the two cannot
    /// be lined up by eye. Orthographic makes the view a box, which is what
    /// "looking at a flat thing" actually means.
    pub ortho: Option<f32>,
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
            ortho: None,
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

    /// Turn the orthographic Scene view on (at `height` world units) or off.
    pub fn set_ortho(&mut self, height: Option<f32>) {
        self.ortho = height.map(|h| h.clamp(Self::ORTHO_MIN, Self::ORTHO_MAX));
    }

    pub fn is_ortho(&self) -> bool {
        self.ortho.is_some()
    }

    /// The smallest / largest orthographic Scene-view height. The floor keeps the
    /// projection matrix invertible (every picking ray goes through its inverse);
    /// the ceiling is where f32 depth stops resolving what it is drawing.
    pub const ORTHO_MIN: f32 = 0.02;
    pub const ORTHO_MAX: f32 = 100_000.0;

    /// Slide the camera in its own view plane by a screen-drag delta (pixels): the
    /// world tracks the pointer (drag right ⏵ the scene moves right). Speed scales
    /// with the fly speed so it feels consistent as you dial that up/down.
    ///
    /// Under an orthographic view it scales with the view HEIGHT instead, because
    /// that is what zoom means there: pan at a 4-unit zoom would otherwise fling
    /// the view across the map at the same pixels-per-second as at 400.
    pub fn pan(&mut self, dx: f32, dy: f32) {
        let rot = self.rotation();
        let right = rot * Vec3::X;
        let up = rot * Vec3::Y;
        let k = match self.ortho {
            // Roughly one world unit per (height / 900) pixels — the drag tracks
            // the pointer at any zoom, which a fly-speed-scaled pan cannot.
            Some(h) => h / 900.0,
            None => self.speed as f32 * 0.01,
        };
        self.position += ((right * -dx) + (up * dy)).as_dvec3() * k as f64;
    }

    /// Dolly along the view direction (mouse wheel): positive `amount` moves forward
    /// (toward what you're looking at). Steps scale with the fly speed.
    ///
    /// Under an orthographic view, moving forward changes NOTHING you can see — the
    /// view is the same height at every distance. So there the wheel changes the
    /// height instead, multiplicatively, which is the only thing "zoom" can mean.
    /// (Getting this wrong is not subtle: the wheel simply appears dead, and the
    /// view eventually slides through whatever it was looking at.)
    pub fn dolly(&mut self, amount: f32) {
        if amount == 0.0 {
            return;
        }
        if let Some(h) = self.ortho {
            // Multiplicative, so each notch is the same proportion of zoom at
            // every scale — additive steps are unusably coarse zoomed in and
            // unusably fine zoomed out.
            let next = h * 0.88f32.powf(amount);
            self.ortho = Some(next.clamp(Self::ORTHO_MIN, Self::ORTHO_MAX));
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
        // An orthographic view needs its near plane BEHIND the camera: the box
        // has no apex, so things level with the camera are in frame, and a near
        // plane at +0.1 would slice the layer you are working on in half. The
        // range comes from `ORTHO_DEPTH` rather than a literal here, because a
        // gameplay camera has to reach exactly the same answer — a Scene view
        // and a Game view that disagree about what is in the scene is the whole
        // bug this constant exists to prevent.
        let proj = match self.ortho {
            Some(height) => Projection::of_camera(0.0, true, height, 0.0, 0.0),
            None => Projection::Perspective { fov_y: 60f32.to_radians(), near: 0.1, far: 2000.0 },
        };
        RenderCamera::new(self.position, self.rotation(), proj)
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

    /// The wheel has to do SOMETHING under an orthographic view. Moving forward
    /// changes nothing you can see there, so it must change the height instead —
    /// otherwise the wheel simply appears dead.
    #[test]
    fn the_wheel_zooms_an_orthographic_view_instead_of_moving_it() {
        let mut cam = FlyCamera { position: DVec3::new(1.0, 2.0, 3.0), ..Default::default() };
        cam.set_ortho(Some(10.0));
        let where_it_was = cam.position;

        cam.dolly(1.0);
        assert_eq!(cam.position, where_it_was, "an ortho dolly must not move the camera");
        let zoomed_in = cam.ortho.unwrap();
        assert!(zoomed_in < 10.0, "scrolling forward zooms IN: {zoomed_in}");

        cam.dolly(-1.0);
        assert!(
            (cam.ortho.unwrap() - 10.0).abs() < 1e-3,
            "one notch back is where it began, got {}",
            cam.ortho.unwrap()
        );

        // Multiplicative, so a notch is the same proportion at every scale.
        let ratio_at = |h: f32| {
            let mut c = FlyCamera::default();
            c.set_ortho(Some(h));
            c.dolly(1.0);
            c.ortho.unwrap() / h
        };
        assert!((ratio_at(4.0) - ratio_at(400.0)).abs() < 1e-4, "zoom must be proportional");
    }

    /// Zoom cannot walk out of the range the projection matrix can express — a
    /// zero-height ortho is singular, and every picking ray through its inverse
    /// comes back NaN.
    #[test]
    fn zoom_stays_inside_what_the_projection_can_express() {
        let mut cam = FlyCamera::default();
        cam.set_ortho(Some(10.0));
        for _ in 0..500 {
            cam.dolly(1.0);
        }
        assert!(cam.ortho.unwrap() >= FlyCamera::ORTHO_MIN);
        for _ in 0..2000 {
            cam.dolly(-1.0);
        }
        assert!(cam.ortho.unwrap() <= FlyCamera::ORTHO_MAX);
        // …and the matrix it produces is usable, which is the thing that matters.
        let m = cam.render_camera().view_proj(16.0 / 9.0);
        assert!(m.is_finite() && m.inverse().is_finite(), "the projection must stay invertible");
    }

    /// An orthographic view's near plane sits BEHIND the eye. Otherwise the layer
    /// you are working on is sliced in half by the near plane the moment the
    /// camera is level with it — which is exactly where a 2D view sits.
    #[test]
    fn an_orthographic_view_does_not_clip_the_layer_it_is_level_with() {
        let mut cam = FlyCamera { position: DVec3::ZERO, ..Default::default() };
        cam.set_lock(ViewLock::Front);
        cam.set_ortho(Some(10.0));
        let vp = cam.render_camera().view_proj(1.0);
        // A point AT the camera plane, in camera-relative render space.
        let p = vp * floptle_core::math::Vec4::new(0.0, 0.0, 0.0, 1.0);
        assert!(p.z >= 0.0 && p.z <= 1.0, "the camera's own plane must be in frame, z = {}", p.z);
        // …and so is something a little behind it.
        let p = vp * floptle_core::math::Vec4::new(0.0, 0.0, 5.0, 1.0);
        assert!(p.z >= 0.0 && p.z <= 1.0, "5 units behind must be in frame, z = {}", p.z);
    }

    /// Panning must track the pointer at any zoom. Scaled by fly speed instead, a
    /// pan is either glacial or wild depending on how far you have zoomed.
    #[test]
    fn panning_tracks_the_pointer_at_any_zoom() {
        let pan_for = |h: f32| {
            let mut cam = FlyCamera { position: DVec3::ZERO, ..Default::default() };
            cam.set_lock(ViewLock::Front);
            cam.set_ortho(Some(h));
            cam.pan(100.0, 0.0);
            cam.position.x.abs()
        };
        // Ten times the zoom, ten times the world distance for the same drag.
        let (near, far) = (pan_for(10.0), pan_for(100.0));
        assert!((far / near - 10.0).abs() < 1e-3, "pan should scale with height: {near} vs {far}");
    }

    /// The ortho flag is orthogonal to the lock — you can have either, both, or
    /// neither, and turning one on must not disturb the other.
    #[test]
    fn ortho_and_the_plane_lock_are_independent() {
        let mut cam = FlyCamera::default();
        cam.set_ortho(Some(20.0));
        assert!(cam.is_ortho() && !cam.lock.is_locked());
        cam.set_lock(ViewLock::Front);
        assert!(cam.is_ortho(), "locking must not clear the projection");
        assert_eq!(cam.ortho, Some(20.0));
        cam.set_ortho(None);
        assert!(!cam.is_ortho() && cam.lock == ViewLock::Front, "and the reverse");
        assert!(!cam.render_camera().projection.is_ortho());
    }
}
