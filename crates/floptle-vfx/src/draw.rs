//! Bridges the sim to the billboard pass: packs live particles into the
//! per-frame instance array `floptle_render::Particles` draws.
//!
//! The caller (editor / runtime / probe) accumulates one packed array + draw list
//! across every live effect instance, resolves each track's texture path to a
//! raster [`TexId`](floptle_render::TexId) through its own registry, and issues one
//! `Particles::draw`.

use crate::effect::{BillboardOrient, Blend, FlipMode, Flipbook, RenderMode, Space};
use crate::sim::{EffectInstance, ParticleSample};
use floptle_core::math::{Mat4, Quat, Vec3};
use floptle_render::particles::{ParticleBlend, ParticleInstance};

/// One track's contribution to this frame's packed instance array. `texture` is
/// the authored project-relative path; the caller maps it to a registered TexId.
#[derive(Clone, Debug)]
pub struct BillboardDraw {
    pub texture: Option<String>,
    pub blend: ParticleBlend,
    pub range: std::ops::Range<u32>,
}

/// Pack every billboard track of `inst` into `instances`, appending one
/// [`BillboardDraw`] per non-empty track.
///
/// `local_xf` maps emitter-local space to camera-relative world space (the node's
/// `render_matrix`, ADR-0015) — used by `Space::Local` tracks; `world_xf` maps the
/// instance's world anchor to camera-relative space — used by `Space::World` tracks
/// (whose particles are already world-baked). Billboard size scales by the chosen
/// transform's mean axis scale. `cam_forward`/`cam_right`/`cam_up` are the camera's
/// world basis (also camera-relative, since the camera sits at the origin): `Alpha`
/// tracks sort back-to-front along `cam_forward`, and face-camera tracks span the
/// right/up vectors. Non-face-camera modes derive their own basis per particle.
#[allow(clippy::too_many_arguments)]
pub fn collect_billboards(
    inst: &EffectInstance,
    local_xf: Mat4,
    world_xf: Mat4,
    cam_forward: Vec3,
    cam_right: Vec3,
    cam_up: Vec3,
    instances: &mut Vec<ParticleInstance>,
    draws: &mut Vec<BillboardDraw>,
) {
    for (ti, ct) in inst.billboard_tracks() {
        let RenderMode::Billboard { texture } = &ct.look.render else { continue };
        let xf = if ct.space == Space::World { world_xf } else { local_xf };
        let scale = {
            let m = glam_mat3_scale(&xf);
            (m.0 + m.1 + m.2) / 3.0
        };
        let orient = ct.look.orient;
        let aspect = inst.track_aspect(ti);
        let stretch = ct.look.stretch.max(1e-3);
        let flip = ct.look.flipbook;
        let start = instances.len();
        inst.sample_track(ti, |s| {
            let world = xf.transform_point3(s.pos);
            let base = s.size * scale;
            // Width takes the aspect ratio; height stays the size (velocity stretch
            // rides the up-vector length, so the shader needs no stretch term).
            let (w, h) = (base * aspect, base);
            let (right, up, spin) =
                billboard_basis(orient, &xf, world, &s, cam_right, cam_up, stretch);
            // Flipbook UV sub-rect [min_u, min_v, du, dv] packed into the spare
            // channels (full quad [0,0,1,1] when there's no flipbook).
            let uv = flipbook_uv(flip, &s);
            instances.push(ParticleInstance {
                pos_rot: [world.x, world.y, world.z, spin],
                size: [w, h, uv[0], uv[1]],
                color: s.color,
                basis_right: [right.x, right.y, right.z, uv[2]],
                basis_up: [up.x, up.y, up.z, uv[3]],
            });
        });
        if instances.len() == start {
            continue;
        }
        if ct.look.blend.needs_sort() {
            // Back-to-front along the view direction so order-dependent modes
            // composite correctly within the track (positions are camera-relative).
            instances[start..].sort_by(|a, b| {
                let da = a.pos_rot[0] * cam_forward.x
                    + a.pos_rot[1] * cam_forward.y
                    + a.pos_rot[2] * cam_forward.z;
                let db = b.pos_rot[0] * cam_forward.x
                    + b.pos_rot[1] * cam_forward.y
                    + b.pos_rot[2] * cam_forward.z;
                db.total_cmp(&da)
            });
        }
        draws.push(BillboardDraw {
            texture: texture.clone(),
            blend: match ct.look.blend {
                Blend::Alpha => ParticleBlend::Alpha,
                Blend::Additive => ParticleBlend::Additive,
                Blend::Premultiplied => ParticleBlend::Premultiplied,
                Blend::Screen => ParticleBlend::Screen,
                Blend::Multiply => ParticleBlend::Multiply,
            },
            range: start as u32..instances.len() as u32,
        });
    }
}

/// The UV sub-rect `[min_u, min_v, du, dv]` a particle samples from a flipbook atlas
/// this frame — the full quad `[0, 0, 1, 1]` when the track has no flipbook. The
/// frame index comes from the particle's age (over its life, or a fixed-fps loop).
fn flipbook_uv(flip: Option<Flipbook>, s: &ParticleSample) -> [f32; 4] {
    let Some(fb) = flip else { return [0.0, 0.0, 1.0, 1.0] };
    let (cols, rows) = (fb.cols.max(1), fb.rows.max(1));
    let n = cols * rows;
    if n <= 1 {
        return [0.0, 0.0, 1.0, 1.0];
    }
    let raw = match fb.mode {
        FlipMode::OverLife => (s.age01.clamp(0.0, 1.0) * n as f32) as u32,
        FlipMode::LoopFps => (s.age.max(0.0) * fb.fps.max(0.0)) as u32,
    };
    let f = (raw % n).min(n - 1);
    let (cx, cy) = (f % cols, f / cols);
    let (du, dv) = (1.0 / cols as f32, 1.0 / rows as f32);
    [cx as f32 * du, cy as f32 * dv, du, dv]
}

/// The world-space in-plane basis (+X width axis, +Y height axis) a particle's quad
/// spans, plus the roll spin to apply, for the track's [`BillboardOrient`]. All
/// vectors are camera-relative (ADR-0015: the camera sits at the origin), so
/// `view_dir` is just the direction from the origin to the particle.
///
/// Degenerate cases (zero velocity, velocity parallel to the view, looking straight
/// down the up axis) fall back to the camera basis so a quad never collapses to a
/// line or NaNs out.
fn billboard_basis(
    orient: BillboardOrient,
    xf: &Mat4,
    world_pos: Vec3,
    s: &ParticleSample,
    cam_right: Vec3,
    cam_up: Vec3,
    stretch: f32,
) -> (Vec3, Vec3, f32) {
    const EPS: f32 = 1e-6;
    let view_dir = world_pos.normalize_or_zero();
    match orient {
        // Classic billboard: the camera basis, spun by roll.
        BillboardOrient::FaceCamera => (cam_right, cam_up, s.rotation.z),
        // Stretched along motion: up = velocity (scaled by stretch), width faces the
        // camera around that axis. Roll is meaningless here, so it's dropped.
        BillboardOrient::Velocity => {
            let vel = xf.transform_vector3(s.velocity);
            let up = vel.normalize_or_zero();
            if up == Vec3::ZERO || view_dir == Vec3::ZERO {
                return (cam_right, cam_up, 0.0);
            }
            let right = view_dir.cross(up);
            if right.length_squared() < EPS {
                // Velocity points at/away from the camera — no stable in-plane right.
                return (cam_right, cam_up, 0.0);
            }
            (right.normalize(), up * stretch, 0.0)
        }
        // Upright: locked to world up, yawing to the camera. Roll would tip it, so 0.
        BillboardOrient::Vertical => {
            let up = Vec3::Y;
            let mut right = up.cross(view_dir);
            if right.length_squared() < EPS {
                // Looking straight down the up axis — use the camera right, flattened.
                right = Vec3::new(cam_right.x, 0.0, cam_right.z);
                if right.length_squared() < EPS {
                    right = Vec3::X;
                }
            }
            (right.normalize(), up, 0.0)
        }
        // Flat on the ground (normal = world up); roll spins it in the ground plane.
        BillboardOrient::Horizontal => (Vec3::X, Vec3::Z, s.rotation.z),
        // Fixed to the birth (emit-direction) frame; rotate it into world space. For
        // World-space tracks `xf` is a pure translation, so the world-baked frame is
        // used as-is.
        BillboardOrient::WorldFixed => {
            let right = xf.transform_vector3(s.frame * Vec3::X).normalize_or_zero();
            let up = xf.transform_vector3(s.frame * Vec3::Y).normalize_or_zero();
            if right == Vec3::ZERO || up == Vec3::ZERO {
                (cam_right, cam_up, s.rotation.z)
            } else {
                (right, up, s.rotation.z)
            }
        }
    }
}

/// One mesh-render track's live particles as camera-relative model matrices +
/// tints. The caller resolves `asset_path` to GPU mesh(es) and appends these to
/// the raster pass's instance list — so mesh particles are lit, sun-shadowed, and
/// SDF-AO'd exactly like scene meshes (proposal §5.2).
#[derive(Clone, Debug)]
pub struct MeshDraw {
    pub asset_path: String,
    /// (camera-relative model matrix, straight-alpha rgba tint) per particle.
    pub instances: Vec<(Mat4, [f32; 4])>,
}

/// Collect every mesh-render track of `inst` into `out`. `local_xf`/`world_xf` map
/// emitter-local / world-anchor space to camera-relative space (see
/// [`collect_billboards`]); each particle becomes
/// `translate(worldpos) · spinY(rotation) · scale(size · emitter_scale)`.
pub fn collect_mesh_particles(inst: &EffectInstance, local_xf: Mat4, world_xf: Mat4, out: &mut Vec<MeshDraw>) {
    for (ti, ct) in inst.mesh_tracks() {
        let RenderMode::Mesh { asset_path } = &ct.look.render else { continue };
        let xf = if ct.space == Space::World { world_xf } else { local_xf };
        let s = glam_mat3_scale(&xf);
        let scale = (s.0 + s.1 + s.2) / 3.0;
        let mut items = Vec::new();
        inst.sample_track(ti, |p| {
            let world = xf.transform_point3(p.pos);
            // Full 3D orientation for meshes: yaw (y) · pitch (x) · roll (z).
            let rot = Quat::from_rotation_y(p.rotation.y)
                * Quat::from_rotation_x(p.rotation.x)
                * Quat::from_rotation_z(p.rotation.z);
            let model = Mat4::from_scale_rotation_translation(
                Vec3::splat((p.size * scale).max(1e-4)),
                rot,
                world,
            );
            items.push((model, p.color));
        });
        if !items.is_empty() {
            out.push(MeshDraw { asset_path: asset_path.clone(), instances: items });
        }
    }
}

/// The lengths of the matrix's three basis axes (its per-axis scale).
fn glam_mat3_scale(m: &Mat4) -> (f32, f32, f32) {
    (
        m.x_axis.truncate().length(),
        m.y_axis.truncate().length(),
        m.z_axis.truncate().length(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect::{Burst, Look, ParticleEffect, Playback, Track};
    use std::sync::Arc;

    #[test]
    fn collect_packs_and_sorts_alpha_back_to_front() {
        let fx = Arc::new(
            ParticleEffect {
                lifetime: 1.0,
                playback: Playback::OneShot,
                tracks: vec![Track {
                    rate: 0.0,
                    bursts: vec![Burst { t: 0.0, count: 20 }],
                    particle_lifetime: 5.0,
                    shape: crate::effect::EmitShape::Sphere { radius: 2.0, shell: false },
                    look: Look { blend: Blend::Alpha, ..Look::default() },
                    ..Track::default()
                }],
                ..ParticleEffect::default()
            }
            .compile(),
        );
        let mut inst = EffectInstance::new(fx, 3);
        inst.simulate_to(0.1, Vec3::ZERO);

        let mut packed = Vec::new();
        let mut draws = Vec::new();
        let fwd = Vec3::Z;
        collect_billboards(
            &inst, Mat4::IDENTITY, Mat4::IDENTITY, fwd, Vec3::X, Vec3::Y, &mut packed, &mut draws,
        );

        assert_eq!(draws.len(), 1);
        assert_eq!(packed.len(), 20);
        assert_eq!(draws[0].range, 0..20);
        for w in packed.windows(2) {
            assert!(w[0].pos_rot[2] >= w[1].pos_rot[2], "not back-to-front along +Z");
        }
        // Face-camera (default) packs the camera basis verbatim.
        for p in &packed {
            assert_eq!([p.basis_right[0], p.basis_right[1], p.basis_right[2]], [1.0, 0.0, 0.0]);
            assert_eq!([p.basis_up[0], p.basis_up[1], p.basis_up[2]], [0.0, 1.0, 0.0]);
        }
    }

    /// Every orientation mode must produce a finite, non-degenerate basis (two
    /// non-parallel axes) so no quad collapses to a line — including the tricky
    /// velocity-parallel-to-view and straight-down cases.
    #[test]
    fn orientation_modes_yield_finite_non_degenerate_bases() {
        use crate::effect::{BillboardOrient, Look};
        use crate::curve::{Value, ValueOrCurve};
        for orient in [
            BillboardOrient::FaceCamera,
            BillboardOrient::Velocity,
            BillboardOrient::Vertical,
            BillboardOrient::Horizontal,
            BillboardOrient::WorldFixed,
        ] {
            let fx = Arc::new(
                ParticleEffect {
                    lifetime: 1.0,
                    playback: Playback::OneShot,
                    tracks: vec![Track {
                        rate: 0.0,
                        bursts: vec![Burst { t: 0.0, count: 16 }],
                        particle_lifetime: 5.0,
                        // Sphere spread gives velocities in every direction, incl.
                        // straight at/away from the camera and along the up axis.
                        shape: crate::effect::EmitShape::Sphere { radius: 1.0, shell: true },
                        velocity: ValueOrCurve::Const(Value::Vec3(Vec3::new(0.0, 2.0, 0.0))),
                        look: Look { orient, ..Look::default() },
                        ..Track::default()
                    }],
                    ..ParticleEffect::default()
                }
                .compile(),
            );
            let mut inst = EffectInstance::new(fx, 3);
            inst.simulate_to(0.2, Vec3::ZERO);
            let mut packed = Vec::new();
            let mut draws = Vec::new();
            // A camera looking down -Z from +Z, plus one looking straight down -Y.
            for (fwd, right, up) in
                [(Vec3::NEG_Z, Vec3::X, Vec3::Y), (Vec3::NEG_Y, Vec3::X, Vec3::NEG_Z)]
            {
                packed.clear();
                draws.clear();
                collect_billboards(
                    &inst, Mat4::IDENTITY, Mat4::IDENTITY, fwd, right, up, &mut packed, &mut draws,
                );
                assert!(!packed.is_empty(), "{orient:?} produced no instances");
                for p in &packed {
                    let r = Vec3::new(p.basis_right[0], p.basis_right[1], p.basis_right[2]);
                    let u = Vec3::new(p.basis_up[0], p.basis_up[1], p.basis_up[2]);
                    assert!(r.is_finite() && u.is_finite(), "{orient:?} NaN basis");
                    assert!(r.length() > 1e-4 && u.length() > 1e-4, "{orient:?} zero basis");
                    // Non-parallel: the cross product (the quad normal) is non-zero.
                    assert!(r.cross(u).length() > 1e-4, "{orient:?} collapsed basis");
                }
            }
        }
    }

    #[test]
    fn velocity_stretch_lengthens_the_up_axis() {
        use crate::effect::{BillboardOrient, Look};
        use crate::curve::{Value, ValueOrCurve};
        // A single particle moving +Y at a viewer on +Z: stretch 3 must triple the
        // up-basis length vs. stretch 1, and drop the roll spin.
        let mk = |stretch: f32| {
            let fx = Arc::new(
                ParticleEffect {
                    lifetime: 1.0,
                    playback: Playback::OneShot,
                    tracks: vec![Track {
                        rate: 0.0,
                        bursts: vec![Burst { t: 0.0, count: 1 }],
                        particle_lifetime: 5.0,
                        velocity: ValueOrCurve::Const(Value::Vec3(Vec3::new(0.0, 4.0, 0.0))),
                        rotation: ValueOrCurve::Const(Value::Vec3(Vec3::new(0.0, 0.0, 1.0))),
                        look: Look { orient: BillboardOrient::Velocity, stretch, ..Look::default() },
                        ..Track::default()
                    }],
                    ..ParticleEffect::default()
                }
                .compile(),
            );
            let mut inst = EffectInstance::new(fx, 1);
            inst.simulate_to(0.1, Vec3::ZERO);
            let (mut packed, mut draws) = (Vec::new(), Vec::new());
            // Push the particle out along +Z so the view direction isn't parallel to
            // its +Y motion (which would trip the degenerate fallback, not stretch).
            let xf = Mat4::from_translation(Vec3::new(0.0, 0.0, 5.0));
            collect_billboards(
                &inst, xf, xf, Vec3::NEG_Z, Vec3::X, Vec3::Y, &mut packed, &mut draws,
            );
            packed[0]
        };
        let a = mk(1.0);
        let b = mk(3.0);
        let up_a = Vec3::new(a.basis_up[0], a.basis_up[1], a.basis_up[2]).length();
        let up_b = Vec3::new(b.basis_up[0], b.basis_up[1], b.basis_up[2]).length();
        assert!((up_b / up_a - 3.0).abs() < 0.05, "stretch should triple up length");
        assert_eq!(a.pos_rot[3], 0.0, "velocity mode drops roll spin");
    }

    #[test]
    fn flipbook_uv_walks_the_atlas_by_age() {
        use crate::effect::{FlipMode, Flipbook};
        let s = |age01: f32| ParticleSample {
            pos: Vec3::ZERO,
            velocity: Vec3::ZERO,
            frame: Quat::IDENTITY,
            size: 1.0,
            rotation: Vec3::ZERO,
            color: [1.0; 4],
            age: 0.0,
            age01,
        };
        // No flipbook → the full quad.
        assert_eq!(flipbook_uv(None, &s(0.5)), [0.0, 0.0, 1.0, 1.0]);
        let fb = Some(Flipbook { cols: 4, rows: 4, mode: FlipMode::OverLife, fps: 12.0 });
        // Frame 0 at birth: top-left cell, 1/4 wide/tall.
        assert_eq!(flipbook_uv(fb, &s(0.0)), [0.0, 0.0, 0.25, 0.25]);
        // Just before death: last cell (frame 15) → col 3, row 3.
        let last = flipbook_uv(fb, &s(0.999));
        assert!((last[0] - 0.75).abs() < 1e-6 && (last[1] - 0.75).abs() < 1e-6, "{last:?}");
        // Mid-life (frame 8) → col 0, row 2.
        let mid = flipbook_uv(fb, &s(0.5));
        assert!((mid[0] - 0.0).abs() < 1e-6 && (mid[1] - 0.5).abs() < 1e-6, "{mid:?}");
    }

    #[test]
    fn mesh_tracks_collect_one_model_matrix_per_particle() {
        use crate::effect::{Clip, RenderMode};
        let fx = Arc::new(
            ParticleEffect {
                lifetime: 1.0,
                playback: Playback::OneShot,
                tracks: vec![
                    // A billboard track (ignored by mesh collection)...
                    Track { rate: 0.0, bursts: vec![Burst { t: 0.0, count: 3 }], particle_lifetime: 5.0, ..Track::default() },
                    // ...and a mesh track that should yield a MeshDraw.
                    Track {
                        rate: 0.0,
                        bursts: vec![Burst { t: 0.0, count: 5 }],
                        particle_lifetime: 5.0,
                        clips: vec![Clip { start: 0.0, end: 1.0 }],
                        look: Look { render: RenderMode::Mesh { asset_path: "models/Spark.glb".into() }, ..Look::default() },
                        ..Track::default()
                    },
                ],
                ..ParticleEffect::default()
            }
            .compile(),
        );
        let mut inst = EffectInstance::new(fx, 1);
        inst.simulate_to(0.1, Vec3::ZERO);

        let mut out = Vec::new();
        collect_mesh_particles(&inst, Mat4::IDENTITY, Mat4::IDENTITY, &mut out);
        assert_eq!(out.len(), 1, "one mesh track -> one MeshDraw (billboard track skipped)");
        assert_eq!(out[0].asset_path, "models/Spark.glb");
        assert_eq!(out[0].instances.len(), 5, "one model matrix per live particle");
        // Each model matrix must be finite + non-degenerate (positive scale).
        for (m, _c) in &out[0].instances {
            assert!(m.determinant().abs() > 1e-9, "degenerate mesh-particle matrix");
        }
    }
}
