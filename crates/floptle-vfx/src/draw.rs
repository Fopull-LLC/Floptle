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
/// `render_matrix`) — used by `Space::Local` tracks; `world_xf` maps the
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
        let speed_stretch = ct.look.speed_stretch.max(0.0);
        let flip = ct.look.flipbook;
        let soft = ct.look.soft.max(0.0);
        let lit = if ct.look.lit { 1.0 } else { 0.0 };
        // The spare lane is the distortion strength for the Distortion blend,
        // which has its own fragment stage, and the translucency for the rest.
        let spare = if ct.look.blend == Blend::Distortion { ct.look.distortion } else { ct.look.translucency };
        let start = instances.len();
        inst.sample_track(ti, |s| {
            let world = xf.transform_point3(s.pos);
            let base = s.size * scale;
            // Width takes the aspect ratio and the squash; height takes the size
            // over the squash (velocity stretch rides the up-vector length, so the
            // shader needs no stretch term).
            let (w, h) = (base * aspect * s.squash, base / s.squash);
            let (right, up, spin) = billboard_basis(
                orient,
                &xf,
                world,
                &s,
                (cam_right, cam_up, inst.up),
                (stretch, speed_stretch),
            );
            // Flipbook UV sub-rect [min_u, min_v, du, dv] packed into the spare
            // channels (full quad [0,0,1,1] when there's no flipbook).
            let (uv, next, t) = flipbook_uv(flip, &s);
            let effect_up = inst.up;
            instances.push(ParticleInstance {
                pos_rot: [world.x, world.y, world.z, spin],
                size: [w, h, uv[0], uv[1]],
                color: s.color,
                basis_right: [right.x, right.y, right.z, uv[2]],
                basis_up: [up.x, up.y, up.z, uv[3]],
                params: [soft, 0.0, 0.0, 0.0],
                light: [effect_up.x, effect_up.y, effect_up.z, lit],
                extra: [next[0], next[1], t, spare],
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
/// lets trails and beams ride the existing billboard pass with no new pipeline: the
/// quad's up axis is the segment vector `b - a` (with `size.y = 1` it spans the
/// full segment around the midpoint), its right axis faces the camera via
/// `cross(view_dir, segment_dir)` (falling back to `cam_right` when the segment
/// points at the camera), and the flipbook UV-rect lanes carry the ribbon's
/// along-length `v` slice instead — `v = r0` at the `a` end, `r1` at the `b` end,
/// `u` running 0→1 across the width. (Ribbon slicing and flipbooks are therefore
/// mutually exclusive; trails/beams simply don't use flipbooks.)
#[allow(clippy::too_many_arguments)]
fn ribbon_segment(
    a: Vec3,
    b: Vec3,
    width: f32,
    color: [f32; 4],
    r0: f32,
    r1: f32,
    cam_right: Vec3,
    soft: f32,
) -> ParticleInstance {
    let mid = 0.5 * (a + b);
    let seg = b - a;
    let dir = seg.normalize_or_zero();
    // Camera-relative space: the camera sits at the origin, so the view
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
        params: [soft, 0.0, 0.0, 0.0],
        ..Default::default()
    }
}

/// How far a mitred corner may slide, in ribbon widths. Past it a sharp kink
/// keeps a small notch rather than throwing a spike: the usual mitre limit.
const MITRE_LIMIT: f32 = 1.0;

/// `extra.z` of a ribbon segment that carries its mitres: how far each corner
/// slides along the segment, `+right` tail and head in `extra.xy`, `−right`
/// tail in `size.z` and head in `extra.w` (see `particles.wgsl`). Negative, so
/// it cannot be a flipbook's crossfade amount.
const MITRED: f32 = -1.0;

/// How far the corners of the two segments meeting at joint `b` slide along
/// their own segments so that, seen from the camera, each side lands on one
/// point: `[(+right: the a→b end, the b→c start), (−right: the same)]`. The
/// pair then joins on the mitre line, with no wedge between their square
/// ends. Solved along the line of sight through each corner, because that is
/// where a gap shows, and each side on its own, because a beam thrown
/// sideways in depth is not flat. Capped at [`MITRE_LIMIT`] widths, so a
/// hairpin cannot throw a spike across the screen.
fn mitre_shifts(a: Vec3, b: Vec3, c: Vec3, width: f32, cam_right: Vec3) -> [(f32, f32); 2] {
    let right_of = |p: Vec3, q: Vec3| {
        let dir = (q - p).normalize_or_zero();
        let r = (0.5 * (p + q)).normalize_or_zero().cross(dir);
        (dir, if r.length_squared() < 1e-10 { cam_right } else { r.normalize() })
    };
    let ((d0, r0), (d1, r1)) = (right_of(a, b), right_of(b, c));
    let side = |sign: f32| {
        // side·r0·w/2 + d0·s0 = side·r1·w/2 + d1·s1 + t·(the view ray there),
        // with the ray re-aimed at the corner it found, twice: the corner of a
        // sharp kink lands well off the joint, and so does its line of sight.
        let mut corner = b + (r0 + r1) * (0.25 * width * sign);
        let mut x = floptle_core::math::Vec3::ZERO;
        for _ in 0..3 {
            let m = floptle_core::math::Mat3::from_cols(d0, -d1, -corner.normalize_or_zero());
            if m.determinant().abs() < 1e-6 {
                return (0.0, 0.0);
            }
            x = m.inverse() * ((r1 - r0) * (0.5 * width * sign));
            corner = b + r0 * (0.5 * width * sign) + d0 * x.x;
        }
        let cap = MITRE_LIMIT * width;
        (x.x.clamp(-cap, cap), x.y.clamp(-cap, cap))
    };
    [side(1.0), side(-1.0)]
}

/// Pack every trailed billboard track of `inst` as connected ribbon segments,
/// appending one [`BillboardDraw`] per non-empty track. Each live particle with ≥ 2
/// polyline points (its recorded history plus its current position as the head)
/// contributes `points − 1` quads, colored with the particle's current color; when
/// the trail fades, width and alpha taper to zero at the tail. Segments are pushed
/// tail→head and not depth-sorted — a ribbon must keep its connected
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
        // The history's own frame: the world matrix for a history kept in
        // anchor-relative world space (a World track, or an emitter-path trail),
        // the node matrix otherwise. The live head is always the particle's
        // position through its track's frame.
        let head_xf = if ct.space == Space::World { world_xf } else { local_xf };
        let hist_xf = if ct.trail_in_world() { world_xf } else { local_xf };
        let scale = {
            let m = glam_mat3_scale(&head_xf);
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
                pts.push(hist_xf.transform_point3(pt.truncate()));
            }
            // The live head: the particle's current position (history only samples
            // every `min_distance`, so the ribbon must still reach the particle).
            let head = head_xf.transform_point3(s.pos);
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
                // Fade tapers width and alpha by the segment's mid ribbon-coord
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
                    ct.look.soft.max(0.0),
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
/// come from the track's `size`/`color` properties sampled at the effect's
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
        let curved = inst.beam_control(ti).is_some();
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
        let jitter = ct.jitter * scale;
        let side = axis.cross(ripple_dir).normalize_or_zero();
        let seed = inst.beam_jitter_seed(ti);
        let point = |k: u32| -> Vec3 {
            let u = k as f32 / n as f32;
            // The chord, or the curve through the control point's pull: an
            // affine map keeps a quadratic curve one, so the local curve is
            // carried out whole.
            let mut p = if curved { xf.transform_point3(inst.beam_point(ti, u)) } else { origin.lerp(end, u) };
            if amp != 0.0 {
                // `wave_frequency` cycles along the chain, phase advancing one full
                // cycle per second; sin(πu) pins both endpoints in place.
                let phase = std::f32::consts::TAU * (u * ct.wave_frequency + inst.t);
                p += ripple_dir * (amp * phase.sin() * (std::f32::consts::PI * u).sin());
            }
            if jitter != 0.0 {
                let [a, b] = jitter_offset(seed, k);
                p += (ripple_dir * a + side * b) * (jitter * (std::f32::consts::PI * u).sin().max(0.0).sqrt());
            }
            p
        };
        // Scroll: shift the whole chain's v coordinate, wrapping each segment's
        // slice as a pair so its gradient never runs backwards across the seam.
        let shift = (inst.t * ct.scroll).rem_euclid(1.0);
        let start = instances.len();
        let pts: Vec<Vec3> = (0..=n).map(point).collect();
        for k in 0..n as usize {
            let r0 = (k as f32 / n as f32 - shift).rem_euclid(1.0);
            let r1 = r0 + 1.0 / n as f32;
            let mut seg = ribbon_segment(pts[k], pts[k + 1], width, color, r0, r1, cam_right, ct.look.soft.max(0.0));
            // Mitre both ends against the neighbours, so the chain is one
            // ribbon: without it each kink left a wedge uncovered on its
            // outside and doubled on its inside.
            // A distortion beam keeps `extra.w` for its strength, and square ends.
            if ct.look.blend != crate::effect::Blend::Distortion {
                let none = [(0.0, 0.0); 2];
                let tail = if k > 0 { mitre_shifts(pts[k - 1], pts[k], pts[k + 1], width, cam_right) } else { none };
                let head =
                    if k + 1 < n as usize { mitre_shifts(pts[k], pts[k + 1], pts[k + 2], width, cam_right) } else { none };
                // A side may not fold over itself: on the inside of a tight
                // kink in a wide ribbon both ends slide toward each other, and
                // past the segment's own length the quad turns into a bow tie
                // that streaks across the screen. Such a side shortens to a
                // point and the notch it leaves is the kink's.
                let len = (pts[k + 1] - pts[k]).length();
                let unfold = |t: f32, h: f32| {
                    let over = t - h - len;
                    if over > 0.0 && t - h > 1e-6 {
                        let k = len / (t - h);
                        (t * k, h * k)
                    } else {
                        (t, h)
                    }
                };
                let (tp, hp) = unfold(tail[0].1, head[0].0);
                let (tm, hm) = unfold(tail[1].1, head[1].0);
                // +right corners in extra.xy, −right in size.z (a ribbon's
                // u always starts at 0) and extra.w.
                seg.extra = [tp, hp, MITRED, hm];
                seg.size[2] = tm;
            }
            instances.push(seg);
        }
        draws.push(BillboardDraw {
            texture: texture.clone(),
            blend: particle_blend(ct.look.blend),
            range: start as u32..instances.len() as u32,
        });
    }
}

/// Where chain point `k` of a jittered beam is thrown, as two sideways
/// components in `[-1, 1]`: a coarse walk through every fourth point carries
/// the channel's big turns and a per-point kick its kinks, which is what makes
/// a bolt read as a bolt rather than as noise. The same seed and point always
/// land in the same place, so a frame is stable until the seed is thrown again.
fn jitter_offset(seed: u32, k: u32) -> [f32; 2] {
    const STRIDE: u32 = 4;
    let r = |i: u32, salt: u32| (crate::sim::hash(seed ^ crate::sim::hash(i.wrapping_mul(4).wrapping_add(salt))) >> 8) as f32
        / (1u32 << 23) as f32
        - 1.0;
    let (i0, f) = (k / STRIDE, (k % STRIDE) as f32 / STRIDE as f32);
    let coarse = |salt: u32| r(i0, salt) + (r(i0 + 1, salt) - r(i0, salt)) * f;
    [0.7 * coarse(0) + 0.3 * r(k, 2), 0.7 * coarse(1) + 0.3 * r(k, 3)]
}

/// The GPU blend mode for an authoring [`Blend`].
fn particle_blend(b: Blend) -> ParticleBlend {
    match b {
        Blend::Alpha => ParticleBlend::Alpha,
        Blend::Additive => ParticleBlend::Additive,
        Blend::Premultiplied => ParticleBlend::Premultiplied,
        Blend::Screen => ParticleBlend::Screen,
        Blend::Multiply => ParticleBlend::Multiply,
        Blend::Distortion => ParticleBlend::Distortion,
    }
}

/// The UV sub-rect `[min_u, min_v, du, dv]` a particle samples from a flipbook atlas
/// this frame — the full quad `[0, 0, 1, 1]` when the track has no flipbook — with
/// the next frame's UV min and how far into it the particle is, for a track that
/// crossfades its frames (`0` for one that steps). The frame comes from the
/// particle's age (over its life, or a fixed-fps loop).
fn flipbook_uv(flip: Option<Flipbook>, s: &ParticleSample) -> ([f32; 4], [f32; 2], f32) {
    let Some(fb) = flip else { return ([0.0, 0.0, 1.0, 1.0], [0.0, 0.0], 0.0) };
    let (cols, rows) = (fb.cols.max(1), fb.rows.max(1));
    let n = cols * rows;
    if n <= 1 {
        return ([0.0, 0.0, 1.0, 1.0], [0.0, 0.0], 0.0);
    }
    let (pos, looping) = match fb.mode {
        FlipMode::OverLife => (s.age01.clamp(0.0, 1.0) * n as f32, false),
        FlipMode::LoopFps => (s.age.max(0.0) * fb.fps.max(0.0), true),
    };
    let f = (pos as u32 % n).min(n - 1);
    // Over a life the last frame holds; a loop wraps to the first.
    let next = if looping { (f + 1) % n } else { (f + 1).min(n - 1) };
    let t = if fb.blend && next != f { pos.fract() } else { 0.0 };
    let (du, dv) = (1.0 / cols as f32, 1.0 / rows as f32);
    let min = |f: u32| [(f % cols) as f32 * du, (f / cols) as f32 * dv];
    let here = min(f);
    ([here[0], here[1], du, dv], min(next), t)
}

/// The world-space in-plane basis (+X width axis, +Y height axis) a particle's quad
/// spans, plus the roll spin to apply, for the track's [`BillboardOrient`]. All
/// vectors are camera-relative (the camera sits at the origin), so
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
    (cam_right, cam_up, world_up): (Vec3, Vec3, Vec3),
    (stretch, speed_stretch): (f32, f32),
) -> (Vec3, Vec3, f32) {
    let world_up = world_up.normalize_or(Vec3::Y);
    const EPS: f32 = 1e-6;
    let view_dir = world_pos.normalize_or_zero();
    match orient {
        // Classic billboard: the camera basis, spun by roll.
        BillboardOrient::FaceCamera => (cam_right, cam_up, s.rotation.z),
        // Stretched along motion: up = velocity (scaled by stretch, plus the speed
        // term), width faces the camera around that axis. Roll is meaningless
        // here, so it's dropped.
        BillboardOrient::Velocity => {
            let vel = xf.transform_vector3(s.velocity);
            let stretch = stretch + vel.length() * speed_stretch;
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
        // Upright: along the effect's up (world +Y unless the host says
        // otherwise, as on a round world), yawing to the camera. Roll would tip
        // it, so 0.
        BillboardOrient::Vertical => {
            let up = world_up;
            let mut right = up.cross(view_dir);
            if right.length_squared() < EPS {
                // Looking straight down the up axis — use the camera right, flattened.
                right = cam_right - up * cam_right.dot(up);
                if right.length_squared() < EPS {
                    right = up.any_orthonormal_vector();
                }
            }
            (right.normalize(), up, 0.0)
        }
        // Flat on the ground (normal = the effect's up); roll spins it in the
        // ground plane.
        BillboardOrient::Horizontal => {
            let lay = Quat::from_rotation_arc(Vec3::Y, world_up);
            (lay * Vec3::X, lay * Vec3::Z, s.rotation.z)
        }
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
/// SDF-ao'd exactly like scene meshes (proposal §5.2).
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
            // Squash keeps the volume: the two lateral axes take it, the local Y
            // gives it back.
            let sz = (p.size * scale).max(1e-4);
            let model = Mat4::from_scale_rotation_translation(
                Vec3::new(sz * p.squash, sz / p.squash, sz * p.squash),
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

    /// On a round world the effect's up is not world +Y: an upright billboard
    /// stands along it and a flat one lies across it, rather than tipping over.
    #[test]
    fn upright_and_flat_billboards_follow_the_effects_up() {
        use crate::effect::{BillboardOrient, Look};
        let up = Vec3::new(1.0, 0.0, 0.0);
        for orient in [BillboardOrient::Vertical, BillboardOrient::Horizontal] {
            let fx = Arc::new(
                ParticleEffect {
                    lifetime: 1.0,
                    playback: Playback::OneShot,
                    tracks: vec![Track {
                        clips: vec![burst_clip(4, 5.0)],
                        shape: crate::effect::EmitShape::Sphere { radius: 1.0, shell: true },
                        look: Look { orient, ..Look::default() },
                        ..Track::default()
                    }],
                    ..ParticleEffect::default()
                }
                .compile(),
            );
            let mut inst = EffectInstance::new(fx, 3);
            inst.up = up;
            inst.simulate_to(0.1, Vec3::ZERO);
            let (mut packed, mut draws) = (Vec::new(), Vec::new());
            let at = Mat4::from_translation(Vec3::new(0.0, 0.0, -10.0));
            collect_billboards(&inst, at, at, Vec3::NEG_Z, Vec3::X, Vec3::Y, &mut packed, &mut draws);
            for p in &packed {
                let r = Vec3::new(p.basis_right[0], p.basis_right[1], p.basis_right[2]).normalize();
                let u = Vec3::new(p.basis_up[0], p.basis_up[1], p.basis_up[2]).normalize();
                match orient {
                    BillboardOrient::Vertical => assert!(u.dot(up) > 0.999, "upright along {u}, not the effect's up"),
                    _ => assert!(r.cross(u).normalize().dot(up).abs() > 0.999, "flat across {}, not the effect's up", r.cross(u)),
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
            squash: 1.0,
            rotation: Vec3::ZERO,
            color: [1.0; 4],
            age: 0.0,
            age01,
        };
        // No flipbook → the full quad.
        assert_eq!(flipbook_uv(None, &s(0.5)).0, [0.0, 0.0, 1.0, 1.0]);
        let fb = Some(Flipbook { cols: 4, rows: 4, mode: FlipMode::OverLife, fps: 12.0, blend: false });
        // Frame 0 at birth: top-left cell, 1/4 wide/tall.
        assert_eq!(flipbook_uv(fb, &s(0.0)).0, [0.0, 0.0, 0.25, 0.25]);
        // Just before death: last cell (frame 15) → col 3, row 3.
        let last = flipbook_uv(fb, &s(0.999)).0;
        assert!((last[0] - 0.75).abs() < 1e-6 && (last[1] - 0.75).abs() < 1e-6, "{last:?}");
        // Mid-life (frame 8) → col 0, row 2.
        let mid = flipbook_uv(fb, &s(0.5)).0;
        assert!((mid[0] - 0.0).abs() < 1e-6 && (mid[1] - 0.5).abs() < 1e-6, "{mid:?}");
        // Stepping, a frame never crossfades.
        assert_eq!(flipbook_uv(fb, &s(0.53)).2, 0.0);
    }

    /// A blending flipbook crossfades each frame into the next, so a slow
    /// flipbook plays smoothly: partway through frame 8 it names frame 9 and
    /// how far in it is. The last frame of a life holds; a loop wraps.
    #[test]
    fn a_blending_flipbook_crossfades_into_the_next_frame() {
        use crate::effect::{FlipMode, Flipbook};
        let s = |age: f32, age01: f32| ParticleSample {
            pos: Vec3::ZERO,
            velocity: Vec3::ZERO,
            frame: Quat::IDENTITY,
            size: 1.0,
            squash: 1.0,
            rotation: Vec3::ZERO,
            color: [1.0; 4],
            age,
            age01,
        };
        let life = Some(Flipbook { cols: 4, rows: 4, mode: FlipMode::OverLife, fps: 12.0, blend: true });
        // 8.25 frames in: frame 8 (col 0, row 2) a quarter of the way to 9.
        let (here, next, t) = flipbook_uv(life, &s(0.0, 8.25 / 16.0));
        assert!((here[0], here[1]) == (0.0, 0.5), "{here:?}");
        assert!((next[0] - 0.25).abs() < 1e-6 && (next[1] - 0.5).abs() < 1e-6, "{next:?}");
        assert!((t - 0.25).abs() < 1e-4, "{t}");
        // The last frame of a life has nothing after it.
        assert_eq!(flipbook_uv(life, &s(0.0, 0.999)).2, 0.0);
        // A loop at 12 fps, 15.5 frames in: frame 15 halfway back to frame 0.
        let looped = Some(Flipbook { cols: 4, rows: 4, mode: FlipMode::LoopFps, fps: 12.0, blend: true });
        let (_, next, t) = flipbook_uv(looped, &s(15.5 / 12.0, 0.0));
        assert_eq!(next, [0.0, 0.0]);
        assert!((t - 0.5).abs() < 1e-3, "{t}");
    }

    /// Reconstruct a ribbon segment's endpoints from its packed instance: the up
    /// basis is the segment vector (size.y = 1), the position its midpoint.
    fn segment_ends(p: &ParticleInstance) -> (Vec3, Vec3) {
        let mid = Vec3::new(p.pos_rot[0], p.pos_rot[1], p.pos_rot[2]);
        let seg = Vec3::new(p.basis_up[0], p.basis_up[1], p.basis_up[2]);
        (mid - 0.5 * seg, mid + 0.5 * seg)
    }

    #[test]
    fn emitter_path_ribbon_lies_along_the_nodes_world_path() {
        use crate::curve::{Value, ValueOrCurve};
        use crate::effect::{Space, Trail};
        use floptle_core::math::{DVec3, Quat};
        use floptle_core::transform::Transform;
        // A still Local particle on a node that slides 3 u along world +X while
        // turned a quarter turn about Y. Its emitter-path history is world points,
        // so the ribbon must run along world X behind the node's current place —
        // through the node matrix it would come out swung 90° along Z instead.
        let track = Track {
            clips: vec![burst_clip(1, 5.0)],
            velocity: ValueOrCurve::Const(Value::Vec3(Vec3::ZERO)),
            space: Space::Local,
            trail: Some(Trail { time: 5.0, width: 0.1, min_distance: 0.05, emitter_path: true, ..Trail::default() }),
            ..Track::default()
        };
        let fx = Arc::new(
            ParticleEffect { lifetime: 1.0, playback: Playback::OneShot, tracks: vec![track], ..ParticleEffect::default() }
                .compile(),
        );
        let mut inst = EffectInstance::new(fx, 1);
        let rot = Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
        for k in 0..4 {
            let emitter = Transform { translation: DVec3::new(k as f64, 0.0, -5.0), rotation: rot, scale: Vec3::ONE };
            inst.advance_at(0.05, Vec3::ZERO, emitter);
        }
        // Camera at the world origin: the node matrix carries its rotation, the
        // world matrix is the anchor's translation alone (as the editor builds them).
        let node_xf = Mat4::from_rotation_translation(rot, Vec3::new(3.0, 0.0, -5.0));
        let world_xf = Mat4::from_translation(inst.anchor().as_vec3());
        let (mut packed, mut draws) = (Vec::new(), Vec::new());
        collect_trails(&inst, node_xf, world_xf, Vec3::X, &mut packed, &mut draws);
        assert_eq!(draws.len(), 1);
        assert_eq!(packed.len(), 3, "three steps of motion = three segments");
        let (tail, _) = segment_ends(&packed[0]);
        let (_, head) = segment_ends(&packed[2]);
        assert!((tail - Vec3::new(0.0, 0.0, -5.0)).length() < 1e-4, "tail at the path's start, got {tail}");
        assert!((head - Vec3::new(3.0, 0.0, -5.0)).length() < 1e-4, "head at the node, got {head}");
        for p in &packed {
            let (a, b) = segment_ends(p);
            assert!((a.z + 5.0).abs() < 1e-4 && (b.z + 5.0).abs() < 1e-4, "the ribbon stays on the path's line: {a} {b}");
        }
    }

    /// **A beam bows through its control point.** A track ran origin → end
    /// as a straight line, so a tether, a rope or a hose could only be drawn
    /// straight. With a control point the chain is a quadratic curve: pinned at
    /// both ends, and halfway along it is halfway to the control point.
    #[test]
    fn a_beam_with_a_control_point_bows_through_its_pull_and_keeps_its_ends() {
        use crate::curve::ValueOrCurve;
        let track = |control: Option<Vec3>| Track {
            look: Look { render: RenderMode::Beam { texture: None }, ..Look::default() },
            size: ValueOrCurve::constant(0.2),
            segments: 16,
            beam_end: Vec3::new(8.0, 0.0, 0.0),
            beam_control: control,
            ..Track::default()
        };
        let chain = |inst: &EffectInstance| {
            let (mut packed, mut draws) = (Vec::new(), Vec::new());
            let xf = Mat4::from_translation(Vec3::new(0.0, 0.0, -10.0));
            collect_beams(inst, xf, xf, Vec3::X, &mut packed, &mut draws);
            let mut pts: Vec<Vec3> = packed.iter().map(|p| segment_ends(p).0).collect();
            pts.push(segment_ends(packed.last().unwrap()).1);
            pts
        };
        let fx = |c| Arc::new(ParticleEffect { lifetime: 1.0, playback: Playback::Looping, tracks: vec![track(c)], ..ParticleEffect::default() }.compile());
        let at = Vec3::new(0.0, 0.0, -10.0);
        // Authored: a sag of 3 below the middle.
        let mut sag = EffectInstance::new(fx(Some(Vec3::new(4.0, -3.0, 0.0))), 1);
        sag.simulate_to(0.25, Vec3::ZERO);
        let pts = chain(&sag);
        assert_eq!(pts.len(), 17);
        assert!((pts[0] - at).length() < 1e-4, "the origin moved: {}", pts[0]);
        assert!((pts[16] - (at + Vec3::new(8.0, 0.0, 0.0))).length() < 1e-4, "the end moved: {}", pts[16]);
        // B(1/2) = c/2 + e/4: (4, -1.5, 0).
        assert!((pts[8] - (at + Vec3::new(4.0, -1.5, 0.0))).length() < 1e-4, "the middle is at {}", pts[8]);
        for (k, p) in pts.iter().enumerate() {
            let u = k as f32 / 16.0;
            let want = at + Vec3::new(4.0, -3.0, 0.0) * (2.0 * u * (1.0 - u)) + Vec3::new(8.0, 0.0, 0.0) * (u * u);
            assert!((*p - want).length() < 1e-4, "point {k} is {p}, the curve has {want}");
        }
        // A script's control point wins over the authored one, and clearing it
        // goes back to the authored one.
        sag.set_beam_control(Some(Vec3::new(4.0, 2.0, 0.0)));
        assert!((chain(&sag)[8] - (at + Vec3::new(4.0, 1.0, 0.0))).length() < 1e-4, "the script's point did not bow it");
        sag.set_beam_control(None);
        assert!((chain(&sag)[8] - (at + Vec3::new(4.0, -1.5, 0.0))).length() < 1e-4, "clearing did not go back to the authored point");
        // No control point is the straight beam it always was.
        let mut straight = EffectInstance::new(fx(None), 1);
        straight.simulate_to(0.25, Vec3::ZERO);
        for p in chain(&straight) {
            assert!((p.y).abs() < 1e-5 && (p.z + 10.0).abs() < 1e-5, "a straight beam bowed: {p}");
        }
    }

    /// **An effect fills a space of any size with the same motes.**
    /// `setIntensity` shades size with emission and a node's scale scales its
    /// billboards, so one mark could not be fitted to a 0.4 m tuft and a 12 m
    /// tree without oversized or dim motes. `volume` scales where particles
    /// are born and nothing else.
    #[test]
    fn one_effect_at_volume_0_4_and_12_fills_each_with_the_same_particles() {
        use crate::curve::{Value, ValueOrCurve};
        use crate::effect::EmitShape;
        let fx = Arc::new(
            ParticleEffect {
                lifetime: 2.0,
                playback: Playback::OneShot,
                tracks: vec![Track {
                    clips: vec![burst_clip(400, 2.0)],
                    shape: EmitShape::Sphere { radius: 1.0, shell: false },
                    velocity: ValueOrCurve::Const(Value::Vec3(Vec3::ZERO)),
                    size: ValueOrCurve::constant(0.1),
                    color: ValueOrCurve::Const(Value::Rgba([0.9, 0.8, 0.3, 0.7])),
                    ..Track::default()
                }],
                ..ParticleEffect::default()
            }
            .compile(),
        );
        let drawn = |volume: f32| {
            let mut inst = EffectInstance::new(fx.clone(), 5);
            inst.set_volume(volume);
            inst.simulate_to(0.5, Vec3::ZERO);
            let (mut packed, mut draws) = (Vec::new(), Vec::new());
            collect_billboards(&inst, Mat4::IDENTITY, Mat4::IDENTITY, Vec3::NEG_Z, Vec3::X, Vec3::Y, &mut packed, &mut draws);
            packed
        };
        let (small, big) = (drawn(0.4), drawn(12.0));
        assert_eq!(small.len(), 400, "every particle of the burst draws");
        assert_eq!(big.len(), small.len(), "volume changed the count");
        let reach = |ps: &[ParticleInstance]| {
            ps.iter().map(|p| Vec3::new(p.pos_rot[0], p.pos_rot[1], p.pos_rot[2]).length()).fold(0.0f32, f32::max)
        };
        for (ps, v) in [(&small, 0.4f32), (&big, 12.0)] {
            let r = reach(ps);
            assert!(r <= v * 1.0001 && r >= v * 0.9, "volume {v}: the burst reaches {r}, not the volume");
        }
        for (a, b) in small.iter().zip(&big) {
            assert_eq!(a.size, b.size, "the particle size changed with the volume");
            assert_eq!(a.color, b.color, "the brightness changed with the volume");
            assert!((a.size[0] - 0.1).abs() < 1e-6, "size {} is not the authored 0.1", a.size[0]);
        }
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

    /// **A jittered beam is a jagged channel, thrown again per pulse.** A beam
    /// could only wave as one sine, so a bolt was a smooth curve. Jitter throws
    /// each chain point sideways from a seed: the ends stay pinned, two tracks
    /// with one seed draw one channel (a core and its glow), the same moment
    /// draws the same shape, and a new loop or a `jitter_rate` tick throws a new
    /// one.
    #[test]
    fn a_jittered_beam_is_jagged_and_rethrown_per_pulse() {
        use crate::effect::{Look, RenderMode};
        let beam = |seed: u32, rate: f32| Track {
            look: Look { render: RenderMode::Beam { texture: None }, ..Look::default() },
            segments: 16,
            beam_end: Vec3::new(0.0, 8.0, 0.0),
            jitter: 1.0,
            jitter_rate: rate,
            jitter_seed: seed,
            ..Track::default()
        };
        let fx = |tracks: Vec<Track>| {
            Arc::new(
                ParticleEffect { lifetime: 1.0, playback: Playback::Looping, tracks, ..ParticleEffect::default() }
                    .compile(),
            )
        };
        let xf = Mat4::from_translation(Vec3::new(0.0, 0.0, 10.0));
        let chain = |inst: &EffectInstance| {
            let (mut packed, mut draws) = (Vec::new(), Vec::new());
            collect_beams(inst, xf, xf, Vec3::X, &mut packed, &mut draws);
            packed.iter().map(|p| segment_ends(p).0).collect::<Vec<Vec3>>()
        };
        let mut inst = EffectInstance::new(fx(vec![beam(3, 0.0), beam(3, 0.0), beam(4, 0.0)]), 1);
        inst.simulate_to(0.3, Vec3::ZERO);
        let all = chain(&inst);
        let (core, glow, other) = (&all[0..16], &all[16..32], &all[32..48]);
        assert!((core[0] - Vec3::new(0.0, 0.0, 10.0)).length() < 1e-4, "the origin stays pinned");
        let (_, tip) = segment_ends(&{
            let (mut packed, mut draws) = (Vec::new(), Vec::new());
            collect_beams(&inst, xf, xf, Vec3::X, &mut packed, &mut draws);
            packed[15]
        });
        assert!((tip - Vec3::new(0.0, 8.0, 10.0)).length() < 1e-4, "the end stays pinned: {tip}");
        let sideways: Vec<f32> = core.iter().map(|p| Vec3::new(p.x, 0.0, p.z - 10.0).length()).collect();
        let widest = sideways.iter().cloned().fold(0.0, f32::max);
        assert!(widest > 0.3 && widest <= 1.0 + 1e-4, "thrown sideways within the jitter: {widest}");
        // Jagged, not a smooth curve: neighbouring segments turn sharply somewhere.
        let turns = core.windows(3).filter(|w| (w[1] - w[0]).normalize().dot((w[2] - w[1]).normalize()) < 0.97).count();
        assert!(turns >= 3, "only {turns} kinks");
        assert_eq!(core, glow, "one seed, one channel");
        assert_ne!(core, other, "another seed, another channel");
        // The same moment draws the same shape…
        let mut again = EffectInstance::new(Arc::clone(&inst.effect), 1);
        again.simulate_to(0.3, Vec3::ZERO);
        assert_eq!(chain(&again), all);
        // …a later moment in the same pulse too, and the next loop another.
        inst.simulate_to(0.9, Vec3::ZERO);
        assert_eq!(&chain(&inst)[0..16], core, "rethrown inside one pulse");
        inst.advance(0.2, Vec3::ZERO);
        assert_ne!(&chain(&inst)[0..16], core, "the next loop is a new throw");
        // A rate rethrows within a pulse.
        let mut fast = EffectInstance::new(fx(vec![beam(3, 10.0)]), 1);
        fast.simulate_to(0.31, Vec3::ZERO);
        let first = chain(&fast);
        fast.simulate_to(0.34, Vec3::ZERO);
        assert_eq!(chain(&fast), first, "steady inside one tick");
        fast.simulate_to(0.46, Vec3::ZERO);
        assert_ne!(chain(&fast), first, "a tick is a new throw");
    }

    /// **A jittered beam is one ribbon, joined at every kink.** Each segment
    /// was its own square-ended quad, so every kink left a wedge uncovered on
    /// its outside: a dark sliver in a wide glow, and a fan of shards at 48
    /// segments. The segments are mitred now, so at every joint the corners of
    /// the segment arriving are the corners of the segment leaving, as the
    /// particle shader places them.
    #[test]
    fn a_jittered_beams_segments_meet_at_every_joint() {
        use crate::effect::{Look, RenderMode};
        let fx = Arc::new(
            ParticleEffect {
                lifetime: 1.0,
                playback: Playback::Looping,
                tracks: vec![Track {
                    look: Look { render: RenderMode::Beam { texture: None }, ..Look::default() },
                    segments: 48,
                    beam_end: Vec3::new(0.0, 40.0, 0.0),
                    jitter: 2.5,
                    size: crate::curve::ValueOrCurve::constant(6.0),
                    ..Track::default()
                }],
                ..ParticleEffect::default()
            }
            .compile(),
        );
        let mut inst = EffectInstance::new(fx, 1);
        inst.simulate_to(0.2, Vec3::ZERO);
        let (mut packed, mut draws) = (Vec::new(), Vec::new());
        let xf = Mat4::from_translation(Vec3::new(0.0, -20.0, -60.0));
        collect_beams(&inst, xf, xf, Vec3::X, &mut packed, &mut draws);
        assert_eq!(packed.len(), 48);
        // A corner as `particles.wgsl` places it (a ribbon is never spun).
        let corner = |p: &ParticleInstance, cx: f32, cy: f32| {
            let v = |a: [f32; 4]| Vec3::new(a[0], a[1], a[2]);
            let up = v(p.basis_up);
            let mut w = v(p.pos_rot) + v(p.basis_right) * (cx * p.size[0]) + up * (cy * p.size[1]);
            if p.extra[2] < 0.0 {
                let s = match (cx > 0.0, cy > 0.0) {
                    (true, false) => p.extra[0],
                    (true, true) => p.extra[1],
                    (false, false) => p.size[2],
                    (false, true) => p.extra[3],
                };
                w += up.normalize() * s;
            }
            w
        };
        // On screen, every point across the ribbon from a joint, on the
        // outside of its turn, is covered: no wedge opens at a kink. (The camera sits at the origin looking down -Z.)
        let screen = |w: Vec3| (w.x / -w.z, w.y / -w.z);
        let inside_tri = |p: (f32, f32), a: (f32, f32), b: (f32, f32), c: (f32, f32)| {
            let cross = |o: (f32, f32), u: (f32, f32), v: (f32, f32)| (u.0 - o.0) * (v.1 - o.1) - (u.1 - o.1) * (v.0 - o.0);
            let (d1, d2, d3) = (cross(a, b, p), cross(b, c, p), cross(c, a, p));
            !((d1 < 0.0 || d2 < 0.0 || d3 < 0.0) && (d1 > 0.0 || d2 > 0.0 || d3 > 0.0))
        };
        let covers = |q: &ParticleInstance, p: (f32, f32)| {
            let c = [(-0.5, -0.5), (0.5, -0.5), (0.5, 0.5), (-0.5, 0.5)].map(|(x, y)| screen(corner(q, x, y)));
            inside_tri(p, c[0], c[1], c[2]) || inside_tri(p, c[0], c[2], c[3])
        };
        let mut holes = 0;
        for pair in packed.windows(2) {
            // The joint, and across the ribbon there on each side: along the
            // bisector of the two segments' screen normals, which is where the
            // wedge of a square-ended kink opens.
            let (a, j, b) = (screen(corner(&pair[0], 0.0, -0.5)), screen(corner(&pair[0], 0.0, 0.5)), screen(corner(&pair[1], 0.0, 0.5)));
            let normal = |p: (f32, f32), q: (f32, f32)| {
                let (dx, dy) = (q.0 - p.0, q.1 - p.1);
                let l = (dx * dx + dy * dy).sqrt().max(1e-9);
                (-dy / l, dx / l)
            };
            let (n0, n1) = (normal(a, j), normal(j, b));
            let (bx, by) = (n0.0 + n1.0, n0.1 + n1.1);
            let bl = (bx * bx + by * by).sqrt();
            // A hairpin (turning back on itself past 150°, or a segment seen
            // end-on) has no mitre short enough to draw; past the mitre limit
            // a kink keeps a notch, as every ribbon renderer's does.
            let degenerate = (n0.0 * n0.0 + n0.1 * n0.1) < 0.5 || (n1.0 * n1.0 + n1.1 * n1.1) < 0.5;
            if degenerate || n0.0 * n1.0 + n0.1 * n1.1 < -0.866 {
                continue;
            }
            // Half the ribbon's width on screen at the joint.
            let half = 3.0 / -corner(&pair[0], 0.0, 0.5).z;
            // The outside of the turn, where a square-ended kink opens its
            // wedge. (On the inside the two segments overlap.)
            let turn = n0.0 * n1.1 - n0.1 * n1.0;
            for side in [turn.signum()] {
                for r in [0.2, 0.5, 0.8, 0.95] {
                    let p = (j.0 + side * bx / bl * half * r, j.1 + side * by / bl * half * r);
                    if !packed.iter().any(|q| covers(q, p)) {
                        holes += 1;
                    }
                }
            }
        }
        assert_eq!(holes, 0, "points beside a joint that the ribbon does not cover");
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
                        emitter_path: false,
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
