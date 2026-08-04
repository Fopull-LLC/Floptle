//! Frustum culling: what is on screen, and how big a thing is (`floptle/0075`).
//!
//! Until this existed, culling lived in exactly one place — terrain chunks — and
//! every other node in the scene became an instance every frame whether or not it
//! was on screen. Roughly half of any scene is behind the camera. Scatter fields
//! were culled by *distance* but never by *direction*, so a full field submitted
//! its whole disc, including the part behind you.
//!
//! # The one thing that can go wrong
//!
//! A bounding radius that is too SMALL pops geometry out at the screen edge,
//! which is far more visible than the cost being saved. So every radius here is
//! derived from measured bounds and rounded **up**, and the source of each bound
//! is written down:
//!
//! * [`radius_from_longest_edge`] — for an imported model, whose
//!   `ImportedModel::size` is the longest edge of its AABB *after recentering
//!   about that box's own centre*. The tightest sphere that certainly contains
//!   such a box has radius `size · √3/2`.
//! * [`radius_from_half_extents`] — for anything whose extents are known
//!   directly (a water box, a tilemap's grid).
//! * [`inflate_for_pose`] — for a skinned mesh, whose bind-pose sphere is wrong
//!   the moment an animation reaches outside it. The symptom of getting this
//!   wrong is a character vanishing as it swings a weapon near the screen edge.
//!
//! Camera-relative throughout (ADR-0015): the planes are built from the same
//! `view_proj` the instance matrices are, so a position that is correct for a
//! draw is correct for the test without conversion.

use floptle_core::math::{Mat4, Vec3, Vec4};

/// √3/2 — half the diagonal of a unit cube.
///
/// The conversion from "longest edge of a box" to "radius of a sphere that
/// contains it". Using `size/2` instead would be a sphere INSIDE the box, and a
/// long thin model rotated 45° would pop.
const BOX_TO_SPHERE: f32 = 0.866_025_4;

/// The six view-frustum planes, in the camera-relative space instance matrices
/// use.
///
/// Gribb–Hartmann: the rows of the transposed `view_proj` combine into planes
/// whose normals point INWARD, so "inside" is a positive signed distance.
#[derive(Clone, Copy, Debug)]
pub struct Frustum {
    /// left, right, bottom, top, near, far — `xyz` = normal, `w` = offset.
    /// Not normalised; [`Frustum::contains_sphere`] divides by the normal length,
    /// which is one reciprocal per plane per test and keeps construction free.
    planes: [Vec4; 6],
}

impl Frustum {
    /// Build from a camera-relative view-projection matrix.
    pub fn from_view_proj(view_proj: Mat4) -> Self {
        let m = view_proj.transpose();
        Self {
            planes: [
                m.w_axis + m.x_axis, // left
                m.w_axis - m.x_axis, // right
                m.w_axis + m.y_axis, // bottom
                m.w_axis - m.y_axis, // top
                m.w_axis + m.z_axis, // near
                m.w_axis - m.z_axis, // far
            ],
        }
    }

    /// Does a sphere at camera-relative `centre` with radius `radius` touch the
    /// frustum at all?
    ///
    /// Conservative in the only direction that matters: a sphere straddling a
    /// plane is IN. Rejecting on any single plane is enough — the classic
    /// false-positive in a corner region costs one wasted instance, while a false
    /// negative costs a visible pop.
    pub fn contains_sphere(&self, centre: Vec3, radius: f32) -> bool {
        self.planes.iter().all(|pl| {
            let n = Vec3::new(pl.x, pl.y, pl.z);
            let len = n.length().max(1e-6);
            (n.dot(centre) + pl.w) / len > -radius
        })
    }

    /// A frustum that contains everything — for the paths that must not cull
    /// (a shadow gather, an offscreen target being warmed).
    ///
    /// Spelled out rather than left to a `None` at every call site, because an
    /// `Option<Frustum>` at eight submission sites is eight chances to forget
    /// which way round the missing case means.
    pub fn everything() -> Self {
        // Six planes with a zero normal: `n.dot(c) + w` is `w`, which is
        // positive, so nothing is ever rejected.
        Self { planes: [Vec4::new(0.0, 0.0, 0.0, 1.0); 6] }
    }
}

/// The radius of a sphere that certainly contains a model whose AABB's longest
/// edge is `size`, drawn at `scale`.
///
/// `size` here is exactly `ImportedModel::size` —
/// `(max - min).max_element()` measured after every part was recentered on the
/// combined box's centre, so the box is symmetric about the node's own origin and
/// a sphere at that origin is the right shape to ask about. `scale` takes the
/// largest component of a non-uniform scale, because a sphere has no axes.
///
/// A model that reported no size at all (a degenerate import) gets a radius big
/// enough never to cull rather than a zero that would cull it always: an asset
/// problem must not read as an engine problem.
pub fn radius_from_longest_edge(size: f32, scale: Vec3) -> f32 {
    let s = scale.x.abs().max(scale.y.abs()).max(scale.z.abs());
    if !size.is_finite() || size <= 0.0 || !s.is_finite() {
        return f32::INFINITY;
    }
    size * BOX_TO_SPHERE * s
}

/// Apply a node's scale to an already-computed local radius.
///
/// The largest component wins, because a sphere has no axes — a node scaled
/// `(1, 1, 4)` reaches four times as far in one direction and the sphere has to
/// cover that direction.
pub fn scale_radius(local_radius: f32, scale: Vec3) -> f32 {
    let s = scale.x.abs().max(scale.y.abs()).max(scale.z.abs());
    if !local_radius.is_finite() || !s.is_finite() {
        return f32::INFINITY;
    }
    local_radius * s
}

/// The radius of a sphere containing a box with the given half-extents at
/// `scale`. For anything whose extents are known outright.
pub fn radius_from_half_extents(half: Vec3, scale: Vec3) -> f32 {
    let s = scale.x.abs().max(scale.y.abs()).max(scale.z.abs());
    let h = Vec3::new(half.x.abs(), half.y.abs(), half.z.abs());
    if !h.is_finite() || !s.is_finite() {
        return f32::INFINITY;
    }
    h.length() * s
}

/// Grow a bind-pose radius to cover where an animation has actually put the
/// joints.
///
/// A skinned vertex ends up at `Σ wⱼ · (Jⱼ · v)`, a convex combination, so it can
/// be no further from the origin than the furthest single `Jⱼ · v` — and
/// `|Jⱼ · v| ≤ |translation of Jⱼ| + (largest scale in Jⱼ) · |v|`. Taking the
/// maximum over the joints gives a bound that holds for every vertex without
/// touching a vertex.
///
/// That is why this reads the pose rather than padding by a constant: a reach
/// animation can put a hand a long way outside the bind box, and a fudge factor
/// that covers the worst clip in the project makes the cull useless for
/// everything else.
///
/// `joints` are the skinning matrices for this frame (joint-space → model-space,
/// i.e. what the shader would multiply by). An empty list means unskinned: the
/// bind radius already is the answer.
pub fn inflate_for_pose(bind_radius: f32, joints: &[Mat4]) -> f32 {
    if joints.is_empty() || !bind_radius.is_finite() {
        return bind_radius;
    }
    let mut worst = 0.0f32;
    for j in joints {
        let t = j.w_axis.truncate().length();
        // The largest length a unit vector can reach through the linear part —
        // approximated by the longest column, which is ≥ the true operator norm
        // divided by √3 and ≤ it, so scaling by √3 keeps the bound safe for a
        // sheared joint.
        let l = j
            .x_axis
            .truncate()
            .length()
            .max(j.y_axis.truncate().length())
            .max(j.z_axis.truncate().length());
        worst = worst.max(t + bind_radius * l * BOX_TO_SPHERE * 2.0);
    }
    worst.max(bind_radius)
}

#[cfg(test)]
mod tests {
    use super::*;
    use floptle_core::math::Mat4;

    /// A camera-relative view-projection looking down −Z, which is what the
    /// engine's cameras produce.
    fn looking_forward() -> Mat4 {
        let proj = Mat4::perspective_rh(60f32.to_radians(), 16.0 / 9.0, 0.1, 1000.0);
        // The eye is the origin in camera-relative space.
        proj * Mat4::IDENTITY
    }

    /// The point of the whole task: what is behind you is rejected, and what is
    /// in front of you is not.
    #[test]
    fn things_behind_the_camera_are_culled_and_things_ahead_are_not() {
        let f = Frustum::from_view_proj(looking_forward());
        assert!(f.contains_sphere(Vec3::new(0.0, 0.0, -10.0), 1.0), "dead ahead");
        assert!(!f.contains_sphere(Vec3::new(0.0, 0.0, 10.0), 1.0), "directly behind");
        assert!(!f.contains_sphere(Vec3::new(0.0, 0.0, -5000.0), 1.0), "past the far plane");
        // Far off to the side at close range: outside the cone.
        assert!(!f.contains_sphere(Vec3::new(500.0, 0.0, -10.0), 1.0));
    }

    /// A sphere STRADDLING a plane stays in. This is the direction the test has
    /// to be wrong in: keeping something that is half off screen costs one
    /// instance, dropping something that is half on screen is a visible pop.
    #[test]
    fn a_sphere_straddling_a_plane_survives() {
        let f = Frustum::from_view_proj(looking_forward());
        // Just behind the camera, but big enough to poke in front of it.
        assert!(f.contains_sphere(Vec3::new(0.0, 0.0, 5.0), 20.0));
        // Its centre is outside the side plane; its body is not.
        let near_edge = Vec3::new(12.0, 0.0, -10.0);
        assert!(!f.contains_sphere(near_edge, 0.01), "the centre really is outside");
        assert!(f.contains_sphere(near_edge, 30.0), "a big sphere there still touches the cone");
    }

    /// The radius conversion is CONSERVATIVE: it contains the box it came from,
    /// including the corners, at any rotation.
    ///
    /// This is the calculation the task warned about. `size/2` — the obvious
    /// wrong answer — is a sphere inscribed in the box, and a long thin model
    /// turned 45° pops out of it.
    #[test]
    fn the_model_radius_contains_every_corner_of_its_box() {
        // A cube 2 units on its longest edge is recentered to ±1 on each axis.
        let r = radius_from_longest_edge(2.0, Vec3::ONE);
        let corner = Vec3::new(1.0, 1.0, 1.0).length(); // √3
        assert!(r >= corner, "radius {r} does not reach the corner at {corner}");
        assert!(r < corner * 1.05, "radius {r} is more than 5% loose");
        // Scale multiplies it, and a non-uniform scale takes the largest axis.
        assert!((radius_from_longest_edge(2.0, Vec3::splat(3.0)) - r * 3.0).abs() < 1e-4);
        assert!((radius_from_longest_edge(2.0, Vec3::new(1.0, 1.0, 4.0)) - r * 4.0).abs() < 1e-4);
        // …and `scale_radius` is the same rule, applied to a radius that was
        // measured some other way.
        assert!((scale_radius(r, Vec3::new(1.0, 1.0, 4.0)) - r * 4.0).abs() < 1e-4);
        assert_eq!(scale_radius(f32::INFINITY, Vec3::ONE), f32::INFINITY);
        assert_eq!(scale_radius(1.0, Vec3::splat(f32::NAN)), f32::INFINITY);
    }

    /// A model with no measurable size never culls, rather than always culling.
    /// A broken import must not look like a broken renderer.
    #[test]
    fn an_unmeasurable_model_is_never_culled() {
        let f = Frustum::from_view_proj(looking_forward());
        for bad in [0.0, -1.0, f32::NAN] {
            let r = radius_from_longest_edge(bad, Vec3::ONE);
            assert!(f.contains_sphere(Vec3::new(0.0, 0.0, 10.0), r), "size {bad} culled");
        }
    }

    /// Half-extents convert to the corner distance, not the largest one.
    #[test]
    fn a_box_radius_reaches_its_corner() {
        let r = radius_from_half_extents(Vec3::new(3.0, 4.0, 12.0), Vec3::ONE);
        assert!((r - 13.0).abs() < 1e-3, "3-4-12 has a 13 corner, got {r}");
    }

    /// A pose that reaches outside the bind box grows the radius, which is the
    /// bug the acceptance criteria names: a character vanishing mid-swing near
    /// the screen edge.
    #[test]
    fn a_reaching_pose_grows_the_radius() {
        let bind = 1.0;
        // Rest pose: every joint at the origin, no scale. Must not grow much.
        let rest = vec![Mat4::IDENTITY; 4];
        let r_rest = inflate_for_pose(bind, &rest);
        assert!(r_rest >= bind);
        // One joint flung 10 units out — a sword arm at full extension.
        let mut posed = rest.clone();
        posed[2] = Mat4::from_translation(Vec3::new(10.0, 0.0, 0.0));
        let r_posed = inflate_for_pose(bind, &posed);
        assert!(
            r_posed >= 10.0 + bind,
            "a joint 10 units out must be inside the sphere, got {r_posed}"
        );
        assert!(r_posed > r_rest, "the reach has to cost something");
        // No joints at all = unskinned = the bind radius, unchanged.
        assert_eq!(inflate_for_pose(bind, &[]), bind);
    }

    /// `everything()` really does keep everything, so a path that must not cull
    /// can say so without an Option at every call site.
    #[test]
    fn the_everything_frustum_never_rejects() {
        let f = Frustum::everything();
        for p in [
            Vec3::ZERO,
            Vec3::new(0.0, 0.0, 1e9),
            Vec3::new(-1e9, 1e9, -1e9),
        ] {
            assert!(f.contains_sphere(p, 0.0), "{p} was culled by everything()");
        }
    }
}
