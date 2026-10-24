//! editor actions — Lua tooling that runs in edit mode (the Unity
//! editor-script analog). A script declares a button:
//!
//! ```lua
//! --@editorButton Generate roll
//! function roll(node) ... end
//! ```
//!
//! and the Inspector shows **Generate** on that script component; clicking
//! runs `roll(node)` against the open scene: transform/component writes,
//! `createNode`/`spawn`/`destroy`, the construction setters and
//! `terrain.generatePlanet` all land in the edited scene (undo-recorded).
//! Heavy terrain generations run on a background thread and adopt in as they
//! finish.

use crate::Editor;

impl Editor {
    /// Run one editor action: `func(node)` from script `kind` on node `e`,
    /// against the edit-mode world, then apply everything it queued.
    pub(crate) fn run_editor_action(&mut self, e: floptle_core::Entity, kind: &str, func: &str) {
        // One undo step for the whole action (record() no-ops during Play).
        self.record();
        self.point_scripts_at_project();
        let scripts_dir = self.project_root.join("scripts");
        let ran =
            self.script_host.call_action(&mut self.world, &scripts_dir, e.index(), kind, func);
        // Surface prints + errors in the Console like a play pass would.
        for l in self.script_host.drain_logs() {
            self.console.push(l.level, l.msg, l.source);
        }
        if !ran {
            for err in self.script_host.errors() {
                self.console.push(floptle_script::LogLevel::Error, err.clone(), None);
            }
        }
        // Apply the queues an action is allowed to touch. Node/component
        // writes already flushed inside call_action.
        self.apply_script_spawns(); // createNode + spawn(prefab) + destroy
        self.drain_script_terrain_ops(); // dabs/paints (sim mirror no-ops without a sim)
        self.drain_terrain_generates(); // whole-planet fills → background thread
        // Physics/runtime-only queues make no sense in edit mode — drop them
        // so they can't leak into the next Play.
        let _ = self.script_host.take_body_changes();
        let _ = self.script_host.take_body_height_changes();
        let _ = self.script_host.take_body_pos_changes();
        let _ = self.script_host.take_gizmos();
        let _ = self.script_host.take_draw_lines();
        let _ = self.script_host.take_vfx_commands();
        let _ = self.script_host.take_anim_commands();
        let _ = self.script_host.take_audio_commands();
        let _ = self.script_host.take_warp_request();
        // Model swaps need their GPU meshes.
        for (_, path) in self.script_host.take_model_changes() {
            self.import_model(&path);
        }
        self.register_scene_meshes();
        // Prune terrain state for nodes the action destroyed (a generator
        // replacing a planet system destroys the old body nodes).
        let live: std::collections::HashSet<floptle_core::Entity> = self
            .world
            .query::<floptle_core::Matter>()
            .filter_map(|(t, m)| matches!(m, floptle_core::Matter::Terrain { .. }).then_some(t))
            .collect();
        let before = self.terrains.len();
        self.terrains.retain(|t, _| live.contains(t));
        if self.terrains.len() != before {
            // Their slots stay drawn-as-absent until the atlas is re-laid.
            self.terrain_gpu_dirty = true;
        }
    }

    /// Hand queued `terrain.generatePlanet` fills to a background thread —
    /// seconds per body; the editor stays interactive and fields adopt in
    /// via [`Self::poll_terrain_generates`] as they finish.
    pub(crate) fn drain_terrain_generates(&mut self) {
        let gens = self.script_host.take_terrain_generates();
        self.start_terrain_generates(gens);
    }

    /// Start a batch of planet fills on a background thread: the work of
    /// [`Self::drain_terrain_generates`], for fills from anywhere.
    pub(crate) fn start_terrain_generates(&mut self, gens: Vec<(u32, floptle_field::procgen::PlanetFill)>) {
        // Where the player is on each body now, for the meshes made with it.
        let views: Vec<Option<crate::terrain_edit::LandingView>> = gens
            .iter()
            .map(|(id, _)| {
                let e = self.world.query::<floptle_core::Matter>().find_map(|(e, m)| match m {
                    floptle_core::Matter::Terrain { id: tid, .. } if tid == id => Some(e),
                    _ => None,
                });
                e.map(|e| self.landing_view(e))
            })
            .collect();
        if gens.is_empty() {
            return;
        }
        if self.planet_gen_job.is_some() {
            self.console.push(
                floptle_script::LogLevel::Warn,
                "terrain.generatePlanet: a generation batch is already running — request dropped"
                    .into(),
                None,
            );
            return;
        }
        self.console.push(
            floptle_script::LogLevel::Debug,
            format!("Δ generating {} terrain field(s) in the background…", gens.len()),
            None,
        );
        self.planet_gen_pending.extend(gens.iter().map(|(id, _)| *id));
        self.planet_gen_radius.extend(gens.iter().map(|(id, fill)| (*id, fill.radius)));
        let (tx, rx) = std::sync::mpsc::channel();
        self.planet_gen_job = Some(rx);
        crate::worker::spawn("planet-gen", move || {
            for ((id, fill), view) in gens.into_iter().zip(views) {
                let t0 = floptle_core::time::Instant::now();
                let mut field = floptle_field::procgen::generate_planet(&fill);
                // The recipe goes with the field, so its saves hold only edits.
                if let Ok(spec) = ron::to_string(&fill) {
                    field.set_spec(spec);
                }
                // The shadow proxy is derived here, with the field: on the main
                // thread it was the 6 to 26 s frame a big world landed in.
                let mut terrain = crate::terrain_edit::EditorTerrain::new(field);
                // …and the meshes its landing frame would make round the player.
                if let Some(v) = view {
                    terrain.premesh(&v);
                }
                if tx.send((id, terrain, t0.elapsed().as_millis() as u64)).is_err() {
                    return;
                }
            }
        });
    }

    /// Adopt finished planet fields: replace the matching terrain node's
    /// authority field, restream render chunks, refresh the SDF atlas.
    pub(crate) fn poll_terrain_generates(&mut self) {
        let Some(rx) = &self.planet_gen_job else { return };
        let mut done = false;
        let mut arrived = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(v) => arrived.push(v),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    done = true;
                    break;
                }
            }
        }
        let t0 = floptle_core::profile::Span::new();
        let landed = !arrived.is_empty();
        for (id, terrain, ms) in arrived {
            self.planet_gen_pending.remove(&id);
            self.planet_gen_radius.remove(&id);
            let target = self
                .world
                .query::<floptle_core::Matter>()
                .find_map(|(e, m)| match m {
                    floptle_core::Matter::Terrain { id: tid, .. } if *tid == id => Some(e),
                    _ => None,
                });
            let Some(e) = target else {
                self.console.push(
                    floptle_script::LogLevel::Warn,
                    format!("Δ generated field for terrain id {id}, but no node carries it"),
                    None,
                );
                continue;
            };
            let chunks = terrain.field.data_chunks();
            self.terrains.insert(e, terrain);
            // Generating it let go of a great deal: see `give_memory_back`.
            self.terrain_trim_due = true;
            // Its meshes and impostor were of whatever stood there before: the
            // starter slab's chunks went on drawing inside the new world, and
            // the ball was the slab's grass.
            if let (Some(raster), Some(mut r)) = (self.raster.as_mut(), self.terrain_render.remove(&e)) {
                r.free_all(raster);
                r.born.clear();
            }
            // A generated field exists only in RAM until the scene is saved — an
            // eviction (G1 residency) must write it to disk before dropping it.
            self.touch_terrain_edit(e);
            // Lay the SDF shadow atlas again around the new field (its meshes
            // plan from scratch: their entry is gone). The old field's volume
            // keeps casting from its slot until the new one is in. Every other
            // terrain's meshes stay as they are.
            self.terrain_atlas_dirty = true;
            self.terrain_replan.insert(e);
            // Generation may finish during Play (▶ Generate then Play before the
            // fill lands, or a runtime regeneration): its collider goes into the
            // sim now, so the new surface is solid immediately — a body standing
            // there must never fall through a planet whose field just arrived.
            if self.sim.is_some() {
                self.land_terrain_in_sim(e);
                self.console.push(
                    floptle_script::LogLevel::Debug,
                    format!("Δ terrain id {id} generated during Play — collision live"),
                    None,
                );
            }
            self.console.push(
                floptle_script::LogLevel::Debug,
                format!("Δ terrain id {id} ready — {chunks} chunks in {:.1}s", ms as f64 / 1000.0),
                None,
            );
        }
        if landed {
            // What a generated world costs the frame it lands in, named.
            let ms = t0.ms();
            self.profile_record(floptle_core::profile::Bucket::Terrain, ms);
            self.script_host.profile().borrow_mut().record_part("planetLanded", ms);
        }
        if done {
            self.planet_gen_job = None;
            self.planet_gen_pending.clear(); // a crashed batch must not wedge streaming
        }
    }
}
