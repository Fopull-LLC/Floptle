//! Taking a [`Matter::ReflectionProbe`]'s picture, and telling the shader where
//! its box is.
//!
//! **A probe is a camera.** Six 90° renders through the same
//! [`Editor::render_world_into`](crate::Editor::render_world_into) every other
//! offscreen view uses, folded into one equirectangular map by
//! [`floptle_render::ReflectionProbes`]. That is the same conclusion the GI bake
//! reached, and reusing the one path is what makes a probe show the scene rather
//! than an approximation of it — the same materials, the same lights, the same
//! sky.
//!
//! **Nothing is written to disk, on purpose.** A GI bake is minutes of work and
//! hundreds of kilobytes, so it earns a file. Six renders at 256² is a fraction
//! of a frame, which makes a stored artefact all cost and no benefit — and a
//! stored one has a failure mode a live one cannot have: a capture that no longer
//! matches the room, in a file, with nothing to say so.
//!
//! **One probe per frame, and only when something changed.** Six scene renders
//! is a real cost, so a frame takes at most one of them and a probe that has not
//! moved is left alone. In the ordinary case — a level loaded, four probes
//! placed — that is four frames of setup and then nothing at all.

use floptle_core::{Entity, Matter, World, math::DVec3, world_transform};
use floptle_render::MAX_PROBES;

use crate::Editor;
use crate::render_frame::OffscreenOpts;

/// What a capture was taken from. A probe whose key still matches is a probe
/// whose picture is still the picture it would take now.
///
/// Position and box are quantised to a millimetre: a node nudged by a rounding
/// error in a transform chain must not re-render the scene six times.
/// `epoch` is the escape hatch for everything a probe cannot see it needs —
/// the room relit, the furniture moved, a scene freshly loaded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ProbeKey {
    at: [i64; 3],
    half: [i32; 3],
    epoch: u64,
}

/// The project's stored detail setting as the renderer's own enum.
pub(crate) fn probe_detail(d: floptle_scene::ProbeDetailDoc) -> floptle_render::ProbeDetail {
    use floptle_render::ProbeDetail as P;
    match d {
        floptle_scene::ProbeDetailDoc::Low => P::Low,
        floptle_scene::ProbeDetailDoc::Medium => P::Medium,
        floptle_scene::ProbeDetailDoc::High => P::High,
        floptle_scene::ProbeDetailDoc::Ultra => P::Ultra,
    }
}

/// The uniform lanes `field.wgsl` reads: `(meta, pos, half)`.
pub(crate) type ProbeUniforms =
    ([f32; 4], [[f32; 4]; MAX_PROBES], [[f32; 4]; MAX_PROBES]);

/// The probe lanes for a frame rendered from `cam_world`.
///
/// A free function taking the three fields it reads rather than a method,
/// because both render paths compute this while the GPU is already borrowed
/// mutably out of `self` — and a whole-`self` borrow here would be a borrow
/// error rather than a design.
///
/// Only slots that have actually been captured are reported. A probe placed
/// this frame has a box and no picture, and announcing it would replace the sky
/// with black until its capture came round.
pub(crate) fn probe_uniforms(
    world: &World,
    slots: &[(Entity, ProbeKey)],
    capturing: bool,
    cam_world: DVec3,
    clamp: f32,
) -> ProbeUniforms {
    let mut pos = [[0.0f32; 4]; MAX_PROBES];
    let mut half = [[1.0f32; 4]; MAX_PROBES];
    if capturing {
        // A capture still wants the bounce ceiling — it is what keeps a mirror
        // inside the room from baking a runaway into the picture of that room.
        return ([0.0, clamp, 0.0, 0.0], pos, half);
    }
    let mut n = 0usize;
    for (e, _) in slots {
        let Some((at, h, intensity, fade)) = placement(world, *e) else { continue };
        let rel = (at - cam_world).as_vec3();
        pos[n] = [rel.x, rel.y, rel.z, intensity.max(0.0)];
        half[n] = [h[0], h[1], h[2], fade.max(0.0)];
        n += 1;
    }
    ([n as f32, clamp, 0.0, 0.0], pos, half)
}

/// Every enabled probe in the scene, nearest-first, capped at [`MAX_PROBES`].
///
/// **Nearest to the camera, not first in the hierarchy.** With more probes than
/// slots something has to be dropped, and dropping the one you are standing in
/// because it was added last is the one behaviour that would be indefensible.
pub(crate) fn probes_near(world: &World, eye: DVec3) -> Vec<Entity> {
    let mut found: Vec<(f64, Entity)> = world
        .query::<Matter>()
        .filter_map(|(e, m)| match m {
            Matter::ReflectionProbe { enabled: true, .. } => {
                Some(((world_transform(world, e).translation - eye).length_squared(), e))
            }
            _ => None,
        })
        .collect();
    found.sort_by(|a, b| a.0.total_cmp(&b.0));
    found.truncate(MAX_PROBES);
    found.into_iter().map(|(_, e)| e).collect()
}

/// A probe's world placement: where it was captured from, and the box it covers.
/// The node's scale multiplies the authored half-extents, so a probe can be
/// sized by dragging it like anything else.
fn placement(world: &World, e: Entity) -> Option<(DVec3, [f32; 3], f32, f32)> {
    let Some(Matter::ReflectionProbe { half_extents, intensity, fade, .. }) =
        world.get::<Matter>(e)
    else {
        return None;
    };
    let t = world_transform(world, e);
    let s = t.scale;
    Some((
        t.translation,
        [
            (half_extents[0] * s.x).max(0.01),
            (half_extents[1] * s.y).max(0.01),
            (half_extents[2] * s.z).max(0.01),
        ],
        *intensity,
        *fade,
    ))
}

impl Editor {
    /// Capture at most one probe that needs it, and retire any that are gone.
    ///
    /// Runs beside `step_gi_bake` and for the same reason: it renders the scene,
    /// so it cannot be re-entered from inside a gather that is already drawing.
    pub(crate) fn step_reflection_probes(&mut self) {
        let Some(gpu) = self.gpu.as_ref() else { return };
        let eye = self.camera.position;

        // Rebuild when the project's detail no longer matches what the maps were
        // allocated at — the size is baked into the texture, so this is the one
        // place a changed setting can take effect. Dropping the maps drops their
        // captures too, which is right: they hold less detail than was just
        // asked for.
        //
        // **Before the slots are scanned, not after.** The scan picks WHICH slot
        // to refill, and clearing the slot list under it would file the new
        // capture's key against a slot index that no longer means the same thing.
        let detail = probe_detail(self.project.probe_detail);
        if self.reflection_probes.as_ref().is_some_and(|p| p.detail() != detail) {
            self.reflection_probes = None;
            self.probe_slots.clear();
        }

        let wanted = probes_near(&self.world, eye);
        // Retire slots whose probe is gone or switched off, so the next capture
        // does not inherit a stale picture along with a reused slot.
        self.probe_slots.truncate(wanted.len());
        for (slot, e) in wanted.iter().enumerate() {
            if self.probe_slots.get(slot).is_some_and(|(had, _)| had != e) {
                self.probe_slots.truncate(slot);
                break;
            }
        }
        if wanted.is_empty() {
            self.reflection_probes = None;
            return;
        }
        // Which one is out of date? The first, so slots fill in order and a
        // level's probes come up over the first few frames rather than all at
        // once on the frame the scene loads.
        let stale = wanted.iter().enumerate().find(|&(slot, &e)| {
            let Some((at, half, _, _)) = placement(&self.world, e) else { return false };
            let key = ProbeKey {
                at: [
                    (at.x * 1000.0) as i64,
                    (at.y * 1000.0) as i64,
                    (at.z * 1000.0) as i64,
                ],
                half: [
                    (half[0] * 1000.0) as i32,
                    (half[1] * 1000.0) as i32,
                    (half[2] * 1000.0) as i32,
                ],
                epoch: self.probe_epoch,
            };
            self.probe_slots.get(slot).is_none_or(|(_, k)| *k != key)
        });
        let Some((slot, &entity)) = stale else { return };
        let Some((at, half, _, _)) = placement(&self.world, entity) else { return };

        if self.reflection_probes.is_none() {
            self.reflection_probes =
                Some(floptle_render::ReflectionProbes::with_detail(gpu, detail));
        }
        // Taken OUT of `self` for the duration: `render_world_into` needs
        // `&mut self`, and the targets it is rendering into live here. Put back
        // at the end, whatever happens in between.
        let Some(probes) = self.reflection_probes.take() else { return };
        // A capture is six ORDINARY scene renders, so its targets have to be in
        // the format the raster pipelines were built against. Windowed rendering
        // runs in HDR while the surface is 8-bit sRGB, so getting this from the
        // surface instead produces a pipeline that cannot be set — and the first
        // capture fails with a wgpu validation error rather than a wrong picture.
        debug_assert_eq!(
            probes.format(),
            gpu.scene_format(),
            "reflection probes were allocated in a different colour format from \
             the one the scene renders in — every capture will fail to bind"
        );

        // The capture must not contain its own reflections. Left on, probe 0's
        // picture would hold probe 0's last picture, which compounds — the same
        // trap glass avoids by drawing into a capture that excludes it.
        let face = probes.face_size();
        self.capturing_probes = true;
        for f in 0..6 {
            let cam = floptle_render::RenderCamera::new(
                at,
                floptle_render::reflect::face_rotation(f),
                // A cube face IS a 90° square frustum. Anything else and the
                // directions the conversion assumes stop matching the pixels.
                floptle_render::Projection::Perspective {
                    fov_y: std::f32::consts::FRAC_PI_2,
                    near: 0.05,
                    far: 4000.0,
                },
            );
            self.render_world_into(
                probes.face_target(f),
                probes.face_depth(f),
                &cam,
                1.0,
                // A capture is a still. Handing it a clock would make an
                // animated sky or a scrolling material differ between the six
                // faces, which reads as a seam down the middle of a reflection.
                0.0,
                u32::MAX,
                None,
                (face, face),
                // A capture is not a view anybody looks at: no depth prepass and
                // no reflection history, exactly as the GI bake asks for.
                OffscreenOpts::default(),
            );
        }
        self.capturing_probes = false;

        if let Some(gpu) = self.gpu.as_ref() {
            probes.resolve(gpu, slot);
            // The texture is allocated once and written into thereafter, so this
            // binds on the first capture and is a no-op rebuild after that.
            if let Some(rm) = self.raymarch.as_mut() {
                rm.set_reflection_probes(gpu, Some((probes.view(), probes.sampler())));
            }
        }
        self.reflection_probes = Some(probes);

        let key = ProbeKey {
            at: [(at.x * 1000.0) as i64, (at.y * 1000.0) as i64, (at.z * 1000.0) as i64],
            half: [
                (half[0] * 1000.0) as i32,
                (half[1] * 1000.0) as i32,
                (half[2] * 1000.0) as i32,
            ],
            epoch: self.probe_epoch,
        };
        if slot < self.probe_slots.len() {
            self.probe_slots[slot] = (entity, key);
        } else {
            self.probe_slots.push((entity, key));
        }
        // Say so. A capture is invisible by design — it changes what reflections
        // fall back to and nothing else — so without a line here the difference
        // between "captured" and "silently did nothing" is unobservable, which
        // is the failure mode this engine keeps finding in itself. Debug level:
        // it happens on load and when a probe moves, not every frame.
        let name = self
            .world
            .get::<floptle_core::Name>(entity)
            .map(|n| n.0.clone())
            .unwrap_or_else(|| "reflection probe".to_string());
        self.console.push(
            floptle_script::LogLevel::Debug,
            format!(
                "captured {name} into reflection slot {slot} \
                 ({:.0}×{:.0}×{:.0} m room)",
                half[0] * 2.0,
                half[1] * 2.0,
                half[2] * 2.0
            ),
            None,
        );
    }

    /// Throw every capture away and take them again — the ⟳ button, and what a
    /// scene load does. Bumping one number is enough: a key that no longer
    /// matches is a probe that re-captures, one per frame, in slot order.
    pub(crate) fn recapture_reflection_probes(&mut self) {
        self.probe_epoch = self.probe_epoch.wrapping_add(1);
    }
}
