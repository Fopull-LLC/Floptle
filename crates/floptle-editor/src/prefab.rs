//! Prefabs: reusable node subtrees saved as assets (`*.prefab.ron`).
//!
//! Created by dragging nodes from the Hierarchy into the Assets panel (or
//! right-click → "Save as Prefab"), instantiated by dragging the asset into
//! the viewport / onto a Hierarchy row, via the asset's context menu, or from
//! Lua with `spawn("name")`. The file body is the same flat `Vec<NodeDoc>`
//! format the node clipboard uses (`parent` = in-list index, `None` = a root),
//! so a prefab is loadable anywhere the clipboard is.

use std::path::{Path, PathBuf};

use floptle_core::Entity;
use floptle_core::math::DVec3;
use floptle_scene::NodeDoc;
use crate::assets::{build_assets, unique_path};
use crate::Editor;

/// Parse a prefab file: pretty RON of `Vec<NodeDoc>`, tolerant of the node
/// clipboard's `//floptle-nodes-v1` tag line (a pasted clipboard IS a prefab).
pub(crate) fn load_prefab_docs(path: &Path) -> Result<Vec<NodeDoc>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let body = text.trim_start().strip_prefix("//floptle-nodes-v1").unwrap_or(&text);
    ron::from_str::<Vec<NodeDoc>>(body.trim_start())
        .map_err(|e| format!("{}: not a prefab ({e})", path.display()))
}

impl Editor {
    /// Open a prefab for editing **on its own** (`floptle/0090`): its nodes
    /// become the whole world, and saving writes back to this same file.
    ///
    /// A prefab is a reusable subtree, and the only way to change one used to be
    /// to drop it into whatever scene happened to be open, edit the instance and
    /// re-save — which never overwrites, so the obvious route left a second file
    /// beside the one you meant to change.
    ///
    /// The editing surface is the ordinary one: Hierarchy, Inspector, gizmos,
    /// undo, Play. What differs is only where a save goes, and that is decided by
    /// `editing_prefab` being set — see [`Editor::save_scene`].
    ///
    /// Note what a prefab does NOT bring with it. A scene open adopts terrain
    /// fields, tilesets, map geometry and paint from beside the scene file; a
    /// prefab is nodes and nothing else. So those stores are cleared rather than
    /// left holding the previous scene's, which would otherwise sit under the
    /// prefab looking like part of it — and would be written out under the
    /// prefab's name on the next scene save.
    pub(crate) fn open_prefab_file(&mut self, path: &str) {
        let p = Path::new(path);
        let docs = match load_prefab_docs(p) {
            Ok(d) if !d.is_empty() => d,
            Ok(_) => {
                self.console.push(
                    floptle_script::LogLevel::Warn,
                    format!("{} has no nodes in it", p.display()),
                    None,
                );
                return;
            }
            Err(e) => {
                self.console.push(floptle_script::LogLevel::Error, e, None);
                return;
            }
        };
        self.reset_anim_bindings();
        self.playing = false;
        self.paused = false;
        self.play_snapshot = None;
        self.world = floptle_core::World::new();
        self.editing_prefab = Some(p.to_path_buf());
        // The prefab's own name, so every readout that says "which scene" says
        // which prefab instead. `scene_rel` stays the real path, which is what
        // the title bar wants. Set BEFORE the adopts below, because the stores
        // they clear-then-reload are keyed by this name.
        self.scene_name = p
            .file_name()
            .map(|n| n.to_string_lossy().trim_end_matches(".prefab.ron").to_string())
            .unwrap_or_else(|| "prefab".into());
        self.scene_rel = p
            .strip_prefix(&self.project_root)
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| path.to_string());
        self.spawn_docs(&docs);
        // The same sequence a scene open runs, and for the opposite reason: each
        // of these clears its store before reloading, and a prefab has no
        // terrain, no map geometry and no paint of its own — so running them
        // empties the previous scene's out instead of leaving it underneath the
        // prefab, looking like part of it.
        self.adopt_terrain();
        self.adopt_tilesets();
        self.adopt_maps();
        self.adopt_paint();
        self.adopt_tex_paint();
        self.hier_fold_pending = true;
        self.collapsed.clear();
        self.register_scene_meshes();
        self.selection.clear();
        self.selected_asset = None;
        self.history = crate::History::default();
        self.scene_dirty = false;
        self.check_autosave(); // crash recovery, same as a scene open
        self.console.push(
            floptle_script::LogLevel::Debug,
            format!(
                "◇ editing prefab {} ({} node{}) — Save writes back to this file",
                self.scene_rel,
                docs.len(),
                if docs.len() == 1 { "" } else { "s" }
            ),
            None,
        );
    }

    /// Write the whole world back over the prefab file being edited.
    ///
    /// Overwrites, deliberately: this is the "in place" half of the task, and it
    /// is the difference between editing a prefab and making another one.
    pub(crate) fn save_prefab_in_place(&mut self) -> bool {
        let Some(path) = self.editing_prefab.clone() else { return false };
        // Every node in the world, top level first — `subtree_docs` walks the
        // children itself. Nodes are enumerated by their `Matter`, the same way
        // the scene serializer does it, so the two agree about what a node is.
        let roots: Vec<Entity> = self
            .world
            .query::<floptle_core::Matter>()
            .map(|(e, _)| e)
            .filter(|e| self.world.get::<floptle_core::Parent>(*e).is_none())
            .collect();
        let docs = self.subtree_docs(&roots);
        // An empty write would silently destroy the prefab. Refuse: the file on
        // disk is the only copy, and "I deleted everything" and "something went
        // wrong" look identical afterwards.
        if docs.is_empty() {
            self.console.push(
                floptle_script::LogLevel::Warn,
                format!(
                    "💾 not saved — {} would be left with no nodes in it",
                    path.display()
                ),
                None,
            );
            return false;
        }
        match ron::ser::to_string_pretty(&docs, ron::ser::PrettyConfig::default())
            .map_err(|e| e.to_string())
            .and_then(|ron| std::fs::write(&path, ron).map_err(|e| e.to_string()))
        {
            Ok(()) => {
                self.console.push(
                    floptle_script::LogLevel::Debug,
                    format!(
                        "💾 saved prefab {} ({} node{})",
                        path.display(),
                        docs.len(),
                        if docs.len() == 1 { "" } else { "s" }
                    ),
                    None,
                );
                true
            }
            Err(e) => {
                self.console.push(
                    floptle_script::LogLevel::Error,
                    format!(
                        "💾 SAVE FAILED — {} — {e} (your changes are still unsaved!)",
                        path.display()
                    ),
                    None,
                );
                false
            }
        }
    }

    /// Save `roots` — whole subtrees — as one prefab file in `dir`, named after
    /// the first root's node name. Never overwrites (auto-suffixes).
    pub(crate) fn save_prefab(&mut self, roots: &[Entity], dir: &Path) {
        let docs = self.subtree_docs(roots);
        if docs.is_empty() {
            return;
        }
        let stem: String = docs[0]
            .name
            .chars()
            .map(|c| if c.is_alphanumeric() || "-_ ".contains(c) { c } else { '_' })
            .collect::<String>()
            .trim()
            .to_string();
        let stem = if stem.is_empty() { "prefab".to_string() } else { stem };
        let _ = std::fs::create_dir_all(dir);
        let path = unique_path(dir, &stem, Some("prefab.ron"));
        match ron::ser::to_string_pretty(&docs, ron::ser::PrettyConfig::default()) {
            Ok(ron) => {
                if let Err(e) = std::fs::write(&path, ron) {
                    self.console.push(
                        floptle_script::LogLevel::Error,
                        format!("save prefab failed: {e}"),
                        None,
                    );
                    return;
                }
                self.console.push(
                    floptle_script::LogLevel::Debug,
                    format!(
                        "◇ saved prefab {} ({} node{})",
                        path.display(),
                        docs.len(),
                        if docs.len() == 1 { "" } else { "s" }
                    ),
                    None,
                );
                self.asset_tree = build_assets(&self.project_root);
                self.selected_asset = Some(path.to_string_lossy().to_string());
                self.asset_selection.clear();
            }
            Err(e) => {
                self.console.push(
                    floptle_script::LogLevel::Error,
                    format!("save prefab failed: {e}"),
                    None,
                );
            }
        }
    }

    /// Instantiate a prefab into the open scene. `at` places the FIRST root
    /// there (sibling roots keep their relative offsets); `None` keeps the
    /// authored placement. `parent` nests the new roots under a node (their
    /// authored root transforms become local offsets). Records undo and
    /// selects the new roots.
    pub(crate) fn instantiate_prefab(
        &mut self,
        path: &str,
        at: Option<DVec3>,
        parent: Option<Entity>,
    ) {
        if self.playing {
            self.console.push(
                floptle_script::LogLevel::Warn,
                "can't place a prefab while playing — stop first (or spawn(\"…\") from a script)"
                    .into(),
                None,
            );
            return;
        }
        let mut docs = match load_prefab_docs(Path::new(path)) {
            Ok(d) if !d.is_empty() => d,
            Ok(_) => return,
            Err(e) => {
                self.console.push(floptle_script::LogLevel::Error, e, None);
                return;
            }
        };
        if let Some(at) = at {
            let base = docs
                .iter()
                .find(|d| d.parent.is_none())
                .map(|d| DVec3::from(d.transform.translation))
                .unwrap_or(DVec3::ZERO);
            let shift = at - base;
            for d in docs.iter_mut().filter(|d| d.parent.is_none()) {
                d.transform.translation[0] += shift.x;
                d.transform.translation[1] += shift.y;
                d.transform.translation[2] += shift.z;
            }
        }
        self.record();
        let ents = self.spawn_docs(&docs);
        self.selection.clear();
        for (e, d) in ents.iter().zip(&docs) {
            if d.parent.is_none() {
                if let Some(p) = parent {
                    self.world.insert(*e, floptle_core::Parent(p));
                }
                self.selection.push(*e);
            }
        }
        // A prefab can carry Mesh nodes — make sure their models are imported
        // and registered with the renderer (idempotent rescan).
        self.register_scene_meshes();
    }

    /// Resolve a Lua `spawn("…")` argument to a prefab file: a name ("bullet"),
    /// a prefabs-relative name ("weapons/sword"), or a project-relative path
    /// ("prefabs/bullet.prefab.ron"). Escapes are rejected (same contract as
    /// `scene.load`) — in multiplayer the string can arrive over the wire.
    pub(crate) fn resolve_prefab_request(&self, req: &str) -> Option<PathBuf> {
        let r = req.trim().replace('\\', "/");
        if r.is_empty() || r.contains("..") || r.starts_with('/') || r.contains(':') {
            return None;
        }
        let with_ext = if r.ends_with(floptle_scene::PREFAB_EXT) {
            r.clone()
        } else {
            format!("{r}{}", floptle_scene::PREFAB_EXT)
        };
        [with_ext.clone(), format!("prefabs/{with_ext}")]
            .into_iter()
            .map(|c| self.project_root.join(c))
            .find(|p| p.is_file())
    }

    /// A prefab's parsed docs, cached by file mtime (spawning the same prefab
    /// every tick must not re-read + re-parse the file).
    fn cached_prefab_docs(&mut self, path: &Path) -> Option<Vec<NodeDoc>> {
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
        if let Some((t, docs)) = self.prefab_cache.get(path)
            && *t == mtime
        {
            return Some(docs.clone());
        }
        match load_prefab_docs(path) {
            Ok(docs) => {
                self.prefab_cache.insert(path.to_path_buf(), (mtime, docs.clone()));
                Some(docs)
            }
            Err(e) => {
                self.console.push(floptle_script::LogLevel::Error, e, None);
                None
            }
        }
    }

    /// Apply the `spawn(...)` / `destroy(...)` requests scripts queued this
    /// pass: spawn prefab subtrees (meshes registered, bodies added, callback
    /// invoked with the new root's handle) and despawn destroy targets (whole
    /// subtree + physics). Runs inside the play loop only — edit-time placement
    /// goes through [`Self::instantiate_prefab`].
    pub(crate) fn apply_script_spawns(&mut self) {
        // Bounded cascade: a spawn/create CALLBACK may itself create more
        // nodes (a generator building a hierarchy) — keep draining until the
        // queues go quiet so nested requests land the same drain.
        for _pass in 0..8 {
            let spawns = self.script_host.take_spawn_requests();
            let creates = self.script_host.take_create_requests();
            if spawns.is_empty() && creates.is_empty() {
                break;
            }
            self.apply_spawn_batch(spawns, creates);
        }
        // `ui.make(...)`: reconcile each described tree against the world. It
        // runs here, with the other node-creating queues, so a screen built in
        // `start` is on screen for the first frame's layout and hit test.
        let mut destroys = self.script_host.take_destroy_requests();
        if self.playing {
            destroys.extend(self.script_host.apply_ui_makes(&mut self.world));
        } else if self.script_host.discard_ui_makes() > 0 {
            // Same rule as a repeater's rows: made elements are runtime
            // content, and an editor action that conjured them into the open
            // scene would put engine-built nodes in a file about to be saved.
            self.console.push(
                floptle_script::LogLevel::Warn,
                "ui.make builds only while the game is playing — nothing was created".into(),
                None,
            );
        }
        if !destroys.is_empty() {
            self.apply_destroys(destroys);
        }

        // `nav.rebake(centre, size)` — AFTER the spawns and the destroys, which
        // is the whole point of it being a queue: a chunk asks for its box to be
        // re-measured in the same breath as it builds it, and the measurement
        // has to see the nodes rather than race them.
        for req in self.script_host.take_nav_rebakes() {
            let centre = DVec3::new(req.centre[0], req.centre[1], req.centre[2]);
            let size = floptle_core::math::Vec3::from(req.size);
            if let Err(why) = self.rebake_region(centre, size) {
                self.console.push(floptle_script::LogLevel::Warn, why, None);
            }
        }
    }

    pub(crate) fn apply_spawn_batch(
        &mut self,
        spawns: Vec<floptle_script::SpawnRequest>,
        creates: Vec<floptle_script::CreateRequest>,
    ) {
        // `createNode(name [, parent] [, fn])` — a plain Empty node; the
        // callback configures it (setTerrain/setCelestial/setPrimitive/
        // setMaterial + transform writes) right after it exists.
        for req in creates {
            let e = self.world.spawn();
            self.world.insert(e, floptle_core::transform::Transform::IDENTITY);
            self.world.insert(e, floptle_core::Name(req.name));
            self.world.insert(e, floptle_core::Matter::Empty);
            if let Some(pid) = req.parent {
                let pe = self
                    .world
                    .entity_with::<floptle_core::Matter>(pid);
                if let Some(pe) = pe {
                    self.world.insert(e, floptle_core::Parent(pe));
                }
            }
            if let Some(cb) = req.cb {
                self.script_host.call_create_callback(&mut self.world, cb, e);
            }
        }
        for req in spawns {
            let Some(path) = self.resolve_prefab_request(&req.prefab) else {
                self.console.push(
                    floptle_script::LogLevel::Error,
                    format!("spawn(\"{}\"): no such prefab (looked in prefabs/)", req.prefab),
                    None,
                );
                continue;
            };
            let Some(mut docs) = self.cached_prefab_docs(&path) else { continue };
            if docs.is_empty() {
                continue;
            }
            if let Some(p) = req.pos {
                let base = docs
                    .iter()
                    .find(|d| d.parent.is_none())
                    .map(|d| DVec3::from(d.transform.translation))
                    .unwrap_or(DVec3::ZERO);
                let shift = DVec3::from(p) - base;
                for d in docs.iter_mut().filter(|d| d.parent.is_none()) {
                    d.transform.translation[0] += shift.x;
                    d.transform.translation[1] += shift.y;
                    d.transform.translation[2] += shift.z;
                }
            }
            let ents = self.spawn_docs(&docs);
            // Only the models this prefab brought with it. See `register_meshes`.
            let fresh: Vec<&str> = docs
                .iter()
                .filter_map(|d| match &d.matter {
                    floptle_scene::MatterDoc::Mesh { asset_path, .. } => Some(asset_path.as_str()),
                    _ => None,
                })
                .collect();
            if !fresh.is_empty() {
                self.register_meshes(fresh);
            }
            // Optional parenting (`spawn(name, pos, fn, parentNode)`): the
            // spawned ROOTS go under the parent, keeping their WORLD pose —
            // convert into the parent's local frame. Done BEFORE physics
            // wiring so ancestry rules (assembly parts) see the hierarchy.
            if let Some(pid) = req.parent {
                let pe = self
                    .world
                    .entity_with::<floptle_core::Matter>(pid);
                if let Some(pe) = pe {
                    let pw = floptle_core::world_transform(&self.world, pe);
                    let inv_rot = pw.rotation.inverse();
                    let roots: Vec<_> = ents
                        .iter()
                        .zip(&docs)
                        .filter(|(_, d)| d.parent.is_none())
                        .map(|(&e, _)| e)
                        .collect();
                    for e in roots {
                        let ew = floptle_core::world_transform(&self.world, e);
                        let local = floptle_core::transform::Transform {
                            translation: inv_rot.as_dquat() * (ew.translation - pw.translation)
                                / pw.scale.as_dvec3().max(DVec3::splat(1e-9)),
                            rotation: (inv_rot * ew.rotation).normalize(),
                            scale: ew.scale
                                / pw.scale.max(floptle_core::math::Vec3::splat(1e-9)),
                        };
                        if let Some(t) =
                            self.world.get_mut::<floptle_core::transform::Transform>(e)
                        {
                            *t = local;
                        }
                        self.world.insert(e, floptle_core::Parent(pe));
                    }
                }
            }
            let root = ents
                .iter()
                .zip(&docs)
                .find(|(_, d)| d.parent.is_none())
                .map(|(&e, _)| e);
            // The callback runs BEFORE physics wiring (its transform writes
            // flush inside call_spawn_callback): a spawned Static prop whose
            // callback orients it (a launchpad aligned to a planet surface)
            // must bake its collider at the ORIENTED pose, not the authored
            // one. Velocity writes still land via the body-changes queue.
            if let (Some(cb), Some(root)) = (req.cb, root) {
                self.script_host.call_spawn_callback(&mut self.world, cb, root.index(), &ents);
            }
            if let Some(sim) = self.sim.as_mut() {
                for &e in &ents {
                    sim.add_body_for(e, &self.world);
                }
                // A spawned VESSEL prefab (assembly root) registers its whole
                // hierarchy as one compound (add_body_for refused the parts).
                for &e in &ents {
                    sim.add_compound_for(e, &self.world);
                }
            }
        }
    }

    /// Feed the per-frame `assembly.info` mirror from the sim's live compounds.
    pub(crate) fn feed_assembly_info(&mut self) {
        let Some(sim) = self.sim.as_ref() else { return };
        let origin = sim.world.origin;
        let mut map = std::collections::HashMap::new();
        for (eid, c) in sim.assemblies() {
            let o = c.origin();
            map.insert(
                eid,
                floptle_script::AssemblyInfo {
                    mass: c.mass,
                    com: [
                        origin.x + c.pos.x as f64,
                        origin.y + c.pos.y as f64,
                        origin.z + c.pos.z as f64,
                    ],
                    origin: [
                        origin.x + o.x as f64,
                        origin.y + o.y as f64,
                        origin.z + o.z as f64,
                    ],
                    vel: [c.vel.x, c.vel.y, c.vel.z],
                    ang_vel: [c.ang_vel.x, c.ang_vel.y, c.ang_vel.z],
                    grounded: c.grounded,
                    anchored: c.anchored,
                    parts: c.shapes.iter().map(|s| s.id as u32).collect(),
                },
            );
        }
        self.script_host.set_assembly_info(map);
        // Per-part contact loads from the last stepped tick (`assembly.impacts`
        // — the damage/stress raw material).
        let mut impacts: std::collections::HashMap<u32, Vec<floptle_script::AssemblyImpact>> =
            std::collections::HashMap::new();
        for (root, part, impulse, speed, speed_abs, point) in sim.compound_impacts() {
            impacts.entry(root).or_default().push(floptle_script::AssemblyImpact {
                part,
                impulse,
                speed,
                speed_abs,
                point: [point.x, point.y, point.z],
            });
        }
        self.script_host.set_assembly_impacts(impacts);
    }

    /// Drain queued `assembly.*` commands: held forces/impulses go to the sim;
    /// SPLITS are performed here — spawn a fresh vessel root, split the physics
    /// compound onto it, re-parent the detached part nodes (world pose kept),
    /// then hand the new root to the script callback.
    pub(crate) fn drain_assembly_cmds(&mut self) {
        let cmds = self.script_host.take_assembly_cmds();
        if cmds.is_empty() {
            return;
        }
        use floptle_core::math::{DVec3, Vec3};
        for cmd in cmds {
            match cmd {
                floptle_script::AssemblyCmd::Hold { root, force, at, torque } => {
                    if let Some(sim) = self.sim.as_mut() {
                        sim.hold_compound_force(
                            root,
                            Vec3::new(force[0] as f32, force[1] as f32, force[2] as f32),
                            at.map(|a| DVec3::new(a[0], a[1], a[2])),
                            Vec3::new(torque[0] as f32, torque[1] as f32, torque[2] as f32),
                        );
                    }
                }
                floptle_script::AssemblyCmd::Impulse { root, imp, at } => {
                    if let Some(sim) = self.sim.as_mut() {
                        sim.compound_impulse(
                            root,
                            Vec3::new(imp[0] as f32, imp[1] as f32, imp[2] as f32),
                            DVec3::new(at[0], at[1], at[2]),
                        );
                    }
                }
                floptle_script::AssemblyCmd::Rebuild { root } => {
                    if let Some(sim) = self.sim.as_mut() {
                        // A rebuild must not silently release launch clamps:
                        // the fresh compound inherits the old one's anchor.
                        let was_anchored =
                            sim.compound_of(root).map(|c| c.anchored).unwrap_or(false);
                        sim.remove_compound(root);
                        let ent = self
                            .world
                            .entity_with::<floptle_core::RigidBody>(root);
                        if let Some(e) = ent {
                            sim.add_compound_for(e, &self.world);
                        }
                        if was_anchored {
                            sim.set_compound_anchored(root, true);
                        }
                    }
                }
                floptle_script::AssemblyCmd::Anchor { root, on } => {
                    if let Some(sim) = self.sim.as_mut() {
                        sim.set_compound_anchored(root, on);
                    }
                }
                floptle_script::AssemblyCmd::KeepLive { root, on } => {
                    if on {
                        self.lod_keep_live.insert(root);
                    } else {
                        self.lod_keep_live.remove(&root);
                    }
                }
                floptle_script::AssemblyCmd::Teleport { root, pos } => {
                    if let Some(sim) = self.sim.as_mut() {
                        sim.set_compound_origin(root, DVec3::new(pos[0], pos[1], pos[2]));
                    }
                }
                floptle_script::AssemblyCmd::SyncColliders { root } => {
                    if let Some(sim) = self.sim.as_mut() {
                        sim.resync_compound_shapes(root, &self.world);
                    }
                }
                floptle_script::AssemblyCmd::Split { root, parts, cb, prefab } => {
                    let new_root = self.perform_assembly_split(root, &parts, prefab.as_deref());
                    match (new_root, cb) {
                        (Some(nr), Some(cb)) => {
                            // Re-feed the mirror FIRST: the callback's whole job
                            // is to act on the fresh half (`assembly.info(stage)`
                            // → kick it clear, place it, read its mass), and the
                            // mirror it would otherwise see was fed before this
                            // split existed — so every such read came back nil
                            // and the separation kick silently did nothing.
                            self.feed_assembly_info();
                            self.script_host.call_resync_callback(&mut self.world, cb, nr);
                        }
                        (_, Some(cb)) => self.script_host.drop_registry_value(cb),
                        _ => {}
                    }
                }
                floptle_script::AssemblyCmd::Merge { root, other } => {
                    self.perform_assembly_merge(root, other);
                }
            }
        }
    }

    /// Split `parts` out of the assembly rooted at `root_eid` into a NEW root
    /// node named after the old vessel. With `prefab`, the detached half is
    /// rooted at a fresh instance of that prefab (so it comes away as a LIVE,
    /// scripted craft — an undocked lander — instead of inert debris); the
    /// prefab's own RigidBody must carry the assembly flag. Returns the new
    /// root's entity index.
    fn perform_assembly_split(
        &mut self,
        root_eid: u32,
        parts: &[u32],
        prefab: Option<&str>,
    ) -> Option<u32> {
        use floptle_core::{Name, Parent, RigidBody};
        use floptle_core::transform::Transform;
        self.sim.as_ref()?;
        let root_ent = self
            .world
            .entity_with::<RigidBody>(root_eid)?;
        // The fresh vessel root: a prefab instance when asked for (scripts and
        // all), otherwise a bare node inheriting the old root's RigidBody
        // (assembly flag, friction...) and a derived name.
        let new_root = match prefab.and_then(|p| self.spawn_split_root(p)) {
            Some(e) => e,
            None => {
                let e = self.world.spawn();
                self.world.insert(e, Transform::IDENTITY);
                if let Some(rb) = self.world.get::<RigidBody>(root_ent).copied() {
                    self.world.insert(e, rb);
                }
                let base = self
                    .world
                    .get::<Name>(root_ent)
                    .map(|n| n.0.clone())
                    .unwrap_or_else(|| "Vessel".into());
                self.world.insert(e, Name(format!("{base} (stage)")));
                e
            }
        };
        let sim = self.sim.as_mut()?;
        if !sim.split_compound(root_eid, parts, new_root, &mut self.world) {
            self.despawn_subtree(new_root); // a prefab root can carry children
            return None;
        }
        // Re-parent each detached part under the new root, preserving its
        // world pose: local = inverse(new_root_world) ∘ part_world.
        let nw = floptle_core::world_transform(&self.world, new_root);
        let inv_rot = nw.rotation.inverse();
        for pid in parts {
            let Some(pe) = self
                .world
                .entity_with::<RigidBody>(*pid)
            else {
                continue;
            };
            let pw = floptle_core::world_transform(&self.world, pe);
            let local = Transform {
                translation: inv_rot.as_dquat() * (pw.translation - nw.translation)
                    / nw.scale.as_dvec3().max(floptle_core::math::DVec3::splat(1e-9)),
                rotation: (inv_rot * pw.rotation).normalize(),
                scale: pw.scale / nw.scale.max(floptle_core::math::Vec3::splat(1e-9)),
            };
            if let Some(t) = self.world.get_mut::<Transform>(pe) {
                *t = local;
            }
            self.world.insert(pe, Parent(new_root));
        }
        Some(new_root.index())
    }

    /// Instantiate `prefab` as the root for a detached assembly half. It must
    /// carry an assembly-flagged RigidBody — without one the split has no body
    /// to land on, so we refuse (and say so) rather than silently dropping the
    /// detached parts into a node the physics never sees.
    fn spawn_split_root(&mut self, prefab: &str) -> Option<Entity> {
        let Some(path) = self.resolve_prefab_request(prefab) else {
            self.console.push(
                floptle_script::LogLevel::Error,
                format!("assembly.split: no such prefab \"{prefab}\" (looked in prefabs/)"),
                None,
            );
            return None;
        };
        let docs = self.cached_prefab_docs(&path)?;
        if docs.is_empty() {
            return None;
        }
        let ents = self.spawn_docs(&docs);
        let root = ents.iter().zip(&docs).find(|(_, d)| d.parent.is_none()).map(|(&e, _)| e)?;
        if !self
            .world
            .get::<floptle_core::RigidBody>(root)
            .is_some_and(|rb| rb.assembly)
        {
            self.console.push(
                floptle_script::LogLevel::Error,
                format!(
                    "assembly.split: prefab \"{prefab}\" isn't an assembly root \
                     (its RigidBody needs the assembly flag)"
                ),
                None,
            );
            self.despawn_subtree(root);
            return None;
        }
        if docs.iter().any(|d| matches!(d.matter, floptle_scene::MatterDoc::Mesh { .. })) {
            self.register_scene_meshes();
        }
        Some(root)
    }

    /// Absorb the assembly rooted at `other_eid` into the one rooted at
    /// `root_eid` — the docking latch closing. The physics compounds merge
    /// (combined momentum, [`floptle_physics::Compound::merge`]), every node
    /// hanging off the absorbed root re-parents under the surviving one with
    /// its world pose kept, and the emptied root is retired. Absorbed parts
    /// keep their entity ids, so per-part contact attribution
    /// (`assembly.impacts`) carries straight across the join.
    fn perform_assembly_merge(&mut self, root_eid: u32, other_eid: u32) -> bool {
        use floptle_core::{Parent, RigidBody};
        use floptle_core::transform::Transform;
        let find = |w: &floptle_core::World, id: u32| {
            w.entity_with::<RigidBody>(id)
        };
        let (Some(root_ent), Some(other_ent)) =
            (find(&self.world, root_eid), find(&self.world, other_eid))
        else {
            return false;
        };
        // World poses BEFORE anything moves — the absorbed subtree must not
        // shift a millimetre through the weld.
        let moving: Vec<Entity> = self
            .world
            .query::<Parent>()
            .filter(|(_, p)| p.0 == other_ent)
            .map(|(e, _)| e)
            .collect();
        let poses: Vec<(Entity, Transform)> = moving
            .into_iter()
            .map(|e| (e, floptle_core::world_transform(&self.world, e)))
            .collect();
        let Some(sim) = self.sim.as_mut() else { return false };
        if !sim.merge_compound(root_eid, other_eid) {
            return false;
        }
        // The surviving root keeps naming the merged assembly's origin, so its
        // world transform is unchanged by the merge — re-parent against it.
        let rw = floptle_core::world_transform(&self.world, root_ent);
        let inv_rot = rw.rotation.inverse();
        for (e, pw) in poses {
            let local = Transform {
                translation: inv_rot.as_dquat() * (pw.translation - rw.translation)
                    / rw.scale.as_dvec3().max(floptle_core::math::DVec3::splat(1e-9)),
                rotation: (inv_rot * pw.rotation).normalize(),
                scale: pw.scale / rw.scale.max(floptle_core::math::Vec3::splat(1e-9)),
            };
            if let Some(t) = self.world.get_mut::<Transform>(e) {
                *t = local;
            }
            self.world.insert(e, Parent(root_ent));
        }
        self.lod_keep_live.remove(&other_eid);
        self.despawn_subtree(other_ent);
        true
    }

    /// Despawn a node and everything under it, clearing its physics
    /// registrations. The engine-internal counterpart of a scripted
    /// `destroy()` (no net-authority checks — these are lifecycle moves the
    /// engine itself makes: a retired docking root, a refused split root).
    fn despawn_subtree(&mut self, root: Entity) {
        let mut kids: std::collections::HashMap<Entity, Vec<Entity>> =
            std::collections::HashMap::new();
        for (e, p) in self.world.query::<floptle_core::Parent>() {
            kids.entry(p.0).or_default().push(e);
        }
        let mut doomed = Vec::new();
        let mut queue: std::collections::VecDeque<Entity> = [root].into();
        while let Some(e) = queue.pop_front() {
            doomed.push(e);
            queue.extend(kids.get(&e).map(|v| v.as_slice()).unwrap_or(&[]));
        }
        for e in doomed {
            let idx = e.index();
            self.world.despawn(e);
            if let Some(sim) = self.sim.as_mut() {
                sim.remove_body(idx);
                sim.remove_compound(idx);
            }
        }
        self.selection.retain(|&e| self.world.is_alive(e));
    }

    pub(crate) fn apply_destroys(&mut self, destroys: Vec<u32>) {
        // Every entity that actually goes away, roots and descendants — see
        // the handler prune at the end.
        let mut gone: Vec<u32> = Vec::new();
        let mut kids: std::collections::HashMap<Entity, Vec<Entity>> =
            std::collections::HashMap::new();
        for (e, p) in self.world.query::<floptle_core::Parent>() {
            kids.entry(p.0).or_default().push(e);
        }
        for eid in destroys {
            let Some(target) = self
                .world
                .entity_with::<floptle_core::Matter>(eid)
            else {
                continue; // already gone (double destroy is harmless)
            };
            // A replicated node on a CLIENT is server-authoritative — destroying
            // it locally would desync (the next snapshot resurrects it anyway).
            let client_owned = self.net_server.is_none()
                && (self.net_client.as_ref().is_some_and(|(s, _)| s.net_id_of(target).is_some())
                    || self
                        .net_play_client
                        .as_ref()
                        .is_some_and(|s| s.net_id_of(target).is_some()));
            if client_owned {
                self.console.push(
                    floptle_script::LogLevel::Warn,
                    "destroy: that node is server-authoritative — only the server can destroy it"
                        .into(),
                    None,
                );
                continue;
            }
            let mut doomed = Vec::new();
            let mut queue: std::collections::VecDeque<Entity> = [target].into();
            while let Some(e) = queue.pop_front() {
                doomed.push(e);
                queue.extend(kids.get(&e).map(|v| v.as_slice()).unwrap_or(&[]));
            }
            for e in doomed {
                let idx = e.index();
                gone.push(idx);
                // On a server session, tracked nodes despawn THROUGH the session
                // (broadcasts to every client); everything else is local.
                let tracked =
                    self.net_server.as_ref().is_some_and(|s| s.net_id_of(e).is_some());
                if tracked {
                    if let Some(s) = self.net_server.as_mut() {
                        s.despawn(&mut self.world, e);
                    }
                    let n = self.net_remote_predicted.len();
                    self.net_remote_predicted.retain(|(re, _)| *re != e);
                    if self.net_remote_predicted.len() != n {
                        self.net_apply_host_filters();
                    }
                } else {
                    self.world.despawn(e);
                }
                if let Some(sim) = self.sim.as_mut() {
                    sim.remove_body(idx);
                    sim.remove_compound(idx);
                }
            }
        }
        // Entity indices are recycled, so a `ui.make` behaviour closure left
        // on a destroyed element would fire on whatever node inherits its
        // slot. Pruned here, at the ONE destroy path, rather than at each
        // caller.
        self.script_host.drop_ui_handlers(&gone);
        // Play-mode selections can now point at despawned entities.
        self.selection.retain(|&e| self.world.is_alive(e));
    }
}

#[cfg(test)]
mod tests {
    use floptle_core::{Matter, Name, ScriptInst, Scripts, Transform};
    use floptle_ui::UiLayer;

    use crate::Editor;

    /// Editing a prefab on its own, end to end (`floptle/0090`): open it, change
    /// something, save, reopen.
    ///
    /// The load-bearing assertion is that the save landed **in the same file**.
    /// Before this, the only route to changing a prefab was Save-as-Prefab,
    /// which never overwrites — so the obvious thing to do produced a second
    /// file beside the one you meant to edit and left the original untouched.
    #[test]
    fn a_prefab_opens_on_its_own_and_saves_back_over_itself() {
        let dir = std::env::temp_dir().join(format!("floptle_0090_{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("prefabs"));
        let path = dir.join("prefabs").join("Turret.prefab.ron");

        // Author a prefab the way the editor does: a root with one child.
        let mut ed = Editor { project_root: dir.clone(), ..Default::default() };
        let root = ed.world.spawn();
        ed.world.insert(root, Transform::IDENTITY);
        ed.world.insert(root, Name("Turret".into()));
        ed.world.insert(root, Matter::Empty);
        let barrel = ed.world.spawn();
        ed.world.insert(barrel, Transform::IDENTITY);
        ed.world.insert(barrel, Name("Barrel".into()));
        ed.world.insert(barrel, Matter::Empty);
        ed.world.insert(barrel, floptle_core::Parent(root));
        let docs = ed.subtree_docs(&[root]);
        std::fs::write(
            &path,
            ron::ser::to_string_pretty(&docs, ron::ser::PrettyConfig::default()).unwrap(),
        )
        .unwrap();

        // Open it on its own. The world becomes the prefab and nothing else.
        let mut ed = Editor { project_root: dir.clone(), ..Default::default() };
        // A leftover scene node, to prove the open REPLACES the world rather
        // than adding to it.
        let stale = ed.world.spawn();
        ed.world.insert(stale, Transform::IDENTITY);
        ed.world.insert(stale, Name("SomeOtherScene".into()));
        ed.world.insert(stale, Matter::Empty);

        ed.open_prefab_file(&path.to_string_lossy());
        let names = |ed: &Editor| -> Vec<String> {
            let mut v: Vec<String> = ed
                .world
                .query::<Matter>()
                .filter_map(|(e, _)| ed.world.get::<Name>(e).map(|n| n.0.clone()))
                .collect();
            v.sort();
            v
        };
        assert_eq!(names(&ed), vec!["Barrel".to_string(), "Turret".to_string()]);
        assert!(ed.editing_prefab.is_some(), "the editor must know it is on a prefab");
        assert_eq!(ed.scene_name, "Turret", "the name shown is the prefab's");

        // Edit it: rename the child and add a second one.
        let barrel = ed
            .world
            .query::<Matter>()
            .map(|(e, _)| e)
            .find(|e| ed.world.get::<Name>(*e).is_some_and(|n| n.0 == "Barrel"))
            .expect("child came back");
        ed.world.insert(barrel, Name("LongBarrel".into()));
        let root = ed
            .world
            .query::<Matter>()
            .map(|(e, _)| e)
            .find(|e| ed.world.get::<Name>(*e).is_some_and(|n| n.0 == "Turret"))
            .expect("root came back");
        let sight = ed.world.spawn();
        ed.world.insert(sight, Transform::IDENTITY);
        ed.world.insert(sight, Name("Sight".into()));
        ed.world.insert(sight, Matter::Empty);
        ed.world.insert(sight, floptle_core::Parent(root));

        assert!(ed.save_scene(), "saving a prefab must succeed");

        // In place: one file, and it is the one we opened.
        let files: Vec<String> = std::fs::read_dir(dir.join("prefabs"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(files, vec!["Turret.prefab.ron".to_string()], "a save must not make a SECOND prefab");
        assert!(
            !dir.join("scenes").exists(),
            "editing a prefab must not write a scene"
        );

        // And the edit survived the round trip.
        let mut ed = Editor { project_root: dir.clone(), ..Default::default() };
        ed.open_prefab_file(&path.to_string_lossy());
        assert_eq!(
            names(&ed),
            vec!["LongBarrel".to_string(), "Sight".to_string(), "Turret".to_string()]
        );
    }

    /// Opening a scene is the way out of prefab editing — and it has to be,
    /// because otherwise the next Ctrl+S would write the scene over the prefab.
    #[test]
    fn opening_a_scene_leaves_prefab_editing() {
        let dir = std::env::temp_dir().join(format!("floptle_0090_exit_{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("prefabs"));
        let _ = std::fs::create_dir_all(dir.join("scenes"));
        let path = dir.join("prefabs").join("Crate.prefab.ron");

        let mut ed = Editor { project_root: dir.clone(), ..Default::default() };
        let root = ed.world.spawn();
        ed.world.insert(root, Transform::IDENTITY);
        ed.world.insert(root, Name("Crate".into()));
        ed.world.insert(root, Matter::Empty);
        let docs = ed.subtree_docs(&[root]);
        std::fs::write(
            &path,
            ron::ser::to_string_pretty(&docs, ron::ser::PrettyConfig::default()).unwrap(),
        )
        .unwrap();

        ed.open_prefab_file(&path.to_string_lossy());
        assert!(ed.editing_prefab.is_some());

        ed.new_scene("level");
        assert!(ed.editing_prefab.is_none(), "a new scene is a scene, not a prefab");
        assert!(ed.save_scene(), "and saving now writes a scene");
        assert!(dir.join("scenes").join("level.ron").exists());
    }

    /// A prefab that would be saved empty is refused. The file on disk is the
    /// only copy, and afterwards "I deleted everything" and "something went
    /// wrong" look exactly alike.
    #[test]
    fn an_emptied_prefab_is_not_written_over_itself() {
        let dir = std::env::temp_dir().join(format!("floptle_0090_empty_{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("prefabs"));
        let path = dir.join("prefabs").join("Lamp.prefab.ron");

        let mut ed = Editor { project_root: dir.clone(), ..Default::default() };
        let root = ed.world.spawn();
        ed.world.insert(root, Transform::IDENTITY);
        ed.world.insert(root, Name("Lamp".into()));
        ed.world.insert(root, Matter::Empty);
        let docs = ed.subtree_docs(&[root]);
        let authored =
            ron::ser::to_string_pretty(&docs, ron::ser::PrettyConfig::default()).unwrap();
        std::fs::write(&path, &authored).unwrap();

        ed.open_prefab_file(&path.to_string_lossy());
        ed.world = floptle_core::World::new(); // everything deleted
        assert!(!ed.save_scene(), "an empty prefab save must be refused");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), authored, "the file is untouched");
    }

    /// A HUD described with `ui.make`, over two Play sessions with a Stop in
    /// between — the shape of the report in `floptle/0061`: "after playing the
    /// game in the editor once, when I try to play again the UI does not show".
    ///
    /// Two sessions is the whole test. One session passes trivially and always
    /// has; everything interesting lives in what the second one inherits.
    #[test]
    fn a_made_hud_comes_back_on_the_second_play() {
        let dir = std::env::temp_dir().join(format!("floptle_0061_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(
            dir.join("hud.lua"),
            "\
function start(node)
  ui.make(node, { 'col', key = 'hud', w = '100%',
                  { 'text', key = 'score', text = 'SCORE 0' } })
end
",
        )
        .expect("write hud.lua");

        let mut ed = Editor { project_root: dir.clone(), ..Default::default() };
        let layer = ed.world.spawn();
        ed.world.insert(layer, Transform::IDENTITY);
        ed.world.insert(layer, Name("HUD".into()));
        ed.world.insert(layer, Matter::Empty);
        ed.world.insert(layer, UiLayer::default());
        ed.world.insert(layer, Scripts(vec![ScriptInst {
            kind: "hud".into(),
            enabled: true,
            params: Vec::new(),
            refs: Vec::new(),
            strs: Vec::new(),
        }]));

        let elements = |ed: &Editor| ed.world.query::<floptle_core::Made>().count();
        let session = |ed: &mut Editor, t: f32| {
            let snap = ed.snapshot();
            ed.script_host.reset_instances();
            ed.playing = true;
            ed.script_host.run(&mut ed.world, &dir, 1.0 / 60.0, t);
            assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
            ed.apply_script_spawns();
            let n = elements(ed);
            ed.playing = false;
            ed.restore(snap);
            n
        };

        assert_eq!(session(&mut ed, 0.0), 2, "first session: the column and its text");
        assert_eq!(session(&mut ed, 1.0), 2, "second session: the same HUD, not an empty screen");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same two sessions, with everything a real HUD does that the plain
    /// case above leaves out: described from `update` rather than `start`, rows
    /// that come and go, a `ui.on` listener, a visibility write, and a
    /// `destroy` still in flight when Stop lands.
    ///
    /// Each of those is a place where an entity index from the finished session
    /// could be read against the fresh one, and an index is exactly the kind of
    /// thing that survives a restore.
    #[test]
    fn a_busy_hud_comes_back_too() {
        let dir = std::env::temp_dir().join(format!("floptle_0061_busy_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(
            dir.join("hud.lua"),
            "\
local ticks = 0

function start(node)
  ticks = 0
  -- A node created at runtime, which the restored scene will not contain.
  createNode('Debris', node, function(n) n.y = 1 end)
end

function update(node, dt)
  ticks = ticks + 1
  local rows = { 'col', key = 'hud', w = '100%',
                 { 'text', key = 'score', text = 'SCORE ' .. ticks } }
  -- A row that appears on the second frame and leaves again on the fourth,
  -- so reconcile is doing real work rather than the same tree twice.
  if ticks >= 2 and ticks < 4 then
    rows[#rows + 1] = { 'button', key = 'go', text = 'GO' }
  end
  ui.make(node, rows)
  node.visible = true
  -- Queued and never drained on the frame Stop lands.
  createNode('Late', node)
end
",
        )
        .expect("write hud.lua");

        let mut ed = Editor { project_root: dir.clone(), ..Default::default() };
        let layer = ed.world.spawn();
        ed.world.insert(layer, Transform::IDENTITY);
        ed.world.insert(layer, Name("HUD".into()));
        ed.world.insert(layer, Matter::Empty);
        ed.world.insert(layer, UiLayer::default());
        ed.world.insert(layer, Scripts(vec![ScriptInst {
            kind: "hud".into(),
            enabled: true,
            params: Vec::new(),
            refs: Vec::new(),
            strs: Vec::new(),
        }]));
        let snap = ed.snapshot();
        ed.restore(snap.clone());

        // Three sessions of five frames each — the third catches anything that
        // needs two restores to go wrong.
        for session in 0..3 {
            ed.script_host.reset_instances();
            ed.playing = true;
            for frame in 0..5 {
                ed.script_host.run(&mut ed.world, &dir, 1.0 / 60.0, frame as f32 / 60.0);
                assert!(
                    ed.script_host.errors().is_empty(),
                    "session {session} frame {frame}: {:?}",
                    ed.script_host.errors()
                );
                ed.apply_script_spawns();
            }
            let made = ed.world.query::<floptle_core::Made>().count();
            assert_eq!(made, 2, "session {session}: the column and its text are on screen");
            // …and they are under the layer, not orphaned onto whatever node
            // inherited the old session's index.
            let under_layer = ed
                .world
                .query::<floptle_core::Made>()
                .filter(|(e, _)| {
                    ed.world.get::<floptle_core::Parent>(*e).is_some_and(|p| p.0 == layer)
                })
                .count();
            assert_eq!(under_layer, 1, "session {session}: the column hangs off the HUD layer");
            ed.playing = false;
            ed.restore(snap.clone());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Stop lands where it lands: after `update` has run and queued work, and
    /// before the driver drains the queue. Everything in flight belongs to the
    /// session that just ended — a `createNode` applied on the NEXT Play names
    /// a parent index the new scene has given to somebody else, and runs a
    /// callback closed over an environment that has been dropped.
    #[test]
    fn work_queued_on_the_last_frame_of_a_session_does_not_land_in_the_next_one() {
        let dir = std::env::temp_dir().join(format!("floptle_0061_q_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // `parent` is the HUD layer's index in THIS session. Next session that
        // index belongs to whatever the fresh world hands it to.
        std::fs::write(
            dir.join("spawner.lua"),
            "function update(node, dt)\n  createNode('Tile', node, function(n) n.y = 3 end)\nend\n",
        )
        .expect("write spawner.lua");

        let mut ed = Editor { project_root: dir.clone(), ..Default::default() };
        let e = ed.world.spawn();
        ed.world.insert(e, Transform::IDENTITY);
        ed.world.insert(e, Name("Spawner".into()));
        ed.world.insert(e, Matter::Empty);
        ed.world.insert(e, Scripts(vec![ScriptInst {
            kind: "spawner".into(),
            enabled: true,
            params: Vec::new(),
            refs: Vec::new(),
            strs: Vec::new(),
        }]));
        let snap = ed.snapshot();
        // The baseline is measured through a restore, because that is what the
        // comparison is against — a round trip through the doc is not the
        // identity (it is where the scene's implicit camera and light arrive).
        ed.restore(snap.clone());
        let authored = ed.world.query::<Transform>().count();

        // A session that ends between `update` and the drain.
        ed.script_host.reset_instances();
        ed.playing = true;
        ed.script_host.run(&mut ed.world, &dir, 1.0 / 60.0, 0.0);
        ed.playing = false;
        ed.restore(snap.clone());

        // Play again. Nothing from last time may be waiting.
        ed.script_host.reset_instances();
        assert_eq!(
            ed.world.query::<Transform>().count(),
            authored,
            "the restored scene is the authored one"
        );
        ed.playing = true;
        ed.apply_script_spawns();
        assert_eq!(
            ed.world.query::<Transform>().count(),
            authored,
            "a node queued in the previous session must not appear in this one"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
