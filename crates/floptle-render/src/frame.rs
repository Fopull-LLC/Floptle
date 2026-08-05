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

/// Half the depth range an orthographic camera spans, in world units: its box
/// runs from `-ORTHO_DEPTH` to `+ORTHO_DEPTH` about the eye.
///
/// **An orthographic near plane belongs BEHIND the camera.** The projection does
/// not divide by `w`, so a negative near is ordinary rather than degenerate —
/// and it is what a flat game needs, because a flat game puts its art on one
/// plane and its camera on that plane. A near plane in front of the eye slices
/// that layer away entirely, which reads as "my tilemap does not render" in
/// whichever view happens not to be pulled back.
///
/// Symmetric about the eye, so two views of the same scene cannot disagree about
/// what is in it. 10,000 each way is far more room than a flat game uses and
/// still leaves a 24-bit depth buffer about a millimetre of resolution; deriving
/// the range from a perspective camera's `far` (300 km) would spend all of that
/// precision on emptiness.
pub const ORTHO_DEPTH: f32 = 10_000.0;

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

    /// The projection a `Matter::Camera` node describes.
    ///
    /// The ONE place a camera component becomes a matrix, called by the editor's
    /// Scene view, its Game view, each render target and the runtime. Four
    /// copies of `if ortho { … } else { … }` is how a game ends up orthographic
    /// in Play and perspective in a build — or, worse, orthographic on the
    /// screen and perspective in the minimap, where nobody thinks to look.
    ///
    /// `near` and `far` are the **perspective** planes, and the orthographic
    /// case deliberately ignores them for [`ORTHO_DEPTH`]. Centralising the
    /// `if ortho` was not enough on its own: every caller still passed the same
    /// `0.05` near plane, which is correct for perspective and clips a flat
    /// game's whole world away. A depth range that has one right answer should
    /// not be asked of four callers.
    pub fn of_camera(fov_y: f32, ortho: bool, ortho_height: f32, near: f32, far: f32) -> Projection {
        if ortho {
            Projection::Orthographic {
                height: ortho_height.max(1e-3),
                near: -ORTHO_DEPTH,
                far: ORTHO_DEPTH,
            }
        } else {
            Projection::Perspective { fov_y, near, far }
        }
    }

    /// Whether this is the orthographic case — for the frustum gizmo, which draws
    /// a box rather than a pyramid.
    pub fn is_ortho(&self) -> bool {
        matches!(self, Projection::Orthographic { .. })
    }

    /// The view's height in world units at `distance` from the camera.
    ///
    /// The number a 2D game actually wants: it is what `camera.pixelsPerUnit`
    /// divides by, and under an orthographic projection it is constant — which is
    /// the whole reason to use one.
    pub fn height_at(&self, distance: f32) -> f32 {
        match *self {
            Projection::Perspective { fov_y, .. } => 2.0 * distance * (fov_y * 0.5).tan(),
            Projection::Orthographic { height, .. } => height,
        }
    }

    /// The vertical field of view, radians.
    ///
    /// An orthographic camera has none — its view is the same height at every
    /// distance — so it reports the angle that covers its height one unit away.
    /// That keeps `camera.pixelsPerUnit` answering something true at the plane
    /// a flat game is actually built on, rather than zero.
    pub fn fov_y(&self) -> f32 {
        match *self {
            Projection::Perspective { fov_y, .. } => fov_y,
            Projection::Orthographic { height, .. } => 2.0 * (height * 0.5).atan(),
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

    /// Whether a point in camera-relative render space survives clipping — the
    /// question "does this draw", asked of the matrix rather than of a screenshot.
    fn visible(p: Projection, at: glam::Vec3) -> bool {
        let cam = RenderCamera::new(DVec3::ZERO, Quat::IDENTITY, p);
        let c = cam.view_proj(16.0 / 9.0) * glam::Vec4::new(at.x, at.y, at.z, 1.0);
        // wgpu clip volume: -w <= x,y <= w and 0 <= z <= w.
        c.w > 0.0
            && c.x.abs() <= c.w
            && c.y.abs() <= c.w
            && (0.0..=c.w).contains(&c.z)
    }

    /// The bug this constant exists for: a flat game puts its art on one plane
    /// and its camera on that plane, and a near plane in front of the eye throws
    /// the whole world away. Fails on a `near` of 0.05.
    #[test]
    fn an_orthographic_camera_sees_what_is_level_with_it() {
        // Exactly the shape of a 2D scene: an ortho camera at the origin and a
        // tilemap in the XY plane at the origin.
        let p = Projection::of_camera(1.05, true, 9.5, 0.05, 300_000.0);
        // What the gameplay camera used to build, kept so this stays a
        // regression test and not a description of the current code.
        let was = Projection::Orthographic { height: 9.5, near: 0.05, far: 300_000.0 };
        assert!(!visible(was, glam::Vec3::ZERO), "the bug: the map was clipped by its own camera");
        assert!(visible(p, glam::Vec3::ZERO), "the plane the camera sits in must be in frame");
        assert!(visible(p, glam::Vec3::new(2.0, -3.0, 0.0)), "and so must the rest of it");
        // Still bounded: the box has to end somewhere in both directions.
        assert!(!visible(p, glam::Vec3::new(0.0, 0.0, -ORTHO_DEPTH * 2.0)));
        assert!(!visible(p, glam::Vec3::new(0.0, 0.0, ORTHO_DEPTH * 2.0)));
        // And it is still a box, not a cone: the frame is the same height at
        // every depth, which is the whole reason to pick orthographic.
        assert!(visible(p, glam::Vec3::new(0.0, 4.0, -500.0)));
        assert!(!visible(p, glam::Vec3::new(0.0, 6.0, -500.0)));
    }

    /// A perspective camera keeps its near plane in FRONT of the eye — moving it
    /// behind would wreck depth precision for every 3D game.
    #[test]
    fn a_perspective_camera_keeps_its_near_plane_where_it_was() {
        let p = Projection::of_camera(1.05, false, 9.5, 0.05, 300_000.0);
        match p {
            Projection::Perspective { near, far, .. } => {
                assert_eq!((near, far), (0.05, 300_000.0));
            }
            Projection::Orthographic { .. } => panic!("not orthographic"),
        }
        assert!(!visible(p, glam::Vec3::ZERO), "nothing sits at a perspective eye");
        assert!(visible(p, glam::Vec3::new(0.0, 0.0, -10.0)));
    }

    /// The editor's own camera and a `Matter::Camera` node must land on the same
    /// projection, or the Scene view and the Game view show different scenes.
    #[test]
    fn the_scene_view_and_a_camera_node_agree_about_depth() {
        let node = Projection::of_camera(1.05, true, 9.5, 0.05, 300_000.0);
        let editor = Projection::Orthographic { height: 9.5, near: -ORTHO_DEPTH, far: ORTHO_DEPTH };
        assert_eq!(node.matrix(1.6), editor.matrix(1.6));
    }
}
