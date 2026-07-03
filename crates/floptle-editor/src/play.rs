//! Play mode + simulation: building the physics sim from the scene (colliders,
//! gravity field), play/pause lifecycle, and script-host synchronization.

use floptle_core::Entity;
use floptle_core::Matter;
use floptle_core::ScriptInst;
use floptle_core::Scripts;
use floptle_core::math::DVec3;
use floptle_core::math::Mat4;
use floptle_core::math::Vec3;
use floptle_core::transform::Transform;
use std::path::Path;
use crate::assets::{is_script, script_name_of};
use crate::dock::{EditorTab};
use crate::{Editor, grab_cursor};

impl Editor {
    /// Build the physics gravity field from the scene's GravityVolume nodes: `Down`
    /// volumes add uniform −Y gravity (the level's base), `Radial` volumes add a planet
    /// gravity well at the node. No GravityVolume node → ZERO gravity (a space/zero-g
    /// world). Takes `&World` (not `&self`) so it can be called from the play loop
    /// while `self.gpu`/egui are mutably borrowed — see call site.
    /// Build the scene's gravity field for the sim. `origin` is the sim's world origin
    /// (ADR-0015): radial centers are converted to the sim frame in f64 here, so a
    /// planet placed far out pulls exactly.
    pub(crate) fn build_gravity_field(world: &floptle_core::World, origin: DVec3) -> floptle_physics::GravityField {
        use floptle_core::{GravityMode, Matter};
        let mut field = floptle_physics::GravityField::default();
        for (e, m) in world.query::<Matter>() {
            if let Matter::GravityVolume { mode, strength, radius } = m {
                match mode {
                    GravityMode::Down => field
                        .sources
                        .push(floptle_physics::GravitySource::Uniform(Vec3::new(0.0, -*strength, 0.0))),
                    GravityMode::Radial => {
                        let p = floptle_core::world_transform(world, e).translation;
                        field.sources.push(floptle_physics::GravitySource::Point {
                            center: (p - origin).as_vec3(),
                            strength: *strength,
                            radius: *radius,
                        });
                    }
                }
            }
        }
        field
    }

    /// Where the sim's local frame should be centered at Play (ADR-0015): the active
    /// camera if there is one, else the first rigidbody, else the world origin —
    /// rounded to whole units so every later rebase shift stays exact in f32.
    pub(crate) fn sim_origin_hint(&self) -> DVec3 {
        use floptle_core::Matter;
        let focus = self
            .world
            .query::<Matter>()
            .find_map(|(e, m)| matches!(m, Matter::Camera { active: true, .. }).then_some(e))
            .or_else(|| self.world.query::<floptle_core::RigidBody>().map(|(e, _)| e).next());
        focus
            .map(|e| floptle_core::world_transform(&self.world, e).translation.round())
            .unwrap_or(DVec3::ZERO)
    }

    pub(crate) fn add_static_colliders(&self, sim: &mut floptle_physics::Sim) {
        // Union of Collidable + legacy MeshCollider entities (dedup; a node flagged both
        // is added once). A node with a RigidBody is a *dynamic* body (Sim::build made it
        // one) — skip it here so its dynamic body doesn't fight a static collider sitting at
        // the same spot (which would freeze/eject it). Collidable = static world geometry
        // only when there's no RigidBody.
        let mut ents: Vec<Entity> = self
            .world
            .query::<floptle_core::Collidable>()
            .map(|(e, _)| e)
            .filter(|e| self.world.get::<floptle_core::RigidBody>(*e).is_none())
            .collect();
        for (e, _) in self.world.query::<floptle_core::MeshCollider>() {
            if !ents.contains(&e) && self.world.get::<floptle_core::RigidBody>(e).is_none() {
                ents.push(e);
            }
        }
        for e in ents {
            let wt = floptle_core::world_transform(&self.world, e);
            // Anchor each collider on its own node (full f64) and bake geometry
            // RELATIVE to it — the residuals stay small and exact no matter how far
            // out the node sits (ADR-0015); the sim re-anchors them per rebase.
            let anchor = wt.translation;
            let s = wt.scale;
            match self.world.get::<Matter>(e) {
                Some(Matter::Mesh { asset_path }) => {
                    let path = asset_path.clone();
                    let Ok(model) = floptle_assets::gltf_import::import(std::path::Path::new(&path)) else {
                        eprintln!("collidable mesh: failed to load {path}");
                        continue;
                    };
                    // Scale + rotate locally (f32 is exact here — model-sized numbers);
                    // the node's translation lives in the f64 anchor, never the verts.
                    let m = Mat4::from_scale_rotation_translation(s, wt.rotation, Vec3::ZERO);
                    let mut verts: Vec<Vec3> = Vec::new();
                    let mut indices: Vec<u32> = Vec::new();
                    for part in &model.parts {
                        let base = verts.len() as u32;
                        verts.extend(part.mesh.vertices.iter().map(|v| m.transform_point3(Vec3::from(v.pos))));
                        indices.extend(part.mesh.indices.iter().map(|i| i + base));
                    }
                    sim.add_static_mesh(anchor, &verts, &indices);
                }
                // Primitive geometry → matching analytic collider, sized to match the
                // mesh the renderer draws (cube half 0.7, sphere r 0.85, capsule r/half 0.5).
                Some(Matter::Primitive { shape, .. }) => match shape {
                    floptle_core::Shape::Cube => {
                        sim.add_static_box(anchor, Vec3::new(0.7 * s.x, 0.7 * s.y, 0.7 * s.z), wt.rotation);
                    }
                    floptle_core::Shape::Sphere => {
                        sim.add_static_sphere(anchor, 0.85 * s.max_element());
                    }
                    floptle_core::Shape::Capsule => {
                        let up = wt.rotation * Vec3::Y;
                        sim.add_static_capsule(anchor, up, 0.5 * s.y, 0.5 * s.x.max(s.z));
                    }
                },
                _ => {}
            }
        }
    }

    /// Rebuild the live physics sim from the current scene. A no-op unless playing —
    /// called after a physics component (rigidbody / collider / type) changes mid-Play
    /// so the edit takes effect immediately. Bodies re-seed at their current transforms.
    pub(crate) fn rebuild_sim(&mut self) {
        if !self.playing {
            return;
        }
        let origin = self.sim_origin_hint();
        let gravity = Self::build_gravity_field(&self.world, origin);
        let terrain_vols = self.terrain_volumes();
        let mut sim = floptle_physics::Sim::build(&self.world, &terrain_vols, gravity, origin);
        drop(terrain_vols);
        self.add_static_colliders(&mut sim);
        self.sim = Some(sim);
    }

    /// Every terrain volume as `(node world translation, node-local field)` — what the
    /// sim colliders anchor on. Each volume collides at its NATIVE resolution (the
    /// combined field is render-only), placed in full `f64` (ADR-0015).
    pub(crate) fn terrain_volumes(&self) -> Vec<(DVec3, &floptle_field::Terrain)> {
        self.terrains
            .iter()
            .map(|(&e, t)| (floptle_core::world_transform(&self.world, e).translation, t))
            .collect()
    }

    /// Enter/leave play mode. Play snapshots the authored scene and runs scripts;
    /// Stop restores the authored scene so script-driven changes aren't persisted.
    /// Drop every animator runtime + the Animating tab's entity bindings —
    /// called whenever the World is rebuilt (scene/project switches), since
    /// entity handles from the old world alias entities in the new one.
    pub(crate) fn reset_anim_bindings(&mut self) {
        self.stop_recording();
        self.anim.clear_instances();
        self.anim_ui.target = None;
        self.anim_ui.sel_anim = None;
        self.anim_ui.clip_doc = None;
        self.anim_ui.preview_playing = false;
        self.anim_ui.last_scene_local.clear();
    }

    /// Turn ● Record off and put the posed subtree back exactly as it was
    /// when recording started — recording authors the CLIP, never the scene.
    pub(crate) fn stop_recording(&mut self) {
        if !self.anim_ui.record && self.anim_ui.record_restore.is_empty() {
            return;
        }
        self.anim_ui.record = false;
        for (e, tr) in self.anim_ui.record_restore.drain(..) {
            if let Some(slot) = self.world.get_mut::<Transform>(e) {
                *slot = tr;
            }
        }
        self.anim_ui.last_scene_local.clear();
    }

    pub(crate) fn toggle_play(&mut self) {
        // Fresh animator runtimes both ways (Play binds against the live scene;
        // Stop drops them so the restored scene isn't posed by stale animators).
        self.anim.clear_instances();
        // Same for particle instances — nothing emits outside Play (phase 1).
        self.vfx.clear_instances();
        self.anim_ui.preview_playing = false;
        // Recording must never run during Play (gameplay motion would bake into
        // the clip asset), and stale queued animator commands must not leak
        // across sessions.
        self.stop_recording();
        self.script_host.clear_anim_state();
        self.script_gizmos.clear();
        if self.playing {
            self.playing = false;
            self.paused = false;
            self.sim = None; // drop the physics sim; restore reverts moved transforms
            // Release any script-held mouse lock so you're not stuck grabbed after Stop.
            if self.script_mouse_lock {
                self.script_mouse_lock = false;
                if let Some(window) = self.window.as_ref() {
                    self.cursor_lock_soft = grab_cursor(window, false);
                }
            }
            if let Some(snap) = self.play_snapshot.take() {
                self.restore(snap);
            }
        } else {
            // Scripts run from what's on DISK — flush unsaved IDE edits first so
            // Play always tests the code you're looking at.
            let mut flushed = 0;
            for f in self.ide.open.iter_mut().filter(|f| f.dirty) {
                if std::fs::write(&f.path, &f.text).is_ok() {
                    f.dirty = false;
                    flushed += 1;
                }
            }
            self.play_snapshot = Some(self.snapshot());
            self.play_t = 0.0;
            self.paused = false;
            // Build the physics sim from the scene: RigidBody nodes + every terrain
            // volume (its own anchored SDF collider, native resolution) + the gravity
            // field from GravityVolume nodes.
            let origin = self.sim_origin_hint();
            let gravity = Self::build_gravity_field(&self.world, origin);
            let terrain_vols = self.terrain_volumes();
            let mut sim = floptle_physics::Sim::build(&self.world, &terrain_vols, gravity, origin);
            drop(terrain_vols);
            // Add static colliders (any node flagged "Collidable", plus legacy mesh
            // colliders) so a character can walk on / bump into them, not just terrain.
            self.add_static_colliders(&mut sim);
            self.sim = Some(sim);
            // Start play with a clean Console so you only see this run's output.
            self.console.entries.clear();
            if flushed > 0 {
                self.console.push(
                    floptle_script::LogLevel::Debug,
                    format!("⏵ auto-saved {flushed} edited script(s)"),
                    None,
                );
            }
            // Press Play → bring the Game tab to the front (active-camera view), so it's
            // clear you're testing the game, not the editor scene view.
            if let Some(dock) = self.dock_state.as_mut()
                && let Some(path) = dock.find_tab(&EditorTab::Game) {
                    let _ = dock.set_active_tab(path);
                }
            // Spawn play-on-start particle effects on their nodes.
            self.vfx.start_play(&self.world);
            self.playing = true;
        }
    }

    /// Freeze/unfreeze the script clock while playing.
    pub(crate) fn toggle_pause(&mut self) {
        if self.playing {
            self.paused = !self.paused;
        }
    }

    /// A script's declared `defaults`, cached by file mtime so we only re-parse the Lua
    /// when the file actually changes (keeps the per-frame inspector sync cheap).
    pub(crate) fn cached_script_defaults(&mut self, name: &str) -> Vec<(String, f32)> {
        let path = self.project_root.join("scripts").join(format!("{name}.lua"));
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let key = name.to_string();
        if let (Some(mt), Some((cached_mt, vals))) = (mtime, self.script_defaults_cache.get(&key))
            && *cached_mt == mt {
                return vals.clone();
            }
        let vals = self.script_host.script_defaults(&path);
        if let Some(mt) = mtime {
            self.script_defaults_cache.insert(key, (mt, vals.clone()));
        }
        vals
    }

    /// Keep the selected node's script `params` in step with each script's current
    /// `defaults`, so editing a script (adding/removing/renaming a `defaults` key)
    /// is reflected live in the Inspector: new defaults appear as tweakable params,
    /// keys removed from `defaults` drop off, and the user's overridden values for
    /// keys that still exist are preserved. Display-only (the runtime already merges
    /// defaults at call time) and not recorded as an undo step.
    pub(crate) fn sync_selected_script_params(&mut self) {
        let Some(e) = self.selection.last().copied() else { return };
        let names: Vec<String> = match self.world.get::<Scripts>(e) {
            Some(s) => s.0.iter().map(|i| i.kind.clone()).collect(),
            None => return,
        };
        // Resolve each script's current defaults first (needs &mut self for the cache).
        let defaults: Vec<Vec<(String, f32)>> =
            names.iter().map(|n| self.cached_script_defaults(n)).collect();
        let Some(scr) = self.world.get_mut::<Scripts>(e) else { return };
        for (inst, defs) in scr.0.iter_mut().zip(defaults) {
            // An empty result means "no defaults declared" OR a transient parse error
            // (e.g. mid-edit) — never wipe the user's overrides in that case.
            if defs.is_empty() {
                continue;
            }
            // Drop params no longer declared in defaults.
            inst.params.retain(|(k, _)| defs.iter().any(|(dk, _)| dk == k));
            // Add any newly-declared defaults (preserving the order defaults come in).
            for (dk, dv) in &defs {
                if !inst.params.iter().any(|(k, _)| k == dk) {
                    inst.params.push((dk.clone(), *dv));
                }
            }
        }
    }

    /// Attach the `.lua` script at `path` to `target`, seeding its `params` from
    /// the script's declared `defaults`.
    pub(crate) fn attach_script_file(&mut self, path: &str, target: Option<Entity>) {
        let Some(e) = target else { return };
        if self.world.get::<Transform>(e).is_none() || !is_script(path) {
            return;
        }
        if !Path::new(path).exists() {
            eprintln!("  script not found: {path}");
            return;
        }
        let name = script_name_of(path);
        let params = self.script_host.script_defaults(Path::new(path));
        self.record();
        let inst = ScriptInst { kind: name, enabled: true, params };
        if let Some(scr) = self.world.get_mut::<Scripts>(e) {
            scr.0.push(inst);
        } else {
            self.world.insert(e, Scripts(vec![inst]));
        }
    }
}
