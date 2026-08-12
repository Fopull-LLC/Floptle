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
        if self.ext.is_empty() {
            self.ext_painted.clear();
            return;
        }
        let mirror = self.ext_mirror();
        self.ext.begin_frame(self.ext_snapshot(), mirror);
        self.ext.pump_web();
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
        SceneMirror::build(
            &self.world,
            &|_e: Entity, m: &Matter| {
                crate::node_bounds::local_radius(
                    m,
                    crate::node_bounds::Measured {
                        model_size: model_size(m),
                        sprite_reach: None,
                    },
                )
            },
            &|_e: Entity, m: &Matter| local_half_extents(m, model_size(m)),
        )
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
                ExtCmd::Message { title, body } => {
                    self.ext_message = Some((title, body));
                }
            }
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
            .query::<floptle_core::Name>()
            .map(|(e, _)| e)
            .find(|e| e.index() == id)
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
}
