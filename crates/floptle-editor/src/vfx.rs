//! Editor-side particle glue: the `.vfx.ron` effect registry, doc → runtime
//! compilation, live play-mode instances, and per-frame billboard packing.
//!
//! The pure runtime lives in `floptle-vfx`; the serializable assets in
//! `floptle-scene::vfx`. This module connects them to the live editor world —
//! the same layering as [`crate::anim`]. Phase 1 (see the proposal §8): effects
//! play on nodes during Play mode; the timeline editor tab arrives in phase 2.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use floptle_core::math::{DVec3, Mat4, Vec3};
use floptle_core::{Entity, ParticleSystem, World};
use floptle_render::particles::{ParticleBatch, ParticleGlobals};
use floptle_render::{ParticleInstance, RenderCamera, TexId};
use floptle_scene::{
    VfxBlendDoc, VfxCurveDoc, VfxEffectDoc, VfxForceDoc, VfxInterpDoc, VfxLaneTargetDoc,
    VfxOrientDoc, VfxPlaybackDoc, VfxPropDoc, VfxRenderDoc, VfxShapeDoc, VfxValueDoc, VFX_EXT,
};
use floptle_vfx::{
    BillboardOrient, Blend, Burst, Clip, CompiledEffect, Curve, EffectInstance, EmitShape,
    EndBehavior, Extrapolate, Force, Interp, Key, Lane, LaneTarget, Look, ParticleEffect, Playback,
    RenderMode, Space, Track, Value, ValueOrCurve, collect_billboards,
};

use crate::anim::asset_key;

/// The gravity particles feel in phase 1 — the default scene "Down" volume's
/// pull. Per-instance sampling of the real gravity field comes with the GPU
/// backend phase (where the field is a texture fetch anyway).
pub(crate) const VFX_GRAVITY: Vec3 = Vec3::new(0.0, -10.0, 0.0);

/// One registered effect asset: the editable doc + its compiled runtime form.
pub struct VfxAsset {
    pub doc: VfxEffectDoc,
    pub compiled: Arc<CompiledEffect>,
}

impl VfxAsset {
    fn build(doc: VfxEffectDoc) -> Self {
        let compiled = Arc::new(effect_from_doc(&doc).compile());
        Self { doc, compiled }
    }
}

/// The Particles tab's live preview: a deterministic instance driven by the
/// tab's playhead, anchored to a scene node carrying the edited effect (or the
/// world origin when none does).
pub struct VfxPreview {
    pub key: String,
    pub inst: EffectInstance,
    pub anchor: Option<Entity>,
}

/// A fire-and-forget one-shot effect spawned from code (`spawnEffect(...)`), not
/// bound to any node: it plays once at a fixed world point and drops itself when done.
pub struct DetachedEffect {
    pub inst: EffectInstance,
    /// The world spawn point — a static emitter transform for the effect.
    pub pos: DVec3,
}

/// Everything particles the editor owns. One field on `Editor`.
#[derive(Default)]
pub struct VfxSystem {
    /// `*.vfx.ron` effect assets: (key, doc + compiled), sorted by key.
    pub effects: Vec<(String, VfxAsset)>,
    /// Live play-mode instances per emitter entity, with the asset key each was
    /// spawned from (an asset swap mid-play rebuilds the instance).
    pub instances: HashMap<Entity, (String, EffectInstance)>,
    /// Fire-and-forget one-shots from `spawnEffect(...)` — ticked + reaped each frame.
    pub detached: Vec<DetachedEffect>,
    /// The Particles tab's edit-mode preview (drawn only outside Play).
    pub preview: Option<VfxPreview>,
}

impl VfxSystem {
    /// Re-scan `assets/` for particle effects (compiling curves to LUTs).
    pub fn rescan(&mut self, project_root: &Path) {
        self.effects.clear();
        let root = project_root.to_path_buf();
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for entry in rd.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if !name.starts_with('.') && name != "target" {
                        stack.push(p);
                    }
                    continue;
                }
                let Some(fname) = p.file_name().and_then(|s| s.to_str()) else { continue };
                if fname.ends_with(VFX_EXT)
                    && let Ok(doc) = floptle_scene::load_vfx_effect(&p)
                {
                    self.effects.push((asset_key(&p, &root, VFX_EXT), VfxAsset::build(doc)));
                }
            }
        }
        self.effects.sort_by(|a, b| a.0.cmp(&b.0));
    }

    /// The registry key `key` resolves to: exact, else a unique file-stem match
    /// (the anim-registry discipline: moving a file degrades gracefully).
    fn resolve_key(&self, key: &str) -> Option<usize> {
        if let Some(i) = self.effects.iter().position(|(k, _)| k == key) {
            return Some(i);
        }
        let stem = key.rsplit('/').next()?;
        let mut hits = self
            .effects
            .iter()
            .enumerate()
            .filter(|(_, (k, _))| k.rsplit('/').next() == Some(stem));
        let first = hits.next()?;
        if hits.next().is_some() {
            return None; // ambiguous — require the full key
        }
        Some(first.0)
    }

    /// Look up a compiled effect by key (with stem fallback).
    pub fn effect(&self, key: &str) -> Option<Arc<CompiledEffect>> {
        self.resolve_key(key).map(|i| Arc::clone(&self.effects[i].1.compiled))
    }

    /// Look up an editable doc by key (with stem fallback).
    pub fn doc(&self, key: &str) -> Option<&VfxEffectDoc> {
        self.resolve_key(key).map(|i| &self.effects[i].1.doc)
    }

    /// Save a doc back to disk + refresh the registry entry in place, and
    /// re-spawn any live play-mode instances of it so edits land immediately.
    pub fn save(&mut self, project_root: &Path, key: &str, doc: &VfxEffectDoc) {
        let path = project_root.join(format!("{key}{VFX_EXT}"));
        if let Err(e) = floptle_scene::save_vfx_effect(doc, &path) {
            eprintln!("  save effect {key} failed: {e}");
            return;
        }
        match self.effects.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = VfxAsset::build(doc.clone()),
            None => {
                self.effects.push((key.to_string(), VfxAsset::build(doc.clone())));
                self.effects.sort_by(|a, b| a.0.cmp(&b.0));
            }
        }
        let respawn: Vec<Entity> = self
            .instances
            .iter()
            .filter(|(_, (k, _))| k == key)
            .map(|(e, _)| *e)
            .collect();
        for e in respawn {
            self.spawn(e, key);
        }
    }

    /// Drop every live instance + detached one-shot (Play start/stop, scene load).
    pub fn clear_instances(&mut self) {
        self.instances.clear();
        self.detached.clear();
    }

    /// Spawn a fire-and-forget one-shot at a world point (`spawnEffect(...)` from a
    /// script). It plays once and is reaped when it finishes — no node needed.
    pub fn spawn_detached(&mut self, key: &str, pos: DVec3) {
        if let Some(fx) = self.effect(key) {
            // Vary the seed by spawn ordinal + position so repeats don't lockstep.
            let seed = (self.detached.len() as u32)
                .wrapping_add(1)
                .wrapping_add(pos.x.to_bits() as u32 ^ pos.z.to_bits() as u32);
            self.detached.push(DetachedEffect { inst: EffectInstance::new(fx, seed), pos });
        }
    }

    /// Spawn instances for every `play_on_start` particle system in the scene.
    pub fn start_play(&mut self, world: &World) {
        self.clear_instances();
        let systems: Vec<(Entity, ParticleSystem)> =
            world.query::<ParticleSystem>().map(|(e, p)| (e, p.clone())).collect();
        for (e, ps) in systems {
            if ps.play_on_start {
                self.spawn(e, &ps.asset);
            }
        }
    }

    /// Spawn (or replace) the instance on `entity` from the effect at `key`.
    /// Seeded by the entity index so two campfires don't march in lockstep.
    pub fn spawn(&mut self, entity: Entity, key: &str) {
        if let Some(fx) = self.effect(key) {
            let inst = EffectInstance::new(fx, entity.index().wrapping_add(1));
            self.instances.insert(entity, (key.to_string(), inst));
        }
    }

    /// Advance every live instance one play frame. Instances whose node lost its
    /// component or swapped its asset are dropped (a swap re-spawns below — the
    /// physics live-sync discipline). Finished one-shots stay as inert entries so
    /// the re-spawn scan can't resurrect them into a loop.
    pub fn advance(&mut self, world: &World, dt: f32) {
        self.instances.retain(|e, (key, _)| {
            world.get::<ParticleSystem>(*e).is_some_and(|ps| ps.asset == *key)
        });
        for (e, (_, inst)) in self.instances.iter_mut() {
            // Feed the emitter's world transform so World-space tracks anchor correctly.
            let emitter = floptle_core::world_transform(world, *e);
            inst.advance_at(dt, VFX_GRAVITY, emitter);
        }
        // Spawn for play-on-start systems without an instance: an asset swapped
        // mid-play, or a component attached mid-play.
        let missing: Vec<(Entity, String)> = world
            .query::<ParticleSystem>()
            .filter(|(e, ps)| ps.play_on_start && !self.instances.contains_key(e))
            .map(|(e, ps)| (e, ps.asset.clone()))
            .collect();
        for (e, key) in missing {
            self.spawn(e, &key);
        }
        // Detached one-shots: tick at their fixed world point, then reap the finished.
        for d in &mut self.detached {
            let emitter = floptle_core::transform::Transform::from_translation(d.pos);
            d.inst.advance_at(dt, VFX_GRAVITY, emitter);
        }
        self.detached.retain(|d| !d.inst.is_done());
    }

    /// The per-node particle state scripts read via `node:particles()`: one entry per
    /// ParticleSystem node — `playing`/`alive` from its live instance (if any) plus the
    /// effect asset key. Fed to the script host before each Play-mode script frame.
    pub fn script_info(&self, world: &World) -> HashMap<u32, floptle_script::VfxInfo> {
        let mut out = HashMap::new();
        for (e, ps) in world.query::<ParticleSystem>() {
            let inst = self.instances.get(&e);
            out.insert(
                e.index(),
                floptle_script::VfxInfo {
                    playing: inst.is_some(),
                    alive: inst.map(|(_, i)| i.alive() as u32).unwrap_or(0),
                    asset: ps.asset.clone(),
                },
            );
        }
        out
    }

    /// Apply the particle commands scripts queued this frame (`node:particles():play()`
    /// / `:stop()` / `:restart()`) to the live instances, before they advance — so a
    /// script that starts an effect this frame sees it emit this frame.
    pub fn apply_script_commands(&mut self, world: &World, cmds: Vec<(u32, floptle_script::VfxCmd)>) {
        for (eid, cmd) in cmds {
            // Resolve the entity (with generation) + its effect asset from the index.
            let Some((e, key)) = world
                .query::<ParticleSystem>()
                .find(|(e, _)| e.index() == eid)
                .map(|(e, ps)| (e, ps.asset.clone()))
            else {
                continue;
            };
            match cmd {
                // Play only if idle; Restart always re-spawns a fresh instance at t=0.
                floptle_script::VfxCmd::Play => {
                    if !self.instances.contains_key(&e) {
                        self.spawn(e, &key);
                    }
                }
                floptle_script::VfxCmd::Restart => self.spawn(e, &key),
                floptle_script::VfxCmd::Stop => {
                    self.instances.remove(&e);
                }
            }
        }
    }

    /// Pack this frame's billboards — every live play instance plus (when
    /// `include_preview`, i.e. outside Play) the Particles tab's preview —
    /// resolving track texture paths through the editor's registered-texture map.
    pub fn collect(
        &self,
        world: &World,
        cam: &RenderCamera,
        textures: &HashMap<String, TexId>,
        include_preview: bool,
        out_instances: &mut Vec<ParticleInstance>,
        out_batches: &mut Vec<ParticleBatch>,
    ) {
        let fwd = cam.rotation * Vec3::NEG_Z;
        let cam_right = cam.rotation * Vec3::X;
        let cam_up = cam.rotation * Vec3::Y;
        // `local_xf` maps the effect's emitter space to camera-relative space (a node's
        // render matrix, a detached one-shot's world point, or the origin for a preview).
        let mut pack = |inst: &EffectInstance, local_xf: Mat4| {
            // World-space tracks live at the instance's world anchor (camera-relative).
            let world_xf = Mat4::from_translation((inst.anchor() - cam.world_position).as_vec3());
            let mut draws = Vec::new();
            collect_billboards(
                inst, local_xf, world_xf, fwd, cam_right, cam_up, out_instances, &mut draws,
            );
            for d in draws {
                out_batches.push(ParticleBatch {
                    texture: d.texture.as_deref().and_then(|p| textures.get(p).copied()),
                    blend: d.blend,
                    range: d.range,
                });
            }
        };
        let node_xf =
            |e: Entity| floptle_core::world_transform(world, e).render_matrix(cam.world_position);
        let point_xf = |p: DVec3| {
            floptle_core::transform::Transform::from_translation(p).render_matrix(cam.world_position)
        };
        for (e, (_, inst)) in &self.instances {
            pack(inst, node_xf(*e));
        }
        for d in &self.detached {
            pack(&d.inst, point_xf(d.pos));
        }
        if include_preview
            && let Some(p) = &self.preview
        {
            let xf = p.anchor.map(node_xf).unwrap_or_else(|| point_xf(DVec3::ZERO));
            pack(&p.inst, xf);
        }
    }

    /// Every texture path any registered effect's billboard tracks reference —
    /// for the editor's texture pre-warm. Includes the live preview's tracks so
    /// a just-picked (unsaved) texture resolves next frame.
    pub fn texture_paths(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut scan = |fx: &CompiledEffect| {
            for track in &fx.tracks {
                if let RenderMode::Billboard { texture: Some(p) } = &track.look.render
                    && !out.contains(p)
                {
                    out.push(p.clone());
                }
            }
        };
        for (_, asset) in &self.effects {
            scan(&asset.compiled);
        }
        if let Some(p) = &self.preview {
            scan(&p.inst.effect);
        }
        out
    }

    /// Every model path any mesh-render track references — so the editor can
    /// import (GPU-load) them before drawing mesh particles.
    pub fn mesh_paths(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut scan = |fx: &CompiledEffect| {
            for track in &fx.tracks {
                if let RenderMode::Mesh { asset_path } = &track.look.render
                    && !asset_path.is_empty()
                    && !out.contains(asset_path)
                {
                    out.push(asset_path.clone());
                }
            }
        };
        for (_, asset) in &self.effects {
            scan(&asset.compiled);
        }
        if let Some(p) = &self.preview {
            scan(&p.inst.effect);
        }
        out
    }

    /// Collect every live mesh-particle track (play instances + preview) as
    /// camera-relative model matrices + tints; the caller resolves `asset_path`
    /// to GPU mesh(es) and appends them to the raster pass.
    pub fn collect_mesh_draws(
        &self,
        world: &World,
        cam: &RenderCamera,
        include_preview: bool,
    ) -> Vec<floptle_vfx::MeshDraw> {
        let mut out = Vec::new();
        let mut pack = |inst: &EffectInstance, local_xf: Mat4| {
            let world_xf = Mat4::from_translation((inst.anchor() - cam.world_position).as_vec3());
            floptle_vfx::collect_mesh_particles(inst, local_xf, world_xf, &mut out);
        };
        let node_xf =
            |e: Entity| floptle_core::world_transform(world, e).render_matrix(cam.world_position);
        let point_xf = |p: DVec3| {
            floptle_core::transform::Transform::from_translation(p).render_matrix(cam.world_position)
        };
        for (e, (_, inst)) in &self.instances {
            pack(inst, node_xf(*e));
        }
        for d in &self.detached {
            pack(&d.inst, point_xf(d.pos));
        }
        if include_preview
            && let Some(p) = &self.preview
        {
            let xf = p.anchor.map(node_xf).unwrap_or_else(|| point_xf(DVec3::ZERO));
            pack(&p.inst, xf);
        }
        out
    }
}

/// The particle pass's frame globals for `cam` (billboard basis from its rotation).
pub fn particle_globals(cam: &RenderCamera, aspect: f32) -> ParticleGlobals {
    let (r, u) = (cam.rotation * Vec3::X, cam.rotation * Vec3::Y);
    ParticleGlobals {
        view_proj: cam.view_proj(aspect).to_cols_array_2d(),
        cam_right: [r.x, r.y, r.z, 0.0],
        cam_up: [u.x, u.y, u.z, 0.0],
    }
}

/// A starter effect for "Add Component › Particle System (new)": a small looping
/// fountain so the node visibly emits the moment Play starts.
pub fn starter_effect_doc(name: &str) -> VfxEffectDoc {
    let tracks = vec![floptle_scene::VfxTrackDoc {
        name: "Fountain".into(),
        enabled: true,
        render: VfxRenderDoc::Billboard { texture: None },
        blend: VfxBlendDoc::Additive,
        orient: VfxOrientDoc::FaceCamera,
        aspect: 1.0,
        stretch: 1.0,
        lit: false,
        cast_shadows: false,
        space: floptle_scene::VfxSpaceDoc::Local,
        clips: vec![floptle_scene::VfxClipDoc { start: 0.0, end: 2.0 }],
        bursts: Vec::new(),
        automation: Vec::new(),
        rate: 40.0,
        shape: VfxShapeDoc::Cone { angle: 25.0, radius: 0.1 },
        particle_lifetime: 1.0,
        lifetime_jitter: 0.4,
        max_alive: None,
        velocity: VfxPropDoc::Const(VfxValueDoc::Vec3([0.0, 3.0, 0.0])),
        size: VfxPropDoc::Curve(VfxCurveDoc {
            keys: vec![
                key_doc(0.0, VfxValueDoc::F32(0.12)),
                key_doc(1.0, VfxValueDoc::F32(0.0)),
            ],
            extrapolate: Default::default(),
        }),
        rotation: VfxPropDoc::Const(VfxValueDoc::Vec3([0.0, 0.0, 0.0])),
        angular_velocity: VfxPropDoc::Const(VfxValueDoc::Vec3([0.0, 0.0, 0.0])),
        color: VfxPropDoc::Curve(VfxCurveDoc {
            keys: vec![
                key_doc(0.0, VfxValueDoc::Rgba([1.0, 0.9, 0.5, 1.0])),
                key_doc(1.0, VfxValueDoc::Rgba([0.9, 0.3, 0.1, 0.0])),
            ],
            extrapolate: Default::default(),
        }),
        gravity: 0.6,
        drag: 0.0,
        forces: Vec::new(),
    }];
    VfxEffectDoc {
        name: name.into(),
        lifetime: 2.0,
        playback: VfxPlaybackDoc::Looping,
        end: Default::default(),
        tracks,
        seed: 1,
    }
}

fn key_doc(t: f32, v: VfxValueDoc) -> floptle_scene::VfxKeyDoc {
    floptle_scene::VfxKeyDoc { t, v, interp: VfxInterpDoc::Linear, in_tan: 0.0, out_tan: 0.0 }
}

// ---- doc → runtime conversion ------------------------------------------------

fn value_from_doc(v: &VfxValueDoc) -> Value {
    match v {
        VfxValueDoc::F32(x) => Value::F32(*x),
        VfxValueDoc::Vec3(x) => Value::Vec3(Vec3::from_array(*x)),
        VfxValueDoc::Rgba(x) => Value::Rgba(*x),
    }
}

pub(crate) fn curve_from_doc(c: &VfxCurveDoc) -> Curve {
    Curve {
        keys: c
            .keys
            .iter()
            .map(|k| Key {
                t: k.t,
                v: value_from_doc(&k.v),
                interp: match k.interp {
                    VfxInterpDoc::Constant => Interp::Constant,
                    VfxInterpDoc::Linear => Interp::Linear,
                    VfxInterpDoc::Bezier => Interp::Bezier,
                },
                in_tan: k.in_tan,
                out_tan: k.out_tan,
            })
            .collect(),
        extrapolate: match c.extrapolate {
            floptle_scene::VfxExtrapolateDoc::Clamp => Extrapolate::Clamp,
            floptle_scene::VfxExtrapolateDoc::Repeat => Extrapolate::Repeat,
        },
    }
}

fn prop_from_doc(p: &VfxPropDoc) -> ValueOrCurve {
    match p {
        VfxPropDoc::Const(v) => ValueOrCurve::Const(value_from_doc(v)),
        VfxPropDoc::Range(a, b) => ValueOrCurve::Range(value_from_doc(a), value_from_doc(b)),
        VfxPropDoc::Curve(c) => ValueOrCurve::Curve(curve_from_doc(c)),
    }
}

/// Build the authoring-model effect from its RON doc (compile separately).
pub fn effect_from_doc(doc: &VfxEffectDoc) -> ParticleEffect {
    ParticleEffect {
        name: doc.name.clone(),
        lifetime: doc.lifetime,
        playback: match doc.playback {
            VfxPlaybackDoc::Looping => Playback::Looping,
            VfxPlaybackDoc::OneShot => Playback::OneShot,
        },
        end: match doc.end {
            floptle_scene::VfxEndDoc::Destroy => EndBehavior::Destroy,
            floptle_scene::VfxEndDoc::Persist => EndBehavior::Persist,
        },
        seed: doc.seed,
        tracks: doc
            .tracks
            .iter()
            .map(|t| Track {
                name: t.name.clone(),
                enabled: t.enabled,
                look: Look {
                    render: match &t.render {
                        VfxRenderDoc::Billboard { texture } => {
                            RenderMode::Billboard { texture: texture.clone() }
                        }
                        VfxRenderDoc::Mesh { asset_path } => {
                            RenderMode::Mesh { asset_path: asset_path.clone() }
                        }
                    },
                    blend: match t.blend {
                        VfxBlendDoc::Alpha => Blend::Alpha,
                        VfxBlendDoc::Additive => Blend::Additive,
                    },
                    orient: match t.orient {
                        VfxOrientDoc::FaceCamera => BillboardOrient::FaceCamera,
                        VfxOrientDoc::Velocity => BillboardOrient::Velocity,
                        VfxOrientDoc::Vertical => BillboardOrient::Vertical,
                        VfxOrientDoc::Horizontal => BillboardOrient::Horizontal,
                        VfxOrientDoc::WorldFixed => BillboardOrient::WorldFixed,
                    },
                    aspect: t.aspect,
                    stretch: t.stretch,
                    lit: t.lit,
                    cast_shadows: t.cast_shadows,
                },
                space: match t.space {
                    floptle_scene::VfxSpaceDoc::Local => Space::Local,
                    floptle_scene::VfxSpaceDoc::World => Space::World,
                },
                clips: t.clips.iter().map(|c| Clip { start: c.start, end: c.end }).collect(),
                bursts: t.bursts.iter().map(|b| Burst { t: b.t, count: b.count }).collect(),
                automation: t
                    .automation
                    .iter()
                    .map(|l| Lane {
                        target: match l.target {
                            VfxLaneTargetDoc::Rate => LaneTarget::Rate,
                            VfxLaneTargetDoc::Count => LaneTarget::Count,
                            VfxLaneTargetDoc::Speed => LaneTarget::Speed,
                            VfxLaneTargetDoc::Size => LaneTarget::Size,
                            VfxLaneTargetDoc::Tint => LaneTarget::Tint,
                            VfxLaneTargetDoc::ShapeScale => LaneTarget::ShapeScale,
                        },
                        curve: curve_from_doc(&l.curve),
                    })
                    .collect(),
                rate: t.rate,
                shape: match t.shape {
                    VfxShapeDoc::Point => EmitShape::Point,
                    VfxShapeDoc::Cone { angle, radius } => EmitShape::Cone { angle, radius },
                    VfxShapeDoc::Sphere { radius, shell } => EmitShape::Sphere { radius, shell },
                    VfxShapeDoc::Edge { length } => EmitShape::Edge { length },
                    VfxShapeDoc::Ring { radius } => EmitShape::Ring { radius },
                },
                particle_lifetime: t.particle_lifetime,
                lifetime_jitter: t.lifetime_jitter,
                max_alive: t.max_alive,
                velocity: prop_from_doc(&t.velocity),
                size: prop_from_doc(&t.size),
                rotation: prop_from_doc(&t.rotation),
                angular_velocity: prop_from_doc(&t.angular_velocity),
                color: prop_from_doc(&t.color),
                gravity: t.gravity,
                drag: t.drag,
                forces: t.forces.iter().map(force_from_doc).collect(),
            })
            .collect(),
    }
}

fn force_from_doc(f: &VfxForceDoc) -> Force {
    match *f {
        VfxForceDoc::Directional { dir, strength } => {
            Force::Directional { dir: Vec3::from_array(dir), strength }
        }
        VfxForceDoc::Point { center, strength } => {
            Force::Point { center: Vec3::from_array(center), strength }
        }
        VfxForceDoc::Vortex { center, axis, strength } => Force::Vortex {
            center: Vec3::from_array(center),
            axis: Vec3::from_array(axis),
            strength,
        },
        VfxForceDoc::Turbulence { frequency, strength } => Force::Turbulence { frequency, strength },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starter_effect_round_trips_and_emits() {
        let dir = std::env::temp_dir().join(format!("floptle-vfx-starter-{}", std::process::id()));
        let path = dir.join("Starter.vfx.ron");
        let doc = starter_effect_doc("Starter");
        floptle_scene::save_vfx_effect(&doc, &path).unwrap();
        let back = floptle_scene::load_vfx_effect(&path).unwrap();
        assert_eq!(doc, back, "starter effect must RON round-trip exactly");

        let fx = Arc::new(effect_from_doc(&back).compile());
        let mut inst = EffectInstance::new(fx, 1);
        inst.simulate_to(0.5, VFX_GRAVITY);
        assert!(inst.alive() > 5, "starter fountain must emit (got {})", inst.alive());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn timeline_authored_lane_shapes_the_rate_over_seconds() {
        // The DAW timeline authors automation keys in SECONDS spanning [0, dur]; the
        // bake normalizes that onto the LUT (domain = lifetime). A Rate lane 1→3 over
        // a 2 s effect must therefore read 2× at the mid-timeline. Guards that whole
        // second-domain path from the editor doc through compile.
        use floptle_scene::{
            VfxCurveDoc, VfxExtrapolateDoc, VfxInterpDoc, VfxKeyDoc, VfxLaneDoc, VfxLaneTargetDoc,
            VfxValueDoc,
        };
        let mut doc = starter_effect_doc("Ramp");
        doc.lifetime = 2.0;
        let key = |t, v| VfxKeyDoc {
            t,
            v: VfxValueDoc::F32(v),
            interp: VfxInterpDoc::Linear,
            in_tan: 0.0,
            out_tan: 0.0,
        };
        doc.tracks[0].automation.push(VfxLaneDoc {
            target: VfxLaneTargetDoc::Rate,
            curve: VfxCurveDoc {
                keys: vec![key(0.0, 1.0), key(2.0, 3.0)],
                extrapolate: VfxExtrapolateDoc::Clamp,
            },
        });
        let fx = effect_from_doc(&doc).compile();
        let m = fx.tracks[0].lane_rate.sample(0.5);
        assert!((m - 2.0).abs() < 0.05, "rate ×{m} at mid-timeline; expected ≈2");
    }

    #[test]
    fn rescan_registers_effects_with_stem_fallback() {
        let dir = std::env::temp_dir().join(format!("floptle-vfx-rescan-{}", std::process::id()));
        floptle_scene::save_vfx_effect(
            &starter_effect_doc("Spark"),
            &dir.join("vfx").join("Spark.vfx.ron"),
        )
        .unwrap();
        let mut sys = VfxSystem::default();
        sys.rescan(&dir);
        assert_eq!(sys.effects.len(), 1);
        assert!(sys.effect("vfx/Spark").is_some(), "exact key");
        assert!(sys.effect("Spark").is_some(), "stem fallback (moved-file grace)");
        assert!(sys.effect("vfx/Nope").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Localizes the "texture never applies" bug: proves the editor-side resolve
    // path (preview → texture_paths → collect → batch.texture) is correct, so any
    // remaining failure is registration/GPU, not this logic.
    #[test]
    fn preview_texture_resolves_through_registry() {
        const P: &str = "assets/textures/Grass.png";
        let mut doc = starter_effect_doc("T");
        if let VfxRenderDoc::Billboard { texture } = &mut doc.tracks[0].render {
            *texture = Some(P.into());
        }
        let fx = Arc::new(effect_from_doc(&doc).compile());
        let mut inst = EffectInstance::new(fx, 1);
        inst.simulate_to(0.5, VFX_GRAVITY);
        assert!(inst.alive() > 0, "must emit");

        let sys = VfxSystem {
            preview: Some(VfxPreview { key: "T".into(), inst, anchor: None }),
            ..Default::default()
        };
        assert_eq!(sys.texture_paths(), vec![P.to_string()], "prewarm sees the preview texture");

        let mut registry = HashMap::new();
        registry.insert(P.to_string(), TexId(7));
        let cam = RenderCamera::new(
            floptle_core::math::DVec3::ZERO,
            floptle_core::math::Quat::IDENTITY,
            floptle_render::Projection::Perspective { fov_y: 1.0, near: 0.1, far: 100.0 },
        );
        let world = World::new();
        let (mut instances, mut batches) = (Vec::new(), Vec::new());
        sys.collect(&world, &cam, &registry, true, &mut instances, &mut batches);
        assert!(!batches.is_empty(), "preview must produce a batch");
        assert_eq!(batches[0].texture, Some(TexId(7)), "path must resolve to the registered id");
    }
}
