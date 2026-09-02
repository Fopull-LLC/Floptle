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
    env.set("nav", nav_table(lua, shared)?)?;
    env.set("mesh", mesh_table_api(lua, shared)?)?;
    env.set("tilemap", tilemap_table(lua, shared)?)?;

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

/// `nav` — the scene's baked navmesh, read-only.
///
/// The same calls a running script has, minus everything that moves: no
/// `nav.agent`, no `nav.obstacle`, no opening and closing links. An extension
/// looks at the level the author has open; it does not drive anything around
/// it, and there is no simulation running for those to act on.
///
/// No permission gates it. It says less about the project than `scene` already
/// does, and asking for a permission to read a shape the same package can
/// measure itself out of `scene.nodes()` would be a warning that means nothing.
fn nav_table(lua: &Lua, shared: &Rc<Shared>) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    floptle_script::nav_api::install_mesh_reads(lua, &t, shared.nav.clone());
    Ok(t)
}

/// `tilemap` — a 2D level's grid, read-only (`floptle/0155`).
///
/// The reading half of the tilemap surface a game script already has
/// (`node:tilemap()`), for the same reason `nav` is "the reading half of the
/// scripting `nav` API and nothing that moves": no `set`, no `resize`, no
/// `fill`. An edit to somebody's level made by a panel is a different
/// conversation, and `scene.set` already exists for a package that genuinely
/// means to write.
///
/// No permission gates it, for the same reason `nav` needs none: it answers
/// questions about a grid the same package can already see the bounding box
/// of via `scene.bounds`.
fn tilemap_table(lua: &Lua, shared: &Rc<Shared>) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    let s = shared.clone();
    t.set(
        "of",
        lua.create_function(move |lua, id: u32| {
            if s.scene.borrow().tilemap(id).is_none() {
                return Ok(Value::Nil);
            }
            Ok(Value::Table(new_tilemap_handle(lua, id, s.clone())?))
        })?,
    )?;
    Ok(t)
}

fn new_tilemap_handle(lua: &Lua, id: u32, shared: Rc<Shared>) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.raw_set("__id", id)?;

    // tm:size() -> cols, rows
    {
        let shared = shared.clone();
        t.set(
            "size",
            lua.create_function(move |_, this: Table| {
                let id: u32 = this.raw_get("__id")?;
                let s = shared.scene.borrow();
                Ok(s.tilemap(id).map(|g| (g.cols, g.rows)).unwrap_or((0, 0)))
            })?,
        )?;
    }
    // tm:tileSize() -> the world edge length of one square.
    {
        let shared = shared.clone();
        t.set(
            "tileSize",
            lua.create_function(move |_, this: Table| {
                let id: u32 = this.raw_get("__id")?;
                let s = shared.scene.borrow();
                Ok(s.tilemap(id).map(|g| g.tile).unwrap_or(0.0))
            })?,
        )?;
    }
    // tm:tileset() -> the project-relative .tileset.ron, or nil.
    {
        let shared = shared.clone();
        t.set(
            "tileset",
            lua.create_function(move |_, this: Table| {
                let id: u32 = this.raw_get("__id")?;
                let s = shared.scene.borrow();
                Ok(s.tilemap(id)
                    .map(|g| g.tileset.clone())
                    .filter(|p| !p.is_empty()))
            })?,
        )?;
    }
    // tm:get(x, y) -> cell index, or nil outside the grid / on an empty
    // square. The orientation is stripped, same as the game API's `tm:get`.
    {
        let shared = shared.clone();
        t.set(
            "get",
            lua.create_function(move |_, (this, x, y): (Table, u32, u32)| {
                let id: u32 = this.raw_get("__id")?;
                let s = shared.scene.borrow();
                match tile_packed_at(&s, id, x, y) {
                    Some(p) if p != floptle_core::EMPTY_TILE => {
                        Ok(Value::Integer(floptle_core::tile_index(p) as mlua::Integer))
                    }
                    _ => Ok(Value::Nil),
                }
            })?,
        )?;
    }
    // tm:at(x, y) -> cell, rotDeg, flipX — the full answer, same shape as the
    // game API's `tm:at`.
    {
        let shared = shared.clone();
        t.set(
            "at",
            lua.create_function(move |_, (this, x, y): (Table, u32, u32)| {
                let id: u32 = this.raw_get("__id")?;
                let s = shared.scene.borrow();
                match tile_packed_at(&s, id, x, y) {
                    Some(p) if p != floptle_core::EMPTY_TILE => {
                        let xf = floptle_core::tile_xform(p);
                        Ok((
                            Value::Integer(floptle_core::tile_index(p) as mlua::Integer),
                            Value::Integer(xf.rot as mlua::Integer * 90),
                            Value::Boolean(xf.flip_x),
                        ))
                    }
                    _ => Ok((Value::Nil, Value::Nil, Value::Nil)),
                }
            })?,
        )?;
    }
    // tm:solid(x, y) -> whether the tileset says that square collides.
    // **The one that matters** — without it the rest is decoration, because
    // solidity is what makes the grid a level rather than a picture.
    {
        let shared = shared.clone();
        t.set(
            "solid",
            lua.create_function(move |_, (this, x, y): (Table, u32, u32)| {
                let id: u32 = this.raw_get("__id")?;
                let s = shared.scene.borrow();
                let Some(grid) = s.tilemap(id) else { return Ok(false) };
                let Some(p) = tile_packed_at(&s, id, x, y) else { return Ok(false) };
                let Some(set) = s.tileset_of(grid) else { return Ok(false) };
                if set.is_empty_square(p) {
                    return Ok(false);
                }
                Ok(set.collision(floptle_core::tile_index(p)).is_solid())
            })?,
        )?;
    }
    // tm:tags(x, y) -> the tileset's tags for that square, as a list.
    {
        let shared = shared.clone();
        t.set(
            "tags",
            lua.create_function(move |lua, (this, x, y): (Table, u32, u32)| {
                let id: u32 = this.raw_get("__id")?;
                let s = shared.scene.borrow();
                let out = lua.create_table()?;
                let Some(grid) = s.tilemap(id) else { return Ok(out) };
                let (Some(p), Some(set)) = (tile_packed_at(&s, id, x, y), s.tileset_of(grid))
                else {
                    return Ok(out);
                };
                if set.is_empty_square(p) {
                    return Ok(out);
                }
                for (i, tag) in set.tags(floptle_core::tile_index(p)).iter().enumerate() {
                    out.raw_set(i + 1, tag.clone())?;
                }
                Ok(out)
            })?,
        )?;
    }
    // tm:hasTag(x, y, tag) -> the common case of the above without a table
    // allocation per square.
    {
        let shared = shared.clone();
        t.set(
            "hasTag",
            lua.create_function(move |_, (this, x, y, tag): (Table, u32, u32, String)| {
                let id: u32 = this.raw_get("__id")?;
                let s = shared.scene.borrow();
                let Some(grid) = s.tilemap(id) else { return Ok(false) };
                let (Some(p), Some(set)) = (tile_packed_at(&s, id, x, y), s.tileset_of(grid))
                else {
                    return Ok(false);
                };
                if set.is_empty_square(p) {
                    return Ok(false);
                }
                Ok(set.tags(floptle_core::tile_index(p)).contains(&tag))
            })?,
        )?;
    }
    // tm:cellAt(worldPoint) -> x, y — or nil off the map. Through the node's
    // own world transform, so a moved, turned or scaled tilemap needs no
    // arithmetic at the caller's end.
    {
        let shared = shared.clone();
        t.set(
            "cellAt",
            lua.create_function(move |_, (this, p): (Table, Value)| {
                let id: u32 = this.raw_get("__id")?;
                let p = super::handles::vec3_of(&p)?;
                let s = shared.scene.borrow();
                match tile_cell_of_world(&s, id, floptle_core::math::DVec3::from(p)) {
                    Some((x, y)) => Ok((
                        Value::Integer(x as mlua::Integer),
                        Value::Integer(y as mlua::Integer),
                    )),
                    None => Ok((Value::Nil, Value::Nil)),
                }
            })?,
        )?;
    }
    // tm:worldAt(x, y) -> the world position of that square's centre.
    {
        let shared = shared.clone();
        t.set(
            "worldAt",
            lua.create_function(move |lua, (this, x, y): (Table, i64, i64)| {
                let id: u32 = this.raw_get("__id")?;
                let s = shared.scene.borrow();
                match tile_world_of_cell(&s, id, x, y) {
                    Some(p) => xyz(lua, [p.x, p.y, p.z]).map(Value::Table),
                    None => Ok(Value::Nil),
                }
            })?,
        )?;
    }
    Ok(t)
}

/// One square as the mirror holds it, or `None` outside the grid.
///
/// Outside reads as absent rather than wrapping, same rule the game API's
/// `packed_at` follows: a loop that runs one past the edge is a bug in the
/// caller's bounds, and wrapping would hide it.
fn tile_packed_at(s: &super::scene_mirror::SceneMirror, id: u32, x: u32, y: u32) -> Option<u32> {
    let m = s.tilemap(id)?;
    (x < m.cols && y < m.rows).then(|| m.data.get((y * m.cols + x) as usize).copied())?
}

/// Which square of a tilemap a world point falls in — the package-API twin of
/// `floptle_script::api`'s `cell_of_world`, reading the mirror's own
/// (per-frame, non-cached) world transform instead of walking the game
/// mirror's parent chain.
fn tile_cell_of_world(
    s: &super::scene_mirror::SceneMirror,
    id: u32,
    p: floptle_core::math::DVec3,
) -> Option<(u32, u32)> {
    let m = s.tilemap(id)?;
    let node = s.get(id)?;
    if m.tile <= 0.0 || m.cols == 0 || m.rows == 0 {
        return None;
    }
    let rot = floptle_core::math::Quat::from_array(node.world_rot);
    let rot = if rot.is_normalized() { rot } else { floptle_core::math::Quat::IDENTITY };
    let origin = floptle_core::math::DVec3::from(node.world_pos);
    let rel = rot.inverse() * (p - origin).as_vec3();
    let s3 = floptle_core::math::Vec3::from(node.world_scale);
    if s3.x.abs() < 1e-9 || s3.y.abs() < 1e-9 {
        return None;
    }
    let local = floptle_core::math::Vec2::new(rel.x / s3.x, rel.y / s3.y);
    let (w, h) = (m.cols as f32 * m.tile * 0.5, m.rows as f32 * m.tile * 0.5);
    let fx = (local.x + w) / m.tile;
    // Row 0 is the top, so the row index counts DOWN from +h — matches the
    // mesh builder and the game API exactly.
    let fy = (h - local.y) / m.tile;
    if fx < 0.0 || fy < 0.0 {
        return None;
    }
    let (x, y) = (fx.floor() as i64, fy.floor() as i64);
    (x >= 0 && y >= 0 && x < m.cols as i64 && y < m.rows as i64).then_some((x as u32, y as u32))
}

/// The world position of a square's centre — the inverse of
/// [`tile_cell_of_world`].
fn tile_world_of_cell(
    s: &super::scene_mirror::SceneMirror,
    id: u32,
    x: i64,
    y: i64,
) -> Option<floptle_core::math::DVec3> {
    let m = s.tilemap(id)?;
    let node = s.get(id)?;
    if x < 0 || y < 0 || x >= m.cols as i64 || y >= m.rows as i64 {
        return None;
    }
    let (w, h) = (m.cols as f32 * m.tile * 0.5, m.rows as f32 * m.tile * 0.5);
    let local = floptle_core::math::Vec3::new(
        x as f32 * m.tile - w + m.tile * 0.5,
        h - (y as f32 * m.tile + m.tile * 0.5),
        0.0,
    );
    let rot = floptle_core::math::Quat::from_array(node.world_rot);
    let rot = if rot.is_normalized() { rot } else { floptle_core::math::Quat::IDENTITY };
    let s3 = floptle_core::math::Vec3::from(node.world_scale);
    let origin = floptle_core::math::DVec3::from(node.world_pos);
    Some(origin + (rot * (s3 * local)).as_dvec3())
}

/// `mesh` — the triangles behind a node or a model file.
///
/// A callback rather than a return value, for the same reason `http` is one:
/// the first read of a model is a file off disk, and a binding has no route to
/// the editor. The callback runs on a later frame, on the main thread.
///
/// No permission gates it. It reads geometry the same package can already see
/// the bounding box of and can already draw — and a model's triangles are not a
/// secret from a tool the author installed into their own editor. Reading a
/// FILE outside the package still needs `Files`; this reads what is in the
/// scene, by node.
/// How many node documents one `scene.docs` call serves.
///
/// A bound rather than a budget: the read happens in one go, on the frame after
/// the ask, and a level big enough to exceed this is a level where doing it all
/// at once would be a visible hitch. The error names the number and says to
/// batch, which is a loop the caller can write and the engine cannot.
const MAX_DOCS: usize = 4096;

fn mesh_table_api(lua: &Lua, shared: &Rc<Shared>) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    {
        let shared = shared.clone();
        t.set(
            "read",
            lua.create_function(move |lua, (what, cb): (Value, Function)| {
                let source = match &what {
                    Value::Integer(i) => crate::mesh_read::MeshSource::Node(*i as u32),
                    Value::Number(n) => crate::mesh_read::MeshSource::Node(*n as u32),
                    Value::String(s) => {
                        crate::mesh_read::MeshSource::Asset(s.to_string_lossy().to_string())
                    }
                    _ => {
                        return Err(mlua::Error::runtime(
                            "mesh.read takes a node id or an asset path, then a callback",
                        ))
                    }
                };
                let key = lua.create_registry_value(cb)?;
                shared
                    .mesh_reqs
                    .borrow_mut()
                    .push(super::MeshReq { source, cb: key });
                Ok(())
            })?,
        )?;
    }
    t.set("maxTriangles", crate::mesh_read::MAX_TRIANGLES)?;
    Ok(t)
}

/// The Lua that is safe to hand a package: values and pure functions, no I/O,
/// no dynamic loading, no reaching into the host.
fn base_globals(lua: &Lua, env: &Table) -> mlua::Result<()> {
    let g = lua.globals();
    for name in [
        "assert", "error", "ipairs", "next", "pairs", "pcall", "rawequal", "rawget", "rawlen",
        "rawset", "select", "setmetatable", "getmetatable", "tonumber", "tostring", "type",
        "unpack", "xpcall", "string", "table", "math", "coroutine",
        // LuaJIT's bit library. Pure integer arithmetic with no route to the
        // host — and the difference between a package being able to compute a
        // hash and not. Hashing is not exotic: a tool that uploads a scene
        // wants to know whether it has changed, and one that signs in wants a
        // challenge.
        "bit",
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
        let pkg_id = state.id.clone();
        t.set(
            "tab",
            lua.create_function(move |lua, (title, cb): (String, Function)| {
                let id = shared.alloc_id();
                let key = super::tab_key(&pkg_id, &title);
                let cb = lua.create_registry_value(cb)?;
                shared.pending.borrow_mut().push(Registration::Tab {
                    pkg,
                    id,
                    title: title.clone(),
                    cb,
                });
                // Closed until asked for. A tab that opened itself would push
                // the user's own panels aside on every project open, which is
                // the one thing a docked layout must never do.
                shared.open_state.borrow_mut().insert(id, false);
                tab_handle(lua, &shared, id, key)
            })?,
        )?;
    }
    {
        let shared = shared.clone();
        t.set(
            "overlay",
            // `ed.overlay(name, fn)` or `ed.overlay(name, opts, fn)`. The
            // options are second so the callback stays last, which is where
            // every other registration puts it and where a long inline
            // function reads best.
            lua.create_function(move |lua, (name, a, b): (String, Value, Option<Function>)| {
                let (opts, cb) = match (a, b) {
                    (Value::Function(f), _) => (None, f),
                    (Value::Table(t), Some(f)) => (Some(t), f),
                    _ => {
                        return Err(mlua::Error::runtime(
                            "ed.overlay wants (name, drawFn) or (name, options, drawFn)",
                        ));
                    }
                };
                let mut look = super::OverlayLook::default();
                if let Some(t) = opts {
                    if let Ok(c) = t.get::<String>("corner") {
                        look.left = c.eq_ignore_ascii_case("topleft")
                            || c.eq_ignore_ascii_case("bottomleft")
                            || c.eq_ignore_ascii_case("left");
                        look.bottom = c.eq_ignore_ascii_case("bottomleft")
                            || c.eq_ignore_ascii_case("bottomright")
                            || c.eq_ignore_ascii_case("bottom");
                    }
                    if let Ok(v) = t.get::<bool>("bare") {
                        look.bare = v;
                    }
                    if let Ok(v) = t.get::<bool>("fill") {
                        look.fill = v;
                    }
                    if let Ok(w) = t.get::<f32>("width")
                        && w.is_finite()
                        && w > 40.0
                    {
                        look.width = w.min(900.0);
                    }
                }
                let id = shared.alloc_id();
                let key = lua.create_registry_value(cb)?;
                shared.pending.borrow_mut().push(Registration::Overlay {
                    pkg,
                    id,
                    name: name.clone(),
                    cb: key,
                    look,
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
    // ---- randomness the OS vouches for --------------------------------------
    //
    // `math.random` is a PRNG seeded from the clock. It is right for a puff of
    // smoke and wrong for anything an attacker gets to guess at — a sign-in
    // challenge, a nonce, a token, an id that must not collide. A package cannot
    // build one out of what it has, so without this the only options are to use
    // the wrong thing or to not do the job.
    //
    // Ungated: reading entropy tells the package nothing about the machine, and a
    // permission prompt in front of it would train people to click through the
    // prompts that matter.
    t.set(
        "randomBytes",
        lua.create_function(|lua, n: usize| {
            if n == 0 || n > 1024 {
                return Err(mlua::Error::runtime(
                    "ed.randomBytes(n): n has to be between 1 and 1024",
                ));
            }
            let mut buf = vec![0u8; n];
            getrandom::getrandom(&mut buf)
                .map_err(|e| mlua::Error::runtime(format!("no system randomness: {e}")))?;
            // A Lua string, because Lua strings are byte strings — the caller
            // decides whether that becomes hex, base64url or a raw key, and a
            // hex-only answer would make half of them decode it back again.
            lua.create_string(&buf)
        })?,
    )?;

    // ---- timers ------------------------------------------------------------
    //
    // Every package that waits for anything was writing the same four lines:
    // keep a deadline, compare it against `ed.time()` from inside `onUpdate`,
    // remember to take it down. Polling a job, debouncing a text box, retrying a
    // request, stepping an animation — all of them, all the same, and each copy
    // its own chance to leave a dead deadline behind.
    for (name, repeat) in [("after", false), ("every", true)] {
        let shared = shared.clone();
        t.set(
            name,
            lua.create_function(move |lua, (secs, cb): (f64, Function)| {
                if !secs.is_finite() || secs < 0.0 {
                    return Err(mlua::Error::runtime(format!(
                        "ed.{name}({secs}, fn): the delay has to be a number of seconds"
                    )));
                }
                // `ed.every(0, fn)` is a request for an infinite loop; a frame
                // is the fastest anything here can happen anyway.
                let secs = if repeat { secs.max(1e-3) } else { secs };
                let id = shared.alloc_id();
                let key = lua.create_registry_value(cb)?;
                shared
                    .pending
                    .borrow_mut()
                    .push(Registration::Timer { pkg, id, every: secs, repeat, cb: key });
                timer_handle(lua, &shared, id)
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
        // **A write, never a read.** Putting a string somewhere the person can
        // paste it is them asking for it — the package had the string already.
        // Reading the clipboard is a different question with a real answer
        // owed (it holds whatever they last copied, from anywhere), and
        // nothing in this API can do it. That absence is a decision.
        //
        // No permission for the same reason: nothing leaves the machine and
        // nothing arrives. `Browser` guards `openUrl` because that hands a
        // string to the network; this hands it to the person at the keyboard.
        let shared = shared.clone();
        t.set(
            "copy",
            lua.create_function(move |_, text: String| {
                if text.len() > MAX_COPY_BYTES {
                    return Err(mlua::Error::runtime(format!(
                        "ed.copy: that is {:.1} MB of text and the clipboard takes at most 1 MB \
                         — something that big wants ed.write and a file",
                        text.len() as f64 / (1024.0 * 1024.0)
                    )));
                }
                shared.cmds.borrow_mut().push(ExtCmd::Copy(text));
                Ok(())
            })?,
        )?;
    }
    {
        // Take the author to a place. Every tool that lists positions — a
        // search result, a lint hit, a suggested placement — has somewhere it
        // wants to show you, and until this the only way to get there was to
        // select something, which is an edit to the author's selection made on
        // their behalf just to move a camera.
        let shared = shared.clone();
        t.set(
            "lookAt",
            lua.create_function(move |_, (at, distance): (Value, Option<f64>)| {
                let at = super::handles::vec3_of(&at)?;
                if let Some(d) = distance
                    && !(d.is_finite() && d > 0.0)
                {
                    return Err(mlua::Error::runtime(
                        "ed.lookAt's distance must be a positive number of metres",
                    ));
                }
                shared.cmds.borrow_mut().push(ExtCmd::LookAt { at, distance });
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
    // **The OS's own file picker.** Gated on `Files`, because the paths it
    // returns are outside the package and reading one is exactly what it is
    // for. The dialog itself is the editor's — a package never touches rfd,
    // whose synchronous API cannot run on this thread at all.
    // **Absent without `Files`, not refused at the call.** Same rule as `http`:
    // the failure belongs at the top of the file where an author sees it, not
    // three menus deep in front of a user who cannot act on it.
    if state.permissions.contains(&Permission::Files) {
        let shared = shared.clone();
        t.set(
            "pickFile",
            lua.create_function(move |lua, (opts, cb): (Value, Function)| {
                let mut title = "Choose a file".to_string();
                let mut filter = None;
                let mut multiple = false;
                if let Value::Table(o) = &opts {
                    if let Ok(s) = o.get::<String>("title") {
                        title = s;
                    }
                    if let Ok(m) = o.get::<bool>("multiple") {
                        multiple = m;
                    }
                    if let Ok(exts) = o.get::<Table>("extensions") {
                        let mut v = Vec::new();
                        for e in exts.sequence_values::<String>().flatten() {
                            v.push(e.trim_start_matches('.').to_lowercase());
                        }
                        if !v.is_empty() {
                            let label =
                                o.get::<String>("label").unwrap_or_else(|_| "Files".to_string());
                            filter = Some((label, v));
                        }
                    }
                } else if let Value::String(s) = &opts {
                    title = s.to_string_lossy().to_string();
                }
                let cb = lua.create_registry_value(cb)?;
                shared
                    .pick_reqs
                    .borrow_mut()
                    .push(super::PickReq { title, filter, multiple, cb });
                Ok(())
            })?,
        )?;
    }
    t.set("read", read_fn(lua, shared, state)?)?;
    t.set("readBytes", read_bytes_fn(lua, shared, state)?)?;
    t.set("write", write_fn(lua, shared, state)?)?;
    t.set("exists", exists_fn(lua, shared, state)?)?;
    t.set("list", list_fn(lua, shared, state)?)?;
    Ok(t)
}

/// A `{ show, hide, toggle, focus, isOpen }` handle over a registered panel.
/// What `ed.after` / `ed.every` hand back: an id and a way to stop it.
///
/// A handle rather than an id, for the same reason `nav.obstacle` gives one —
/// `t:cancel()` needs nothing written down, and there is no second call that
/// takes a number and could be given the wrong one.
fn timer_handle(lua: &Lua, shared: &Rc<Shared>, id: u32) -> mlua::Result<Table> {
    let h = lua.create_table()?;
    h.set("id", id)?;
    let s = shared.clone();
    h.set(
        "cancel",
        lua.create_function(move |_, _: Value| {
            s.cancelled.borrow_mut().insert(id);
            Ok(())
        })?,
    )?;
    Ok(h)
}

/// The handle `ed.tab` gives back: `show`, `hide`, `toggle`, `isOpen`.
///
/// No `focus` — showing a tab already brings it to the front of whatever leaf it
/// lives in, and there is nothing else a package could sensibly mean by focusing
/// one. Where it sits is the user's arrangement, not the package's.
fn tab_handle(lua: &Lua, shared: &Rc<Shared>, id: u32, key: u64) -> mlua::Result<Table> {
    let h = lua.create_table()?;
    h.set("id", id)?;
    for (name, open) in [("show", true), ("hide", false)] {
        let shared = shared.clone();
        h.set(
            name,
            lua.create_function(move |_, _: Value| {
                shared.cmds.borrow_mut().push(ExtCmd::TabOpen(key, open));
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
                shared.cmds.borrow_mut().push(ExtCmd::TabOpen(key, !now));
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
    Ok(h)
}

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

    /// Resolve a path for reading that may ALSO be one the user picked.
    ///
    /// Everything `read_path` allows, plus any absolute path `ed.pickFile`
    /// handed this package during this session. **The picker is the grant**: a
    /// path the package made up is scoped to the package and the project, but a
    /// path the *user* chose in an OS dialog, for this purpose, is a file they
    /// have already decided to hand over. Without this an image picker can only
    /// tell a package the name of a file it cannot open.
    ///
    /// Checked against the canonicalised path, so `…/picked/../../etc/passwd`
    /// is not the file that was granted.
    fn any_read_path(&self, rel: &str) -> Result<PathBuf, String> {
        if let Ok(c) = std::fs::canonicalize(rel)
            && self.shared.picked_files.borrow().contains(&c)
        {
            return Ok(c);
        }
        self.read_path(rel)
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

/// `ed.readBytes(path)` — a file's raw bytes as a Lua string, or `nil`.
///
/// `ed.read` is `read_to_string`, so a PNG comes back as **nil** rather than as
/// an error: not valid UTF-8, `.ok()` swallows it, and the caller sees the same
/// answer it would get for a file that is not there. Anything binary — an image
/// to upload, a font to inspect — was unreachable, and unreachable in the most
/// confusing available way.
///
/// Lua strings are byte strings, so there is nothing to encode: `#bytes` is the
/// file's length and `string.byte` indexes it.
fn read_bytes_fn(lua: &Lua, shared: &Rc<Shared>, state: &PkgState) -> mlua::Result<Function> {
    let scope = FileScope::of(shared, state);
    lua.create_function(move |lua, rel: String| {
        let path = scope.any_read_path(&rel).map_err(mlua::Error::runtime)?;
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(lua.create_string(&bytes)?)),
            Err(_) => Ok(None),
        }
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

/// A Lua table as JSON, for the node-document commands.
///
/// JSON rather than the Lua table itself because the command queue outlives the
/// call: a held `mlua::Table` costs one of LuaJIT's ~8000 registry slots, and a
/// tool that queues a few hundred edits in a loop would run the state out of
/// them ([[lua-table-vs-registrykey-ceiling]]). `serde_json::Value` is a plain
/// owned tree with nothing of Lua's in it.
///
/// A Lua array and a Lua map are the same type, so the ambiguity is resolved the
/// way the rest of this API resolves it: `{}` is an empty **object**, because
/// every place a document takes a table it takes a named one.
fn lua_to_json(_lua: &Lua, t: Table) -> mlua::Result<serde_json::Value> {
    // **The same encoder `json.encode` uses**, deliberately. It used to go
    // through mlua's serde conversion, which decides array-vs-object by its own
    // rule and knows nothing about `json.array` — so `scene.set(id, { scripts =
    // json.array{} })` wrote `{}` and the document then failed to parse with
    // "invalid type: map, expected a sequence". There was no expression a
    // package could write to clear a list field of a node, which is the exact
    // thing `json.array` exists for. Routing it here also brings the recursion
    // limit and `json.null` to `scene.set`, which had neither.
    to_json(&Value::Table(t), 0)
}

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
                // The ORIENTED half-extents, in world units, beside the
                // rotation that turns them. A tool that cares which way a thing
                // is facing needs the pair, not a box that has forgotten.
                t.set(
                    "extents",
                    match n.half {
                        Some(h) => Value::Table(xyz(
                            lua,
                            [h[0] as f64, h[1] as f64, h[2] as f64],
                        )?),
                        None => Value::Nil,
                    },
                )?;
                // `ui` is present only on a node that IS one, so `if n.ui`
                // is the test a package writes — a table of defaults on every
                // node would make every folder look like a panel.
                if let Some(u) = &n.ui {
                    let ui = lua.create_table()?;
                    ui.set("element", u.element)?;
                    ui.set("text", u.text.clone())?;
                    ui.set("interactive", u.interactive)?;
                    ui.set("disabled", u.disabled)?;
                    t.set("ui", ui)?;
                }
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
    // ---- the node document -------------------------------------------------
    // `scene.doc(id)` reads the whole node — everything `scene.set` can write.
    //
    // **Only for what is selected**, and that is a deliberate limit rather than
    // an oversight. Serialising a node's every component costs far more than
    // the rest of the mirror put together, and building the whole scene's
    // documents would mean rebuilding them on every frame of a gizmo drag, in
    // every project that has such a package installed. The selection is what a
    // tool operates on, it is a handful of nodes, and its documents are free.
    //
    // A node that is not selected RAISES rather than answering nil: a read that
    // quietly returns nothing is how a tool ends up placing an empty node and
    // reporting success.
    {
        let shared = shared.clone();
        t.set(
            "doc",
            lua.create_function(move |lua, id: u32| {
                use mlua::LuaSerdeExt;
                let scene = shared.scene.borrow();
                match scene.doc(id) {
                    Some(v) => lua.to_value(v),
                    None if scene.get(id).is_none() => {
                        Err(mlua::Error::runtime(format!("there is no node {id}")))
                    }
                    None => Err(mlua::Error::runtime(format!(
                        "scene.doc({id}): a node's document is readable while it is selected. \
                         Use scene.docs({{{id}}}, fn) to read any node without touching the \
                         selection, selection.set({{{id}}}) first, or scene.info({id}) for the \
                         summary that is always available"
                    ))),
                }
            })?,
        )?;
    }
    // `scene.docs(ids, done)` reads the documents of ANY nodes, however many,
    // without touching the selection.
    //
    // The selection rule above is right for the mirror and wrong as the only
    // way in. A tool that renames across a level, collects every user-facing
    // string, or proposes a reorganisation has to read the documents of nodes
    // nobody has selected — and `selection.set` is not a workaround, it is an
    // edit to somebody's selection made in order to perform a read.
    //
    // The cost that made the mirror selection-only was **per frame**: rebuilding
    // every document on every frame of a gizmo drag. This is not that. It is one
    // read of the ids that were asked for, served after the frame like
    // `mesh.read`, costing nothing until somebody asks and nothing again
    // afterwards.
    //
    // The callback takes `(docs, missing)` — `docs` keyed by id, and the ids
    // that had no node. A batch read that quietly answers with fewer nodes than
    // it was given is how a tool reports renaming forty having renamed
    // thirty-eight.
    {
        let shared = shared.clone();
        t.set(
            "docs",
            lua.create_function(move |lua, (ids, cb): (Table, Function)| {
                let mut want: Vec<u32> = Vec::new();
                for v in ids.sequence_values::<u32>() {
                    want.push(v?);
                }
                if want.len() > MAX_DOCS {
                    return Err(mlua::Error::runtime(format!(
                        "scene.docs: {} ids is more than the {MAX_DOCS} one read serves — ask \
                         in batches, so a level with a hundred thousand nodes in it costs a \
                         series of frames rather than one long one",
                        want.len()
                    )));
                }
                want.sort_unstable();
                want.dedup();
                let key = lua.create_registry_value(cb)?;
                shared.doc_reqs.borrow_mut().push(super::DocReq { ids: want, cb: key });
                Ok(())
            })?,
        )?;
        t.set("maxDocs", MAX_DOCS)?;
    }
    //
    // The setters above name one property each, and that list will always be
    // behind the node types: reads answer seventeen fields and writes answered
    // five, which is the difference between a tool that can measure a level and
    // one that can build it.
    //
    // These two take the node **document** instead — the same shape a `.ron`
    // scene, a prefab and the clipboard all serialise, so a node type that gains
    // a field is writable by every package the day it lands, with nothing here
    // to update.
    {
        let shared = shared.clone();
        t.set(
            "set",
            lua.create_function(move |lua, (id, patch): (u32, Table)| {
                // PARTIAL, deliberately. A tool that wants to tint a light
                // should not have to read the whole node back and write it out
                // again — and a whole-document write is how a tool silently
                // reverts a field it did not know about.
                let patch = lua_to_json(lua, patch)?;
                shared.cmds.borrow_mut().push(ExtCmd::NodeSet { id, patch });
                Ok(())
            })?,
        )?;
    }
    {
        let shared = shared.clone();
        t.set(
            "add",
            lua.create_function(move |lua, (spec, parent): (Table, Option<u32>)| {
                let spec = lua_to_json(lua, spec)?;
                shared.cmds.borrow_mut().push(ExtCmd::NodeAdd { spec, parent });
                Ok(())
            })?,
        )?;
    }
    {
        let shared = shared.clone();
        t.set(
            "setParent",
            lua.create_function(move |_, (id, parent): (u32, Option<u32>)| {
                shared.cmds.borrow_mut().push(ExtCmd::NodeSetParent { id, parent });
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
    // An open Server-Sent Events stream: `onFrame` is called once per frame,
    // `onEnd` once when the connection closes for any reason.
    //
    // Two callbacks rather than one with a "kind" field, because the two are
    // different jobs — one updates a bar, the other tidies up and decides
    // whether to fall back. Conflating them is how a stream's cleanup ends up
    // running on every frame.
    {
        let shared = shared.clone();
        t.set(
            "stream",
            lua.create_function(
                move |lua, (url, opts, on_frame, on_end): (String, Value, Value, Option<Function>)| {
                    let (opts, on_frame, on_end) = match (opts, on_frame, on_end) {
                        (Value::Function(f), Value::Function(e), None) => (None, f, Some(e)),
                        (Value::Function(f), Value::Nil, None) => (None, f, None),
                        (Value::Table(t), Value::Function(f), e) => (Some(t), f, e),
                        (Value::Nil, Value::Function(f), e) => (None, f, e),
                        _ => {
                            return Err(mlua::Error::runtime(
                                "expected (url, onFrame [, onEnd]) or \
                                 (url, opts, onFrame [, onEnd])",
                            ))
                        }
                    };
                    let (headers, timeout) = super::http::read_opts(opts);
                    let frame_key = lua.create_registry_value(on_frame)?;
                    // `onEnd` is optional but the host always has one to call,
                    // so an absent one becomes a callback that does nothing —
                    // simpler than an `Option` threaded through the queue.
                    let end_key = match on_end {
                        Some(f) => lua.create_registry_value(f)?,
                        None => lua.create_registry_value(lua.create_function(|_, _: Value| Ok(()))?)?,
                    };
                    let id = shared
                        .web
                        .borrow_mut()
                        .stream(url, headers, timeout, frame_key, end_key)
                        .map_err(mlua::Error::runtime)?;
                    stream_handle(lua, shared.clone(), id)
                },
            )?,
        )?;
    }
    Ok(t)
}

/// What `http.stream` hands back: something to call `:cancel()` on.
///
/// A handle rather than a bare id, for the same reason `ed.after` returns one —
/// an id is a number somebody has to remember what to do with, and a handle
/// says what it is for.
fn stream_handle(lua: &Lua, shared: Rc<Shared>, id: u64) -> mlua::Result<Table> {
    let h = lua.create_table()?;
    h.set("id", id)?;
    {
        let shared = shared.clone();
        h.set(
            "cancel",
            lua.create_function(move |_, _: Value| {
                shared.web.borrow_mut().stop_stream(id);
                Ok(())
            })?,
        )?;
    }
    h.set(
        "isOpen",
        lua.create_function(move |_, _: Value| Ok(shared.web.borrow().is_streaming(id)))?,
    )?;
    Ok(h)
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
    // `json.null` — a value that encodes to JSON `null`.
    //
    // Nil cannot do this job. `t.field = nil` does not put a null in the table,
    // it REMOVES the key, so the encoder never sees it and the field simply is
    // not in the output. That difference matters to any API that reads an
    // absent field as "leave this alone" and an explicit null as "clear it" —
    // without a sentinel, a package can set such a field and never unset it.
    //
    // **Decoding is deliberately not symmetric.** JSON `null` still arrives as
    // Lua nil, because every package already reads an optional field with
    // `if body.field then`, and handing them a sentinel that is truthy would
    // silently turn every one of those tests the wrong way round.
    t.set("null", json_null())?;
    // `json.array(t)` / `json.isArray(v)` — the same class of problem as
    // `json.null`, one step along: a value Lua cannot say that the wire needs.
    // An empty table is both an empty list and an empty object and the encoder
    // has to pick; it picks the object, so without this there is no way to
    // send `[]` at all. See `floptle_script::json_array`.
    floptle_script::json_array::install(lua, &t)?;
    Ok(t)
}

/// The identity behind `json.null`.
///
/// Its address is never read — only compared — so any unique static will do.
/// Light userdata rather than a table so that `json.null == json.null` is a
/// pointer comparison and nothing can accidentally construct a second one.
static JSON_NULL: u8 = 0;

fn json_null() -> mlua::LightUserData {
    mlua::LightUserData(&JSON_NULL as *const u8 as *mut std::ffi::c_void)
}

/// The most text `ed.copy` will put on the clipboard.
///
/// A megabyte is far past anything a person pastes and far short of anything
/// that costs the editor a frame. Refused with a sentence rather than
/// truncated: half a snippet on the clipboard is worse than none, because it
/// pastes without complaint.
const MAX_COPY_BYTES: usize = 1 << 20;

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
        // Matched before the catch-all below, which would give the same answer
        // for the wrong reason: that arm nulls a function or a thread too, and
        // this one is the documented way to write a null on purpose.
        Value::LightUserData(p) if *p == json_null() => serde_json::Value::Null,
        Value::Boolean(b) => (*b).into(),
        Value::Integer(i) => (*i).into(),
        Value::Number(n) => serde_json::Number::from_f64(*n)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::String(s) => s.to_string_lossy().to_string().into(),
        Value::Table(t) => {
            // A Lua table is both a list and a map; `raw_len > 0` with no other
            // keys is the only shape that is unambiguously an array. `json.array`
            // marks the ones the shape cannot answer for — an empty list, and
            // any list a decode handed over so it survives being sent back.
            let len = t.raw_len();
            if floptle_script::json_array::encodes_as_array(t) {
                if let Some(why) = floptle_script::json_array::problem(t) {
                    return Err(mlua::Error::runtime(format!(
                        "json.encode: this table is marked as a list (json.array) but {why}"
                    )));
                }
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
            // Tagged even when it has items in it: a package that reads a body,
            // empties one of its lists and posts it back must not have to
            // remember which fields were lists to keep them lists.
            t.set_metatable(Some(floptle_script::json_array::array_metatable(lua)?));
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
