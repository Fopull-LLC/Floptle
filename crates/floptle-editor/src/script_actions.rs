//! EDITOR ACTIONS — Lua tooling that runs in EDIT mode (the Unity
//! editor-script analog). A script declares a button:
//!
//! ```lua
//! --@editorButton Generate roll
//! function roll(node) ... end
//! ```
//!
//! and the Inspector shows **Generate** on that script component; clicking
//! runs `roll(node)` against the OPEN scene: transform/component writes,
//! `createNode`/`spawn`/`destroy`, the construction setters and
//! `terrain.generatePlanet` all land in the edited scene (undo-recorded).
//! Heavy terrain generations run on a background thread and adopt in as they
//! finish.

use crate::Editor;

/// Parse a script source's `--@editorButton <Label> <fn>` annotations
/// (`<fn>` defaults to the label; underscores in the label display as
/// spaces). Cheap enough to run per Inspector frame for the selected node.
pub(crate) fn script_editor_buttons(root: &std::path::Path, kind: &str) -> Vec<(String, String)> {
    let path = root.join("scripts").join(format!("{kind}.lua"));
    let Ok(src) = std::fs::read_to_string(&path) else { return Vec::new() };
    src.lines()
        .filter_map(|l| {
            let rest = l.trim().strip_prefix("--@editorButton")?.trim();
            let mut it = rest.split_whitespace();
            let label = it.next()?.to_string();
            let func = it.next().map(str::to_string).unwrap_or_else(|| label.clone());
            Some((label.replace('_', " "), func))
        })
        .collect()
}

impl Editor {
    /// Run one editor action: `func(node)` from script `kind` on node `e`,
    /// against the edit-mode world, then apply everything it queued.
    pub(crate) fn run_editor_action(&mut self, e: floptle_core::Entity, kind: &str, func: &str) {
        // One undo step for the whole action (record() no-ops during Play).
        self.record();
        self.script_host.set_project_root(self.project_root.clone());
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
            self.terrain_slots.clear();
            self.terrain_gpu_dirty = true;
        }
    }

    /// Hand queued `terrain.generatePlanet` fills to a background thread —
    /// seconds per body; the editor stays interactive and fields adopt in
    /// via [`Self::poll_terrain_generates`] as they finish.
    pub(crate) fn drain_terrain_generates(&mut self) {
        let gens = self.script_host.take_terrain_generates();
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
            format!("⛰ generating {} terrain field(s) in the background…", gens.len()),
            None,
        );
        self.planet_gen_pending.extend(gens.iter().map(|(id, _)| *id));
        let (tx, rx) = std::sync::mpsc::channel();
        self.planet_gen_job = Some(rx);
        std::thread::spawn(move || {
            for (id, fill) in gens {
                let t0 = std::time::Instant::now();
                let field = floptle_field::procgen::generate_planet(&fill);
                if tx.send((id, field, t0.elapsed().as_millis() as u64)).is_err() {
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
        for (id, field, ms) in arrived {
            self.planet_gen_pending.remove(&id);
            let target = self
                .world
                .query::<floptle_core::Matter>()
                .find_map(|(e, m)| match m {
                    floptle_core::Matter::Terrain { id: tid } if *tid == id => Some(e),
                    _ => None,
                });
            let Some(e) = target else {
                self.console.push(
                    floptle_script::LogLevel::Warn,
                    format!("⛰ generated field for terrain id {id}, but no node carries it"),
                    None,
                );
                continue;
            };
            let chunks = field.data_chunks();
            self.terrains.insert(e, crate::terrain_edit::EditorTerrain::new(field));
            // A generated field exists ONLY in RAM until the scene is saved — an
            // eviction (G1 residency) must write it to disk before dropping it.
            self.terrain_disk_dirty.insert(e);
            // Restream every terrain's render chunks (cheap, brief) + rebuild
            // the SDF shadow atlas around the new field.
            self.terrain_slots.clear();
            self.terrain_gpu_dirty = true;
            // Generation may finish DURING Play (▶ Generate then Play before the
            // fill lands, or a runtime regeneration): rebuild the sim so the new
            // surface is solid immediately — a body standing there must never
            // fall through a planet whose field just arrived. This is also what
            // releases the Play-start streaming hold for mid-generation bodies.
            if self.sim.is_some() {
                self.rebuild_sim();
                self.console.push(
                    floptle_script::LogLevel::Debug,
                    format!("⛰ terrain id {id} generated during Play — collision live"),
                    None,
                );
            }
            self.console.push(
                floptle_script::LogLevel::Debug,
                format!("⛰ terrain id {id} ready — {chunks} chunks in {:.1}s", ms as f64 / 1000.0),
                None,
            );
        }
        if done {
            self.planet_gen_job = None;
            self.planet_gen_pending.clear(); // a crashed batch must not wedge streaming
        }
    }
}
