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
    VfxFlipModeDoc, VfxFlipbookDoc, VfxOrientDoc, VfxPlaybackDoc, VfxPropDoc, VfxRenderDoc,
    VfxShapeDoc, VfxTrailDoc, VfxValueDoc, VFX_EXT,
};
use floptle_vfx::{
    BillboardOrient, Blend, Clip, CompiledEffect, Curve, EffectInstance, Emit, EmitShape,
    EndBehavior, Extrapolate, FlipMode, Flipbook, Force, Interp, Key, Lane, LaneTarget, Look,
    ParticleEffect, Playback, RenderMode, Space, Track, Trail, Value, ValueOrCurve,
    collect_beams, collect_billboards, collect_trails,
};

use crate::anim::asset_key;

/// The gravity particles feel in phase 1 — the default scene "Down" volume's
/// pull. Per-instance sampling of the real gravity field comes with the GPU
/// backend phase (where the field is a texture fetch anyway).
pub(crate) const VFX_GRAVITY: Vec3 = Vec3::new(0.0, -10.0, 0.0);

/// The live scene gravity field, handed to `advance` so `GravityMode::Field` effects
/// (debris, dust, embers near a planet) fall toward the ground beneath them instead of
/// world −Y. Sampled at each emitter's world position via the same field the
/// rigidbodies use (radial volumes + celestial µ/r²). `WorldDown` effects ignore it.
pub struct VfxGravity<'a> {
    pub field: &'a floptle_physics::GravityField,
    pub colliders: &'a [floptle_physics::AnchoredCollider],
    /// Sim-frame origin: field source centers are sim-local, so a world
    /// emitter position converts by `world - origin` before sampling.
    pub origin: DVec3,
}

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
    /// The emitter transform the instance was last stepped with: the anchor node's,
    /// or the tab's sweep path from it. The draw and the emitter gizmo both use
    /// this, so the picture agrees with the sim.
    pub emitter: floptle_core::transform::Transform,
}

/// A fire-and-forget one-shot effect spawned from code (`spawnEffect(...)`), not
/// bound to any node: it plays once at a fixed world point and drops itself when done.
pub struct DetachedEffect {
    pub inst: EffectInstance,
    /// The emit point in the world as of the last advance.
    pub pos: DVec3,
    /// The emitter's turn and size in the world: its +Y is the effect's up
    /// (`spawnEffect`'s `normal`).
    pub rot: floptle_core::math::Quat,
    pub scale: f32,
    /// What it rides, if anything: a planet on rails, a ship. Its emit point is
    /// kept in that frame, and its World-track particles move with it.
    pub frame: Option<FrameLink>,
    /// Emitter world velocity at spawn (m/s), passed by `spawnEffect`'s optional
    /// velocity args. Newborns on World tracks with `inherit_velocity > 0` keep a
    /// fraction of it, so a puff off a fast vessel rides its momentum instead of being
    /// stranded in world space. The point also drifts by this each frame so a streaming
    /// effect keeps emitting from where the emitter now is.
    pub vel: Vec3,
}

impl DetachedEffect {
    /// The emitter's world transform as of the last advance.
    pub fn emitter(&self) -> floptle_core::transform::Transform {
        floptle_core::transform::Transform { translation: self.pos, rotation: self.rot, scale: Vec3::splat(self.scale) }
    }
}

/// A detached effect's place on the node it rides.
pub struct FrameLink {
    pub entity: Entity,
    /// The emitter in the frame's own space.
    pub local: floptle_core::transform::Transform,
    /// The frame's world pose at the last advance.
    pub last: floptle_core::transform::Transform,
}

/// How a one-shot is placed: the emitter's world pose, the frame it rides
/// (with the frame's pose at the moment the emitter was placed against it),
/// and its look.
pub struct DetachedSpawn {
    pub emitter: floptle_core::transform::Transform,
    pub vel: Vec3,
    pub frame: Option<(Entity, floptle_core::transform::Transform)>,
    pub intensity: Option<f32>,
    pub tint: Option<[f32; 4]>,
    pub up: Option<Vec3>,
}

/// The world's own up where an effect plays, if anything defines one:
/// against the field gravity there.
fn up_against(g: Vec3) -> Option<Vec3> {
    (g.length_squared() > 1e-6).then(|| -g.normalize())
}

/// The nearest celestial body at or above `e`: the world a node on it is on.
fn celestial_ancestor(world: &World, e: Entity) -> Option<Entity> {
    let mut cur = e;
    for _ in 0..64 {
        if world.get::<floptle_core::CelestialBody>(cur).is_some() {
            return Some(cur);
        }
        cur = world.get::<floptle_core::Parent>(cur)?.0;
    }
    None
}

/// Everything particles the editor owns. One field on `Editor`.
///
/// `Default` is written out rather than derived: `max_detached` deriving to 0
/// would silently refuse every one-shot in the engine, and a ceiling whose
/// default is "none allowed" is a worse bug than the missing ceiling was.
pub struct VfxSystem {
    /// `*.vfx.ron` effect assets: (key, doc + compiled), sorted by key.
    pub effects: Vec<(String, VfxAsset)>,
    /// Live play-mode instances per emitter entity, with the asset key each was
    /// spawned from (an asset swap mid-play rebuilds the instance).
    pub instances: HashMap<Entity, (String, EffectInstance)>,
    /// Fire-and-forget one-shots from `spawnEffect(...)` — ticked + reaped each frame.
    ///
    /// A deque rather than a `Vec` so reaching [`Self::max_detached`] can drop the
    /// Oldest in O(1). A `Vec::remove(0)` would memmove the whole pool on every
    /// over-budget spawn, which is precisely the frame that could least afford it.
    pub detached: std::collections::VecDeque<DetachedEffect>,
    /// The ceiling on live one-shots.
    ///
    /// `spawnEffect` had none, so the live particle count was `spawn rate ×
    /// lifetime × particles per effect` — entirely the caller's to decide, with
    /// no way to ask what it currently costs. The reported case is the shape
    /// that makes this dangerous rather than merely unbounded: a per-frame spawn
    /// budget fires 60·N/s on one machine and 144·N/s on another, so the same
    /// game costs 2.4× more on a better monitor and the engine absorbs it
    /// silently until the frame time moves.
    ///
    /// 256 because the reported workload sat at ~265 and that was already a
    /// visible problem. Public, so a game that genuinely wants more says so.
    pub max_detached: usize,
    /// One-shots refused this frame because the pool was full. Reported through
    /// `perf.counts().effectsDropped` — a cap that silently eats the look is the
    /// failure shape this engine keeps paying for.
    detached_dropped: usize,
    /// Monotonic spawn counter — the detached seed ordinal, so repeats at a fixed
    /// point don't march in lockstep (the live pool length would collide as it reaps).
    detached_seq: u32,
    /// The Particles tab's edit-mode preview (drawn only outside Play).
    pub preview: Option<VfxPreview>,
    /// Each node instance's frame and the frame's pose at its last advance,
    /// so its World-track particles ride the world it is on.
    node_frames: HashMap<Entity, (Entity, floptle_core::transform::Transform)>,
    /// Each node's `setTint`, given to every instance it plays.
    node_tints: HashMap<Entity, [f32; 4]>,
}

impl Default for VfxSystem {
    fn default() -> Self {
        Self {
            effects: Vec::new(),
            instances: HashMap::new(),
            detached: std::collections::VecDeque::new(),
            max_detached: 256,
            detached_dropped: 0,
            detached_seq: 0,
            preview: None,
            node_frames: HashMap::new(),
            node_tints: HashMap::new(),
        }
    }
}

/// The effect's name as shown in the editor: the file stem of its key.
pub(crate) fn effect_stem(key: &str) -> &str {
    key.rsplit('/').next().unwrap_or(key)
}

impl VfxSystem {
    /// Re-scan `assets/` for particle effects (compiling curves to LUTs).
    pub fn rescan(&mut self, project_root: &Path) {
        self.effects.clear();
        let root = project_root.to_path_buf();
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = floptle_vfs::read_dir(&dir) else { continue };
            for entry in rd {
                let p = entry.path();
                if entry.is_dir() {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if !name.starts_with('.') && name != "target" {
                        stack.push(p);
                    }
                    continue;
                }
                let Some(fname) = p.file_name().and_then(|s| s.to_str()) else { continue };
                if fname.ends_with(VFX_EXT)
                    && let Ok(mut doc) = floptle_scene::load_vfx_effect(&p)
                {
                    let key = asset_key(&p, &root, VFX_EXT);
                    // An effect is named by its file, so renaming the file
                    // renames the effect everywhere it is shown.
                    doc.name = effect_stem(&key).to_string();
                    self.effects.push((key, VfxAsset::build(doc)));
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
        let doc = &VfxEffectDoc { name: effect_stem(key).to_string(), ..doc.clone() };
        if let Err(e) = floptle_scene::save_vfx_effect(doc, &path) {
            floptle_say::say_err!("  save effect {key} failed: {e}");
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

    /// Drop the instances on nodes that are leaving (`scene.unload`); every
    /// other effect keeps playing.
    pub fn forget_entities(&mut self, gone: &std::collections::HashSet<Entity>) {
        self.instances.retain(|e, _| !gone.contains(e));
    }

    /// Drop every live instance + detached one-shot (Play start/stop, scene load).
    pub fn clear_instances(&mut self) {
        self.instances.clear();
        self.detached.clear();
        self.node_tints.clear();
    }

    /// Fire a one-shot effect (`spawnEffect`): it plays once and is reaped when
    /// it finishes, no node needed. The emitter can be turned and sized, ride
    /// a frame, and carry an intensity and a tint.
    pub fn spawn_detached_with(&mut self, key: &str, spawn: DetachedSpawn) {
        let DetachedSpawn { emitter, vel, frame, intensity, tint, up } = spawn;
        let pos = emitter.translation;
        if let Some(fx) = self.effect(key) {
            // Fire-and-forget contract: coerce to a self-destructing one-shot even if
            // the asset was authored Looping/Persist, so is_done() reaps it in advance()
            // — otherwise a looping detached instance never drains and the pool grows
            // for the whole play session.
            let fx = if fx.playback == Playback::OneShot && fx.end == EndBehavior::Destroy {
                fx
            } else {
                let mut once = (*fx).clone();
                once.playback = Playback::OneShot;
                once.end = EndBehavior::Destroy;
                Arc::new(once)
            };
            // Seed from a monotonic counter (not the reaping pool length) + a full-64-bit
            // fold of the position, so whole-number coords still vary the seed.
            let bits = |b: f64| -> u32 {
                let u = b.to_bits();
                (u >> 32) as u32 ^ u as u32
            };
            self.detached_seq = self.detached_seq.wrapping_add(1);
            let seed = self.detached_seq.wrapping_add(
                bits(pos.x) ^ bits(pos.y).rotate_left(11) ^ bits(pos.z).rotate_left(22),
            );
            // The ceiling. Drop the oldest rather than refuse
            // the newest: the effect just asked for is the one the player is
            // looking at — the impact they caused, the shot they fired — and the
            // one at the front of the queue is already most of the way through
            // fading out. Refusing the new one would make a busy fight look
            // frozen while stale puffs finished.
            while self.detached.len() >= self.max_detached.max(1) {
                self.detached.pop_front();
                self.detached_dropped = self.detached_dropped.saturating_add(1);
            }
            let mut inst = EffectInstance::new(fx, seed);
            if let Some(i) = intensity {
                inst.set_intensity(i);
            }
            if let Some(t) = tint {
                inst.tint = t;
            }
            inst.up = up.unwrap_or(emitter.rotation * Vec3::Y);
            let frame = frame.map(|(entity, at)| FrameLink { entity, local: at.inv_mul(&emitter), last: at });
            self.detached.push_back(DetachedEffect {
                inst,
                pos,
                rot: emitter.rotation,
                scale: emitter.scale.x,
                frame,
                vel,
            });
        }
    }

    /// Live one-shot effects, and how many have been dropped at the ceiling
    /// since the counter was last taken.
    pub fn detached_counts(&mut self) -> (usize, usize) {
        (self.detached.len(), std::mem::take(&mut self.detached_dropped))
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
            let mut inst = EffectInstance::new(fx, entity.index().wrapping_add(1));
            if let Some(t) = self.node_tints.get(&entity) {
                inst.tint = *t;
            }
            self.instances.insert(entity, (key.to_string(), inst));
        }
    }

    /// Advance every live instance one play frame. Instances whose node lost its
    /// component or swapped its asset are dropped (a swap re-spawns below — the
    /// physics live-sync discipline). Finished one-shots stay as inert entries so
    /// the re-spawn scan can't resurrect them into a loop.
    ///
    /// `frame_at` names the moving world (a celestial on rails) a point is in,
    /// if any: a node's World-track particles and a one-shot with no frame of
    /// its own ride it.
    pub fn advance(
        &mut self,
        world: &World,
        dt: f32,
        grav: Option<VfxGravity<'_>>,
        frame_at: &dyn Fn(DVec3) -> Option<Entity>,
    ) {
        // The gravity vector an effect feels at a world point, honoring its gravity mode.
        let grav_at = |world_pos: DVec3, mode: floptle_vfx::GravityMode| -> Vec3 {
            match (mode, &grav) {
                (floptle_vfx::GravityMode::Field, Some(g)) => {
                    g.field.accel_at((world_pos - g.origin).as_vec3(), g.colliders)
                }
                _ => VFX_GRAVITY,
            }
        };
        self.instances.retain(|e, (key, _)| {
            world.get::<ParticleSystem>(*e).is_some_and(|ps| ps.asset == *key)
        });
        self.node_frames.retain(|e, _| self.instances.contains_key(e));
        for (e, (_, inst)) in self.instances.iter_mut() {
            // Feed the emitter's world transform so World-space tracks anchor correctly.
            let emitter = floptle_core::world_transform(world, *e);
            let g = grav_at(emitter.translation, inst.gravity_mode());
            // The world it is on: a celestial it hangs under, or the one whose
            // sphere of influence it is in. Its World-track particles ride it.
            let frame = celestial_ancestor(world, *e).or_else(|| frame_at(emitter.translation));
            match frame {
                Some(f) => {
                    let now = floptle_core::world_transform(world, f);
                    if let Some((was, last)) = self.node_frames.get(e)
                        && *was == f
                    {
                        inst.carry(last, &now);
                    }
                    self.node_frames.insert(*e, (f, now));
                }
                None => {
                    self.node_frames.remove(e);
                }
            }
            inst.up = match inst.gravity_mode() {
                floptle_vfx::GravityMode::Field => up_against(g),
                _ => None,
            }
            .unwrap_or(emitter.rotation * Vec3::Y);
            inst.advance_at(dt, g, emitter);
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
        // Detached one-shots: tick at their (drifting) world point with inherited
        // emitter velocity, then reap the finished.
        for d in &mut self.detached {
            // Riding a frame: the emitter is where its place on the frame is now,
            // and the World-track particles move with the frame. A frame that
            // has gone leaves the effect where it was.
            if let Some(link) = &d.frame
                && !world.is_alive(link.entity)
            {
                d.frame = None;
            }
            let emitter = match &mut d.frame {
                Some(link) => {
                    let now = floptle_core::world_transform(world, link.entity);
                    d.inst.carry(&link.last, &now);
                    link.last = now;
                    now.mul_transform(&link.local)
                }
                None => floptle_core::transform::Transform {
                    translation: d.pos,
                    rotation: d.rot,
                    scale: Vec3::splat(d.scale),
                },
            };
            d.pos = emitter.translation;
            d.rot = emitter.rotation;
            d.scale = emitter.scale.x;
            let g = grav_at(d.pos, d.inst.gravity_mode());
            d.inst.advance_at_moving(dt, g, emitter, d.vel);
            // Carry the emit point along with the inherited motion so a still-emitting
            // effect keeps pace with the vessel it was fired from. In a frame the
            // velocity is relative to it, so the point moves in the frame.
            match &mut d.frame {
                Some(link) => {
                    let step = link.last.rotation.inverse() * (d.vel * dt);
                    link.local.translation += step.as_dvec3();
                }
                None => d.pos += (d.vel * dt).as_dvec3(),
            }
        }
        self.detached.retain(|d| !d.inst.is_done());
    }

    /// The per-node particle state scripts read via `node:particles()`: one entry per
    /// ParticleSystem node — `playing`/`alive` from its live instance (if any) plus the
    /// effect asset key. Fed to the script host before each Play-mode script frame.
    /// Live particles across every effect, attached and detached.
    ///
    /// One number for the frame readout. Per-effect counts are already reachable
    /// as `node:particles():alive()`; this is the one a game checks against a
    /// budget without knowing which effect went wrong.
    pub fn live_particles(&self) -> usize {
        self.instances.values().map(|(_, i)| i.alive()).sum::<usize>()
            + self.detached.iter().map(|d| d.inst.alive()).sum::<usize>()
    }

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
                .entity_with::<ParticleSystem>(eid)
                .and_then(|e| world.get::<ParticleSystem>(e).map(|ps| (e, ps.asset.clone())))
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
                floptle_script::VfxCmd::Intensity(i) => {
                    if let Some((_, inst)) = self.instances.get_mut(&e) {
                        inst.set_intensity(i);
                    }
                }
                floptle_script::VfxCmd::Tint(t) => {
                    self.node_tints.insert(e, t);
                    if let Some((_, inst)) = self.instances.get_mut(&e) {
                        inst.tint = t;
                    }
                }
                // Aim every Beam track at a world point: convert to effect-local
                // (undo the emitter's rotation/scale) so the beam keeps tracking the
                // target as the node moves — the sim/draw side only knows local.
                floptle_script::VfxCmd::SetBeamEnd(p) => {
                    if let Some((_, inst)) = self.instances.get_mut(&e) {
                        let tr = floptle_core::world_transform(world, e);
                        let rel = (DVec3::from_array(p) - tr.translation).as_vec3();
                        let unrotated = tr.rotation.inverse() * rel;
                        let safe = |v: f32| if v.abs() > 1e-6 { v } else { 1.0 };
                        let local = Vec3::new(
                            unrotated.x / safe(tr.scale.x),
                            unrotated.y / safe(tr.scale.y),
                            unrotated.z / safe(tr.scale.z),
                        );
                        inst.set_beam_end(local);
                    }
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
            // Trails + beams fold into the same instance/batch stream — the render
            // pass draws ribbons as ordinary oriented quads, no extra pipeline.
            collect_trails(inst, local_xf, world_xf, cam_right, out_instances, &mut draws);
            collect_beams(inst, local_xf, world_xf, cam_right, out_instances, &mut draws);
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
        for (e, (_, inst)) in &self.instances {
            pack(inst, node_xf(*e));
        }
        for d in &self.detached {
            pack(&d.inst, d.emitter().render_matrix(cam.world_position));
        }
        if include_preview
            && let Some(p) = &self.preview
        {
            pack(&p.inst, p.emitter.render_matrix(cam.world_position));
        }
    }

    /// Every texture path any registered effect's billboard / beam / trail tracks
    /// reference — for the editor's texture pre-warm. Includes the live preview's
    /// tracks so a just-picked (unsaved) texture resolves next frame.
    pub fn texture_paths(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut scan = |fx: &CompiledEffect| {
            for track in &fx.tracks {
                let mut push = |p: &String| {
                    if !out.contains(p) {
                        out.push(p.clone());
                    }
                };
                match &track.look.render {
                    RenderMode::Billboard { texture: Some(p) } => push(p),
                    RenderMode::Beam { texture: Some(p) } => push(p),
                    _ => {}
                }
                if let Some(Trail { texture: Some(p), .. }) = &track.trail {
                    push(p);
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
        for (e, (_, inst)) in &self.instances {
            pack(inst, node_xf(*e));
        }
        for d in &self.detached {
            pack(&d.inst, d.emitter().render_matrix(cam.world_position));
        }
        if include_preview
            && let Some(p) = &self.preview
        {
            pack(&p.inst, p.emitter.render_matrix(cam.world_position));
        }
        out
    }
}

/// The light lit particles take: the key light (the way to the sun, or the
/// brightest star's camera-relative position), its colour, and the ambient.
#[derive(Clone, Copy, Debug, Default)]
pub struct ParticleLight {
    pub dir: [f32; 4],
    pub color: [f32; 4],
    pub ambient: [f32; 4],
}


/// The particle pass's frame globals for `cam` (billboard basis from its rotation),
/// plus the scene's depth-fog uniforms so distant particles fade into the fog, and
/// the light lit particles take.
pub fn particle_globals(
    cam: &RenderCamera,
    aspect: f32,
    fog_color: [f32; 4],
    fog_params: [f32; 4],
    light: ParticleLight,
) -> ParticleGlobals {
    let (r, u) = (cam.rotation * Vec3::X, cam.rotation * Vec3::Y);
    ParticleGlobals {
        view_proj: cam.view_proj(aspect).to_cols_array_2d(),
        cam_right: [r.x, r.y, r.z, 0.0],
        cam_up: [u.x, u.y, u.z, 0.0],
        fog_color,
        fog_params,
        proj_z: ParticleGlobals::proj_z(&cam.proj_matrix(aspect), cam.projection.is_ortho()),
        light_dir: light.dir,
        light_color: light.color,
        ambient: light.ambient,
    }
}

/// Reserved keys + display labels for the built-in primitive meshes offered in the VFX
/// mesh-render picker, so a mesh track isn't limited to user `.glb` assets. Registered
/// into `mesh_registry` at startup (main.rs `resumed`); they then resolve by key exactly
/// like an imported model, and `ensure_vfx_assets`' disk-import for a `builtin://…` key is
/// a no-op because the key is already present. This slice is the single source of truth
/// for which built-ins exist — both the registration and the picker read it.
pub(crate) const BUILTIN_PARTICLE_MESHES: &[(&str, &str)] = &[
    ("builtin://sphere", "● Sphere"),
    ("builtin://cube", "■ Cube"),
    ("builtin://capsule", "▮ Capsule"),
    ("builtin://pyramid", "▲ Pyramid"),
    ("builtin://cone", "◣ Cone"),
    ("builtin://cylinder", "▯ Cylinder"),
];

/// Geometry for a built-in particle-mesh key (see [`BUILTIN_PARTICLE_MESHES`]). Sized to
/// roughly a unit extent so a particle `size` of 1.0 ≈ one world unit; `None` for any key
/// not in the table.
pub(crate) fn builtin_particle_mesh_data(key: &str) -> Option<floptle_render::MeshData> {
    use floptle_render::{capsule, cone, cube, cylinder, pyramid, uv_sphere};
    Some(match key {
        "builtin://sphere" => uv_sphere(0.5, 24, 36),
        "builtin://cube" => cube(0.5),
        "builtin://capsule" => capsule(0.35, 0.4, 16, 24),
        "builtin://pyramid" => pyramid(0.5, 1.0),
        "builtin://cone" => cone(0.5, 1.0, 32),
        "builtin://cylinder" => cylinder(0.5, 0.5, 32),
        _ => return None,
    })
}

/// The picker label for a built-in mesh key, or `None` for a user asset path.
pub(crate) fn builtin_particle_mesh_label(key: &str) -> Option<&'static str> {
    BUILTIN_PARTICLE_MESHES.iter().find(|(k, _)| *k == key).map(|(_, l)| *l)
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
        speed_stretch: 0.0,
        flipbook: None,
        trail: None,
        segments: 12,
        beam_end: [0.0, 5.0, 0.0],
        wave_amplitude: 0.0,
        wave_frequency: 2.0,
        scroll: 0.0,
        lit: false,
        soft: floptle_scene::vfx::DEFAULT_SOFT,
        cast_shadows: false,
        distortion: 0.015,
        space: floptle_scene::VfxSpaceDoc::Local,
        // A continuous stream over the whole 1 s loop; each particle lives the clip's
        // length (1 s), so it loops seamlessly. Deprecated track-level fields stay at
        // their defaults (not serialized).
        clips: vec![floptle_scene::VfxClipDoc {
            start: 0.0,
            end: 1.0,
            lifetime_jitter: 0.4,
            emit: Some(floptle_scene::VfxEmitDoc::Rate { rate: 40.0 }),
        }],
        automation: Vec::new(),
        shape: VfxShapeDoc::Cone { angle: 25.0, radius: 0.1 },
        max_alive: None,
        bursts: Vec::new(),
        rate: 10.0,
        particle_lifetime: 1.0,
        lifetime_jitter: 0.0,
        velocity: VfxPropDoc::Const(VfxValueDoc::Vec3([0.0, 3.0, 0.0])),
        squash: VfxPropDoc::Const(VfxValueDoc::F32(1.0)),
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
        inherit_velocity: 0.0,
        forces: Vec::new(),
    }];
    VfxEffectDoc {
        name: name.into(),
        // Loop length matches the clip so the continuous fountain wraps seamlessly.
        lifetime: 1.0,
        playback: VfxPlaybackDoc::Looping,
        end: Default::default(),
        tracks,
        seed: 1,
        gravity_mode: Default::default(),
        lifetime_scale_mode: Default::default(),
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

/// A clip's emission mode from its doc form; `fallback_rate` covers an unmigrated legacy
/// clip (`emit == None`) — normally `migrate_clips` has already filled it on load.
fn emit_from_doc(e: Option<floptle_scene::VfxEmitDoc>, fallback_rate: f32) -> Emit {
    match e {
        Some(floptle_scene::VfxEmitDoc::Rate { rate }) => Emit::Rate { rate },
        Some(floptle_scene::VfxEmitDoc::Burst { count, count_jitter, pulses, interval, interval_jitter }) => {
            Emit::Burst { count, count_jitter, pulses, interval, interval_jitter }
        }
        None => Emit::Rate { rate: fallback_rate },
    }
}

/// Build the runtime clips for a track. Also folds any legacy `bursts` still on the doc
/// into single-pulse burst clips (defensive — `migrate_clips` normally does this on load).
fn clips_from_doc(t: &floptle_scene::VfxTrackDoc) -> Vec<Clip> {
    let mut clips: Vec<Clip> = t
        .clips
        .iter()
        .map(|c| Clip {
            start: c.start,
            end: c.end,
            lifetime_jitter: c.lifetime_jitter,
            emit: emit_from_doc(c.emit, t.rate),
        })
        .collect();
    for b in &t.bursts {
        clips.push(Clip {
            start: b.t,
            end: b.t + t.particle_lifetime.max(1e-3),
            lifetime_jitter: t.lifetime_jitter,
            emit: Emit::Burst { count: b.count, count_jitter: 0.0, pulses: 1, interval: 0.0, interval_jitter: 0.0 },
        });
    }
    clips
}

fn prop_from_doc(p: &VfxPropDoc) -> ValueOrCurve {
    match p {
        VfxPropDoc::Const(v) => ValueOrCurve::Const(value_from_doc(v)),
        VfxPropDoc::Range(a, b) => ValueOrCurve::Range(value_from_doc(a), value_from_doc(b)),
        VfxPropDoc::Curve(c) => ValueOrCurve::Curve(curve_from_doc(c)),
        VfxPropDoc::CurveRange(a, b) => ValueOrCurve::CurveRange(curve_from_doc(a), curve_from_doc(b)),
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
                        VfxRenderDoc::Beam { texture } => {
                            RenderMode::Beam { texture: texture.clone() }
                        }
                    },
                    blend: match t.blend {
                        VfxBlendDoc::Alpha => Blend::Alpha,
                        VfxBlendDoc::Additive => Blend::Additive,
                        VfxBlendDoc::Premultiplied => Blend::Premultiplied,
                        VfxBlendDoc::Screen => Blend::Screen,
                        VfxBlendDoc::Multiply => Blend::Multiply,
                        VfxBlendDoc::Distortion => Blend::Distortion,
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
                    speed_stretch: t.speed_stretch,
                    flipbook: t.flipbook.as_ref().map(flipbook_from_doc),
                    lit: t.lit,
                    soft: t.soft,
                    cast_shadows: t.cast_shadows,
                    distortion: t.distortion,
                },
                space: match t.space {
                    floptle_scene::VfxSpaceDoc::Local => Space::Local,
                    floptle_scene::VfxSpaceDoc::World => Space::World,
                },
                clips: clips_from_doc(t),
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
                            VfxLaneTargetDoc::Aspect => LaneTarget::Aspect,
                        },
                        curve: curve_from_doc(&l.curve),
                    })
                    .collect(),
                shape: match t.shape {
                    VfxShapeDoc::Point => EmitShape::Point,
                    VfxShapeDoc::Cone { angle, radius } => EmitShape::Cone { angle, radius },
                    VfxShapeDoc::Sphere { radius, shell } => EmitShape::Sphere { radius, shell },
                    VfxShapeDoc::Edge { length } => EmitShape::Edge { length },
                    VfxShapeDoc::Ring { radius } => EmitShape::Ring { radius },
                    VfxShapeDoc::Box { size } => EmitShape::Box { size: Vec3::from_array(size) },
                },
                max_alive: t.max_alive,
                velocity: prop_from_doc(&t.velocity),
                size: prop_from_doc(&t.size),
                squash: prop_from_doc(&t.squash),
                rotation: prop_from_doc(&t.rotation),
                angular_velocity: prop_from_doc(&t.angular_velocity),
                color: prop_from_doc(&t.color),
                gravity: t.gravity,
                drag: t.drag,
                inherit_velocity: t.inherit_velocity,
                forces: t.forces.iter().map(force_from_doc).collect(),
                trail: t.trail.as_ref().map(trail_from_doc),
                segments: t.segments,
                beam_end: Vec3::from_array(t.beam_end),
                wave_amplitude: t.wave_amplitude,
                wave_frequency: t.wave_frequency,
                scroll: t.scroll,
            })
            .collect(),
        gravity_mode: match doc.gravity_mode {
            floptle_scene::VfxGravityDoc::WorldDown => floptle_vfx::GravityMode::WorldDown,
            floptle_scene::VfxGravityDoc::Field => floptle_vfx::GravityMode::Field,
        },
    }
}

fn trail_from_doc(t: &VfxTrailDoc) -> Trail {
    Trail {
        time: t.time,
        width: t.width,
        fade: t.fade,
        texture: t.texture.clone(),
        min_distance: t.min_distance,
        emitter_path: t.emitter_path,
    }
}

fn flipbook_from_doc(f: &VfxFlipbookDoc) -> Flipbook {
    Flipbook {
        cols: f.cols,
        rows: f.rows,
        mode: match f.mode {
            VfxFlipModeDoc::OverLife => FlipMode::OverLife,
            VfxFlipModeDoc::LoopFps => FlipMode::LoopFps,
        },
        fps: f.fps,
        blend: f.blend,
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
        // The DAW timeline authors automation keys in seconds spanning [0, dur]; the
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
            preview: Some(VfxPreview {
                key: "T".into(),
                inst,
                anchor: None,
                emitter: floptle_core::transform::Transform::IDENTITY,
            }),
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

    // Regression guard for the solar demo's ship VFX: the real Flame + Explosion
    // effects must resolve out of the project registry and actually emit. If this
    // ever fails, the plume/explosion "disappeared" bug is in load/registration —
    // if it passes (as it does), a runtime absence is scene/script/anchor, not data.
    #[test]
    fn solar_demo_ship_effects_resolve_and_emit() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../solar");
        if !root.join("vfx").join("Flame.vfx.ron").exists() {
            return; // demo project not present in this checkout — skip
        }
        let mut sys = VfxSystem::default();
        sys.rescan(&root);
        for (key, tag) in [("vfx/Flame", "throttle plume"), ("Explosion", "crash burst")] {
            let fx = sys.effect(key).unwrap_or_else(|| panic!("{tag}: '{key}' must resolve"));
            let mut inst = EffectInstance::new(fx, 1);
            inst.simulate_to(0.4, VFX_GRAVITY);
            assert!(inst.alive() > 0, "{tag}: '{key}' resolved but emitted nothing");
        }
    }
}

// ---------------------------------------------------------------------------
// What an effect costs
// ---------------------------------------------------------------------------

/// How many particles an effect has alive over its own lifetime, and where it
/// ran out of pool.
///
/// Every surface in the Particles tab showed what you authored and none showed
/// what it does — which is the whole of "it's hard to tell how my change is
/// going to affect it". The one quantity that varies over time is how many
/// particles exist, and the timeline's axis is already time, so plotting it is
/// the answer the tab was built for and never used.
///
/// The sim is deterministic given `(effect.seed, instance_seed, step sizes)`, so
/// this profile is what the game will get and not an estimate. It is measured on
/// its own instance rather than the live preview, which is scrubbable and can be
/// mid-re-simulation.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VfxProfile {
    /// Total particles alive at each sample, from `t = 0` across `span`.
    pub alive: Vec<u32>,
    /// The same, per track.
    pub per_track: Vec<Vec<u32>>,
    /// The seconds the samples cover.
    pub span: f32,
    /// The most alive at any one moment, in total and per track.
    pub peak: u32,
    pub peak_per_track: Vec<u32>,
    /// Pool size per track — what `peak_per_track` is measured against.
    pub capacity: Vec<u32>,
    /// Births each track could not have because its pool was full. Non-zero
    /// means the effect on screen is not the effect that was authored.
    pub dropped: Vec<u32>,
}

impl VfxProfile {
    /// Whether any track was asked for more than it can hold.
    pub fn over_capacity(&self) -> bool {
        self.dropped.iter().any(|&d| d > 0)
    }

    /// Alive at a normalized position through the span, for drawing.
    pub fn at(&self, u: f32) -> u32 {
        if self.alive.is_empty() {
            return 0;
        }
        let i = ((u.clamp(0.0, 1.0) * (self.alive.len() - 1) as f32).round() as usize)
            .min(self.alive.len() - 1);
        self.alive[i]
    }
}

/// How many samples a profile takes across the span. Enough to draw a strip a
/// few hundred pixels wide without aliasing a burst into nothing, and few enough
/// that re-profiling on every edit is not felt.
const PROFILE_SAMPLES: usize = 240;

/// Simulate `effect` from zero over its lifetime (plus the tail a one-shot's
/// last particles need) and record what it costs.
pub fn profile_effect(effect: &ParticleEffect) -> VfxProfile {
    let compiled = std::sync::Arc::new(effect.compile());
    let n_tracks = compiled.tracks.len();
    let capacity: Vec<u32> = compiled.tracks.iter().map(|t| t.capacity).collect();
    // A one-shot keeps drawing after it stops emitting: its last particles live
    // out the longest clip. Profiling only to `lifetime` would report a peak
    // that is real and a tail that is missing.
    let tail = compiled
        .tracks
        .iter()
        .flat_map(|t| t.clips.iter().map(|c| c.lifetime()))
        .fold(0.0f32, f32::max);
    let span = (effect.lifetime + tail).max(0.05);
    let mut inst = floptle_vfx::EffectInstance::new(compiled, 0);
    let dt = span / PROFILE_SAMPLES as f32;

    let mut alive = Vec::with_capacity(PROFILE_SAMPLES + 1);
    let mut per_track = vec![Vec::with_capacity(PROFILE_SAMPLES + 1); n_tracks];
    let mut peak_per_track = vec![0u32; n_tracks];
    let mut peak = 0u32;
    for _ in 0..=PROFILE_SAMPLES {
        let total = inst.alive() as u32;
        alive.push(total);
        peak = peak.max(total);
        for (i, row) in per_track.iter_mut().enumerate() {
            let n = inst.track_alive(i) as u32;
            row.push(n);
            peak_per_track[i] = peak_per_track[i].max(n);
        }
        inst.advance(dt, VFX_GRAVITY);
    }
    let dropped = (0..n_tracks).map(|i| inst.track_dropped(i)).collect();
    VfxProfile { alive, per_track, span, peak, peak_per_track, capacity, dropped }
}

#[cfg(test)]
mod profile_tests {
    use super::*;
    use floptle_vfx::{Clip, Emit, ParticleEffect, Track};

    fn burst_effect(count: u32, max_alive: Option<u32>) -> ParticleEffect {
        let mut t = Track { max_alive, ..Track::default() };
        t.clips = vec![Clip {
            start: 0.0,
            end: 1.0,
            lifetime_jitter: 0.0,
            emit: Emit::Burst {
                count,
                count_jitter: 0.0,
                pulses: 1,
                interval: 0.1,
                interval_jitter: 0.0,
            },
        }];
        ParticleEffect { lifetime: 1.0, tracks: vec![t], ..ParticleEffect::default() }
    }

    /// The profile is measured, not estimated: a 40-particle burst peaks at 40.
    #[test]
    fn the_profile_counts_what_the_effect_actually_emits() {
        let p = profile_effect(&burst_effect(40, None));
        assert_eq!(p.peak, 40, "a 40 burst peaks at 40");
        assert_eq!(p.peak_per_track, vec![40]);
        assert!(!p.over_capacity(), "a pool sized from the clips is not over it");
        assert!(p.span >= 1.0, "the span covers the tail, not only the lifetime");
    }

    /// Asking a track for more than its pool holds is reported. It used to be
    /// dropped in silence, which is how an effect comes out thinner than it was
    /// authored with nothing to point at.
    #[test]
    fn asking_for_more_than_the_pool_holds_is_reported() {
        let p = profile_effect(&burst_effect(500, Some(50)));
        assert_eq!(p.capacity, vec![50]);
        assert!(p.peak <= 50, "the pool is the ceiling, got {}", p.peak);
        assert!(p.over_capacity(), "and going over it must be visible");
        assert_eq!(p.dropped[0], 450, "every birth it could not have is counted");
    }

    /// The strip has something to draw across the whole span, and reading past
    /// either end is clamped rather than panicking.
    #[test]
    fn the_density_can_be_sampled_anywhere() {
        let p = profile_effect(&burst_effect(30, None));
        assert!(!p.alive.is_empty());
        assert_eq!(p.at(-1.0), p.alive[0]);
        assert_eq!(p.at(2.0), *p.alive.last().unwrap());
        assert!(p.alive.iter().any(|&n| n > 0), "a burst is visible somewhere in it");
    }

    /// An effect with no tracks profiles to nothing rather than dividing by zero
    /// somewhere in the drawing.
    #[test]
    fn an_empty_effect_costs_nothing() {
        let p = profile_effect(&ParticleEffect::default());
        assert_eq!(p.peak, 0);
        assert!(!p.over_capacity());
        assert_eq!(p.at(0.5), 0);
    }
}
