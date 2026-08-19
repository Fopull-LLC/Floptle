//! Where the extension host meets the editor: the per-frame handshake, the menu
//! the packages build, their floating panels, and applying what they asked for.
//!
//! [`ext`](crate::ext) deliberately knows nothing about `Editor`. This module is
//! the only place the two touch, which is what keeps the host testable without a
//! window and the editor free of a second Lua state threaded through it.
//!
//! One frame, in order:
//!
//! 1. [`Editor::ext_frame`] — mirror the scene and the editor's own state into
//!    the host, fire `onUpdate` and `onSceneDraw`, project whatever `handles.*`
//!    queued.
//! 2. the UI pass — the packages' menu, their panels, their Scene overlays.
//! 3. [`Editor::apply_ext_commands`] — everything Lua asked for, applied to the
//!    real world with real undo.
//!
//! Steps 1 and 3 are separated by the whole UI pass on purpose: an extension's
//! edit lands *after* the frame that requested it, so a panel cannot delete the
//! node the Inspector is drawing halfway through drawing it.

use std::path::PathBuf;

use floptle_core::math::{DVec3, Quat, Vec3};
use floptle_core::{Entity, Matter};

use crate::ext::{self, ExtCmd, ExtLevel, HookKind, SceneMirror, Snapshot};
use crate::Editor;

impl Editor {
    /// This build's version, as packages declare compatibility against.
    pub(crate) fn engine_version() -> floptle_package::Version {
        env!("CARGO_PKG_VERSION").parse().unwrap_or_default()
    }

    /// Load (or reload) the project's packages and report what happened.
    pub(crate) fn ext_reload(&mut self) {
        if self.project_root.as_os_str().is_empty() {
            return;
        }
        // The snapshot has to hold the project root before any package's
        // top-level code runs — `ed.read` and the stores both need it, and a
        // package's first line is exactly where somebody reads a config file.
        self.ext.begin_frame(self.ext_snapshot(), SceneMirror::default());
        // 0 is never a live revision (`World` starts at 1), so this forces the
        // next tick to rebuild rather than trusting a mirror built for the
        // packages that were loaded a moment ago.
        self.ext_mirror_rev = 0;
        self.ext.reload(&self.project_root, &Self::engine_version());
        // Two things outside the host now depend on what loaded: where a
        // `pkg://` reference points, and where a script name may resolve.
        crate::project::set_package_roots(
            self.ext.report.loaded.iter().map(|l| (l.id().to_string(), l.root.clone())).collect(),
        );
        let script_dirs: Vec<PathBuf> = self
            .ext
            .report
            .loaded
            .iter()
            .flat_map(|l| l.manifest.dirs_that_exist(&l.root, floptle_package::DirKind::Scripts))
            .collect();
        self.script_host.set_extra_script_dirs(script_dirs);
        self.ext_report_problems();
        self.drain_ext_log();
    }

    /// Put the load report's errors and warnings into the Console once, in the
    /// words somebody who installed a package would use.
    fn ext_report_problems(&mut self) {
        let problems: Vec<(floptle_package::Severity, String)> = self
            .ext
            .report
            .problems
            .iter()
            .map(|p| (p.severity, p.message.clone()))
            .collect();
        for (sev, msg) in problems {
            let level = match sev {
                floptle_package::Severity::Error => floptle_script::LogLevel::Error,
                floptle_package::Severity::Warning => floptle_script::LogLevel::Warn,
            };
            self.console.push(level, format!("📦 {msg}"), None);
        }
    }

    /// Everything an extension may read about the editor this frame.
    fn ext_snapshot(&self) -> Snapshot {
        let fwd = self.camera.rotation() * Vec3::NEG_Z;
        Snapshot {
            project_root: self.project_root.clone(),
            project_name: self
                .project_root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            scene: if self.scene_rel.is_empty() {
                self.scene_name.clone()
            } else {
                self.scene_rel.clone()
            },
            playing: self.playing,
            selection: self.selection.iter().map(|e| e.index()).collect(),
            cam_pos: [self.camera.position.x, self.camera.position.y, self.camera.position.z],
            cam_fwd: [fwd.x, fwd.y, fwd.z],
            time: self.ext_clock,
            dt: self.ui_frame_dt,
        }
    }

    /// Step 1 of the frame: mirror the editor into the host and run the hooks.
    ///
    /// Deliberately separate from projecting what `handles.*` queued: this needs
    /// the whole editor, and by the time `view_proj` exists the draw path holds
    /// pieces of it. Running the hooks first also means a hook's `scene.setPos`
    /// is queued before anything reads the queue.
    pub(crate) fn ext_tick(&mut self) {
        // **Load this project's packages once, however the project arrived.**
        //
        // Startup sets `project_root` directly rather than calling
        // `open_project`, so `ext_reload` was never run for a project named on
        // the command line: the editor came up with the level loaded and its
        // packages simply absent — no menus, no panels, no error, because
        // nothing had failed. The only way to get them was to toggle a package
        // in 📦 Packages, which reloads as a side effect.
        //
        // The Hub launches the editor with the project as an argument, so this
        // was every Hub-started session, not just a binary run by hand.
        //
        // Must sit ABOVE the `is_empty` early-out below: with nothing loaded
        // yet, that return is exactly the branch this has to get past.
        if !self.ext_booted && !self.project_root.as_os_str().is_empty() {
            self.ext_booted = true;
            self.ext_reload();
        }
        if self.ext.is_empty() {
            self.ext_painted.clear();
            return;
        }
        // **Write a changed preference now, not at quit.** These used to be
        // flushed only on project close and on shutdown, so anything a package
        // stored — an API key somebody typed, a server address, a switch — was
        // lost outright if the editor did not exit cleanly. Somebody signing in
        // once and having to do it again is not a preference system.
        //
        // Cheap to call every frame: each store checks its own dirty flag and a
        // clean one writes nothing, so this costs a walk over a handful of
        // packages until something actually changes.
        self.ext.save_prefs();
        // The mirror is a copy of the scene, and rebuilding it every frame was
        // work nobody asked for: the scene does not change while somebody is
        // reading a panel. `World::revision` moves on every spawn, despawn,
        // insert, remove and mutable access, so an unchanged revision means an
        // unchanged scene — and it starts at 1, never 0, so a zero-initialised
        // cache cannot read as current.
        //
        // This also makes `scene.doc` affordable: serialising every node's
        // document is far more than the rest of the mirror costs, and now it
        // happens when the scene changes rather than when the editor draws.
        // The selection is part of what the mirror carries (its documents), so a
        // pick with no edit behind it still has to rebuild.
        let rev = self.world.revision();
        let picked = self.selection.len();
        if rev != self.ext_mirror_rev || picked != self.ext_mirror_selection {
            self.ext_mirror_rev = rev;
            self.ext_mirror_selection = picked;
            let mirror = self.ext_mirror();
            self.ext.begin_frame(self.ext_snapshot(), mirror);
        } else {
            self.ext.begin_frame_keeping_scene(self.ext_snapshot());
        }
        self.ext.pump_web();
        self.ext.tick_timers();
        self.ext.fire(HookKind::Update);
        // Selection changes are noticed here rather than pushed from twenty
        // call sites: every path that changes the selection goes through the
        // same field, and comparing it once a frame cannot miss one.
        let selection: Vec<u32> = self.selection.iter().map(|e| e.index()).collect();
        if selection != self.ext_last_selection {
            self.ext_last_selection = selection;
            self.ext.fire(HookKind::SelectionChange);
        }
        self.ext.fire(HookKind::SceneDraw);
        self.drain_ext_log();
    }

    /// The scene, as extensions see it. The radius comes from the same
    /// measurement the draw loop culls with, so a package and the renderer
    /// agree about how big a node is.
    fn ext_mirror(&self) -> SceneMirror {
        let registry = &self.mesh_registry;
        let model_size = |m: &Matter| match m {
            Matter::Mesh { asset_path } => registry.get(asset_path).map(|a| a.size),
            _ => None,
        };
        let mut mirror = SceneMirror::build(
            &self.world,
            &|_e: Entity, m: &Matter| {
                crate::node_bounds::local_radius(
                    m,
                    crate::node_bounds::Measured {
                        model_size: model_size(m),
                        sprite_reach: None,
                        sprite_size: None,
                    },
                )
            },
            &|_e: Entity, m: &Matter| local_half_extents(m, model_size(m)),
        );
        self.fill_mirror_docs(&mut mirror);
        mirror
    }

    /// The selected nodes' full documents, for `scene.doc`.
    ///
    /// **The selection and nothing else.** Serialising one node's every
    /// component costs more than the whole rest of the mirror, so doing it for
    /// the scene would mean rebuilding every document on every frame of a gizmo
    /// drag — in any project that merely had such a package installed. The
    /// selection is what a tool operates on and is a handful of nodes.
    ///
    /// Combined with the revision cache in `ext_tick`, a tool reading documents
    /// from a panel that draws sixty times a second serialises nothing at all
    /// while nobody is editing.
    fn fill_mirror_docs(&self, mirror: &mut SceneMirror) {
        for &e in &self.selection {
            if let Some(doc) = self.node_of(e)
                && let Ok(v) = serde_json::to_value(&doc)
            {
                mirror.docs.insert(e.index(), v);
            }
        }
    }

    /// Move anything the extensions logged into the Console.
    pub(crate) fn drain_ext_log(&mut self) {
        for line in self.ext.take_log() {
            let level = match line.level {
                ExtLevel::Info => floptle_script::LogLevel::Debug,
                ExtLevel::Warn => floptle_script::LogLevel::Warn,
                ExtLevel::Error => floptle_script::LogLevel::Error,
            };
            let msg =
                if line.from.is_empty() { line.msg } else { format!("[{}] {}", line.from, line.msg) };
            self.console.push(level, msg, None);
        }
    }

    /// Step 3 of the frame: apply what the extensions asked for.
    ///
    /// **One undo point per batch, and only if `ed.undo` asked for one.** A
    /// panel that writes a slider's value every frame would otherwise fill the
    /// undo stack with sixty identical scenes a second; a package that means an
    /// edit to be undoable says so.
    pub(crate) fn apply_ext_commands(&mut self) {
        let cmds = self.ext.take_cmds();
        // Collected rather than applied in the loop — see below.
        let mut copies: Vec<String> = Vec::new();
        if cmds.is_empty() {
            return;
        }
        let mut recorded = false;
        for cmd in cmds {
            match cmd {
                ExtCmd::Undo => {
                    if !recorded {
                        self.record();
                        recorded = true;
                    }
                }
                ExtCmd::SelectionSet(ids) => {
                    let ents: Vec<Entity> = ids.iter().filter_map(|i| self.entity_of(*i)).collect();
                    self.selection = ents;
                }
                ExtCmd::NodeSetName(id, name) => {
                    if let Some(e) = self.entity_of(id) {
                        self.world.insert(e, floptle_core::Name(name));
                        self.scene_dirty = true;
                    }
                }
                ExtCmd::NodeSetPos(id, p) => {
                    self.ext_edit_transform(id, |t| t.translation = DVec3::from(p));
                }
                ExtCmd::NodeSetRot(id, q) => {
                    self.ext_edit_transform(id, |t| {
                        t.rotation = Quat::from_xyzw(q[0], q[1], q[2], q[3]).normalize();
                    });
                }
                ExtCmd::NodeSetScale(id, s) => {
                    self.ext_edit_transform(id, |t| t.scale = Vec3::from(s));
                }
                ExtCmd::NodeSetVisible(id, on) => {
                    if let Some(e) = self.entity_of(id) {
                        if on {
                            self.world.remove::<floptle_core::Disabled>(e);
                        } else {
                            self.world.insert(e, floptle_core::Disabled);
                        }
                        self.scene_dirty = true;
                    }
                }
                ExtCmd::NodeCreate { name, parent } => {
                    let e = self.world.spawn();
                    self.world.insert(e, floptle_core::Name(name));
                    self.world.insert(e, Matter::Empty);
                    self.world.insert(e, floptle_core::Transform::IDENTITY);
                    if let Some(p) = parent.and_then(|p| self.entity_of(p)) {
                        self.world.insert(e, floptle_core::Parent(p));
                    }
                    self.scene_dirty = true;
                }
                ExtCmd::NodeSet { id, patch } => {
                    if let Err(why) = self.ext_set_node_doc(id, &patch) {
                        self.ext_reject("scene.set", &why);
                    }
                }
                ExtCmd::NodeAdd { spec, parent } => {
                    if let Err(why) = self.ext_add_node_doc(&spec, parent) {
                        self.ext_reject("scene.add", &why);
                    }
                }
                ExtCmd::NodeSetParent { id, parent } => {
                    if let Err(why) = self.ext_set_parent(id, parent) {
                        self.ext_reject("scene.setParent", &why);
                    }
                }
                ExtCmd::NodeDestroy(id) => {
                    if let Some(e) = self.entity_of(id) {
                        self.ext_delete_subtree(e);
                    }
                }
                ExtCmd::SpawnPrefab { path, pos } => {
                    self.ext_spawn_prefab(&path, pos);
                }
                ExtCmd::OpenScene(rel) => {
                    let path = self.project_root.join(&rel);
                    if path.exists() {
                        self.open_scene_file(&path.display().to_string());
                        self.ext.fire(HookKind::SceneOpen);
                    } else {
                        self.console.push(
                            floptle_script::LogLevel::Error,
                            format!("📦 a package asked to open `{rel}`, which is not there"),
                            None,
                        );
                    }
                }
                ExtCmd::SaveScene => {
                    self.save_scene();
                    self.ext.fire(HookKind::SceneSave);
                }
                ExtCmd::SetPlaying(on) => {
                    if on != self.playing {
                        self.toggle_play();
                    }
                }
                ExtCmd::OpenUrl(url) => {
                    let _ = floptle_script::open_in_browser(&url);
                }
                ExtCmd::WindowOpen(id, open) => {
                    if let Some(i) = self.ext.window_index(id) {
                        self.ext.set_window_open(i, open);
                    }
                }
                ExtCmd::WindowFocus(id) => {
                    if let Some(i) = self.ext.window_index(id) {
                        self.ext.set_window_open(i, true);
                        self.ext_focus_window = Some(i);
                    }
                }
                ExtCmd::OverlayOpen(id, open) => {
                    if let Some(i) = self.ext.overlay_index(id) {
                        self.ext.set_overlay_open(i, open);
                    }
                }
                ExtCmd::TabOpen(key, open) => {
                    if let Some(dock) = self.dock_state.as_mut() {
                        crate::dock::set_package_tab_open(dock, key, open);
                    }
                }
                ExtCmd::LookAt { at, distance } => {
                    // Ten metres when the package does not say. Close enough to
                    // read a room, far enough that the camera does not land
                    // inside whatever it was sent to look at.
                    self.focus_point(DVec3::from(at), distance.unwrap_or(10.0));
                }
                ExtCmd::Message { title, body } => {
                    self.ext_message = Some((title, body));
                }
                ExtCmd::Copy(text) => copies.push(text),
            }
        }
        // **The last copy of the frame, once.** A clipboard holds one value, so
        // applying every queued copy would write the same selection two or
        // three times for no visible difference — and a package calling
        // `ed.copy` from `onUpdate` would take the system selection sixty times
        // a second, which makes the user's own clipboard unusable in every
        // other application on the machine. Identical text is skipped for the
        // same reason.
        if let Some(text) = copies.pop()
            && self.last_ext_copy.as_deref() != Some(text.as_str())
        {
            let n = text.chars().count();
            self.ensure_os_clipboard();
            // The editor's own clipboard, so a package's copy button and
            // Ctrl+C put text in the same place — and so the answer is the
            // same on all three platforms.
            let took = match self.os_clipboard.as_mut() {
                Some(c) => {
                    c.set_text(text.clone());
                    self.window.is_some()
                }
                None => false,
            };
            self.last_ext_copy = Some(text);
            // A copy button with no visible result is one people press three
            // times, so the editor confirms it — but only when there was
            // somewhere to put it. With no window there is no clipboard
            // backend and `set_text` is a silent no-op; saying "copied"
            // there would be the editor asserting something it did not do.
            if took {
                let unit = if n == 1 { "character" } else { "characters" };
                self.toast = Some((format!("⎘  copied {n} {unit}"), 2.0));
            } else {
                self.console.push(
                    floptle_script::LogLevel::Warn,
                    "ed.copy: there is no clipboard to write to here".into(),
                    None,
                );
            }
        }
        self.drain_ext_log();
        self.serve_mesh_reads();
        self.serve_doc_reads();
        self.serve_file_pickers();
    }

    /// Answer everything `mesh.read` asked for this frame.
    ///
    /// After the commands, so a package that adds a node and reads it back in
    /// the same pass gets the node it just made rather than the one that was
    /// there before.
    /// Open any picker a package asked for, and deliver any that has answered.
    ///
    /// Two halves because a native dialog takes as long as a person takes: the
    /// request opens it, and a later frame hands back what they chose. The
    /// dialog goes through [`crate::native_dialog`] like every other picker in
    /// the editor — a package never reaches rfd, whose blocking API cannot run
    /// on this thread at all.
    fn serve_file_pickers(&mut self) {
        for req in self.ext.take_pick_requests() {
            let rx = crate::native_dialog::pick_files_filtered(
                &req.title,
                req.filter.as_ref().map(|(l, e)| (l.as_str(), e.as_slice())),
                req.multiple,
            );
            self.ext_picks.push((rx, req.cb));
        }
        let mut done: Vec<(usize, Vec<String>)> = Vec::new();
        for (i, (rx, _)) in self.ext_picks.iter().enumerate() {
            match crate::native_dialog::poll(rx) {
                crate::native_dialog::Answer::Waiting => {}
                crate::native_dialog::Answer::Chose(paths) => {
                    done.push((i, paths.iter().map(|p| p.display().to_string()).collect()));
                }
                // Cancelled, or the picker could not open. Either way the
                // package is told "no" rather than left waiting forever.
                crate::native_dialog::Answer::Closed => done.push((i, Vec::new())),
            }
        }
        for (i, paths) in done.into_iter().rev() {
            let (_, cb) = self.ext_picks.remove(i);
            self.ext.deliver_pick(cb, paths);
        }
        self.drain_ext_log();
    }

    /// The node documents a package asked for, by id — see `scene.docs`.
    ///
    /// Served here rather than out of the mirror because the mirror carries the
    /// selection's documents and rebuilds them whenever the scene changes; this
    /// is a one-shot read that must not join that per-frame cost.
    fn serve_doc_reads(&mut self) {
        let reqs = self.ext.take_doc_requests();
        for req in reqs {
            let mut docs = Vec::with_capacity(req.ids.len());
            let mut missing = Vec::new();
            for id in req.ids {
                match self.entity_of(id).and_then(|e| self.node_of(e)) {
                    Some(doc) => match serde_json::to_value(&doc) {
                        Ok(v) => docs.push((id, v)),
                        Err(_) => missing.push(id),
                    },
                    None => missing.push(id),
                }
            }
            self.ext.deliver_docs(req.cb, docs, missing);
        }
        self.drain_ext_log();
    }

    fn serve_mesh_reads(&mut self) {
        let reqs = self.ext.take_mesh_requests();
        for req in reqs {
            let result = match &req.source {
                crate::mesh_read::MeshSource::Asset(rel) => {
                    crate::mesh_read::read_asset(&self.project_root, rel)
                }
                crate::mesh_read::MeshSource::Node(id) => match self.entity_of(*id) {
                    Some(e) => crate::mesh_read::read_node(
                        &self.world,
                        e,
                        &self.project_root,
                        &self.maps.meshes,
                    ),
                    None => Err(format!("no node {id}")),
                },
            };
            self.ext.deliver_mesh(req.cb, result);
        }
        self.drain_ext_log();
    }

    fn ext_edit_transform(&mut self, id: u32, f: impl FnOnce(&mut floptle_core::Transform)) {
        if let Some(e) = self.entity_of(id)
            && let Some(t) = self.world.get_mut::<floptle_core::Transform>(e)
        {
            f(t);
            self.scene_dirty = true;
        }
    }

    /// The live entity behind an id an extension is holding.
    ///
    /// Entity indices are reused after a despawn, so this checks the node is
    /// still alive rather than trusting a number a package may have kept from
    /// three scenes ago.
    fn entity_of(&self, id: u32) -> Option<Entity> {
        self.world
            .entity_with::<floptle_core::Name>(id)
    }

    /// A node-document command that would not have made sense, said out loud.
    ///
    /// Refused whole rather than applied in part: a document with one bad field
    /// that writes the other nine leaves a node in a state its author never
    /// asked for and cannot see, which is worse than not writing at all.
    fn ext_reject(&mut self, what: &str, why: &str) {
        self.console.push(floptle_script::LogLevel::Error, format!("{what}: {why}"), None);
    }

    /// Apply a **partial** node document to a live node.
    ///
    /// The merge happens against the node's *current* document, so a package
    /// naming one field changes one field. That is the whole reason this is a
    /// patch and not a replacement — a tool that tints a light should not have
    /// to know what else that light is, and a tool written against 0.64 must
    /// not silently clear a field 0.70 adds.
    ///
    /// **In place.** The node keeps its id, its children and its place in the
    /// selection; a package holding an id from last frame still holds the same
    /// node. Deleting and respawning would have been simpler and would have
    /// silently invalidated every id a tool was working with.
    fn ext_set_node_doc(&mut self, id: u32, patch: &serde_json::Value) -> Result<(), String> {
        let Some(e) = self.entity_of(id) else {
            return Err(format!("no node {id} — it may have been deleted"));
        };
        let Some(current) = self.node_of(e) else {
            return Err(format!("node {id} has no document"));
        };
        let merged = merge_doc(&current, patch)?;
        // Clear first, then write: `insert_doc` only ever ADDS, which is right
        // for a fresh node and would leave a removed rigidbody in place here.
        self.clear_doc_components(e, &merged);
        self.insert_doc(e, &merged);
        self.scene_dirty = true;
        Ok(())
    }

    /// Create a node — and its subtree, if the document names `children` — from
    /// a node document.
    fn ext_add_node_doc(
        &mut self,
        spec: &serde_json::Value,
        parent: Option<u32>,
    ) -> Result<(), String> {
        let parent_entity = match parent {
            Some(p) => {
                Some(self.entity_of(p).ok_or_else(|| format!("no node {p} to parent to"))?)
            }
            None => None,
        };
        // Read the WHOLE tree before spawning any of it: a document with a typo
        // three nodes down should cost a Console line, not half a room.
        let tree = read_spec(spec)?;
        self.spawn_spec_tree(&tree, parent_entity);
        self.scene_dirty = true;
        Ok(())
    }

    fn spawn_spec_tree(&mut self, spec: &NodeSpec, parent: Option<Entity>) {
        let e = self.spawn_node(&spec.doc);
        if let Some(p) = parent {
            self.world.insert(e, floptle_core::Parent(p));
        }
        for child in &spec.children {
            self.spawn_spec_tree(child, Some(e));
        }
    }

    /// Re-parent a node, keeping the place it is standing in.
    ///
    /// Goes through the editor's own `reparent_many`, which is what a drag in
    /// the Hierarchy does: it already refuses cycles, detaches from a bone, and
    /// rewrites the local transform so the node does not teleport. A second
    /// implementation of that here would be a second set of those rules.
    fn ext_set_parent(&mut self, id: u32, parent: Option<u32>) -> Result<(), String> {
        let Some(e) = self.entity_of(id) else {
            return Err(format!("no node {id}"));
        };
        let target = match parent {
            Some(p) => {
                Some(self.entity_of(p).ok_or_else(|| format!("no node {p} to parent to"))?)
            }
            None => None,
        };
        if target == Some(e) {
            return Err("a node cannot be its own parent".into());
        }
        if target.is_some_and(|p| self.is_descendant(p, e)) {
            return Err("that would put a node inside its own subtree".into());
        }
        self.reparent_many(&[e], target);
        self.scene_dirty = true;
        Ok(())
    }

    fn ext_spawn_prefab(&mut self, path: &str, pos: Option<[f64; 3]>) {
        let full = crate::project::resolve_asset_path(&self.project_root, path);
        self.instantiate_prefab(&full.display().to_string(), pos.map(DVec3::from), None);
    }

    /// Delete a node and everything under it — the same rule the Hierarchy's
    /// Delete follows, so an extension cannot leave orphaned children behind
    /// where the editor's own delete would not.
    fn ext_delete_subtree(&mut self, root: Entity) {
        let mut kids: std::collections::HashMap<Entity, Vec<Entity>> =
            std::collections::HashMap::new();
        for (e, p) in self.world.query::<floptle_core::Parent>() {
            kids.entry(p.0).or_default().push(e);
        }
        let mut doomed = Vec::new();
        let mut queue = std::collections::VecDeque::from(vec![root]);
        while let Some(e) = queue.pop_front() {
            // Post Processing is a mandatory scene node — the Hierarchy refuses
            // to delete it and so does this.
            if matches!(self.world.get::<Matter>(e), Some(Matter::PostProcess { .. })) {
                continue;
            }
            doomed.push(e);
            queue.extend(kids.get(&e).map(|v| v.as_slice()).unwrap_or(&[]).iter().copied());
        }
        for e in doomed {
            self.selection.retain(|s| *s != e);
            self.world.despawn(e);
        }
        self.scene_dirty = true;
    }

}

/// The shortcut pressed this frame, in the same spelling `ed.shortcut`
/// normalises to — or `None`.
///
/// A bare letter is never a shortcut here. The editor's own single-key bindings
/// (F1 to play, Tab, the tool keys) are the ones that own the unmodified
/// keyboard, and a package quietly claiming `G` would break them in a way
/// nobody would connect to a package. A function key or a named key is allowed
/// on its own; anything else needs a modifier.
pub(crate) fn pressed_shortcut(ctx: &egui::Context) -> Option<String> {
    ctx.input(|i| {
        for event in &i.events {
            let egui::Event::Key { key, pressed: true, repeat: false, modifiers, .. } = event else {
                continue;
            };
            let name = key_name(*key);
            let named = name.len() > 1;
            if !(modifiers.ctrl || modifiers.command || modifiers.alt || named) {
                continue;
            }
            let mut out = String::new();
            if modifiers.ctrl || modifiers.command {
                out.push_str("Ctrl+");
            }
            if modifiers.shift {
                out.push_str("Shift+");
            }
            if modifiers.alt {
                out.push_str("Alt+");
            }
            out.push_str(&name);
            return Some(out);
        }
        None
    })
}

/// egui's name for a key, with the digit keys spelled as digits — `ed.shortcut
/// ("Ctrl+1")` is what somebody writes, not `"Ctrl+Num1"`.
fn key_name(key: egui::Key) -> String {
    let n = key.name();
    match n.strip_prefix("Num") {
        Some(d) if d.len() == 1 && d.chars().all(|c| c.is_ascii_digit()) => d.to_string(),
        _ => n.to_string(),
    }
}

/// The top-level menus the loaded packages build, grouped by the first segment
/// of each registered path.
///
/// A free function over the host rather than a method on `Editor`: it is called
/// from the middle of `render`, where the draw path already holds pieces of the
/// editor and only disjoint field borrows are available.
pub(crate) fn menu_tree(host: &ext::ExtHost) -> Vec<ExtMenuGroup> {
    let mut groups: Vec<ExtMenuGroup> = Vec::new();
    for (i, m) in host.menus.iter().enumerate() {
        let (top, rest) = match m.path.split_once('/') {
            Some((a, b)) => (a.trim().to_string(), b.trim().to_string()),
            // A path with no slash is its own item under the package's name —
            // better than a menu called "Settings…" in the bar next to File.
            None => (
                host.packages.get(m.pkg).map(|p| p.name.clone()).unwrap_or_else(|| "Packages".into()),
                m.path.clone(),
            ),
        };
        match groups.iter_mut().find(|g| g.title == top) {
            Some(g) => g.items.push((rest, i)),
            None => groups.push(ExtMenuGroup { title: top, items: vec![(rest, i)] }),
        }
    }
    groups
}

/// A node's LOCAL half-extents, for `scene.raycast`.
///
/// The built-in shapes use the same figures the editor's own click-picking does,
/// so a ray and a click agree about where a node is. A model uses the longest
/// edge of its import bounds as a cube — loose on anything long and thin, and
/// the same approximation picking has always made. `None` means "not something a
/// ray can hit": a folder, a light, a camera, a probe.
fn local_half_extents(m: &Matter, model_size: Option<f32>) -> Option<[f32; 3]> {
    const P: f32 = crate::matter_catalog::PRIMITIVE_HALF;
    Some(match m {
        Matter::Primitive { shape, .. } => match shape {
            floptle_core::Shape::Cube | floptle_core::Shape::Plane => [P, P, P],
            floptle_core::Shape::Sphere => [0.85, 0.85, 0.85],
            // capsule(0.5, 0.5): radius 0.5, total Y half-extent 1.0.
            floptle_core::Shape::Capsule => [0.5, 1.0, 0.5],
        },
        Matter::Mesh { .. } | Matter::MapMesh { .. } => {
            let h = model_size? * 0.5;
            [h, h, h]
        }
        Matter::Tilemap { cols, rows, tile, .. } => {
            [*cols as f32 * *tile * 0.5, *rows as f32 * *tile * 0.5, *tile * 0.5]
        }
        Matter::WaterVolume { kind, radius, half_extents, .. } => match kind {
            floptle_core::WaterKind::Sea => [*radius, *radius, *radius],
            floptle_core::WaterKind::Pool => *half_extents,
        },
        // A Blob is an SDF in the raymarch and a Terrain is a whole field —
        // neither has a box worth pretending about, and the rest draw nothing.
        _ => return None,
    })
}

/// One top-level menu built out of the packages' registrations: its title, and
/// `(label, menu index)` for each item under it.
pub(crate) struct ExtMenuGroup {
    pub(crate) title: String,
    pub(crate) items: Vec<(String, usize)>,
}

// ---------------------------------------------------------------------------
// The node document, as a package writes it.
//
// A package sends a Lua table; these two functions turn it into a `NodeDoc` —
// the SAME type a `.ron` scene, a prefab and the clipboard all serialise. That
// is the whole design: there is no second description of what a node is for a
// package to write against, so a node type that gains a field is writable the
// day it lands and nothing here has to be updated to allow it.
// ---------------------------------------------------------------------------

/// A node document plus, optionally, the nodes under it.
///
/// `NodeDoc` has no `children` — a scene file is a flat list with parent
/// indices — but a package building a room wants to say "this, and these inside
/// it" in one call, so that it is one command and therefore one undo step.
pub(crate) struct NodeSpec {
    pub(crate) doc: floptle_scene::NodeDoc,
    pub(crate) children: Vec<NodeSpec>,
}

/// Every key a node document has, checked against what a package sent.
///
/// **This exists because serde IGNORES a key it does not recognise.** Without
/// it, `{ taggs = {"cover"} }` is accepted, does nothing, and reports success —
/// the exact silent-failure shape that is the single most common bug in this
/// engine's history. A misspelt property is now a Console line naming the key.
///
/// The exhaustive `let NodeDoc { .. }` is the maintenance contract: a field
/// added to `NodeDoc` stops this compiling, and whoever adds it puts the name in
/// the list below so packages can write it. Its sibling is
/// `Editor::clear_doc_components`, which breaks the same way for the same
/// reason.
fn known_doc_fields() -> &'static [&'static str] {
    #[allow(dead_code)]
    fn exhaustive(d: &floptle_scene::NodeDoc) {
        let floptle_scene::NodeDoc {
            name,
            transform,
            matter,
            scripts,
            material,
            object_materials,
            rigidbody,
            celestial,
            mesh_collider,
            disabled,
            paint,
            tex_paint,
            terrain_gen,
            collidable,
            trigger,
            nav_exclude,
            visible,
            cast_shadow,
            anim_controller,
            particles,
            id,
            parent_id,
            parent,
            attachment,
            net,
            ui_layer,
            ui,
            audio,
            layer,
            tags,
            sorting,
            sort_mode,
            parallax,
            lit_2d,
            light_layers,
            shadow_2d,
            light_inner,
            light_falloff,
            light_shadows,
            camera_2d,
        } = d;
        let _ = (
            name, transform, matter, scripts, material, object_materials, rigidbody, celestial,
            mesh_collider, disabled, paint, tex_paint, terrain_gen, collidable, trigger,
            nav_exclude, visible, cast_shadow, anim_controller, particles, id, parent_id, parent,
            attachment, net, ui_layer, ui, audio, layer, tags, sorting, sort_mode, parallax, lit_2d,
            light_layers,
            shadow_2d, light_inner, light_falloff, light_shadows, camera_2d,
        );
    }
    &[
        "name",
        "transform",
        "matter",
        "scripts",
        "material",
        "object_materials",
        "rigidbody",
        "celestial",
        "mesh_collider",
        "disabled",
        "paint",
        "tex_paint",
        "terrain_gen",
        "collidable",
        "trigger",
        "nav_exclude",
        "visible",
        "cast_shadow",
        "anim_controller",
        "particles",
        "net",
        "ui_layer",
        "ui",
        "audio",
        "layer",
        "tags",
        "sorting",
        "sort_mode",
        "parallax",
        "lit_2d",
        "light_layers",
        "shadow_2d",
        "light_inner",
        "light_falloff",
        "light_shadows",
        "camera_2d",
        // `id`, `parent_id`, `parent` and `attachment` are the scene FILE's
        // linkage between nodes. A package addresses a node by the id `scene.*`
        // gave it and re-parents with `scene.setParent`; letting one write these
        // would let it point a node at a position in a list it cannot see
        // (floptle/0046 — that moved a whole match HUD onto a line of help text).
    ]
}

/// Refuse a key that is not a node property, naming it and, where it is close to
/// a real one, saying which.
fn check_doc_keys(obj: &serde_json::Map<String, serde_json::Value>) -> Result<(), String> {
    let known = known_doc_fields();
    for key in obj.keys() {
        if known.contains(&key.as_str()) {
            continue;
        }
        let near = known.iter().find(|k| looks_like(k, key));
        return Err(match near {
            Some(k) => format!("{key:?} is not a node property — did you mean {k:?}?"),
            None => format!(
                "{key:?} is not a node property. See docs/editor-scripting.md, or                  scene.info(id) for what a node carries"
            ),
        });
    }
    Ok(())
}

/// Are these two names one small mistake apart? Used only to make an error
/// message helpful; a wrong guess costs nothing but a worse sentence.
fn looks_like(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    // Same letters in a different order, or one character out — which covers
    // the typos people actually make: "taggs", "tag", "Tags", "trasform".
    let norm = |s: &str| {
        let mut c: Vec<char> = s.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_lowercase()).collect();
        c.sort_unstable();
        c
    };
    if norm(a) == norm(b) {
        return true;
    }
    let (a, b) = (a.to_lowercase(), b.to_lowercase());
    a.len().abs_diff(b.len()) <= 1 && (a.starts_with(&b[..b.len().min(3)]) || b.starts_with(&a[..a.len().min(3)]))
}

/// Merge a partial document over a node's current one.
///
/// Object-valued keys merge one level deep and everything else replaces, so
/// `{ transform = { pos = {...} } }` moves a node without rewriting its
/// rotation, while `{ tags = {"a"} }` sets the tags to exactly that list. A
/// list is a value: a tool that means to add a tag reads the tags and writes
/// the longer list, which is the only reading of `tags = {...}` that is not a
/// guess.
fn merge_doc(
    current: &floptle_scene::NodeDoc,
    patch: &serde_json::Value,
) -> Result<floptle_scene::NodeDoc, String> {
    let mut base = serde_json::to_value(current).map_err(|e| e.to_string())?;
    let patch = patch
        .as_object()
        .ok_or_else(|| "expected a table of properties, e.g. { name = \"Door\" }".to_string())?;
    check_doc_keys(patch)?;
    let obj = base
        .as_object_mut()
        .ok_or_else(|| "this node's document is not a table".to_string())?;
    for (k, v) in patch {
        if k == "children" {
            return Err("children belongs to scene.add, not scene.set — \
                        use scene.add(doc, parentId) to build a subtree"
                .into());
        }
        match (obj.get_mut(k), v) {
            // One level of merging, so a nested group can be touched a field at
            // a time. Deeper than that and "replace this whole sub-object"
            // stops being expressible at all.
            (Some(serde_json::Value::Object(into)), serde_json::Value::Object(from)) => {
                for (k2, v2) in from {
                    into.insert(k2.clone(), v2.clone());
                }
            }
            _ => {
                obj.insert(k.clone(), v.clone());
            }
        }
    }
    serde_json::from_value(base).map_err(|e| describe_doc_error(&e.to_string()))
}

/// Read a document (and any `children`) a package sent, whole.
fn read_spec(spec: &serde_json::Value) -> Result<NodeSpec, String> {
    let mut value = spec.clone();
    let obj = value
        .as_object_mut()
        .ok_or_else(|| "expected a table describing a node".to_string())?;
    let children = match obj.remove("children") {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(serde_json::Value::Array(kids)) => {
            kids.iter().map(read_spec).collect::<Result<Vec<_>, _>>()?
        }
        Some(_) => return Err("children must be a list of nodes".into()),
    };
    // A node needs a name or the Hierarchy has nothing to draw and `entity_of`
    // cannot find it again — `Name` is what marks a row as a node at all.
    if !obj.get("name").is_some_and(|n| n.as_str().is_some_and(|s| !s.is_empty())) {
        return Err("a node needs a name".into());
    }
    check_doc_keys(obj)?;
    let doc: floptle_scene::NodeDoc =
        serde_json::from_value(value).map_err(|e| describe_doc_error(&e.to_string()))?;
    Ok(NodeSpec { doc, children })
}

/// serde's message, in words a package author can act on.
///
/// The raw text names JSON and Rust types, neither of which is what somebody
/// wrote — they wrote a Lua table, and the fix is nearly always a misspelt key
/// or a number where a table goes.
fn describe_doc_error(raw: &str) -> String {
    let hint = if raw.contains("unknown field") {
        " — check the spelling against docs/editor-scripting.md, or scene.info(id) for \
         what a node of this kind carries"
    } else if raw.contains("invalid type") {
        " — a table was expected where a value was given, or the other way round"
    } else {
        ""
    };
    format!("{raw}{hint}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Menu paths group by their first segment, so two packages both filing
    /// under "Tools" build one menu rather than two.
    #[test]
    fn no_packages_means_no_empty_menu_in_the_bar() {
        assert!(menu_tree(&ext::ExtHost::new()).is_empty());
    }

    #[test]
    fn the_engine_version_parses() {
        let v = Editor::engine_version();
        assert!(v.major > 0 || v.minor > 0, "{v}");
    }

    // ---- the node document (`floptle/0142`) -------------------------------

    /// An editor holding one plain node, and its id as a package sees it.
    fn with_a_node(name: &str) -> (Editor, u32) {
        let mut ed = Editor::default();
        let e = ed.world.spawn();
        ed.world.insert(e, floptle_core::Name(name.into()));
        ed.world.insert(e, Matter::Empty);
        ed.world.insert(e, floptle_core::Transform::IDENTITY);
        (ed, e.index())
    }

    fn json(s: &str) -> serde_json::Value {
        serde_json::from_str(s).unwrap()
    }

    /// The whole point: a package can set a field the old setters never named.
    #[test]
    fn a_patch_writes_a_field_no_setter_ever_named() {
        let (mut ed, id) = with_a_node("Crate");
        ed.ext_set_node_doc(id, &json(r#"{"tags": ["cover", "movable"], "layer": "props"}"#))
            .unwrap();
        let e = ed.entity_of(id).unwrap();
        assert_eq!(
            ed.world.get::<floptle_core::Tags>(e).map(|t| t.0.clone()),
            Some(vec!["cover".to_string(), "movable".to_string()])
        );
        assert_eq!(ed.world.get::<floptle_core::Layer>(e).map(|l| l.0.clone()), Some("props".into()));
    }

    /// A patch is a PATCH. Naming one field must not blank the others — which is
    /// the failure a whole-document write would have every time a package was
    /// written against an older engine than it runs on.
    #[test]
    fn a_patch_leaves_everything_it_did_not_name_alone() {
        let (mut ed, id) = with_a_node("Crate");
        ed.ext_set_node_doc(id, &json(r#"{"tags": ["cover"], "layer": "props"}"#)).unwrap();
        ed.ext_set_node_doc(id, &json(r#"{"name": "Renamed"}"#)).unwrap();
        let e = ed.entity_of(id).unwrap();
        assert_eq!(ed.world.get::<floptle_core::Name>(e).map(|n| n.0.clone()), Some("Renamed".into()));
        assert_eq!(
            ed.world.get::<floptle_core::Tags>(e).map(|t| t.0.clone()),
            Some(vec!["cover".to_string()]),
            "renaming a node must not drop its tags"
        );
    }

    /// And a field CAN be cleared, which only works because the write clears
    /// before it inserts. `insert_doc` alone would have left the component on.
    #[test]
    fn writing_a_field_away_actually_removes_it() {
        let (mut ed, id) = with_a_node("Crate");
        ed.ext_set_node_doc(id, &json(r#"{"tags": ["cover"]}"#)).unwrap();
        ed.ext_set_node_doc(id, &json(r#"{"tags": []}"#)).unwrap();
        let e = ed.entity_of(id).unwrap();
        assert!(
            ed.world.get::<floptle_core::Tags>(e).is_none_or(|t| t.0.is_empty()),
            "a tool that clears the tags has to actually clear them"
        );
    }

    /// The node keeps its identity. A package holding an id from last frame is
    /// still holding the same node — delete-and-respawn would have been simpler
    /// and would have invalidated every id a tool was working with.
    #[test]
    fn a_write_keeps_the_nodes_id_and_its_children() {
        let (mut ed, id) = with_a_node("Room");
        let parent = ed.entity_of(id).unwrap();
        let kid = ed.world.spawn();
        ed.world.insert(kid, floptle_core::Name("Lamp".into()));
        ed.world.insert(kid, Matter::Empty);
        ed.world.insert(kid, floptle_core::Transform::IDENTITY);
        ed.world.insert(kid, floptle_core::Parent(parent));

        ed.ext_set_node_doc(id, &json(r#"{"layer": "rooms"}"#)).unwrap();

        assert_eq!(ed.entity_of(id), Some(parent), "the id must still name the same node");
        assert_eq!(
            ed.world.get::<floptle_core::Parent>(kid).map(|p| p.0),
            Some(parent),
            "and its children must still be under it"
        );
    }

    /// One call, one subtree, one undo step. This is what "build a room" needs.
    #[test]
    fn add_builds_a_whole_subtree_from_one_document() {
        let mut ed = Editor::default();
        ed.ext_add_node_doc(
            &json(
                r#"{"name": "Room", "children": [
                     {"name": "Lamp"},
                     {"name": "Crate", "tags": ["cover"]}
                   ]}"#,
            ),
            None,
        )
        .unwrap();
        let names: Vec<String> = ed
            .world
            .query::<floptle_core::Name>()
            .map(|(_, n)| n.0.clone())
            .collect();
        assert!(names.contains(&"Room".to_string()), "{names:?}");
        assert!(names.contains(&"Lamp".to_string()), "{names:?}");
        assert!(names.contains(&"Crate".to_string()), "{names:?}");

        let room = ed
            .world
            .query::<floptle_core::Name>()
            .find(|(_, n)| n.0 == "Room")
            .map(|(e, _)| e)
            .unwrap();
        let under_room = ed
            .world
            .query::<floptle_core::Parent>()
            .filter(|(_, p)| p.0 == room)
            .count();
        assert_eq!(under_room, 2, "both children belong to the room");
    }

    /// A typo three nodes down costs a Console line, not half a room. Checked
    /// because the alternative — spawning as it reads — leaves a scene somebody
    /// has to clean up by hand.
    #[test]
    fn a_bad_child_stops_the_whole_subtree_from_being_built() {
        let mut ed = Editor::default();
        let err = ed
            .ext_add_node_doc(
                &json(
                    r#"{"name": "Room", "children": [
                         {"name": "Lamp"},
                         {"name": "Crate", "taggs": ["cover"]}
                       ]}"#,
                ),
                None,
            )
            .unwrap_err();
        assert!(err.contains("taggs") || err.contains("unknown field"), "{err}");
        assert_eq!(
            ed.world.query::<floptle_core::Name>().count(),
            0,
            "nothing at all should have been spawned"
        );
    }

    #[test]
    fn a_node_without_a_name_is_refused_rather_than_drawn_as_a_blank_row() {
        let mut ed = Editor::default();
        let err = ed.ext_add_node_doc(&json(r#"{"tags": ["x"]}"#), None).unwrap_err();
        assert!(err.contains("name"), "{err}");
    }

    /// Nested keys merge one level, so a tool can move a node without knowing
    /// its rotation.
    #[test]
    fn a_nested_key_merges_rather_than_replacing_the_group() {
        let (mut ed, id) = with_a_node("Crate");
        let e = ed.entity_of(id).unwrap();
        if let Some(t) = ed.world.get_mut::<floptle_core::Transform>(e) {
            t.scale = floptle_core::math::Vec3::splat(3.0);
        }
        ed.ext_set_node_doc(id, &json(r#"{"transform": {"translation": [1.0, 2.0, 3.0]}}"#))
            .unwrap();
        let t = ed.world.get::<floptle_core::Transform>(e).copied().unwrap();
        assert_eq!(t.translation, DVec3::new(1.0, 2.0, 3.0));
        assert_eq!(t.scale, floptle_core::math::Vec3::splat(3.0), "the scale was not named");
    }

    #[test]
    fn a_node_cannot_be_put_inside_its_own_subtree() {
        let (mut ed, room_id) = with_a_node("Room");
        let room = ed.entity_of(room_id).unwrap();
        let kid = ed.world.spawn();
        ed.world.insert(kid, floptle_core::Name("Lamp".into()));
        ed.world.insert(kid, Matter::Empty);
        ed.world.insert(kid, floptle_core::Transform::IDENTITY);
        ed.world.insert(kid, floptle_core::Parent(room));

        let err = ed.ext_set_parent(room_id, Some(kid.index())).unwrap_err();
        assert!(err.contains("own subtree"), "{err}");
        assert!(ed.world.get::<floptle_core::Parent>(room).is_none(), "the room stayed a root");
    }

    /// Re-parenting keeps the node where it is standing. A tool that puts a
    /// prop into a room did not mean to teleport it to the room's origin.
    #[test]
    fn re_parenting_does_not_move_the_node_in_the_world() {
        let (mut ed, room_id) = with_a_node("Room");
        let room = ed.entity_of(room_id).unwrap();
        if let Some(t) = ed.world.get_mut::<floptle_core::Transform>(room) {
            t.translation = DVec3::new(10.0, 0.0, 0.0);
        }
        let prop = ed.world.spawn();
        ed.world.insert(prop, floptle_core::Name("Crate".into()));
        ed.world.insert(prop, Matter::Empty);
        ed.world.insert(
            prop,
            floptle_core::Transform {
                translation: DVec3::new(4.0, 0.0, 0.0),
                ..floptle_core::Transform::IDENTITY
            },
        );

        ed.ext_set_parent(prop.index(), Some(room_id)).unwrap();

        let world = floptle_core::world_transform(&ed.world, prop);
        assert!(
            (world.translation - DVec3::new(4.0, 0.0, 0.0)).length() < 1e-9,
            "the crate moved: {:?}",
            world.translation
        );
    }

    /// The message is the feature. A tool author writing `taggs` gets told the
    /// name they meant, not a silent success.
    #[test]
    fn a_misspelt_property_names_the_one_that_was_meant() {
        let (mut ed, id) = with_a_node("Crate");
        let err = ed.ext_set_node_doc(id, &json(r#"{"taggs": ["cover"]}"#)).unwrap_err();
        assert!(err.contains("taggs"), "{err}");
        assert!(err.contains("\"tags\""), "{err}");

        let err = ed.ext_set_node_doc(id, &json(r#"{"rigid_body": {}}"#)).unwrap_err();
        assert!(err.contains("rigidbody"), "{err}");
    }

    /// The scene file's own linkage is not a package's to write: a `parent`
    /// index points at a POSITION in a list, and re-pointing one silently wires
    /// a scene to something else (floptle/0046).
    #[test]
    fn a_package_cannot_write_the_scene_files_parent_index() {
        let (mut ed, id) = with_a_node("Crate");
        let err = ed.ext_set_node_doc(id, &json(r#"{"parent": 3}"#)).unwrap_err();
        assert!(err.contains("parent"), "{err}");
    }

    #[test]
    fn a_write_to_a_node_that_is_gone_says_so_rather_than_doing_nothing() {
        let mut ed = Editor::default();
        let err = ed.ext_set_node_doc(4242, &json(r#"{"name": "x"}"#)).unwrap_err();
        assert!(err.contains("4242"), "{err}");
    }
}
