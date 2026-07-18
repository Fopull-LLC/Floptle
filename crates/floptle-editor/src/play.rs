//! Play mode + simulation: building the physics sim from the scene (colliders,
//! gravity field), play/pause lifecycle, and script-host synchronization.

use floptle_core::Entity;
use floptle_core::Matter;
use floptle_core::World;
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
        // Celestial bodies (solar demo S2): real µ/r² sources with patched-conic
        // SOI dominance — the deepest body whose SOI contains you is the ONE
        // that pulls (see `GravitySource::InvSq`). SOI 0 auto-derives Laplace
        // from the parent's µ and the orbit's semi-major axis.
        let cb: Vec<(Entity, floptle_core::CelestialBody, DVec3)> = world
            .query::<floptle_core::CelestialBody>()
            .map(|(e, b)| (e, b.clone(), floptle_core::world_transform(world, e).translation))
            .collect();
        let name_of = |e: Entity| {
            world.get::<floptle_core::Name>(e).map(|n| n.0.clone()).unwrap_or_default()
        };
        for (e, b, pos) in &cb {
            if b.mu <= 0.0 {
                continue;
            }
            let _ = e;
            let soi = if b.soi > 0.0 {
                b.soi
            } else if b.parent.is_empty() {
                0.0 // root: infinite (≤ 0 means unbounded in the source)
            } else if let Some((_, pb, _)) =
                cb.iter().find(|(pe, ..)| name_of(*pe) == b.parent)
            {
                floptle_core::frames::System::soi_radius(b.a.abs(), b.mu, pb.mu)
            } else {
                0.0
            };
            field.sources.push(floptle_physics::GravitySource::InvSq {
                center: (*pos - origin).as_vec3(),
                mu: b.mu as f32,
                soi: soi as f32,
                body_r: b.body_radius as f32,
            });
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
            // The node's identity for this collider: resolved layer bit (the
            // collision matrix + masked raycasts filter with it), entity (what
            // touch events name), and the trigger flag (sensor: events only).
            let layer = sim.tag_for(&self.world, e);
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
                    sim.add_static_mesh(anchor, &verts, &indices, layer);
                }
                // Primitive geometry → matching analytic collider, sized to match the
                // mesh the renderer draws (cube half 0.7, sphere r 0.85, capsule r/half 0.5).
                Some(Matter::Primitive { shape, .. }) => match shape {
                    floptle_core::Shape::Cube => {
                        sim.add_static_box(anchor, Vec3::new(0.7 * s.x, 0.7 * s.y, 0.7 * s.z), wt.rotation, layer);
                    }
                    floptle_core::Shape::Plane => {
                        // Flat in Z → a thin box so you can stand on / collide with the quad.
                        sim.add_static_box(anchor, Vec3::new(0.7 * s.x, 0.7 * s.y, 0.02 * s.z.max(1.0)), wt.rotation, layer);
                    }
                    floptle_core::Shape::Sphere => {
                        sim.add_static_sphere(anchor, 0.85 * s.max_element(), layer);
                    }
                    floptle_core::Shape::Capsule => {
                        let up = wt.rotation * Vec3::Y;
                        sim.add_static_capsule(anchor, up, 0.5 * s.y, 0.5 * s.x.max(s.z), layer);
                    }
                },
                _ => {}
            }
        }
    }

    /// Build the play sim under the PROJECT'S LAYER TABLE: terrain + static
    /// colliders carry their node's layer bit, dynamic bodies resolve theirs,
    /// the collision matrix lands in the world, and the script host is lent
    /// the same table (`node.layer` validation + `raycast` layer filters).
    /// Nodes naming a layer the project no longer defines get a Console
    /// warning (they behave as Default). The one path every sim build takes —
    /// Play start, mid-play rebuilds, and scene switches.
    pub(crate) fn build_play_sim(&mut self) -> floptle_physics::Sim {
        let layers = self.project.build_layers();
        for (e, l) in self.world.query::<floptle_core::Layer>() {
            if layers.index_of(&l.0).is_none() {
                let name = self
                    .world
                    .get::<floptle_core::Name>(e)
                    .map(|n| n.0.clone())
                    .unwrap_or_else(|| format!("#{}", e.index()));
                self.console.push(
                    floptle_script::LogLevel::Warn,
                    format!(
                        "node '{name}' is on unknown layer '{}' — treated as Default \
                         (define it in Project Settings → Layers)",
                        l.0
                    ),
                    None,
                );
            }
        }
        // Foot-gun guard: a celestial scene with a UNIFORM-Down GravityVolume
        // adds a constant world −Y pull on top of µ/r² — on the far side of a
        // planet that pushes AWAY from it, pumping orbital energy every pass
        // (it cost two debugging sessions as a mystery "orbit escape").
        {
            let has_celestial = self
                .world
                .query::<floptle_core::CelestialBody>()
                .any(|(_, b)| b.mu > 0.0);
            let down_volume = self.world.query::<floptle_core::Matter>().any(|(_, m)| {
                matches!(
                    m,
                    floptle_core::Matter::GravityVolume {
                        mode: floptle_core::GravityMode::Down,
                        strength,
                        ..
                    } if *strength != 0.0
                )
            });
            if has_celestial && down_volume {
                self.console.push(
                    floptle_script::LogLevel::Warn,
                    "scene mixes Celestial-Body µ/r² gravity with a uniform DOWN \
                     GravityVolume — the constant world −Y pull adds energy to orbits \
                     on a planet's far side (looks like mysterious escapes). Set the \
                     volume's strength to 0 or delete it."
                        .into(),
                    None,
                );
            }
        }
        let origin = self.sim_origin_hint();
        let gravity = Self::build_gravity_field(&self.world, origin);
        let terrain_vols = self.terrain_volumes(&layers);
        let mut sim =
            floptle_physics::Sim::build_layered(&self.world, &terrain_vols, gravity, origin, layers);
        drop(terrain_vols);
        // Add static colliders (any node flagged "Collidable", plus legacy mesh
        // colliders) so a character can walk on / bump into them, not just terrain.
        self.add_static_colliders(&mut sim);
        self.script_host.set_layers(sim.layers().clone());
        sim
    }

    /// Rebuild the live physics sim from the current scene. A no-op unless playing —
    /// called after a physics component (rigidbody / collider / type) changes mid-Play
    /// so the edit takes effect immediately. Bodies re-seed at their current transforms.
    pub(crate) fn rebuild_sim(&mut self) {
        if !self.playing {
            return;
        }
        let sim = self.build_play_sim();
        self.sim = Some(sim);
    }

    /// Every terrain volume as `(node world translation, node-local field, layer
    /// bit, node entity)` — what the sim colliders anchor on (the entity is what
    /// touch events name). Each volume collides at its NATIVE resolution (the
    /// combined field is render-only), placed in full `f64` (ADR-0015).
    pub(crate) fn terrain_volumes(
        &self,
        layers: &floptle_core::Layers,
    ) -> Vec<floptle_physics::TerrainVolume<'_>> {
        self.terrains
            .iter()
            .map(|(&e, t)| {
                let (anchor, rot, scale) = self.terrain_world_frame_of(e);
                floptle_physics::TerrainVolume {
                    anchor,
                    field: &t.field,
                    layer: layers.index_for(&self.world, e),
                    eid: Some(e.index()),
                    rot,
                    scale,
                }
            })
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
    /// One implementation for every path (transport, play start, undo, save):
    /// restores transforms AND recorded property values, and forgets the
    /// preview snapshot (stale mid-record state — never to be applied).
    pub(crate) fn stop_recording(&mut self) {
        if !self.anim_ui.record && self.anim_ui.record_restore.is_empty() {
            return;
        }
        crate::anim_ui::stop_record_ui(&mut self.world, &mut self.anim_ui);
        self.anim.forget_preview();
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
        self.script_lines.clear();
        if self.playing {
            self.playing = false;
            self.paused = false;
            // Make the revert EXPLICIT — "where did my tweaks go" is a classic
            // lost-work surprise: Play-mode changes are a simulation, not edits.
            self.console.push(
                floptle_script::LogLevel::Debug,
                "⏹ stopped — the scene reverted to its pre-Play state (changes made \
                 during Play are not kept)"
                    .into(),
                None,
            );
            // Silence the play session's sounds and revert Lua mixer tweaks.
            let mixer = self.project.mixer.clone();
            self.audio.stop_play(&mixer);
            self.sim = None; // drop the physics sim; restore reverts moved transforms
            // Multiplayer sessions live inside a play session — never across Stop.
            self.net_stop("play stopped");
            // Release any script-held mouse lock or Game-view cursor trap so you're not
            // stuck grabbed after Stop.
            if self.script_mouse_lock || self.game_trap {
                self.script_mouse_lock = false;
                self.game_trap = false;
                if let Some(window) = self.window.as_ref() {
                    self.cursor_lock_soft = grab_cursor(window, false);
                }
            }
            // A mid-play `scene.load(...)` renamed the scene for the session —
            // the restored world is the PRE-PLAY scene, so its name must come
            // back BEFORE `restore()` runs: restore's `adopt_terrain()` loads
            // terrain fields by scene name, and doing this after it once made
            // Stop fill the editor scene's terrain nodes with the PLAYED
            // scene's fields (the next save then overwrote the real terrain
            // on disk — real lost work).
            self.pending_scene = None;
            if let Some((name, rel)) = self.play_scene_name.take() {
                self.scene_name = name;
                self.scene_rel = rel;
            }
            if let Some(snap) = self.play_snapshot.take() {
                self.restore(snap);
            }
            // Terrain fields live OUTSIDE the scene doc, so the snapshot above
            // doesn't carry them — bring back the exact pre-Play fields (+
            // texture palette). Disk can't stand in: it may be behind unsaved
            // sculpts, and a mid-play scene switch swapped the live fields for
            // the played scene's.
            // Persistent `save.*` data flushes on Stop — the one guarantee scripts
            // rely on (periodic flushes during Play only bound crash loss).
            self.script_host.flush_save();
            if let Some((fields, palette)) = self.play_terrains.take() {
                for (id, t) in fields {
                    if let Some(e) = self.terrain_entity_of_id(id) {
                        self.terrains.insert(e, crate::terrain_edit::EditorTerrain::new(t));
                    }
                }
                self.terrain_textures = palette;
                self.terrain_textures_dirty = true;
                self.terrain_gpu_dirty = !self.terrains.is_empty();
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
            self.play_scene_name = Some((self.scene_name.clone(), self.scene_rel.clone()));
            // Snapshot the live terrain fields (id-keyed) + texture palette so
            // Stop restores them exactly — unsaved sculpts survive Play, and a
            // mid-play scene switch can never leak another scene's terrain
            // into this one (see Stop above).
            self.play_terrains = Some((
                self.terrains
                    .iter()
                    .filter_map(|(&e, t)| match self.world.get::<floptle_core::Matter>(e) {
                        Some(floptle_core::Matter::Terrain { id }) => {
                            Some((*id, t.field.clone()))
                        }
                        _ => None,
                    })
                    .collect(),
                self.terrain_textures.clone(),
            ));
            self.pending_scene = None;
            self.play_t = 0.0;
            self.paused = false;
            self.terrain_mirror_warned = false; // fresh Play, fresh one-shot warning
            self.space_time = 0.0; // rails restart from the authored epoch
            self.space_warp = 1.0;
            self.space_coast.clear();
            self.script_lines.clear(); // no stale map lines across runs
            // Every Play is a FRESH RUN: drop all script instances so top-level
            // script state can't leak across sessions (Ty's ship still thought
            // he was piloting after Stop → Play). `start()` re-fires for all.
            self.script_host.reset_instances();
            // Fresh gameplay-tick clock (the netcode timebase): no banked time, tick 0,
            // and no stale per-tick input edges from before Play.
            self.game_tick.reset();
            self.game_tick_no = 0;
            self.tick_keys_pressed.clear();
            self.tick_keys_released.clear();
            self.tick_buttons_pressed = [false; 3];
            self.tick_mouse_delta = (0.0, 0.0);
            self.tick_scroll = 0.0;
            // Build the physics sim from the scene: RigidBody nodes + every terrain
            // volume (its own anchored SDF collider, native resolution) + the gravity
            // field from GravityVolume nodes + static colliders — all under the
            // project's layer table (collision matrix + raycast filters).
            let sim = self.build_play_sim();
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
            // Fire play-on-start sounds through the project mixer.
            let mixer = self.project.mixer.clone();
            let root = self.project_root.clone();
            self.audio.start_play(&self.world, &root, &mixer);
            self.playing = true;
            // Outside a session, only player slot #1 takes input: extra
            // Predicted nodes (multiplayer slots) idle instead of mirroring
            // the keyboard into every copy of the controller.
            self.net_apply_offline_slots();
        }
    }

    /// Freeze/unfreeze the script clock while playing.
    pub(crate) fn toggle_pause(&mut self) {
        if self.playing {
            self.paused = !self.paused;
        }
    }

    /// Resolve a `scene.load(...)` argument to a scene file: a name ("arena"),
    /// a scenes-relative name ("arenas/desert"), or a project-relative path
    /// ("scenes/arena.ron"). Escapes are REJECTED — in multiplayer the string
    /// arrives over the wire, so it must never reach outside the project.
    pub(crate) fn resolve_scene_request(&self, req: &str) -> Option<std::path::PathBuf> {
        let r = req.trim().replace('\\', "/");
        if r.is_empty() || r.contains("..") || r.starts_with('/') || r.contains(':') {
            return None;
        }
        let with_ext = if r.ends_with(".ron") { r.clone() } else { format!("{r}.ron") };
        [with_ext.clone(), format!("scenes/{with_ext}")]
            .into_iter()
            .map(|c| self.project_root.join(c))
            .find(|p| p.is_file())
    }

    /// Perform a scene transition while Play runs: swap the world to the new
    /// scene and rebuild every play-session runtime (scripts, physics, anim,
    /// vfx, audio) against it — `start` re-fires everywhere, exactly like the
    /// scene booting fresh. The editor's own scene (play snapshot + name) is
    /// untouched: Stop still restores exactly what you were editing. Returns
    /// the new scene's project-relative path (what a server announces).
    ///
    /// Session roles (filters, prediction, NetId rebinds) are the CALLER's job
    /// — see [`Self::perform_scene_request`].
    pub(crate) fn switch_scene_during_play(&mut self, req: &str) -> Option<String> {
        let Some(path) = self.resolve_scene_request(req) else {
            self.console.push(
                floptle_script::LogLevel::Error,
                format!("scene.load(\"{req}\"): no such scene (looked in scenes/)"),
                None,
            );
            return None;
        };
        let doc = match floptle_scene::load(&path) {
            Ok(d) => d,
            Err(e) => {
                self.console.push(
                    floptle_script::LogLevel::Error,
                    format!("scene.load(\"{req}\"): {e}"),
                    None,
                );
                return None;
            }
        };
        // Tear down the old scene's play runtimes…
        self.reset_anim_bindings();
        self.anim.clear_instances();
        self.vfx.clear_instances();
        self.script_host.clear_anim_state();
        self.script_host.reset_instances();
        self.script_gizmos.clear();
        let mixer = self.project.mixer.clone();
        self.audio.stop_play(&mixer);
        // …swap the world…
        self.world = World::new();
        floptle_scene::spawn_into(&doc, &mut self.world);
        self.set_scene_file(&path);
        self.adopt_terrain();
        self.register_scene_meshes();
        self.selection.clear();
        self.grabbed = None;
        self.drag = None;
        // …and rebuild the play session against it (the same steps as Play).
        let sim = self.build_play_sim();
        self.sim = Some(sim);
        self.vfx.start_play(&self.world);
        let root = self.project_root.clone();
        self.audio.start_play(&self.world, &root, &mixer);
        self.console.push(
            floptle_script::LogLevel::Debug,
            format!("⏵ scene → {}", self.scene_name),
            None,
        );
        Some(self.scene_rel_or_default())
    }

    /// A script's declared `defaults`, cached by file mtime so we only re-parse the Lua
    /// when the file actually changes (keeps the per-frame inspector sync cheap).
    /// Returns `(numeric params, node-ref param names)`.
    pub(crate) fn cached_script_defaults(
        &mut self,
        name: &str,
    ) -> crate::ScriptDefaults {
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
        let defaults: Vec<crate::ScriptDefaults> =
            names.iter().map(|n| self.cached_script_defaults(n)).collect();
        // Refresh the Inspector's ref-kind map for this selection.
        self.ref_kinds.clear();
        for (name, (_, refs, _)) in names.iter().zip(&defaults) {
            for (param, kind) in refs {
                self.ref_kinds.insert((name.clone(), param.clone()), kind.clone());
            }
        }
        let Some(scr) = self.world.get_mut::<Scripts>(e) else { return };
        for (inst, (defs, ref_decls, str_decls)) in scr.0.iter_mut().zip(defaults) {
            // An empty result means "no defaults declared" OR a transient parse error
            // (e.g. mid-edit) — never wipe the user's overrides in that case.
            if defs.is_empty() && ref_decls.is_empty() && str_decls.is_empty() {
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
            // Same for reference params (wired targets survive; stale keys drop).
            inst.refs.retain(|(k, _)| ref_decls.iter().any(|(rk, _)| rk == k));
            for (rk, _) in &ref_decls {
                if !inst.refs.iter().any(|(k, _)| k == rk) {
                    inst.refs.push((rk.clone(), String::new()));
                }
            }
            // Same for string params (overridden text survives; stale keys drop).
            inst.strs.retain(|(k, _)| str_decls.iter().any(|(sk, _)| sk == k));
            for (sk, sv) in &str_decls {
                if !inst.strs.iter().any(|(k, _)| k == sk) {
                    inst.strs.push((sk.clone(), sv.clone()));
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
        let (params, ref_decls, strs) = self.script_host.script_defaults(Path::new(path));
        self.record();
        let refs = ref_decls.into_iter().map(|(k, _)| (k, String::new())).collect();
        let inst = ScriptInst { kind: name, enabled: true, params, refs, strs };
        if let Some(scr) = self.world.get_mut::<Scripts>(e) {
            scr.0.push(inst);
        } else {
            self.world.insert(e, Scripts(vec![inst]));
        }
    }
}

#[cfg(test)]
mod scene_request_tests {
    use crate::Editor;

    /// `scene.load` strings resolve inside the project only: names,
    /// scenes-relative paths, and project-relative paths all work; escapes
    /// never do — in multiplayer the string arrives over the WIRE, so it must
    /// not be able to name anything outside the project.
    #[test]
    fn scene_requests_resolve_safely() {
        let root =
            std::env::temp_dir().join(format!("floptle-scene-req-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("scenes/arenas")).unwrap();
        std::fs::write(root.join("scenes/first.ron"), "()").unwrap();
        std::fs::write(root.join("scenes/arenas/desert.ron"), "()").unwrap();
        let ed = Editor { project_root: root.clone(), ..Default::default() };

        let first = root.join("scenes/first.ron");
        assert_eq!(ed.resolve_scene_request("first").as_deref(), Some(first.as_path()));
        assert_eq!(ed.resolve_scene_request("first.ron").as_deref(), Some(first.as_path()));
        assert_eq!(ed.resolve_scene_request("scenes/first.ron").as_deref(), Some(first.as_path()));
        let desert = root.join("scenes/arenas/desert.ron");
        assert_eq!(ed.resolve_scene_request("arenas/desert").as_deref(), Some(desert.as_path()));

        assert!(ed.resolve_scene_request("nope").is_none(), "missing scenes are None");
        assert!(ed.resolve_scene_request("../first").is_none(), "no escaping the project");
        assert!(ed.resolve_scene_request("/etc/passwd").is_none(), "no absolute paths");
        assert!(ed.resolve_scene_request("C:\\x").is_none(), "no Windows drives");
        assert!(ed.resolve_scene_request("").is_none());

        let _ = std::fs::remove_dir_all(&root);
    }
}
