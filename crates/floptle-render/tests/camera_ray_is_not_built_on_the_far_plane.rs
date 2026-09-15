//! The raymarch builds its camera ray from the near plane and a MID-DEPTH
//! point, never the far plane.
//!
//! Unprojecting depth 1.0 lands on the far plane, whose homogeneous `w` is
//! `1/far`. With the camera's 300000-unit far plane that is 3e-6, formed in
//! the shader as the difference of two ~20-sized terms — the edge of f32. The
//! f32 inverse of the view projection leaves stray x/y coefficients of the same
//! order in that row, so for some camera poses `w` changes sign along a straight
//! line across the screen, the direction beyond it overflows to NaN, and the sky
//! shows a wedge of its horizon colour with a ruler-straight edge for one frame
//! (the KnightFight sky flicker: a few frames in every thousand while looking
//! around). The mid-depth point has `w ≈ 10` and none of this.
//!
//! Two guards: the shader source must not unproject depth 1.0 with the inverse
//! view projection, and the arithmetic itself is shown to fail on the far plane
//! and hold at mid depth over the same camera poses — so a future "simplify the
//! ray to the far point" is caught by the first, and a change to the projection
//! that reintroduces the conditioning problem by the second.

use glam::{Mat4, Quat, Vec3, Vec4};

const RAYMARCH: &str = include_str!("../src/raymarch.wgsl");

#[test]
fn the_shader_never_unprojects_the_far_plane() {
    for line in RAYMARCH.lines() {
        let code = line.split("//").next().unwrap_or("");
        assert!(
            !(code.contains("inv_view_proj") && code.contains("1.0, 1.0)")),
            "a far-plane unproject is back in raymarch.wgsl: `{}`",
            line.trim()
        );
    }
    assert!(RAYMARCH.contains("fn camera_ray_dir("), "the shared ray builder is gone");
}

/// The camera KnightFight plays through: 300000 far, 0.05 near, sixty degrees.
fn view_proj(pos: Vec3, yaw: f32, pitch: f32) -> Mat4 {
    let rot = Quat::from_rotation_y(yaw) * Quat::from_rotation_x(pitch);
    let view = Mat4::from_rotation_translation(rot, pos).inverse();
    let proj = Mat4::perspective_rh(60f32.to_radians(), 1.77, 0.05, 300000.0);
    proj * view
}

/// A deterministic spread of camera poses — a walk around a level, looking about.
fn poses() -> impl Iterator<Item = (Vec3, f32, f32)> {
    (0..4000u32).map(|i| {
        let f = i as f32;
        let pos = Vec3::new(
            (f * 0.731).sin() * 60.0,
            2.0 + (f * 0.113).cos() * 3.0,
            (f * 0.417).cos() * 60.0,
        );
        (pos, (f * 0.0173) % std::f32::consts::TAU, (f * 0.037).sin() * 0.75)
    })
}

/// How many poses put a sign change of the unprojected point's `w` somewhere
/// on screen at NDC depth `z` — the condition that turns a ray direction into
/// NaN on one side of a straight line.
fn poses_with_a_sign_flip_at(z: f32) -> usize {
    poses()
        .filter(|&(pos, yaw, pitch)| {
            let inv = view_proj(pos, yaw, pitch).inverse();
            let mut pos_seen = false;
            let mut neg_seen = false;
            for iy in 0..24 {
                for ix in 0..24 {
                    let ndc_x = -1.0 + 2.0 * ix as f32 / 23.0;
                    let ndc_y = -1.0 + 2.0 * iy as f32 / 23.0;
                    let w = (inv * Vec4::new(ndc_x, ndc_y, z, 1.0)).w;
                    if w > 0.0 {
                        pos_seen = true;
                    } else {
                        neg_seen = true;
                    }
                }
            }
            pos_seen && neg_seen
        })
        .count()
}

#[test]
fn the_far_plane_flips_sign_and_mid_depth_does_not() {
    let far = poses_with_a_sign_flip_at(1.0);
    let mid = poses_with_a_sign_flip_at(0.5);
    assert!(far > 0, "the far-plane unproject no longer misbehaves in f32 — is the projection still 0.05..300000?");
    assert_eq!(mid, 0, "the mid-depth unproject changed sign on screen for {mid} poses");
}
