//! Bridges the sim to the billboard pass: packs live particles into the
//! per-frame instance array `floptle_render::Particles` draws.
//!
//! The caller (editor / runtime / probe) accumulates one packed array + draw list
//! across every live effect instance, resolves each track's texture path to a
//! raster [`TexId`](floptle_render::TexId) through its own registry, and issues one
//! `Particles::draw`.

use crate::effect::{Blend, RenderMode, Space};
use crate::sim::EffectInstance;
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
/// transform's mean axis scale. `cam_forward` is the view direction — `Alpha` tracks
/// sort back-to-front along it (`Additive` needs no order).
pub fn collect_billboards(
    inst: &EffectInstance,
    local_xf: Mat4,
    world_xf: Mat4,
    cam_forward: Vec3,
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
        let start = instances.len();
        inst.sample_track(ti, |s| {
            let world = xf.transform_point3(s.pos);
            let size = s.size * scale;
            instances.push(ParticleInstance {
                pos_rot: [world.x, world.y, world.z, s.rotation],
                size: [size, size, 0.0, 0.0],
                color: s.color,
            });
        });
        if instances.len() == start {
            continue;
        }
        if ct.look.blend == Blend::Alpha {
            // Back-to-front along the view direction so alpha composites correctly
            // within the track (positions are already camera-relative).
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
            },
            range: start as u32..instances.len() as u32,
        });
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
            let model = Mat4::from_scale_rotation_translation(
                Vec3::splat((p.size * scale).max(1e-4)),
                Quat::from_rotation_y(p.rotation),
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
        collect_billboards(&inst, Mat4::IDENTITY, Mat4::IDENTITY, fwd, &mut packed, &mut draws);

        assert_eq!(draws.len(), 1);
        assert_eq!(packed.len(), 20);
        assert_eq!(draws[0].range, 0..20);
        for w in packed.windows(2) {
            assert!(w[0].pos_rot[2] >= w[1].pos_rot[2], "not back-to-front along +Z");
        }
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
