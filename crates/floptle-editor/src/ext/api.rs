//! The environment an editor extension runs in: `ed`, `scene`, `selection`,
//! `handles`, and — only if the package asked for them — `http` and `sys`.
//!
//! ## A package does not get `_G`
//!
//! The environment is built from an **allow-list**, not by falling through to
//! the real globals. `io`, `os` beyond the clock, `package`, `require`,
//! `load`/`loadstring` and `dofile` are not in it. Otherwise the permission
//! list would be decoration: a package that never declared `Files` could open
//! one with `io.open`, and one that never declared `Network` could shell out.
//!
//! This is a barrier, not a jail — Lua embedded in a native host has never been
//! a security boundary, and a package is code somebody chose to install. What
//! it buys is that the *declared* capabilities are the *reachable* ones, so the
//! list shown before install describes what the package can actually do.
//!
//! ## Everything is a mirror or a queue
//!
//! Reads come from [`Shared::scene`] and [`Shared::snap`], rebuilt once a frame.
//! Writes push an [`ExtCmd`]. Nothing here holds a reference into the editor,
//! which is what lets a panel body run in the middle of an egui pass.

use std::path::PathBuf;
use std::rc::Rc;

use floptle_package::Permission;
use mlua::{Function, Lua, Table, Value, Variadic};

use super::prefs::Kind as StoreKind;
use super::{ExtCmd, ExtLevel, ExtLog, HookKind, PkgState, Registration, Shared};

/// Build one package's environment table.
pub(crate) fn build_env(
    lua: &Lua,
    shared: &Rc<Shared>,
    pkg: usize,
    state: &PkgState,
    dynamic: Option<Table>,
) -> mlua::Result<Table> {
    let env = lua.create_table()?;
    base_globals(lua, &env)?;
    // The one route out of the allow-list, and it leads to a table the host
    // fills in for the length of a draw call — see `ExtHost::dynamic`. Without
    // it `gui` would have nowhere to live that a package could see.
    if let Some(d) = dynamic {
        let mt = lua.create_table()?;
        mt.set("__index", d)?;
        env.set_metatable(Some(mt));
    }

    let id = state.id.clone();
    env.set("print", print_fn(lua, shared, &state.name)?)?;
    env.set("json", json_table(lua)?)?;
    env.set("vec3", vec3_ctor(lua)?)?;
    env.set("vec2", vec2_ctor(lua)?)?;
    env.set("ed", ed_table(lua, shared, pkg, state)?)?;
    env.set("scene", scene_table(lua, shared)?)?;
    env.set("selection", selection_table(lua, shared)?)?;
    env.set("handles", super::handles::bind(lua, shared)?)?;

    if state.permissions.contains(&Permission::Network) {
        env.set("http", http_table(lua, shared)?)?;
    }
    if state.permissions.contains(&Permission::Browser) {
        env.set("sys", sys_table(lua, shared)?)?;
    }
    // `require` reaches only inside this package. A package's second file is
    // the first thing anybody wants and `dofile` would be a hole.
    env.set("require", require_fn(lua, shared, &env, state.root.clone(), id)?)?;
    Ok(env)
}

/// The Lua that is safe to hand a package: values and pure functions, no I/O,
/// no dynamic loading, no reaching into the host.
fn base_globals(lua: &Lua, env: &Table) -> mlua::Result<()> {
    let g = lua.globals();
    for name in [
        "assert", "error", "ipairs", "next", "pairs", "pcall", "rawequal", "rawget", "rawlen",
        "rawset", "select", "setmetatable", "getmetatable", "tonumber", "tostring", "type",
        "unpack", "xpcall", "string", "table", "math", "coroutine",
    ] {
        if let Ok(v) = g.get::<Value>(name) {
            env.set(name, v)?;
        }
    }
    env.set("_VERSION", g.get::<Value>("_VERSION").unwrap_or(Value::Nil))?;
    // The clock, and nothing else `os` carries: no `getenv`, no `execute`, no
    // `remove`, no `rename`.
    if let Ok(os) = g.get::<Table>("os") {
        let trimmed = lua.create_table()?;
        for k in ["time", "clock", "date", "difftime"] {
            if let Ok(v) = os.get::<Value>(k) {
                trimmed.set(k, v)?;
            }
        }
        env.set("os", trimmed)?;
    }
    Ok(())
}

fn print_fn(lua: &Lua, shared: &Rc<Shared>, from: &str) -> mlua::Result<Function> {
    let shared = shared.clone();
    let from = from.to_string();
    lua.create_function(move |_, args: Variadic<Value>| {
        shared.log.borrow_mut().push(ExtLog {
            level: ExtLevel::Info,
            msg: join_values(&args),
            from: from.clone(),
        });
        Ok(())
    })
}

fn join_values(args: &[Value]) -> String {
    args.iter().map(describe).collect::<Vec<_>>().join("\t")
}

/// What `print` shows for a value. Tables print their contents one level deep —
/// `table: 0x…` is never the answer somebody printing a table wanted.
fn describe(v: &Value) -> String {
    match v {
        Value::Nil => "nil".into(),
        Value::Boolean(b) => b.to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Number(n) => {
            if n.fract() == 0.0 && n.abs() < 1e15 {
                format!("{n:.0}")
            } else {
                n.to_string()
            }
        }
        Value::String(s) => s.to_string_lossy().to_string(),
        Value::Table(t) => {
            let mut parts: Vec<String> = Vec::new();
            for pair in t.clone().pairs::<Value, Value>().flatten().take(24) {
                let (k, v) = pair;
                parts.push(match k {
                    Value::String(s) => format!("{} = {}", s.to_string_lossy(), shallow(&v)),
                    other => shallow(&other) + " = " + &shallow(&v),
                });
            }
            format!("{{ {} }}", parts.join(", "))
        }
        Value::Function(_) => "function".into(),
        other => format!("{other:?}"),
    }
}

fn shallow(v: &Value) -> String {
    match v {
        Value::Table(_) => "{…}".into(),
        other => describe(other),
    }
}

// ---------------------------------------------------------------------------
// ed
// ---------------------------------------------------------------------------

fn ed_table(lua: &Lua, shared: &Rc<Shared>, pkg: usize, state: &PkgState) -> mlua::Result<Table> {
    let t = lua.create_table()?;

    // ---- the package's own identity, so a message can name itself ---------
    {
        let p = lua.create_table()?;
        p.set("id", state.id.clone())?;
        p.set("name", state.name.clone())?;
        p.set("version", state.version.clone())?;
        p.set("root", state.root.display().to_string())?;
        let root = state.root.clone();
        p.set(
            "path",
            lua.create_function(move |_, rel: String| {
                Ok(safe_join(&root, &rel).map(|p| p.display().to_string()))
            })?,
        )?;
        t.set("package", p)?;
    }

    // ---- logging -----------------------------------------------------------
    for (name, level) in
        [("log", ExtLevel::Info), ("warn", ExtLevel::Warn), ("error", ExtLevel::Error)]
    {
        let shared = shared.clone();
        let from = state.name.clone();
        t.set(
            name,
            lua.create_function(move |_, args: Variadic<Value>| {
                shared.log.borrow_mut().push(ExtLog {
                    level,
                    msg: join_values(&args),
                    from: from.clone(),
                });
                Ok(())
            })?,
        )?;
    }

    // ---- registration ------------------------------------------------------
    {
        let shared = shared.clone();
        t.set(
            "window",
            lua.create_function(move |lua, (title, cb): (String, Function)| {
                let id = shared.alloc_id();
                let key = lua.create_registry_value(cb)?;
                shared.pending.borrow_mut().push(Registration::Window {
                    pkg,
                    id,
                    title: title.clone(),
                    cb: key,
                    open: false,
                });
                shared.open_state.borrow_mut().insert(id, false);
                panel_handle(lua, &shared, id, true)
            })?,
        )?;
    }
    {
        let shared = shared.clone();
        t.set(
            "overlay",
            lua.create_function(move |lua, (name, cb): (String, Function)| {
                let id = shared.alloc_id();
                let key = lua.create_registry_value(cb)?;
                shared.pending.borrow_mut().push(Registration::Overlay {
                    pkg,
                    id,
                    name: name.clone(),
                    cb: key,
                    // An overlay is on as soon as it is registered: an extension
                    // that draws a region marker means it to be visible, and a
                    // panel you have to find a switch for is a panel nobody
                    // finds.
                    open: true,
                });
                shared.open_state.borrow_mut().insert(id, true);
                panel_handle(lua, &shared, id, false)
            })?,
        )?;
    }
    {
        let shared = shared.clone();
        t.set(
            "menu",
            lua.create_function(move |lua, (path, cb): (String, Function)| {
                if path.trim().is_empty() {
                    return Err(mlua::Error::runtime(
                        "ed.menu needs a path like \"My Tool/Settings…\"",
                    ));
                }
                let key = lua.create_registry_value(cb)?;
                shared.pending.borrow_mut().push(Registration::Menu { pkg, path, cb: key });
                Ok(())
            })?,
        )?;
    }
    {
        let shared = shared.clone();
        t.set(
            "shortcut",
            lua.create_function(move |lua, (keys, cb): (String, Function)| {
                let norm = normalise_shortcut(&keys).ok_or_else(|| {
                    mlua::Error::runtime(format!(
                        "`{keys}` is not a shortcut — write it like \"Ctrl+L\" or \
                         \"Ctrl+Shift+F5\""
                    ))
                })?;
                let key = lua.create_registry_value(cb)?;
                shared
                    .pending
                    .borrow_mut()
                    .push(Registration::Shortcut { pkg, keys: norm, cb: key });
                Ok(())
            })?,
        )?;
    }
    for kind in HookKind::ALL {
        let shared = shared.clone();
        let kind = *kind;
        t.set(
            kind.lua_name(),
            lua.create_function(move |lua, cb: Function| {
                let key = lua.create_registry_value(cb)?;
                shared.pending.borrow_mut().push(Registration::Hook { pkg, kind, cb: key });
                Ok(())
            })?,
        )?;
    }

    // ---- the editor's state, read-only ------------------------------------
    {
        let shared = shared.clone();
        t.set(
            "project",
            lua.create_function(move |lua, ()| {
                let s = shared.snap.borrow();
                let t = lua.create_table()?;
                t.set("root", s.project_root.display().to_string())?;
                t.set("name", s.project_name.clone())?;
                t.set("scene", s.scene.clone())?;
                t.set("engineVersion", env!("CARGO_PKG_VERSION"))?;
                Ok(t)
            })?,
        )?;
    }
    {
        let shared = shared.clone();
        t.set(
            "camera",
            lua.create_function(move |lua, ()| {
                let s = shared.snap.borrow();
                let t = lua.create_table()?;
                t.set("pos", xyz(lua, s.cam_pos)?)?;
                t.set(
                    "forward",
                    xyz(lua, [s.cam_fwd[0] as f64, s.cam_fwd[1] as f64, s.cam_fwd[2] as f64])?,
                )?;
                Ok(t)
            })?,
        )?;
    }
    {
        let shared = shared.clone();
        t.set("playing", lua.create_function(move |_, ()| Ok(shared.snap.borrow().playing))?)?;
    }
    {
        let shared = shared.clone();
        t.set("time", lua.create_function(move |_, ()| Ok(shared.snap.borrow().time))?)?;
    }
    {
        let shared = shared.clone();
        t.set("dt", lua.create_function(move |_, ()| Ok(shared.snap.borrow().dt))?)?;
    }
    {
        let shared = shared.clone();
        t.set(
            "repaint",
            lua.create_function(move |_, ()| {
                shared.repaint.set(true);
                Ok(())
            })?,
        )?;
    }

    // ---- doing things ------------------------------------------------------
    // `ed.undo()` marks the edits that follow as ONE undo step. It takes no
    // label because the editor's history has none — a name here would be a
    // promise the Ctrl+Z that follows could not keep.
    cmd_fn(lua, &t, shared, "undo", |_: ()| ExtCmd::Undo)?;
    cmd_fn(lua, &t, shared, "saveScene", |_: ()| ExtCmd::SaveScene)?;
    cmd_fn(lua, &t, shared, "openScene", ExtCmd::OpenScene)?;
    cmd_fn(lua, &t, shared, "play", |_: ()| ExtCmd::SetPlaying(true))?;
    cmd_fn(lua, &t, shared, "stop", |_: ()| ExtCmd::SetPlaying(false))?;
    {
        let shared = shared.clone();
        t.set(
            "message",
            lua.create_function(move |_, (title, body): (String, String)| {
                shared.cmds.borrow_mut().push(ExtCmd::Message { title, body });
                Ok(())
            })?,
        )?;
    }
    {
        // Opening a URL is the Browser capability: it hands whatever is in the
        // string to the user's session, and the point of declaring it is that
        // somebody installing the package can see that coming.
        let shared = shared.clone();
        let allowed = state.permissions.contains(&Permission::Browser);
        t.set(
            "openUrl",
            lua.create_function(move |_, url: String| {
                if !allowed {
                    return Err(mlua::Error::runtime(
                        "ed.openUrl needs the `Browser` permission — add it to package.ron",
                    ));
                }
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    return Err(mlua::Error::runtime(
                        "ed.openUrl only opens http:// and https:// addresses",
                    ));
                }
                shared.cmds.borrow_mut().push(ExtCmd::OpenUrl(url));
                Ok(())
            })?,
        )?;
    }

    // ---- the three stores --------------------------------------------------
    for (field, kind) in
        [("prefs", StoreKind::User), ("store", StoreKind::Project), ("session", StoreKind::Session)]
    {
        t.set(field, store_table(lua, shared, &state.id, kind)?)?;
    }

    // ---- files -------------------------------------------------------------
    t.set("read", read_fn(lua, shared, state)?)?;
    t.set("write", write_fn(lua, shared, state)?)?;
    t.set("exists", exists_fn(lua, shared, state)?)?;
    t.set("list", list_fn(lua, shared, state)?)?;
    Ok(t)
}

/// A `{ show, hide, toggle, focus, isOpen }` handle over a registered panel.
fn panel_handle(lua: &Lua, shared: &Rc<Shared>, id: u32, is_window: bool) -> mlua::Result<Table> {
    let h = lua.create_table()?;
    h.set("id", id)?;
    for (name, open) in [("show", true), ("hide", false)] {
        let shared = shared.clone();
        h.set(
            name,
            lua.create_function(move |_, _: Value| {
                shared.cmds.borrow_mut().push(if is_window {
                    ExtCmd::WindowOpen(id, open)
                } else {
                    ExtCmd::OverlayOpen(id, open)
                });
                shared.open_state.borrow_mut().insert(id, open);
                Ok(())
            })?,
        )?;
    }
    {
        let shared = shared.clone();
        h.set(
            "toggle",
            lua.create_function(move |_, _: Value| {
                let now = shared.open_state.borrow().get(&id).copied().unwrap_or(false);
                shared.cmds.borrow_mut().push(if is_window {
                    ExtCmd::WindowOpen(id, !now)
                } else {
                    ExtCmd::OverlayOpen(id, !now)
                });
                shared.open_state.borrow_mut().insert(id, !now);
                Ok(())
            })?,
        )?;
    }
    {
        let shared = shared.clone();
        h.set(
            "isOpen",
            lua.create_function(move |_, _: Value| {
                Ok(shared.open_state.borrow().get(&id).copied().unwrap_or(false))
            })?,
        )?;
    }
    if is_window {
        let shared = shared.clone();
        h.set(
            "focus",
            lua.create_function(move |_, _: Value| {
                shared.cmds.borrow_mut().push(ExtCmd::WindowOpen(id, true));
                shared.cmds.borrow_mut().push(ExtCmd::WindowFocus(id));
                shared.open_state.borrow_mut().insert(id, true);
                Ok(())
            })?,
        )?;
    }
    Ok(h)
}

/// Register a function that turns its argument into one queued command.
fn cmd_fn<A, F>(
    lua: &Lua,
    t: &Table,
    shared: &Rc<Shared>,
    name: &str,
    make: F,
) -> mlua::Result<()>
where
    A: mlua::FromLuaMulti + 'static,
    F: Fn(A) -> ExtCmd + 'static,
{
    let shared = shared.clone();
    t.set(
        name,
        lua.create_function(move |_, a: A| {
            shared.cmds.borrow_mut().push(make(a));
            Ok(())
        })?,
    )
}

fn store_table(
    lua: &Lua,
    shared: &Rc<Shared>,
    id: &str,
    kind: StoreKind,
) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    {
        let (shared, id) = (shared.clone(), id.to_string());
        t.set(
            "get",
            lua.create_function(move |lua, (key, default): (String, Option<Value>)| {
                let stores = shared.prefs.borrow();
                match stores.get(&id).and_then(|s| s.get(kind, &key)) {
                    Some(v) => v.to_lua(lua),
                    None => Ok(default.unwrap_or(Value::Nil)),
                }
            })?,
        )?;
    }
    {
        let (shared, id) = (shared.clone(), id.to_string());
        t.set(
            "set",
            lua.create_function(move |_, (key, value): (String, Value)| {
                let stored = super::prefs::Value::from_lua(&value);
                if stored.is_none() && !matches!(value, Value::Nil) {
                    return Err(mlua::Error::runtime(
                        "a store holds strings, numbers and booleans — put anything structured \
                         through json.encode first",
                    ));
                }
                shared.prefs.borrow_mut().entry(&id).set(kind, key, stored);
                Ok(())
            })?,
        )?;
    }
    {
        let (shared, id) = (shared.clone(), id.to_string());
        t.set(
            "keys",
            lua.create_function(move |_, ()| {
                Ok(shared.prefs.borrow().get(&id).map(|s| s.keys(kind)).unwrap_or_default())
            })?,
        )?;
    }
    Ok(t)
}

// ---------------------------------------------------------------------------
// files
// ---------------------------------------------------------------------------

/// Which paths a package may touch, and why.
///
/// Reading its own folder always: a package reading a file it shipped needs no
/// permission. Anywhere else in the project only with [`Permission::Files`].
/// Outside the project, never — an extension is a tool for a project, and a
/// package that wants somebody's home directory has left what this API is for.
#[derive(Clone)]
struct FileScope {
    root: PathBuf,
    shared: Rc<Shared>,
    allow_files: bool,
}

impl FileScope {
    fn of(shared: &Rc<Shared>, state: &PkgState) -> Self {
        Self {
            root: state.root.clone(),
            shared: shared.clone(),
            allow_files: state.permissions.contains(&Permission::Files),
        }
    }

    /// Resolve a package-relative path for READING.
    fn read_path(&self, rel: &str) -> Result<PathBuf, String> {
        if let Some(p) = safe_join(&self.root, rel)
            && p.exists()
        {
            return Ok(p);
        }
        if !self.allow_files {
            return Err(format!(
                "`{rel}` is not in this package, and reading elsewhere needs the `Files` \
                 permission — add it to package.ron"
            ));
        }
        let project = self.shared.snap.borrow().project_root.clone();
        safe_join(&project, rel)
            .ok_or_else(|| format!("`{rel}` reaches outside the project, which is never allowed"))
    }

    /// Resolve a project-relative path for WRITING. Writing is `Files`, full
    /// stop — including into the package's own folder. A package that edits
    /// itself is a package that survives being reinstalled in a shape nobody
    /// chose.
    fn write_path(&self, rel: &str) -> Result<PathBuf, String> {
        if !self.allow_files {
            return Err("ed.write needs the `Files` permission — add it to package.ron".into());
        }
        let project = self.shared.snap.borrow().project_root.clone();
        safe_join(&project, rel)
            .ok_or_else(|| format!("`{rel}` reaches outside the project, which is never allowed"))
    }
}

/// Join `rel` onto `root`, refusing anything that climbs out or is absolute.
pub(crate) fn safe_join(root: &std::path::Path, rel: &str) -> Option<PathBuf> {
    let p = std::path::Path::new(rel);
    if p.is_absolute() {
        return None;
    }
    for c in p.components() {
        if matches!(
            c,
            std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        ) {
            return None;
        }
    }
    Some(root.join(p))
}

fn read_fn(lua: &Lua, shared: &Rc<Shared>, state: &PkgState) -> mlua::Result<Function> {
    let scope = FileScope::of(shared, state);
    lua.create_function(move |_, rel: String| {
        let path = scope.read_path(&rel).map_err(mlua::Error::runtime)?;
        Ok(std::fs::read_to_string(path).ok())
    })
}

fn exists_fn(lua: &Lua, shared: &Rc<Shared>, state: &PkgState) -> mlua::Result<Function> {
    let scope = FileScope::of(shared, state);
    lua.create_function(move |_, rel: String| {
        Ok(scope.read_path(&rel).map(|p| p.exists()).unwrap_or(false))
    })
}

fn list_fn(lua: &Lua, shared: &Rc<Shared>, state: &PkgState) -> mlua::Result<Function> {
    let scope = FileScope::of(shared, state);
    lua.create_function(move |_, rel: String| {
        let dir = scope.read_path(&rel).map_err(mlua::Error::runtime)?;
        let mut out: Vec<String> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        out.sort();
        Ok(out)
    })
}

fn write_fn(lua: &Lua, shared: &Rc<Shared>, state: &PkgState) -> mlua::Result<Function> {
    let scope = FileScope::of(shared, state);
    lua.create_function(move |_, (rel, text): (String, String)| {
        let path = scope.write_path(&rel).map_err(mlua::Error::runtime)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(mlua::Error::runtime)?;
        }
        std::fs::write(&path, text).map_err(mlua::Error::runtime)?;
        Ok(())
    })
}

/// `require("sub/file")` — load another Lua file from this package, once, into
/// the same environment. Returns whatever it returned.
fn require_fn(
    lua: &Lua,
    shared: &Rc<Shared>,
    env: &Table,
    root: PathBuf,
    id: String,
) -> mlua::Result<Function> {
    let env = env.clone();
    let shared = shared.clone();
    let cache = lua.create_table()?;
    lua.create_function(move |lua, name: String| {
        if let Ok(v) = cache.get::<Value>(name.clone())
            && !matches!(v, Value::Nil)
        {
            return Ok(v);
        }
        let rel = if name.ends_with(".lua") { name.clone() } else { format!("{name}.lua") };
        let path = safe_join(&root, &rel).ok_or_else(|| {
            mlua::Error::runtime(format!(
                "require(\"{name}\") reaches outside the package — a package can only require \
                 its own files"
            ))
        })?;
        let text = std::fs::read_to_string(&path).map_err(|_| {
            mlua::Error::runtime(format!("require(\"{name}\"): no such file in this package"))
        })?;
        let v: Value = lua
            .load(&text)
            .set_name(format!("@{id}/{rel}"))
            .set_environment(env.clone())
            .eval()?;
        cache.set(name, v.clone())?;
        let _ = &shared;
        Ok(v)
    })
}

// ---------------------------------------------------------------------------
// scene
// ---------------------------------------------------------------------------

fn scene_table(lua: &Lua, shared: &Rc<Shared>) -> mlua::Result<Table> {
    let t = lua.create_table()?;

    {
        let shared = shared.clone();
        t.set(
            "all",
            lua.create_function(move |_, ()| {
                Ok(shared.scene.borrow().nodes.iter().map(|n| n.id).collect::<Vec<_>>())
            })?,
        )?;
    }
    {
        let shared = shared.clone();
        t.set("roots", lua.create_function(move |_, ()| Ok(shared.scene.borrow().roots.clone()))?)?;
    }
    {
        let shared = shared.clone();
        t.set(
            "find",
            lua.create_function(move |_, name: String| {
                Ok(shared.scene.borrow().find_all(&name).first().copied())
            })?,
        )?;
    }
    {
        let shared = shared.clone();
        t.set(
            "findAll",
            lua.create_function(move |_, name: String| Ok(shared.scene.borrow().find_all(&name)))?,
        )?;
    }
    {
        let shared = shared.clone();
        t.set(
            "children",
            lua.create_function(move |_, id: u32| {
                Ok(shared.scene.borrow().get(id).map(|n| n.children.clone()).unwrap_or_default())
            })?,
        )?;
    }
    // One `info` rather than fifteen getters: an extension almost always wants
    // several fields of the same node, and fifteen calls is fifteen borrows.
    {
        let shared = shared.clone();
        t.set(
            "info",
            lua.create_function(move |lua, id: u32| {
                let scene = shared.scene.borrow();
                let Some(n) = scene.get(id) else { return Ok(Value::Nil) };
                let t = lua.create_table()?;
                t.set("id", n.id)?;
                t.set("name", n.name.clone())?;
                t.set("kind", n.kind)?;
                t.set("parent", n.parent)?;
                t.set("children", n.children.clone())?;
                t.set("pos", xyz(lua, n.pos)?)?;
                t.set("worldPos", xyz(lua, n.world_pos)?)?;
                t.set("scale", xyz(lua, [n.scale[0] as f64, n.scale[1] as f64, n.scale[2] as f64])?)?;
                let rot = lua.create_table()?;
                rot.set("x", n.rot[0])?;
                rot.set("y", n.rot[1])?;
                rot.set("z", n.rot[2])?;
                rot.set("w", n.rot[3])?;
                t.set("rot", rot)?;
                t.set("radius", n.radius)?;
                t.set("tags", n.tags.clone())?;
                t.set("layer", n.layer.clone())?;
                t.set("visible", n.visible)?;
                t.set("scripts", n.scripts.clone())?;
                t.set("asset", n.asset.clone())?;
                Ok(Value::Table(t))
            })?,
        )?;
    }
    {
        let shared = shared.clone();
        t.set(
            "bounds",
            lua.create_function(move |lua, id: u32| {
                let scene = shared.scene.borrow();
                let Some((min, max)) = scene.aabb(id) else { return Ok(Value::Nil) };
                let t = lua.create_table()?;
                t.set("min", xyz(lua, min)?)?;
                t.set("max", xyz(lua, max)?)?;
                t.set("center", xyz(lua, scene.get(id).map(|n| n.world_pos).unwrap_or_default())?)?;
                t.set("radius", scene.get(id).and_then(|n| n.radius))?;
                Ok(Value::Table(t))
            })?,
        )?;
    }

    {
        let shared = shared.clone();
        t.set(
            "raycast",
            lua.create_function(move |lua, (origin, dir, max): (Value, Value, Option<f64>)| {
                let o = super::handles::vec3_of(&origin)?;
                let d = super::handles::vec3_of(&dir)?;
                let scene = shared.scene.borrow();
                let Some(hit) = scene.raycast(o, d, max.unwrap_or(1.0e6)) else {
                    return Ok(Value::Nil);
                };
                let t = lua.create_table()?;
                t.set("node", hit.node)?;
                t.set("distance", hit.t)?;
                t.set("point", xyz(lua, hit.point)?)?;
                t.set(
                    "normal",
                    xyz(lua, [hit.normal[0] as f64, hit.normal[1] as f64, hit.normal[2] as f64])?,
                )?;
                Ok(Value::Table(t))
            })?,
        )?;
    }

    // ---- edits -------------------------------------------------------------
    {
        let shared = shared.clone();
        t.set(
            "setName",
            lua.create_function(move |_, (id, name): (u32, String)| {
                shared.cmds.borrow_mut().push(ExtCmd::NodeSetName(id, name));
                Ok(())
            })?,
        )?;
    }
    {
        let shared = shared.clone();
        t.set(
            "setPos",
            lua.create_function(move |_, (id, v): (u32, Value)| {
                shared
                    .cmds
                    .borrow_mut()
                    .push(ExtCmd::NodeSetPos(id, super::handles::vec3_of(&v)?));
                Ok(())
            })?,
        )?;
    }
    {
        let shared = shared.clone();
        t.set(
            "setScale",
            lua.create_function(move |_, (id, v): (u32, Value)| {
                let s = super::handles::vec3_of(&v)?;
                shared
                    .cmds
                    .borrow_mut()
                    .push(ExtCmd::NodeSetScale(id, [s[0] as f32, s[1] as f32, s[2] as f32]));
                Ok(())
            })?,
        )?;
    }
    {
        let shared = shared.clone();
        t.set(
            "setRot",
            lua.create_function(move |_, (id, x, y, z, w): (u32, f32, f32, f32, f32)| {
                shared.cmds.borrow_mut().push(ExtCmd::NodeSetRot(id, [x, y, z, w]));
                Ok(())
            })?,
        )?;
    }
    {
        let shared = shared.clone();
        t.set(
            "setVisible",
            lua.create_function(move |_, (id, on): (u32, bool)| {
                shared.cmds.borrow_mut().push(ExtCmd::NodeSetVisible(id, on));
                Ok(())
            })?,
        )?;
    }
    {
        let shared = shared.clone();
        t.set(
            "create",
            lua.create_function(move |_, (name, parent): (String, Option<u32>)| {
                shared.cmds.borrow_mut().push(ExtCmd::NodeCreate { name, parent });
                Ok(())
            })?,
        )?;
    }
    {
        let shared = shared.clone();
        t.set(
            "destroy",
            lua.create_function(move |_, id: u32| {
                shared.cmds.borrow_mut().push(ExtCmd::NodeDestroy(id));
                Ok(())
            })?,
        )?;
    }
    {
        let shared = shared.clone();
        t.set(
            "spawnPrefab",
            lua.create_function(move |_, (path, at): (String, Option<Value>)| {
                let pos = match at {
                    Some(v) => Some(super::handles::vec3_of(&v)?),
                    None => None,
                };
                shared.cmds.borrow_mut().push(ExtCmd::SpawnPrefab { path, pos });
                Ok(())
            })?,
        )?;
    }
    Ok(t)
}

fn selection_table(lua: &Lua, shared: &Rc<Shared>) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    {
        let shared = shared.clone();
        t.set("get", lua.create_function(move |_, ()| Ok(shared.snap.borrow().selection.clone()))?)?;
    }
    {
        let shared = shared.clone();
        t.set(
            "active",
            lua.create_function(move |_, ()| Ok(shared.snap.borrow().selection.first().copied()))?,
        )?;
    }
    {
        let shared = shared.clone();
        t.set(
            "set",
            lua.create_function(move |_, ids: Vec<u32>| {
                shared.cmds.borrow_mut().push(ExtCmd::SelectionSet(ids));
                Ok(())
            })?,
        )?;
    }
    {
        let shared = shared.clone();
        t.set(
            "clear",
            lua.create_function(move |_, ()| {
                shared.cmds.borrow_mut().push(ExtCmd::SelectionSet(Vec::new()));
                Ok(())
            })?,
        )?;
    }
    Ok(t)
}

// ---------------------------------------------------------------------------
// http / sys / json / vectors
// ---------------------------------------------------------------------------

fn http_table(lua: &Lua, shared: &Rc<Shared>) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    for method in ["get", "delete"] {
        let shared = shared.clone();
        t.set(
            method,
            lua.create_function(move |lua, (url, opts, cb): (String, Value, Option<Function>)| {
                let (opts, cb) = split_opts_cb(opts, cb)?;
                let (headers, timeout) = super::http::read_opts(opts);
                let key = lua.create_registry_value(cb)?;
                shared
                    .web
                    .borrow_mut()
                    .request(&method.to_uppercase(), url, None, headers, timeout, key)
                    .map_err(mlua::Error::runtime)
            })?,
        )?;
    }
    for method in ["post", "put", "patch"] {
        let shared = shared.clone();
        t.set(
            method,
            lua.create_function(
                move |lua, (url, body, opts, cb): (String, String, Value, Option<Function>)| {
                    let (opts, cb) = split_opts_cb(opts, cb)?;
                    let (headers, timeout) = super::http::read_opts(opts);
                    let key = lua.create_registry_value(cb)?;
                    shared
                        .web
                        .borrow_mut()
                        .request(&method.to_uppercase(), url, Some(body), headers, timeout, key)
                        .map_err(mlua::Error::runtime)
                },
            )?,
        )?;
    }
    {
        let shared = shared.clone();
        t.set(
            "listen",
            lua.create_function(move |lua, (port, cb): (Value, Function)| {
                let port = match port {
                    Value::Integer(i) => i as u16,
                    Value::Number(n) => n as u16,
                    _ => 0,
                };
                let key = lua.create_registry_value(cb)?;
                shared.web.borrow_mut().listen(port, key).map_err(mlua::Error::runtime)
            })?,
        )?;
    }
    {
        let shared = shared.clone();
        t.set(
            "stopListening",
            lua.create_function(move |_, ()| {
                shared.web.borrow_mut().stop_listening();
                Ok(())
            })?,
        )?;
    }
    Ok(t)
}

/// `http.get(url, cb)` and `http.get(url, opts, cb)` are both what people write.
fn split_opts_cb(opts: Value, cb: Option<Function>) -> mlua::Result<(Option<Table>, Function)> {
    match (opts, cb) {
        (Value::Function(f), None) => Ok((None, f)),
        (Value::Table(t), Some(f)) => Ok((Some(t), f)),
        (Value::Nil, Some(f)) => Ok((None, f)),
        _ => Err(mlua::Error::runtime(
            "expected (url, callback) or (url, opts, callback)",
        )),
    }
}

fn sys_table(lua: &Lua, shared: &Rc<Shared>) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    {
        let shared = shared.clone();
        t.set(
            "openUrl",
            lua.create_function(move |_, url: String| {
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    return Err(mlua::Error::runtime(
                        "sys.openUrl only opens http:// and https:// addresses",
                    ));
                }
                shared.cmds.borrow_mut().push(ExtCmd::OpenUrl(url));
                Ok(())
            })?,
        )?;
    }
    t.set("platform", std::env::consts::OS)?;
    Ok(t)
}

fn json_table(lua: &Lua) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set(
        "encode",
        lua.create_function(|_, v: Value| {
            let j = to_json(&v, 0)?;
            serde_json::to_string(&j).map_err(mlua::Error::runtime)
        })?,
    )?;
    t.set(
        "decode",
        lua.create_function(|lua, s: String| match serde_json::from_str::<serde_json::Value>(&s) {
            Ok(v) => from_json(lua, &v),
            Err(e) => Err(mlua::Error::runtime(format!("that is not JSON: {e}"))),
        })?,
    )?;
    Ok(t)
}

/// How deep a table may nest before `json.encode` gives up. A table holding
/// itself is a hang, not an error, without a limit.
const JSON_MAX_DEPTH: usize = 64;

fn to_json(v: &Value, depth: usize) -> mlua::Result<serde_json::Value> {
    if depth > JSON_MAX_DEPTH {
        return Err(mlua::Error::runtime(
            "json.encode: this table nests more than 64 deep — does it contain itself?",
        ));
    }
    Ok(match v {
        Value::Nil => serde_json::Value::Null,
        Value::Boolean(b) => (*b).into(),
        Value::Integer(i) => (*i).into(),
        Value::Number(n) => serde_json::Number::from_f64(*n)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::String(s) => s.to_string_lossy().to_string().into(),
        Value::Table(t) => {
            // A Lua table is both a list and a map; `raw_len > 0` with no other
            // keys is the only shape that is unambiguously an array.
            let len = t.raw_len();
            let keys = t.clone().pairs::<Value, Value>().count();
            if len > 0 && keys == len {
                let mut arr = Vec::with_capacity(len);
                for i in 1..=len {
                    arr.push(to_json(&t.get::<Value>(i)?, depth + 1)?);
                }
                serde_json::Value::Array(arr)
            } else {
                let mut map = serde_json::Map::new();
                for pair in t.clone().pairs::<Value, Value>() {
                    let (k, v) = pair?;
                    let key = match k {
                        Value::String(s) => s.to_string_lossy().to_string(),
                        Value::Integer(i) => i.to_string(),
                        Value::Number(n) => n.to_string(),
                        _ => continue,
                    };
                    map.insert(key, to_json(&v, depth + 1)?);
                }
                serde_json::Value::Object(map)
            }
        }
        _ => serde_json::Value::Null,
    })
}

fn from_json(lua: &Lua, v: &serde_json::Value) -> mlua::Result<Value> {
    Ok(match v {
        serde_json::Value::Null => Value::Nil,
        serde_json::Value::Bool(b) => Value::Boolean(*b),
        serde_json::Value::Number(n) => Value::Number(n.as_f64().unwrap_or(0.0)),
        serde_json::Value::String(s) => Value::String(lua.create_string(s)?),
        serde_json::Value::Array(a) => {
            let t = lua.create_table()?;
            for (i, item) in a.iter().enumerate() {
                t.set(i + 1, from_json(lua, item)?)?;
            }
            Value::Table(t)
        }
        serde_json::Value::Object(o) => {
            let t = lua.create_table()?;
            for (k, item) in o {
                t.set(k.clone(), from_json(lua, item)?)?;
            }
            Value::Table(t)
        }
    })
}

fn xyz(lua: &Lua, v: [f64; 3]) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("x", v[0])?;
    t.set("y", v[1])?;
    t.set("z", v[2])?;
    Ok(t)
}

fn vec3_ctor(lua: &Lua) -> mlua::Result<Function> {
    lua.create_function(|lua, (x, y, z): (Option<f64>, Option<f64>, Option<f64>)| {
        xyz(lua, [x.unwrap_or(0.0), y.unwrap_or(0.0), z.unwrap_or(0.0)])
    })
}

fn vec2_ctor(lua: &Lua) -> mlua::Result<Function> {
    lua.create_function(|lua, (x, y): (Option<f64>, Option<f64>)| {
        let t = lua.create_table()?;
        t.set("x", x.unwrap_or(0.0))?;
        t.set("y", y.unwrap_or(0.0))?;
        Ok(t)
    })
}

/// Put a shortcut string into one spelling, so `"ctrl+L"`, `"Control+l"` and
/// `"Ctrl + L"` are the same shortcut and two packages claiming it can be seen
/// to be doing so.
pub(crate) fn normalise_shortcut(s: &str) -> Option<String> {
    let mut ctrl = false;
    let mut shift = false;
    let mut alt = false;
    let mut key: Option<String> = None;
    for part in s.split('+') {
        let p = part.trim().to_ascii_lowercase();
        match p.as_str() {
            "" => continue,
            "ctrl" | "control" | "cmd" | "command" | "super" => ctrl = true,
            "shift" => shift = true,
            "alt" | "option" => alt = true,
            other => {
                if key.is_some() {
                    return None; // two keys is not a shortcut
                }
                key = Some(other.to_string());
            }
        }
    }
    let key = key?;
    let mut out = String::new();
    if ctrl {
        out.push_str("Ctrl+");
    }
    if shift {
        out.push_str("Shift+");
    }
    if alt {
        out.push_str("Alt+");
    }
    // A single letter or digit reads best capitalised; a named key keeps its
    // own capitalisation (`F5`, `Escape`).
    if key.chars().count() == 1 {
        out.push_str(&key.to_uppercase());
    } else {
        let mut c = key.chars();
        let first = c.next()?.to_uppercase().to_string();
        out.push_str(&first);
        out.push_str(c.as_str());
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcuts_normalise_to_one_spelling() {
        for s in ["ctrl+l", "Control + L", "CTRL+L", "cmd+l"] {
            assert_eq!(normalise_shortcut(s).as_deref(), Some("Ctrl+L"), "{s}");
        }
        assert_eq!(normalise_shortcut("ctrl+shift+alt+f5").as_deref(), Some("Ctrl+Shift+Alt+F5"));
        assert_eq!(normalise_shortcut("escape").as_deref(), Some("Escape"));
        assert_eq!(normalise_shortcut("F1").as_deref(), Some("F1"));
    }

    #[test]
    fn a_shortcut_with_no_key_or_two_keys_is_refused() {
        assert!(normalise_shortcut("ctrl").is_none());
        assert!(normalise_shortcut("ctrl+a+b").is_none());
        assert!(normalise_shortcut("").is_none());
    }

    #[test]
    fn safe_join_refuses_to_climb_out() {
        let root = std::path::Path::new("/pkg");
        assert!(safe_join(root, "../etc/passwd").is_none());
        assert!(safe_join(root, "/etc/passwd").is_none());
        assert!(safe_join(root, "a/../../b").is_none());
        assert_eq!(safe_join(root, "sub/file.lua"), Some(PathBuf::from("/pkg/sub/file.lua")));
    }
}
