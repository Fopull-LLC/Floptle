//! Bridges the sim to the billboard pass: packs live particles into the
//! per-frame instance array `floptle_render::Particles` draws.
//!
//! The caller (editor / runtime / probe) accumulates one packed array + draw list
//! across every live effect instance, resolves each track's texture path to a
//! raster [`TexId`](floptle_render::TexId) through its own registry, and issues one
//! `Particles::draw`.

use crate::effect::{Blend, RenderMode};
use crate::sim::EffectInstance;
use floptle_core::math::{Mat4, Vec3};
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
/// `xf` maps emitter-local space to camera-relative world space (the node's
/// `render_matrix`, ADR-0015); billboard size scales by its mean axis scale.
/// `cam_forward` is the camera's view direction in that same space — `Alpha`
/// tracks are sorted back-to-front along it (`Additive` needs no order).
pub fn collect_billboards(
    inst: &EffectInstance,
    xf: Mat4,
    cam_forward: Vec3,
    instances: &mut Vec<ParticleInstance>,
    draws: &mut Vec<BillboardDraw>,
) {
    let scale = {
        let m = glam_mat3_scale(&xf);
        (m.0 + m.1 + m.2) / 3.0
    };
    for (ti, ct) in inst.billboard_tracks() {
        let RenderMode::Billboard { texture } = &ct.look.render else { continue };
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
        collect_billboards(&inst, Mat4::IDENTITY, fwd, &mut packed, &mut draws);

        assert_eq!(draws.len(), 1);
        assert_eq!(packed.len(), 20);
        assert_eq!(draws[0].range, 0..20);
        for w in packed.windows(2) {
            assert!(w[0].pos_rot[2] >= w[1].pos_rot[2], "not back-to-front along +Z");
        }
    }
}
