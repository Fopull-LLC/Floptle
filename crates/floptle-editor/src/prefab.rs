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
        let spawns = self.script_host.take_spawn_requests();
        let destroys = self.script_host.take_destroy_requests();
        if spawns.is_empty() && destroys.is_empty() {
            return;
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
            if let Some(sim) = self.sim.as_mut() {
                for &e in &ents {
                    sim.add_body_for(e, &self.world);
                }
            }
            let root = ents
                .iter()
                .zip(&docs)
                .find(|(_, d)| d.parent.is_none())
                .map(|(&e, _)| e);
            if let (Some(cb), Some(root)) = (req.cb, root) {
                self.script_host.call_spawn_callback(&mut self.world, cb, root.index());
            }
        }
        if destroys.is_empty() {
            return;
        }
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
                }
            }
        }
        // Play-mode selections can now point at despawned entities.
        self.selection.retain(|&e| self.world.is_alive(e));
    }
}
