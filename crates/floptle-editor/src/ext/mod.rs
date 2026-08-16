//! **Editor extensions** — the Lua a package runs *inside the editor*.
//!
//! A package's `editor/*.lua` is loaded when the project opens and gets an API
//! the game never sees: menus, dockable panels, Scene-view overlays, world-space
//! handles, the node graph as an editable thing, undo, dialogs, preferences and
//! (if it asked for them) the network and the browser.
//!
//! ```lua
//! local panel = ed.window("Grass", function()
//!     gui.label("Brush")
//!     radius = gui.slider(radius, 0.1, 20, "radius")
//!     if gui.button("Scatter") then scatter() end
//! end)
//! ed.menu("Grass/Brush…", function() panel:show() end)
//! ed.onSceneDraw(function()
//!     handles.color(0.3, 1, 0.5)
//!     handles.wireDisc(cursor, vec3(0,1,0), radius)
//! end)
//! ```
//!
//! ## The shape of it, and why
//!
//! **Lua never touches the editor.** Every binding either reads a per-frame
//! *mirror* ([`Snapshot`], [`SceneMirror`]) or pushes an [`ExtCmd`] onto a
//! queue the editor drains after the frame. This is the same contract
//! `floptle-script` runs the game under, and it is what makes an extension safe
//! to call from the middle of an egui pass: there is no `&mut Editor` to hand
//! out, so no extension can be holding one when the editor needs it back.
//!
//! The one exception is drawing, and it is scoped rather than stored:
//! `gui.*` is installed by [`Lua::scope`] for the length of one callback,
//! bound to the `egui::Ui` that callback is drawing into, and taken away again.
//! A panel cannot squirrel a widget function away and call it next frame — Lua
//! raises rather than drawing into a dead layout.
//!
//! **A package gets what it declared, and nothing else.** `http.*` is absent
//! from a package that did not ask for [`Permission::Network`] — absent, not
//! refused at the call, so the failure is at the top of the file where an
//! author will see it, not three menus deep in front of a user.
//!
//! **One broken extension is one broken extension.** Every entry point catches,
//! reports to the Console once, and disables the callback that raised rather
//! than raising sixty times a second. The editor keeps running; the package list
//! shows what happened.

pub(crate) mod api;
pub(crate) mod gui;
pub(crate) mod handles;
pub(crate) mod http;
pub(crate) mod prefs;
pub(crate) mod scene_mirror;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use floptle_package::{LoadReport, Loaded, Permission};
use mlua::{Lua, RegistryKey};

pub(crate) use handles::HandleCmd;
pub(crate) use scene_mirror::SceneMirror;

/// How loud a line from an extension is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ExtLevel {
    Info,
    Warn,
    Error,
}

/// One thing an extension asked the editor to do. Drained and applied after the
/// UI pass — see `Editor::apply_ext_commands`.
#[derive(Debug)]
pub(crate) enum ExtCmd {
    /// Record an undo point before the edits that follow it in this batch.
    Undo,
    SelectionSet(Vec<u32>),
    NodeSetName(u32, String),
    NodeSetPos(u32, [f64; 3]),
    NodeSetRot(u32, [f32; 4]),
    NodeSetScale(u32, [f32; 3]),
    NodeSetVisible(u32, bool),
    NodeCreate { name: String, parent: Option<u32> },
    NodeDestroy(u32),
    SpawnPrefab { path: String, pos: Option<[f64; 3]> },
    OpenScene(String),
    SaveScene,
    SetPlaying(bool),
    /// Open a URL in the user's browser. Only reachable with
    /// [`Permission::Browser`].
    OpenUrl(String),
    /// Show/hide/focus a registered panel by the id `ed.window` handed back.
    WindowOpen(u32, bool),
    WindowFocus(u32),
    OverlayOpen(u32, bool),
    /// A modal message, shown by the editor next frame.
    Message { title: String, body: String },
}

/// A line an extension logged.
pub(crate) struct ExtLog {
    pub(crate) level: ExtLevel,
    pub(crate) msg: String,
    /// The package's display name, so the Console says who is talking.
    pub(crate) from: String,
}

/// What the editor tells extensions about itself each frame. Read-only from
/// Lua; rebuilt before the UI pass.
#[derive(Clone, Default)]
pub(crate) struct Snapshot {
    pub(crate) project_root: PathBuf,
    pub(crate) project_name: String,
    /// Project-relative path of the scene that is open.
    pub(crate) scene: String,
    pub(crate) playing: bool,
    pub(crate) selection: Vec<u32>,
    /// Editor camera: world position and forward, for extensions that reason
    /// about what the author is looking at.
    pub(crate) cam_pos: [f64; 3],
    pub(crate) cam_fwd: [f32; 3],
    /// Seconds since the editor started, and this frame's delta.
    pub(crate) time: f64,
    pub(crate) dt: f32,
}

/// The queues and mirrors every binding reads or writes. One per host, shared
/// with every closure by `Rc`.
#[derive(Default)]
pub(crate) struct Shared {
    pub(crate) log: RefCell<Vec<ExtLog>>,
    pub(crate) cmds: RefCell<Vec<ExtCmd>>,
    pub(crate) handles: RefCell<Vec<HandleCmd>>,
    pub(crate) scene: RefCell<SceneMirror>,
    pub(crate) snap: RefCell<Snapshot>,
    /// The scene's baked navmesh, or `None` where nobody has baked one.
    ///
    /// Not rebuilt with the other mirrors: a navmesh changes when somebody
    /// bakes it and at no other time, and a level's worth of polygons is not a
    /// thing to copy sixty times a second. Written by
    /// [`crate::Editor::publish_nav_mesh`].
    ///
    /// Deliberately the **editor's** bake and not the running game's, which
    /// during Play may have obstacles carved out of it. A package reads the
    /// level as it was authored.
    pub(crate) nav: floptle_script::nav_api::NavShared,
    /// Per-package stores: user preferences, per-project state, per-session
    /// scratch.
    pub(crate) prefs: RefCell<prefs::Stores>,
    /// Set to true by anything that should make the editor draw again promptly.
    pub(crate) repaint: std::cell::Cell<bool>,
    /// Registrations made during a load pass — collected here because the
    /// closures cannot reach `&mut ExtHost`.
    pub(crate) pending: RefCell<Vec<Registration>>,
    /// Web requests in flight and their replies. Only used by packages holding
    /// [`Permission::Network`].
    pub(crate) web: RefCell<http::WebState>,
    /// Panel id → is it open. Mirrored here so a handle's `isOpen()` can answer
    /// without reaching into the host, and written by both sides.
    pub(crate) open_state: RefCell<HashMap<u32, bool>>,
    /// Timer ids a handle's `cancel()` has struck out. A set rather than a
    /// removal because the binding cannot reach the host's list, and because a
    /// timer that cancels itself from inside its own callback must not shorten
    /// the list the fire pass is walking.
    pub(crate) cancelled: RefCell<std::collections::HashSet<u32>>,
    /// The id `ed.window` / `ed.overlay` hands back. Allocated from Lua, so it
    /// lives on the shared state rather than on the host.
    next_id: std::cell::Cell<u32>,
}

impl Shared {
    pub(crate) fn alloc_id(&self) -> u32 {
        let id = self.next_id.get() + 1;
        self.next_id.set(id);
        id
    }
}

/// A registration a package made while loading (or later, from a callback).
pub(crate) enum Registration {
    Window { pkg: usize, id: u32, title: String, cb: RegistryKey, open: bool },
    Menu { pkg: usize, path: String, cb: RegistryKey },
    Overlay { pkg: usize, id: u32, name: String, cb: RegistryKey, open: bool },
    Shortcut { pkg: usize, keys: String, cb: RegistryKey },
    Hook { pkg: usize, kind: HookKind, cb: RegistryKey },
    Timer { pkg: usize, id: u32, every: f64, repeat: bool, cb: RegistryKey },
}

/// Which editor moment a callback wants.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub(crate) enum HookKind {
    /// Every editor frame, before the UI is drawn.
    Update,
    /// The Scene view is about to be painted — the place `handles.*` works.
    SceneDraw,
    SceneOpen,
    SceneSave,
    SelectionChange,
    Play,
    Stop,
    /// The project (or the package) is going away.
    Unload,
}

impl HookKind {
    pub(crate) fn lua_name(self) -> &'static str {
        match self {
            HookKind::Update => "onUpdate",
            HookKind::SceneDraw => "onSceneDraw",
            HookKind::SceneOpen => "onSceneOpen",
            HookKind::SceneSave => "onSceneSave",
            HookKind::SelectionChange => "onSelectionChange",
            HookKind::Play => "onPlay",
            HookKind::Stop => "onStop",
            HookKind::Unload => "onUnload",
        }
    }

    pub(crate) const ALL: &'static [HookKind] = &[
        HookKind::Update,
        HookKind::SceneDraw,
        HookKind::SceneOpen,
        HookKind::SceneSave,
        HookKind::SelectionChange,
        HookKind::Play,
        HookKind::Stop,
        HookKind::Unload,
    ];
}

/// A package, as the host holds it.
pub(crate) struct PkgState {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) root: PathBuf,
    pub(crate) permissions: Vec<Permission>,
    /// Set when something in this package raised: it stops being called, and the
    /// list says why.
    pub(crate) failed: Option<String>,
}

/// A dockable panel a package registered.
pub(crate) struct WindowReg {
    pub(crate) pkg: usize,
    /// The id `ed.window` handed the package.
    pub(crate) id: u32,
    /// Stable across a reload: `ed.window` keyed by title within a package, so
    /// a docked panel comes back to the same place after the package reloads.
    pub(crate) key: String,
    pub(crate) title: String,
    pub(crate) cb: RegistryKey,
    pub(crate) open: bool,
    /// Set when this panel's callback raised — it draws the error instead.
    pub(crate) error: Option<String>,
}

/// A Scene-view overlay a package registered.
pub(crate) struct OverlayReg {
    pub(crate) pkg: usize,
    pub(crate) id: u32,
    pub(crate) name: String,
    pub(crate) cb: RegistryKey,
    pub(crate) open: bool,
    pub(crate) error: Option<String>,
}

/// A menu item a package registered. `path` is `"Menu/Item"` or
/// `"Menu/Sub/Item"`.
pub(crate) struct MenuReg {
    pub(crate) pkg: usize,
    pub(crate) path: String,
    pub(crate) cb: RegistryKey,
}

/// A keyboard shortcut a package registered.
pub(crate) struct ShortcutReg {
    pub(crate) pkg: usize,
    /// Normalised: modifiers in `Ctrl+Shift+Alt` order, then one key name.
    pub(crate) keys: String,
    pub(crate) cb: RegistryKey,
}

pub(crate) struct HookReg {
    pub(crate) pkg: usize,
    pub(crate) kind: HookKind,
    pub(crate) cb: RegistryKey,
}

/// A callback waiting for a clock rather than for an event.
///
/// Registered by `ed.after` / `ed.every`, which exist because the alternative
/// was every package writing the same thing: keep a deadline, compare it against
/// `ed.time()` from inside `onUpdate`, and remember to take it down again. That
/// is four lines of bookkeeping to say "in two seconds", and each copy of it is
/// a place to get the take-down wrong.
pub(crate) struct TimerReg {
    pub(crate) pkg: usize,
    pub(crate) id: u32,
    /// Seconds between firings. Clamped away from zero at registration, because
    /// `ed.every(0, …)` is a request for an infinite loop.
    pub(crate) every: f64,
    pub(crate) repeat: bool,
    /// Editor clock reading this next fires at.
    pub(crate) due: f64,
    pub(crate) cb: RegistryKey,
    /// Set by the handle's `cancel()`. Swept after the fire pass rather than
    /// removed on the spot, so a timer cancelling itself — or its neighbour —
    /// from inside its own callback cannot shift the list being walked.
    pub(crate) cancelled: bool,
}

/// The editor's extension host. Present even with no packages installed, in
/// which case every entry point is a cheap no-op.
pub(crate) struct ExtHost {
    lua: Lua,
    pub(crate) shared: Rc<Shared>,
    pub(crate) packages: Vec<PkgState>,
    pub(crate) windows: Vec<WindowReg>,
    pub(crate) menus: Vec<MenuReg>,
    pub(crate) overlays: Vec<OverlayReg>,
    pub(crate) shortcuts: Vec<ShortcutReg>,
    pub(crate) hooks: Vec<HookReg>,
    pub(crate) timers: Vec<TimerReg>,
    /// What [`floptle_package::resolve`] found — the package list's data.
    pub(crate) report: LoadReport,
    /// Which panels were open before the last reload, so a reload does not
    /// close everything the author had arranged.
    reopen: HashMap<String, bool>,
    /// The one table every package's environment falls through to.
    ///
    /// This is how `gui` can exist for the length of one callback and not a
    /// moment longer: a package's environment is an allow-list table with no
    /// route to the real globals, so a scoped binding has to be reachable
    /// through something the environment *does* see. Putting `gui` here for the
    /// duration of a draw call, and taking it out after, is that route — and it
    /// is one table rather than one per package, so a panel and an overlay
    /// cannot end up looking at different `gui`s.
    dynamic: Option<RegistryKey>,
}

impl Default for ExtHost {
    fn default() -> Self {
        Self::new()
    }
}

impl ExtHost {
    pub(crate) fn new() -> Self {
        Self {
            lua: Lua::new(),
            shared: Rc::new(Shared::default()),
            packages: Vec::new(),
            windows: Vec::new(),
            menus: Vec::new(),
            overlays: Vec::new(),
            shortcuts: Vec::new(),
            hooks: Vec::new(),
            timers: Vec::new(),
            report: LoadReport::default(),
            reopen: HashMap::new(),
            dynamic: None,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.packages.is_empty()
    }

    /// Load (or reload) every enabled package in `project_root`.
    ///
    /// Everything registered by the previous load is dropped first — including
    /// the Lua state, so a reload cannot leave a callback from the old copy of a
    /// file running beside the new one. Open panels are remembered by title and
    /// reopened.
    pub(crate) fn reload(&mut self, project_root: &Path, engine: &floptle_package::Version) {
        // Remember what was open, keyed by the same key the new registration
        // will compute.
        for w in &self.windows {
            self.reopen.insert(w.key.clone(), w.open);
        }
        let report = floptle_package::resolve(project_root, engine);
        self.teardown();
        self.report = report;
        self.dynamic = self
            .lua
            .create_table()
            .and_then(|t| self.lua.create_registry_value(t))
            .ok();

        for (i, pkg) in self.report.loaded.clone().iter().enumerate() {
            self.load_package(i, pkg, project_root);
        }
        self.drain_pending();
    }

    /// Drop every package and the Lua state with them.
    pub(crate) fn teardown(&mut self) {
        if !self.hooks.is_empty() {
            self.fire(HookKind::Unload);
        }
        self.packages.clear();
        self.windows.clear();
        self.menus.clear();
        self.overlays.clear();
        self.shortcuts.clear();
        self.hooks.clear();
        self.timers.clear();
        self.shared.cancelled.borrow_mut().clear();
        self.shared.pending.borrow_mut().clear();
        self.shared.handles.borrow_mut().clear();
        self.shared.cmds.borrow_mut().clear();
        self.shared.web.borrow_mut().cancel_all();
        // A fresh state, so nothing survives a reload by hiding in a global.
        self.lua = Lua::new();
        self.dynamic = None;
        self.report = LoadReport::default();
    }

    fn load_package(&mut self, idx: usize, pkg: &Loaded, project_root: &Path) {
        let scripts = pkg.editor_scripts();
        let state = PkgState {
            id: pkg.id().to_string(),
            name: pkg.manifest.name.clone(),
            version: pkg.manifest.version.to_string(),
            root: pkg.root.clone(),
            permissions: pkg.manifest.permissions.clone(),
            failed: None,
        };
        self.packages.push(state);
        if scripts.is_empty() {
            return; // an assets-only package is a perfectly good package
        }
        self.shared.prefs.borrow_mut().open(pkg.id(), project_root);

        let dynamic = self.dynamic_table();
        let env = match api::build_env(
            &self.lua,
            &self.shared,
            idx,
            self.packages.last().unwrap(),
            dynamic,
        ) {
            Ok(e) => e,
            Err(e) => {
                self.fail(idx, format!("could not build the extension environment: {e}"));
                return;
            }
        };
        for path in &scripts {
            let text = match std::fs::read_to_string(path) {
                Ok(t) => t,
                Err(e) => {
                    self.fail(idx, format!("{}: {e}", path.display()));
                    return;
                }
            };
            let name = path
                .strip_prefix(&pkg.root)
                .unwrap_or(path)
                .display()
                .to_string();
            let res = self
                .lua
                .load(&text)
                .set_name(format!("@{}/{name}", pkg.id()))
                .set_environment(env.clone())
                .exec();
            if let Err(e) = res {
                self.fail(idx, format!("{name}: {}", trim_lua_error(&e.to_string())));
                return;
            }
        }
    }

    fn fail(&mut self, pkg: usize, why: String) {
        let name = self.packages.get(pkg).map(|p| p.name.clone()).unwrap_or_default();
        if let Some(p) = self.packages.get_mut(pkg) {
            p.failed = Some(why.clone());
        }
        self.shared.log.borrow_mut().push(ExtLog {
            level: ExtLevel::Error,
            msg: why,
            from: name,
        });
    }

    /// Move registrations made from Lua into the host's own lists.
    pub(crate) fn drain_pending(&mut self) {
        let pending: Vec<Registration> = self.shared.pending.borrow_mut().drain(..).collect();
        for r in pending {
            match r {
                Registration::Window { pkg, id, title, cb, open } => {
                    let key = format!(
                        "{}::{title}",
                        self.packages.get(pkg).map(|p| p.id.as_str()).unwrap_or("?")
                    );
                    let open = self.reopen.get(&key).copied().unwrap_or(open);
                    self.shared.open_state.borrow_mut().insert(id, open);
                    self.windows.push(WindowReg { pkg, id, key, title, cb, open, error: None });
                }
                Registration::Menu { pkg, path, cb } => {
                    self.menus.push(MenuReg { pkg, path, cb });
                }
                Registration::Overlay { pkg, id, name, cb, open } => {
                    self.overlays.push(OverlayReg { pkg, id, name, cb, open, error: None });
                }
                Registration::Shortcut { pkg, keys, cb } => {
                    self.shortcuts.push(ShortcutReg { pkg, keys, cb });
                }
                Registration::Hook { pkg, kind, cb } => {
                    self.hooks.push(HookReg { pkg, kind, cb });
                }
                Registration::Timer { pkg, id, every, repeat, cb } => {
                    // Due relative to the clock the fire pass reads, so a timer
                    // registered mid-frame does not fire in that same frame.
                    let now = self.shared.snap.borrow().time;
                    self.timers.push(TimerReg {
                        pkg,
                        id,
                        every,
                        repeat,
                        due: now + every,
                        cb,
                        cancelled: false,
                    });
                }
            }
        }
    }

    // ---- per-frame entry points -------------------------------------------

    /// Hand the extensions this frame's view of the editor. Call before the UI
    /// pass.
    /// Hand the packages the scene's baked navmesh. See [`Shared::nav`] for why
    /// this is not part of `begin_frame`.
    pub(crate) fn set_nav_mesh(&self, mesh: Option<floptle_nav::NavMesh>) {
        *self.shared.nav.borrow_mut() = mesh;
    }

    pub(crate) fn begin_frame(&mut self, snap: Snapshot, scene: SceneMirror) {
        *self.shared.snap.borrow_mut() = snap;
        *self.shared.scene.borrow_mut() = scene;
        self.shared.handles.borrow_mut().clear();
        self.shared.repaint.set(false);
        self.shared.web.borrow_mut().pump();
        self.drain_pending();
    }

    /// Run one hook kind across every package, in load order.
    pub(crate) fn fire(&mut self, kind: HookKind) {
        if self.hooks.is_empty() {
            return;
        }
        let calls: Vec<(usize, usize)> = self
            .hooks
            .iter()
            .enumerate()
            .filter(|(_, h)| h.kind == kind)
            .map(|(i, h)| (i, h.pkg))
            .collect();
        for (i, pkg) in calls {
            if self.packages.get(pkg).is_some_and(|p| p.failed.is_some()) {
                continue;
            }
            let Some(func) = self.func(&self.hooks[i].cb) else { continue };
            if let Err(e) = func.call::<()>(()) {
                let where_ = kind.lua_name();
                self.fail(pkg, format!("{where_}: {}", trim_lua_error(&e.to_string())));
            }
        }
        self.drain_pending();
    }

    /// Fire whichever timers are due, and take the spent ones away.
    ///
    /// Runs before `onUpdate`, so a package that registers a timer and a hook
    /// sees them in the order it wrote them. The clock is the editor's, the same
    /// one `ed.time()` answers with — which means timers do not run while the
    /// editor is not drawing, and a package must not use one to measure real
    /// time. What they are for is "in two seconds", "every half second", and
    /// that is what the docs say they are for.
    ///
    /// A repeating timer's next due time is computed from the one it just met
    /// rather than from now, so a slow frame does not make every subsequent
    /// firing late — but it is never allowed to fall more than one period
    /// behind, because catching up on a stalled minute by firing a hundred and
    /// twenty times is worse than missing them.
    pub(crate) fn tick_timers(&mut self) {
        if self.timers.is_empty() {
            return;
        }
        let now = self.shared.snap.borrow().time;
        let due: Vec<usize> = self
            .timers
            .iter()
            .enumerate()
            .filter(|(_, t)| !t.cancelled && t.due <= now)
            .map(|(i, _)| i)
            .collect();
        for i in due {
            let (pkg, id, repeat, every) = {
                let t = &self.timers[i];
                (t.pkg, t.id, t.repeat, t.every)
            };
            if self.packages.get(pkg).is_some_and(|p| p.failed.is_some()) {
                continue;
            }
            // Re-arm BEFORE the call, so a callback that cancels itself wins.
            if repeat {
                let t = &mut self.timers[i];
                t.due = (t.due + every).max(now - every);
            } else {
                self.shared.cancelled.borrow_mut().insert(id);
            }
            let Some(func) = self.func(&self.timers[i].cb) else { continue };
            if let Err(e) = func.call::<()>(()) {
                self.fail(pkg, format!("timer: {}", trim_lua_error(&e.to_string())));
            }
        }
        let struck = std::mem::take(&mut *self.shared.cancelled.borrow_mut());
        if !struck.is_empty() {
            for t in self.timers.iter_mut().filter(|t| struck.contains(&t.id)) {
                t.cancelled = true;
            }
            self.timers.retain(|t| !t.cancelled);
        }
        self.drain_pending();
    }

    /// Fire the web replies that arrived since the last frame.
    pub(crate) fn pump_web(&mut self) {
        let replies = self.shared.web.borrow_mut().take_ready();
        for (cb, value) in replies {
            let Some(func) = self.func(&cb) else { continue };
            let table = match http::reply_table(&self.lua, &value) {
                Ok(t) => t,
                Err(_) => continue,
            };
            if let Err(e) = func.call::<()>(table) {
                self.shared.log.borrow_mut().push(ExtLog {
                    level: ExtLevel::Error,
                    msg: format!("a web reply raised: {}", trim_lua_error(&e.to_string())),
                    from: String::new(),
                });
            }
            self.lua.remove_registry_value(cb).ok();
        }
        self.drain_pending();
    }

    fn func(&self, key: &RegistryKey) -> Option<mlua::Function> {
        self.lua.registry_value::<mlua::Function>(key).ok()
    }

    fn dynamic_table(&self) -> Option<mlua::Table> {
        self.dynamic.as_ref().and_then(|k| self.lua.registry_value::<mlua::Table>(k).ok())
    }

    /// Draw a registered panel into `ui`. `which` indexes [`Self::windows`].
    pub(crate) fn draw_window(&mut self, which: usize, ui: &mut egui::Ui) {
        let Some(w) = self.windows.get(which) else { return };
        if let Some(err) = &w.error {
            draw_failure(ui, err);
            return;
        }
        let pkg = w.pkg;
        if let Some(p) = self.packages.get(pkg)
            && let Some(err) = &p.failed
        {
            let err = err.clone();
            draw_failure(ui, &err);
            return;
        }
        let Some(func) = self.func(&self.windows[which].cb) else { return };
        if let Err(e) = self.call_with_ui(&func, ui) {
            let msg = trim_lua_error(&e.to_string());
            self.windows[which].error = Some(msg.clone());
            self.fail(pkg, format!("panel: {msg}"));
        }
        self.drain_pending();
    }

    /// Draw a registered Scene-view overlay.
    pub(crate) fn draw_overlay(&mut self, which: usize, ui: &mut egui::Ui) {
        let Some(o) = self.overlays.get(which) else { return };
        if o.error.is_some() {
            return;
        }
        let pkg = o.pkg;
        if self.packages.get(pkg).is_some_and(|p| p.failed.is_some()) {
            return;
        }
        let Some(func) = self.func(&self.overlays[which].cb) else { return };
        if let Err(e) = self.call_with_ui(&func, ui) {
            let msg = trim_lua_error(&e.to_string());
            self.overlays[which].error = Some(msg.clone());
            self.fail(pkg, format!("overlay: {msg}"));
        }
        self.drain_pending();
    }

    /// Run a menu item's callback.
    pub(crate) fn run_menu(&mut self, which: usize) {
        let Some(m) = self.menus.get(which) else { return };
        let pkg = m.pkg;
        let Some(func) = self.func(&self.menus[which].cb) else { return };
        if let Err(e) = func.call::<()>(()) {
            self.fail(pkg, format!("menu: {}", trim_lua_error(&e.to_string())));
        }
        self.drain_pending();
    }

    /// Run a shortcut's callback.
    pub(crate) fn run_shortcut(&mut self, which: usize) {
        let Some(s) = self.shortcuts.get(which) else { return };
        let pkg = s.pkg;
        let Some(func) = self.func(&self.shortcuts[which].cb) else { return };
        if let Err(e) = func.call::<()>(()) {
            self.fail(pkg, format!("shortcut: {}", trim_lua_error(&e.to_string())));
        }
        self.drain_pending();
    }

    /// Call `func` with `gui.*` bound to `ui` for exactly the length of the
    /// call. See the module docs: this is the one place an extension touches
    /// anything of the editor's directly, and it is undone before returning.
    fn call_with_ui(&self, func: &mlua::Function, ui: &mut egui::Ui) -> mlua::Result<()> {
        let Some(dynamic) = self.dynamic_table() else {
            return Err(mlua::Error::runtime("the extension host has no environment"));
        };
        let slot = RefCell::new(gui::UiSlot::new(ui));
        self.lua.scope(|scope| {
            let table = gui::bind(&self.lua, scope, &slot)?;
            dynamic.set("gui", table)?;
            let r = func.call::<()>(());
            // Taken away again whatever happened — a raised callback must not
            // leave a live `gui` pointing at a layout that is about to end.
            dynamic.set("gui", mlua::Value::Nil)?;
            r
        })
    }

    // ---- draining ----------------------------------------------------------

    pub(crate) fn take_log(&self) -> Vec<ExtLog> {
        self.shared.log.borrow_mut().drain(..).collect()
    }

    pub(crate) fn take_cmds(&self) -> Vec<ExtCmd> {
        self.shared.cmds.borrow_mut().drain(..).collect()
    }

    pub(crate) fn handles(&self) -> std::cell::Ref<'_, Vec<HandleCmd>> {
        self.shared.handles.borrow()
    }

    pub(crate) fn wants_repaint(&self) -> bool {
        self.shared.repaint.get() || self.shared.web.borrow().in_flight() > 0
    }

    /// Persist every package's stores. Called on project close and on quit.
    pub(crate) fn save_prefs(&self) {
        self.shared.prefs.borrow_mut().save_all();
    }

    /// A registered panel's index, from the id its handle carries.
    pub(crate) fn window_index(&self, id: u32) -> Option<usize> {
        self.windows.iter().position(|w| w.id == id)
    }

    pub(crate) fn overlay_index(&self, id: u32) -> Option<usize> {
        self.overlays.iter().position(|o| o.id == id)
    }

    pub(crate) fn set_window_open(&mut self, which: usize, open: bool) {
        if let Some(w) = self.windows.get_mut(which) {
            w.open = open;
            let (key, id) = (w.key.clone(), w.id);
            self.reopen.insert(key, open);
            self.shared.open_state.borrow_mut().insert(id, open);
        }
    }

    pub(crate) fn set_overlay_open(&mut self, which: usize, open: bool) {
        if let Some(o) = self.overlays.get_mut(which) {
            o.open = open;
            self.shared.open_state.borrow_mut().insert(o.id, open);
        }
    }
}

fn draw_failure(ui: &mut egui::Ui, err: &str) {
    ui.colored_label(egui::Color32::from_rgb(230, 120, 110), "This panel stopped:");
    ui.label(err);
    ui.small("Fix the script and reload the package from ⚙ Packages.");
}

/// Lua prefixes a runtime error with the chunk name and line, then repeats the
/// traceback. The first line is the part a package author acts on; the rest is
/// noise in a Console row.
pub(crate) fn trim_lua_error(e: &str) -> String {
    let first = e.lines().find(|l| !l.trim().is_empty()).unwrap_or(e);
    first.trim().to_string()
}

#[cfg(test)]
mod tests;
