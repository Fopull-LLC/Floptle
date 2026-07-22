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
                        "⬡ saved prefab {} ({} node{})",
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
        let destroys = self.script_host.take_destroy_requests();
        if destroys.is_empty() {
            return;
        }
        self.apply_destroys(destroys);
    }

    fn apply_spawn_batch(
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
                    .query::<floptle_core::Matter>()
                    .map(|(pe, _)| pe)
                    .find(|pe| pe.index() == pid);
                if let Some(pe) = pe {
                    self.world.insert(e, floptle_core::Parent(pe));
                }
            }
            if let Some(cb) = req.cb {
                self.script_host.call_create_callback(&mut self.world, cb, e.index());
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
            if docs.iter().any(|d| matches!(d.matter, floptle_scene::MatterDoc::Mesh { .. })) {
                self.register_scene_meshes();
            }
            // Optional parenting (`spawn(name, pos, fn, parentNode)`): the
            // spawned ROOTS go under the parent, keeping their WORLD pose —
            // convert into the parent's local frame. Done BEFORE physics
            // wiring so ancestry rules (assembly parts) see the hierarchy.
            if let Some(pid) = req.parent {
                let pe = self
                    .world
                    .query::<floptle_core::Matter>()
                    .map(|(pe, _)| pe)
                    .find(|pe| pe.index() == pid);
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
                self.script_host.call_spawn_callback(&mut self.world, cb, root.index());
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
        for (root, part, impulse, speed, point) in sim.compound_impacts() {
            impacts.entry(root).or_default().push(floptle_script::AssemblyImpact {
                part,
                impulse,
                speed,
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
                            .query::<floptle_core::RigidBody>()
                            .map(|(e, _)| e)
                            .find(|e| e.index() == root);
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
                floptle_script::AssemblyCmd::Teleport { root, pos } => {
                    if let Some(sim) = self.sim.as_mut() {
                        sim.set_compound_origin(root, DVec3::new(pos[0], pos[1], pos[2]));
                    }
                }
                floptle_script::AssemblyCmd::Split { root, parts, cb } => {
                    let new_root = self.perform_assembly_split(root, &parts);
                    match (new_root, cb) {
                        (Some(nr), Some(cb)) => {
                            self.script_host.call_spawn_callback(&mut self.world, cb, nr);
                        }
                        (_, Some(cb)) => self.script_host.drop_registry_value(cb),
                        _ => {}
                    }
                }
            }
        }
    }

    /// Split `parts` out of the assembly rooted at `root_eid` into a NEW root
    /// node named after the old vessel. Returns the new root's entity index.
    fn perform_assembly_split(&mut self, root_eid: u32, parts: &[u32]) -> Option<u32> {
        use floptle_core::{Name, Parent, RigidBody};
        use floptle_core::transform::Transform;
        self.sim.as_ref()?;
        let root_ent = self
            .world
            .query::<RigidBody>()
            .map(|(e, _)| e)
            .find(|e| e.index() == root_eid)?;
        // The fresh vessel root: inherits the old root's RigidBody (assembly
        // flag, friction...) and a derived name.
        let new_root = self.world.spawn();
        self.world.insert(new_root, Transform::IDENTITY);
        if let Some(rb) = self.world.get::<RigidBody>(root_ent).copied() {
            self.world.insert(new_root, rb);
        }
        let base = self
            .world
            .get::<Name>(root_ent)
            .map(|n| n.0.clone())
            .unwrap_or_else(|| "Vessel".into());
        self.world.insert(new_root, Name(format!("{base} (stage)")));
        let sim = self.sim.as_mut()?;
        if !sim.split_compound(root_eid, parts, new_root, &mut self.world) {
            self.world.despawn(new_root);
            return None;
        }
        // Re-parent each detached part under the new root, preserving its
        // world pose: local = inverse(new_root_world) ∘ part_world.
        let nw = floptle_core::world_transform(&self.world, new_root);
        let inv_rot = nw.rotation.inverse();
        for pid in parts {
            let Some(pe) = self
                .world
                .query::<RigidBody>()
                .map(|(e, _)| e)
                .find(|e| e.index() == *pid)
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

    fn apply_destroys(&mut self, destroys: Vec<u32>) {
        let mut kids: std::collections::HashMap<Entity, Vec<Entity>> =
            std::collections::HashMap::new();
        for (e, p) in self.world.query::<floptle_core::Parent>() {
            kids.entry(p.0).or_default().push(e);
        }
        for eid in destroys {
            let Some(target) = self
                .world
                .query::<floptle_core::Matter>()
                .map(|(e, _)| e)
                .find(|e| e.index() == eid)
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
        // Play-mode selections can now point at despawned entities.
        self.selection.retain(|&e| self.world.is_alive(e));
    }
}
