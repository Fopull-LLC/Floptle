//! Bridges the sim to the billboard pass: packs live particles into the
//! per-frame instance array `floptle_render::Particles` draws.
//!
//! The caller (editor / runtime / probe) accumulates one packed array + draw list
//! across every live effect instance, resolves each track's texture path to a
//! raster [`TexId`](floptle_render::TexId) through its own registry, and issues one
//! `Particles::draw`.

use crate::effect::{BillboardOrient, Blend, EndBehavior, FlipMode, Flipbook, Playback, RenderMode, Space};
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
            blend: particle_blend(ct.look.blend),
            range: start as u32..instances.len() as u32,
        });
    }
}

/// One camera-facing ribbon segment as a particle-quad instance — the trick that
/// lets trails and beams ride the existing billboard pass with NO new pipeline: the
/// quad's up axis IS the segment vector `b - a` (with `size.y = 1` it spans the
/// full segment around the midpoint), its right axis faces the camera via
/// `cross(view_dir, segment_dir)` (falling back to `cam_right` when the segment
/// points at the camera), and the flipbook UV-rect lanes carry the ribbon's
/// along-length `v` slice instead — `v = r0` at the `a` end, `r1` at the `b` end,
/// `u` running 0→1 across the width. (Ribbon slicing and flipbooks are therefore
/// mutually exclusive; trails/beams simply don't use flipbooks.)
fn ribbon_segment(
    a: Vec3,
    b: Vec3,
    width: f32,
    color: [f32; 4],
    r0: f32,
    r1: f32,
    cam_right: Vec3,
) -> ParticleInstance {
    let mid = 0.5 * (a + b);
    let seg = b - a;
    let dir = seg.normalize_or_zero();
    // Camera-relative space: the camera sits at the origin (ADR-0015), so the view
    // direction to the segment is just the midpoint's direction.
    let view = mid.normalize_or_zero();
    let mut right = view.cross(dir);
    right = if right.length_squared() < 1e-10 { cam_right } else { right.normalize() };
    ParticleInstance {
        pos_rot: [mid.x, mid.y, mid.z, 0.0],
        // width across the right axis; height 1 = the full segment vector below.
        // zw are the UV-rect min: u starts at 0, v at the b (head-ward) end's r1.
        size: [width, 1.0, 0.0, r1],
        color,
        // du = 1: u spans the full texture width. dv = r0 − r1: the shader's base
        // v runs 0 at the +up (b) end to 1 at the −up (a) end, so this lands
        // v = r1 at b and v = r0 at a — v increases tail→head along the ribbon.
        basis_right: [right.x, right.y, right.z, 1.0],
        basis_up: [seg.x, seg.y, seg.z, r0 - r1],
    }
}

/// Pack every trailed billboard track of `inst` as connected ribbon segments,
/// appending one [`BillboardDraw`] per non-empty track. Each live particle with ≥ 2
/// polyline points (its recorded history plus its current position as the head)
/// contributes `points − 1` quads, colored with the particle's CURRENT color; when
/// the trail fades, width and alpha taper to zero at the tail. Segments are pushed
/// tail→head and deliberately NOT depth-sorted — a ribbon must keep its connected
/// order even under an order-dependent blend.
pub fn collect_trails(
    inst: &EffectInstance,
    local_xf: Mat4,
    world_xf: Mat4,
    cam_right: Vec3,
    instances: &mut Vec<ParticleInstance>,
    draws: &mut Vec<BillboardDraw>,
) {
    for (ti, ct) in inst.billboard_tracks() {
        let (Some(trail), RenderMode::Billboard { texture }) = (&ct.trail, &ct.look.render) else {
            continue;
        };
        let Some(hists) = &inst.track_particles(ti).trail else { continue };
        let xf = if ct.space == Space::World { world_xf } else { local_xf };
        let scale = {
            let m = glam_mat3_scale(&xf);
            (m.0 + m.1 + m.2) / 3.0
        };
        let head_w = trail.width.max(0.0) * scale;
        let start = instances.len();
        // Reused per particle: the camera-relative polyline + its cumulative lengths.
        let mut pts: Vec<Vec3> = Vec::new();
        let mut cum: Vec<f32> = Vec::new();
        inst.sample_track_indexed(ti, |i, s| {
            pts.clear();
            for pt in &hists[i] {
                pts.push(xf.transform_point3(pt.truncate()));
            }
            // The live head: the particle's current position (history only samples
            // every `min_distance`, so the ribbon must still reach the particle).
            let head = xf.transform_point3(s.pos);
            if pts.last().is_none_or(|l| (head - *l).length_squared() > 1e-10) {
                pts.push(head);
            }
            if pts.len() < 2 {
                return;
            }
            // Normalized arc length 0 (tail) → 1 (head) drives UV v and the taper.
            cum.clear();
            cum.push(0.0);
            for w in pts.windows(2) {
                cum.push(cum.last().unwrap() + (w[1] - w[0]).length());
            }
            let total = *cum.last().unwrap();
            if total <= 1e-6 {
                return;
            }
            for k in 0..pts.len() - 1 {
                let (r0, r1) = (cum[k] / total, cum[k + 1] / total);
                // Fade tapers width AND alpha by the segment's mid ribbon-coord
                // (0 at the tail → 1 at the head).
                let taper = if trail.fade { 0.5 * (r0 + r1) } else { 1.0 };
                let mut color = s.color;
                color[3] *= taper;
                instances.push(ribbon_segment(
                    pts[k],
                    pts[k + 1],
                    head_w * taper,
                    color,
                    r0,
                    r1,
                    cam_right,
                ));
            }
        });
        if instances.len() == start {
            continue;
        }
        draws.push(BillboardDraw {
            // A trail-specific texture wins; else the ribbon shares the track's.
            texture: trail.texture.clone().or_else(|| texture.clone()),
            blend: particle_blend(ct.look.blend),
            range: start as u32..instances.len() as u32,
        });
    }
}

/// Pack every beam track of `inst` as one origin→endpoint ribbon, appending one
/// [`BillboardDraw`] per track. The chain runs from the effect origin to the
/// track's endpoint (script override via `set_beam_end`, else the authored
/// `beam_end`), subdivided into `segments` camera-facing quads. Width and color
/// come from the track's `size`/`color` properties sampled at the EFFECT's
/// normalized time; `wave_amplitude`/`wave_frequency` add a time-animated sine
/// ripple (pinned at both endpoints), and `scroll` flows the texture along the
/// beam (segments wrap their `v` slice together, so a seam only ever lands inside
/// one segment — use a seamlessly tiling beam texture).
pub fn collect_beams(
    inst: &EffectInstance,
    local_xf: Mat4,
    world_xf: Mat4,
    cam_right: Vec3,
    instances: &mut Vec<ParticleInstance>,
    draws: &mut Vec<BillboardDraw>,
) {
    // A finished one-shot instance can linger inertly (hosts keep the entry so a
    // re-spawn scan can't loop it) — its beams must vanish with the effect unless
    // it explicitly persists.
    if inst.effect.playback == Playback::OneShot
        && inst.t >= inst.effect.lifetime
        && inst.effect.end == EndBehavior::Destroy
    {
        return;
    }
    let un = (inst.t / inst.effect.lifetime.max(1e-3)).clamp(0.0, 1.0);
    for (ti, ct) in inst.beam_tracks() {
        let RenderMode::Beam { texture } = &ct.look.render else { continue };
        let xf = if ct.space == Space::World { world_xf } else { local_xf };
        let scale = {
            let m = glam_mat3_scale(&xf);
            (m.0 + m.1 + m.2) / 3.0
        };
        let width = ct.size.sample(un).max(0.0) * scale;
        let color = ct.color.sample(un);
        if width <= 0.0 || color[3] <= 0.0 {
            continue;
        }
        let origin = xf.transform_point3(Vec3::ZERO);
        let end = xf.transform_point3(inst.beam_end(ti));
        let axis = (end - origin).normalize_or_zero();
        if axis == Vec3::ZERO {
            continue;
        }
        // The ripple displaces in the camera-facing plane so it always reads.
        let mid_view = (0.5 * (origin + end)).normalize_or_zero();
        let mut ripple_dir = mid_view.cross(axis);
        ripple_dir =
            if ripple_dir.length_squared() < 1e-10 { cam_right } else { ripple_dir.normalize() };
        let n = ct.segments.max(1);
        let amp = ct.wave_amplitude * scale;
        let point = |k: u32| -> Vec3 {
            let u = k as f32 / n as f32;
            let mut p = origin.lerp(end, u);
            if amp != 0.0 {
                // `wave_frequency` cycles along the chain, phase advancing one full
                // cycle per second; sin(πu) pins both endpoints in place.
                let phase = std::f32::consts::TAU * (u * ct.wave_frequency + inst.t);
                p += ripple_dir * (amp * phase.sin() * (std::f32::consts::PI * u).sin());
            }
            p
        };
        // Scroll: shift the whole chain's v coordinate, wrapping each segment's
        // slice as a pair so its gradient never runs backwards across the seam.
        let shift = (inst.t * ct.scroll).rem_euclid(1.0);
        let start = instances.len();
        let mut prev = point(0);
        for k in 0..n {
            let next = point(k + 1);
            let r0 = (k as f32 / n as f32 - shift).rem_euclid(1.0);
            let r1 = r0 + 1.0 / n as f32;
            instances.push(ribbon_segment(prev, next, width, color, r0, r1, cam_right));
            prev = next;
        }
        draws.push(BillboardDraw {
            texture: texture.clone(),
            blend: particle_blend(ct.look.blend),
            range: start as u32..instances.len() as u32,
        });
    }
}

/// The GPU blend mode for an authoring [`Blend`].
fn particle_blend(b: Blend) -> ParticleBlend {
    match b {
        Blend::Alpha => ParticleBlend::Alpha,
        Blend::Additive => ParticleBlend::Additive,
        Blend::Premultiplied => ParticleBlend::Premultiplied,
        Blend::Screen => ParticleBlend::Screen,
        Blend::Multiply => ParticleBlend::Multiply,
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
    use crate::effect::{Clip, Emit, Look, ParticleEffect, Playback, Track};
    use std::sync::Arc;

    /// A single-burst clip firing `count` at t=0 whose particles live `life`.
    fn burst_clip(count: u32, life: f32) -> Clip {
        Clip {
            start: 0.0,
            end: life,
            lifetime_jitter: 0.0,
            emit: Emit::Burst { count, count_jitter: 0.0, pulses: 1, interval: 0.0, interval_jitter: 0.0 },
        }
    }

    #[test]
    fn collect_packs_and_sorts_alpha_back_to_front() {
        let fx = Arc::new(
            ParticleEffect {
                lifetime: 1.0,
                playback: Playback::OneShot,
                tracks: vec![Track {
                    clips: vec![burst_clip(20, 5.0)],
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
                        clips: vec![burst_clip(16, 5.0)],
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
                        clips: vec![burst_clip(1, 5.0)],
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

    /// Reconstruct a ribbon segment's endpoints from its packed instance: the up
    /// basis IS the segment vector (size.y = 1), the position its midpoint.
    fn segment_ends(p: &ParticleInstance) -> (Vec3, Vec3) {
        let mid = Vec3::new(p.pos_rot[0], p.pos_rot[1], p.pos_rot[2]);
        let seg = Vec3::new(p.basis_up[0], p.basis_up[1], p.basis_up[2]);
        (mid - 0.5 * seg, mid + 0.5 * seg)
    }

    #[test]
    fn beam_track_generates_a_connected_segment_chain() {
        use crate::curve::ValueOrCurve;
        use crate::effect::{Look, RenderMode};
        let fx = Arc::new(
            ParticleEffect {
                lifetime: 1.0,
                playback: Playback::Looping,
                tracks: vec![Track {
                    look: Look { render: RenderMode::Beam { texture: None }, ..Look::default() },
                    size: ValueOrCurve::constant(0.5),
                    segments: 8,
                    beam_end: Vec3::new(0.0, 4.0, 0.0),
                    ..Track::default()
                }],
                ..ParticleEffect::default()
            }
            .compile(),
        );
        let mut inst = EffectInstance::new(fx, 1);
        inst.simulate_to(0.25, Vec3::ZERO);
        let (mut packed, mut draws) = (Vec::new(), Vec::new());
        // Push the beam away from the camera so the facing cross-product is stable.
        let xf = Mat4::from_translation(Vec3::new(3.0, 0.0, 5.0));
        collect_beams(&inst, xf, xf, Vec3::X, &mut packed, &mut draws);

        assert_eq!(draws.len(), 1);
        assert_eq!(packed.len(), 8, "one quad per segment");
        assert_eq!(draws[0].range, 0..8);
        // The chain spans origin → beam_end (both in the xf'd space)…
        let (a0, _) = segment_ends(&packed[0]);
        let (_, b_last) = segment_ends(&packed[7]);
        assert!((a0 - Vec3::new(3.0, 0.0, 5.0)).length() < 1e-4, "starts at the origin: {a0}");
        assert!((b_last - Vec3::new(3.0, 4.0, 5.0)).length() < 1e-4, "ends at beam_end: {b_last}");
        // …with every segment welded to the next, width from `size`, and the UV v
        // slice walking 0→1 along the chain (r1 of segment k = (k+1)/8 in size.w).
        for k in 0..7 {
            let (_, b) = segment_ends(&packed[k]);
            let (a, _) = segment_ends(&packed[k + 1]);
            assert!((b - a).length() < 1e-4, "segment {k} not welded to {}", k + 1);
        }
        for (k, p) in packed.iter().enumerate() {
            assert!((p.size[0] - 0.5).abs() < 1e-5, "width from the size property");
            let r1 = (k + 1) as f32 / 8.0;
            assert!((p.size[3] - r1).abs() < 1e-5, "v slice head at {r1}, got {}", p.size[3]);
            assert!((p.basis_up[3] - (-1.0 / 8.0)).abs() < 1e-5, "dv spans one slice");
            // Camera-facing: the right axis is unit and perpendicular to the segment.
            let r = Vec3::new(p.basis_right[0], p.basis_right[1], p.basis_right[2]);
            let s = Vec3::new(p.basis_up[0], p.basis_up[1], p.basis_up[2]);
            assert!((r.length() - 1.0).abs() < 1e-4);
            assert!(r.dot(s).abs() < 1e-4, "right must be perpendicular to the segment");
        }
    }

    #[test]
    fn finished_oneshot_beam_vanishes_unless_persisting() {
        use crate::effect::{Look, RenderMode};
        // A OneShot+Destroy beam past its lifetime must stop drawing (hosts keep
        // finished node instances around inertly); Persist keeps the frozen beam.
        let mk = |end: EndBehavior| {
            Arc::new(
                ParticleEffect {
                    lifetime: 0.5,
                    playback: Playback::OneShot,
                    end,
                    tracks: vec![Track {
                        look: Look { render: RenderMode::Beam { texture: None }, ..Look::default() },
                        ..Track::default()
                    }],
                    ..ParticleEffect::default()
                }
                .compile(),
            )
        };
        let xf = Mat4::from_translation(Vec3::new(0.0, 0.0, 5.0));
        for (end, expect) in [(EndBehavior::Destroy, 0), (EndBehavior::Persist, 12)] {
            let mut inst = EffectInstance::new(mk(end), 1);
            inst.simulate_to(2.0, Vec3::ZERO); // well past the 0.5 s lifetime
            let (mut packed, mut draws) = (Vec::new(), Vec::new());
            collect_beams(&inst, xf, xf, Vec3::X, &mut packed, &mut draws);
            assert_eq!(packed.len(), expect, "{end:?} beam past lifetime");
        }
    }

    #[test]
    fn beam_wave_ripples_but_pins_both_endpoints() {
        use crate::effect::{Look, RenderMode};
        let fx = Arc::new(
            ParticleEffect {
                lifetime: 1.0,
                playback: Playback::Looping,
                tracks: vec![Track {
                    look: Look { render: RenderMode::Beam { texture: None }, ..Look::default() },
                    segments: 12,
                    beam_end: Vec3::new(0.0, 6.0, 0.0),
                    wave_amplitude: 0.8,
                    wave_frequency: 3.0,
                    ..Track::default()
                }],
                ..ParticleEffect::default()
            }
            .compile(),
        );
        let mut inst = EffectInstance::new(fx, 1);
        inst.simulate_to(0.37, Vec3::ZERO);
        let (mut packed, mut draws) = (Vec::new(), Vec::new());
        let xf = Mat4::from_translation(Vec3::new(0.0, 0.0, 8.0));
        collect_beams(&inst, xf, xf, Vec3::X, &mut packed, &mut draws);
        let (a0, _) = segment_ends(&packed[0]);
        let (_, b_last) = segment_ends(&packed[11]);
        assert!((a0 - Vec3::new(0.0, 0.0, 8.0)).length() < 1e-4, "wave must pin the origin");
        assert!((b_last - Vec3::new(0.0, 6.0, 8.0)).length() < 1e-4, "wave must pin the end");
        // The interior actually ripples off the straight line.
        let off_axis = packed.iter().skip(2).take(8).any(|p| {
            let mid = Vec3::new(p.pos_rot[0], p.pos_rot[1], p.pos_rot[2]);
            (Vec3::new(mid.x, 0.0, mid.z) - Vec3::new(0.0, 0.0, 8.0)).length() > 0.05
        });
        assert!(off_axis, "wave amplitude must displace interior points");
    }

    #[test]
    fn trail_ribbon_connects_tail_to_head_and_fades() {
        use crate::curve::{Value, ValueOrCurve};
        use crate::effect::Trail;
        // One particle rising steadily with a fading trail: the ribbon must be a
        // connected tail→head chain whose alpha (and width) grow toward the head,
        // ending exactly at the particle's current position.
        let fx = Arc::new(
            ParticleEffect {
                lifetime: 1.0,
                playback: Playback::OneShot,
                tracks: vec![Track {
                    clips: vec![burst_clip(1, 5.0)],
                    velocity: ValueOrCurve::Const(Value::Vec3(Vec3::new(0.0, 2.0, 0.0))),
                    trail: Some(Trail {
                        time: 1.0,
                        width: 0.2,
                        fade: true,
                        texture: Some("vfx/streak.png".into()),
                        min_distance: 0.05,
                    }),
                    ..Track::default()
                }],
                ..ParticleEffect::default()
            }
            .compile(),
        );
        let mut inst = EffectInstance::new(fx, 1);
        inst.simulate_to(0.5, Vec3::ZERO);
        let (mut packed, mut draws) = (Vec::new(), Vec::new());
        let xf = Mat4::from_translation(Vec3::new(0.0, 0.0, 6.0));
        collect_trails(&inst, xf, xf, Vec3::X, &mut packed, &mut draws);

        assert!(packed.len() >= 2, "a moving particle must leave a multi-segment ribbon");
        assert_eq!(draws.len(), 1);
        assert_eq!(draws[0].texture.as_deref(), Some("vfx/streak.png"), "trail texture wins");
        for k in 0..packed.len() - 1 {
            let (_, b) = segment_ends(&packed[k]);
            let (a, _) = segment_ends(&packed[k + 1]);
            assert!((b - a).length() < 1e-4, "ribbon must stay connected at joint {k}");
        }
        // Head reaches the particle's current position.
        let mut head_pos = Vec3::ZERO;
        inst.sample_track(0, |s| head_pos = s.pos);
        let (_, ribbon_head) = segment_ends(packed.last().unwrap());
        assert!((ribbon_head - xf.transform_point3(head_pos)).length() < 1e-4);
        // Fade: alpha and width strictly grow tail → head.
        for w in packed.windows(2) {
            assert!(w[1].color[3] > w[0].color[3], "alpha must grow toward the head");
            assert!(w[1].size[0] > w[0].size[0], "width must grow toward the head");
        }
        // UV v walks 0 → 1 tail → head (v at a segment's tail end = size.w + basis_up.w).
        let first_tail_v = packed[0].size[3] + packed[0].basis_up[3];
        let last_head_v = packed.last().unwrap().size[3];
        assert!(first_tail_v.abs() < 1e-5, "tail v = 0, got {first_tail_v}");
        assert!((last_head_v - 1.0).abs() < 1e-5, "head v = 1, got {last_head_v}");
        // No-fade + no trail texture: constant width and the track texture fallback.
        let mut track2 = Track {
            clips: vec![burst_clip(1, 5.0)],
            velocity: ValueOrCurve::Const(Value::Vec3(Vec3::new(0.0, 2.0, 0.0))),
            trail: Some(Trail { fade: false, texture: None, ..Trail::default() }),
            ..Track::default()
        };
        track2.look.render = RenderMode::Billboard { texture: Some("vfx/dot.png".into()) };
        let fx2 = Arc::new(
            ParticleEffect {
                lifetime: 1.0,
                playback: Playback::OneShot,
                tracks: vec![track2],
                ..ParticleEffect::default()
            }
            .compile(),
        );
        let mut inst2 = EffectInstance::new(fx2, 1);
        inst2.simulate_to(0.5, Vec3::ZERO);
        let (mut packed2, mut draws2) = (Vec::new(), Vec::new());
        collect_trails(&inst2, xf, xf, Vec3::X, &mut packed2, &mut draws2);
        assert_eq!(draws2[0].texture.as_deref(), Some("vfx/dot.png"), "falls back to the track texture");
        for p in &packed2 {
            assert!((p.size[0] - 0.15).abs() < 1e-5, "no fade = constant width");
        }
    }

    #[test]
    fn mesh_tracks_collect_one_model_matrix_per_particle() {
        use crate::effect::RenderMode;
        let fx = Arc::new(
            ParticleEffect {
                lifetime: 1.0,
                playback: Playback::OneShot,
                tracks: vec![
                    // A billboard track (ignored by mesh collection)...
                    Track { clips: vec![burst_clip(3, 5.0)], ..Track::default() },
                    // ...and a mesh track that should yield a MeshDraw.
                    Track {
                        clips: vec![burst_clip(5, 5.0)],
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
