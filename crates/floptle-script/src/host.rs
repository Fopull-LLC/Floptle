//! The [`ScriptHost`] engine loop: source hot-reload generations, per-(node,
//! script) sandbox instances, the per-frame update (mirror the scene, call
//! `start`/`update`, apply node writes), and log/error capture.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use floptle_core::transform::Transform;
use floptle_core::{Entity, Material, Matter, Scripts, Visible, World};
use mlua::{Lua, RegistryKey, Table, Value, Variadic};

use crate::api::{apply_component_field, mirror_components};
use crate::env::{
    apply_node, build_env, lifecycle_fn, material_key, new_node_handle, node_pre, node_table,
    params_table,
};
use crate::preprocess::preprocess;

/// Collect the dotted names reachable from a Lua table, for
/// [`ScriptHost::api_surface`].
///
/// `seen` breaks the reference cycles Lua tables freely contain (`_G._G`, a
/// module that stores itself). The depth cap keeps the walk to the shape an API
/// actually has — a table, its members, and one level of nesting under those.
fn flatten(
    t: &Table,
    prefix: &str,
    seen: &mut std::collections::HashSet<usize>,
    depth: usize,
) -> Vec<String> {
    let mut out = Vec::new();
    if depth > 2 || !seen.insert(t.to_pointer() as usize) {
        return out;
    }
    for pair in t.pairs::<Value, Value>().flatten() {
        let (k, v) = pair;
        let Value::String(name) = k else { continue };
        let Ok(name) = name.to_str() else { continue };
        let name = name.to_string();
        // `_G`, `__index` and friends are plumbing, not API. `package` and
        // `debug` are stdlib bulk that the diff would drop anyway — skipping
        // them keeps the walk cheap.
        if name.starts_with('_') || name == "package" || name == "debug" {
            continue;
        }
        let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}.{name}") };
        match v {
            Value::Table(inner) => {
                out.push(path.clone());
                out.extend(flatten(&inner, &path, seen, depth + 1));
            }
            Value::Function(_) => out.push(path),
            // A plain value the engine seeded (`dt`, `time`, `synced`).
            _ if depth == 0 => out.push(path),
            _ => {}
        }
    }
    out
}

/// Lua argument tuples for the `gizmo.*` draw calls: positions, then the
/// optional size/length and 0–1 RGB tail.
type GizmoLineArgs = (f64, f64, f64, f64, f64, f64, Option<f64>, Option<f64>, Option<f64>);
type GizmoRayArgs = (f64, f64, f64, f64, f64, f64, Option<f64>, Option<f64>, Option<f64>, Option<f64>);
type GizmoBallArgs = (f64, f64, f64, Option<f64>, Option<f64>, Option<f64>, Option<f64>);
use crate::{
    error_line, gizmo_color, install_handle_api, AnimCmd, AnimInfo, BodyState, GizmoCmd,
    InputSnapshot, Instance, LogLevel, SceneMirror, ScriptHost, ScriptLog, Shared, Source,
    VfxCmd, VfxInfo,
};

/// Every key a `scene.load` options table reads (`floptle/0082`).
pub(crate) const SCENE_LOAD_KEYS: &[&str] = &["additive", "environment"];

/// Render any Lua value as readable Console text: primitives plainly, engine
/// handles by IDENTITY (`node "Player" (#4)`, `component "RigidBody" of …`),
/// userdata via `__tostring` (vec3/vec2 print their components), and tables
/// DEEPLY — short arrays inline, everything else as an indented block with
/// sorted keys, cycle detection, and depth/entry caps so a self-referential
/// or huge table can never wedge a frame.
fn pretty_value(v: &Value, depth: usize, seen: &mut Vec<*const std::ffi::c_void>) -> String {
    const MAX_DEPTH: usize = 4;
    match v {
        Value::Nil => "nil".into(),
        Value::Boolean(b) => b.to_string(),
        Value::Integer(n) => n.to_string(),
        Value::Number(n) => n.to_string(),
        // Bare at the top level (what you print reads as you wrote it),
        // quoted inside tables (so "" vs nil vs 5 stay distinguishable).
        Value::String(s) if depth == 0 => s.to_string_lossy().to_string(),
        Value::String(s) => format!("\"{}\"", s.to_string_lossy()),
        Value::Function(_) => "<function>".into(),
        Value::Thread(_) => "<thread>".into(),
        Value::LightUserData(_) => "<pointer>".into(),
        Value::UserData(_) => v.to_string().unwrap_or_else(|_| "<userdata>".into()),
        Value::Error(e) => format!("<error: {e}>"),
        Value::Table(t) => {
            if depth >= MAX_DEPTH {
                return "{…}".into();
            }
            let p = t.to_pointer();
            if seen.contains(&p) {
                return "<cycle>".into();
            }
            seen.push(p);
            let out = pretty_table(t, depth, seen);
            seen.pop();
            out
        }
        _ => "<value>".into(),
    }
}

/// The table arm of [`pretty_value`]: engine handles first, then arrays
/// (inline when short), then key-sorted blocks.
fn pretty_table(t: &Table, depth: usize, seen: &mut Vec<*const std::ffi::c_void>) -> String {
    // Engine handles are `{__id, …}` tables with a metatable — print WHAT
    // they point at, not their internals.
    if let Ok(Some(id)) = t.raw_get::<Option<u32>>("__id") {
        if let Ok(Some(comp)) = t.raw_get::<Option<String>>("__comp") {
            return format!("component \"{comp}\" (node #{id})");
        }
        if let Ok(Some(script)) = t.raw_get::<Option<String>>("__script") {
            return format!("script \"{script}\" (node #{id})");
        }
        // A node handle: name + position through its own metatable getters.
        let name = t.get::<Option<String>>("name").ok().flatten();
        let pos = t
            .get::<Value>("pos")
            .ok()
            .filter(|p| !p.is_nil())
            .and_then(|p| p.to_string().ok());
        return match (name, pos) {
            (Some(n), Some(p)) => format!("node \"{n}\" (#{id}) at {p}"),
            (Some(n), None) => format!("node \"{n}\" (#{id})"),
            _ => format!("node #{id} (not in the scene)"),
        };
    }

    const MAX_ENTRIES: usize = 40;
    let mut items: Vec<(Value, Value)> = Vec::new();
    let mut extra = 0usize;
    for pair in t.clone().pairs::<Value, Value>() {
        let Ok((k, v)) = pair else { continue };
        if items.len() < MAX_ENTRIES {
            items.push((k, v));
        } else {
            extra += 1;
        }
    }
    if items.is_empty() && extra == 0 {
        return "{}".into();
    }
    // Array part: keys exactly 1..=n (any order) render without keys.
    let is_array = extra == 0
        && items.iter().all(|(k, _)| matches!(k, Value::Integer(i) if *i >= 1))
        && {
            let mut ks: Vec<i64> = items
                .iter()
                .filter_map(|(k, _)| if let Value::Integer(i) = k { Some(*i) } else { None })
                .collect();
            ks.sort_unstable();
            ks.iter().enumerate().all(|(i, &k)| k == i as i64 + 1)
        };
    let pad = "  ".repeat(depth + 1);
    let close_pad = "  ".repeat(depth);
    if is_array {
        items.sort_by_key(|(k, _)| if let Value::Integer(i) = k { *i } else { 0 });
        let vals: Vec<String> =
            items.iter().map(|(_, v)| pretty_value(v, depth + 1, seen)).collect();
        let width: usize = vals.iter().map(|s| s.len() + 2).sum();
        if width <= 64 && vals.iter().all(|s| !s.contains('\n')) {
            return format!("{{{}}}", vals.join(", "));
        }
        let body: Vec<String> = vals.iter().map(|v| format!("{pad}{v},")).collect();
        return format!("{{\n{}\n{close_pad}}}", body.join("\n"));
    }
    // Map: sort by rendered key so output is stable run to run.
    let mut rows: Vec<(String, String)> = items
        .iter()
        .map(|(k, v)| {
            let key = match k {
                Value::String(s) => {
                    let s = s.to_string_lossy().to_string();
                    let ident = !s.is_empty()
                        && s.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_')
                        && s.chars().all(|c| c.is_alphanumeric() || c == '_');
                    if ident { s } else { format!("[\"{s}\"]") }
                }
                other => format!("[{}]", pretty_value(other, depth + 1, seen)),
            };
            (key, pretty_value(v, depth + 1, seen))
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let mut body: Vec<String> =
        rows.iter().map(|(k, v)| format!("{pad}{k} = {v},")).collect();
    if extra > 0 {
        body.push(format!("{pad}… (+{extra} more)"));
    }
    format!("{{\n{}\n{close_pad}}}", body.join("\n"))
}

/// Which lifecycle pass a script run is: the per-frame pass (`start`/`update`),
/// the per-gameplay-tick pass (`fixedUpdate`), or the post-physics camera pass
/// (`lateUpdate` — after the interpolated transform writeback, so followers
/// sample this frame's FINAL poses instead of last frame's).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pass {
    Frame,
    Fixed,
    Late,
}

impl Default for ScriptHost {
    fn default() -> Self {
        Self::new()
    }
}

impl ScriptHost {
    pub fn new() -> Self {
        let lua = Lua::new();
        let logs: Rc<RefCell<Vec<ScriptLog>>> = Rc::new(RefCell::new(Vec::new()));
        // The current script's `(name, line)` taken from the Lua call stack, so a
        // Console line can jump to where it was logged.
        let caller = |lua: &Lua| -> Option<(String, u32)> {
            let d = lua.inspect_stack(1)?;
            let src = d.source();
            let name = src.source.as_ref().map(|c| c.trim_start_matches(['@', '=']).to_string())?;
            Some((name, d.curr_line().max(0) as u32))
        };
        // `log("...")` and Lua's stdlib `print(...)` both feed the engine Console.
        {
            let sink = logs.clone();
            if let Ok(log) = lua.create_function(move |lua, msg: String| {
                eprintln!("[lua] {msg}");
                sink.borrow_mut().push(ScriptLog { level: LogLevel::Debug, msg, source: caller(lua) });
                Ok(())
            }) {
                let _ = lua.globals().set("log", log);
            }
        }
        {
            let sink = logs.clone();
            if let Ok(print) = lua.create_function(move |lua, args: Variadic<Value>| {
                // Deep, Console-ready rendering of ANY value: nested tables,
                // node/component/script handles, vec3s — see `pretty_value`.
                let parts: Vec<String> = args
                    .iter()
                    .map(|v| pretty_value(v, 0, &mut Vec::new()))
                    .collect();
                let msg = if parts.iter().any(|p| p.contains('\n')) {
                    parts.join("\n")
                } else {
                    parts.join("\t")
                };
                eprintln!("[lua] {msg}");
                sink.borrow_mut().push(ScriptLog { level: LogLevel::Debug, msg, source: caller(lua) });
                Ok(())
            }) {
                let _ = lua.globals().set("print", print);
            }
        }
        // The `input` global: a table of functions reading this frame's input
        // snapshot (so games can poll the keyboard/mouse).
        let input: Rc<RefCell<InputSnapshot>> = Rc::new(RefCell::new(InputSnapshot::default()));
        // The action layer, shared with the driver: it resolves devices into
        // this, scripts read named actions out of it.
        let input_sys: crate::input_api::SharedInput =
            Rc::new(RefCell::new(floptle_input::InputSystem::default()));
        let input_domain: crate::input_api::SharedDomain =
            Rc::new(std::cell::Cell::new(floptle_input::Domain::Frame));
        // Mouse-lock request channel (drained by the editor each frame). See the field docs.
        let mouse_lock: Rc<RefCell<Option<bool>>> = Rc::new(RefCell::new(None));
        // Keys the HOST keeps for itself, and which of them a script has already
        // been told about (`floptle/0084`). The driver fills the list — the editor
        // reserves Play/Pause/Step; a headless test reserves nothing — and the
        // first poll of a reserved key writes one Console line naming it and what
        // takes it. A key that is never going to arrive must not be
        // indistinguishable from a key the player did not press: that is exactly
        // how a game shipped a bag on Tab and heard about it from a player.
        let reserved_keys: crate::ReservedKeys = Rc::new(RefCell::new(Vec::new()));
        let reserved_warned: Rc<RefCell<std::collections::HashSet<String>>> =
            Rc::new(RefCell::new(std::collections::HashSet::new()));
        if let Ok(t) = lua.create_table() {
            // One check behind all three raw pollers, so they cannot disagree
            // about which keys are reachable.
            let warn_reserved = {
                let list = reserved_keys.clone();
                let warned = reserved_warned.clone();
                let sink = logs.clone();
                move |lua: &Lua, name: &str| {
                    let Some(why) = list
                        .borrow()
                        .iter()
                        .find(|(k, _)| k == name)
                        .map(|(_, why)| why.clone())
                    else {
                        return;
                    };
                    if !warned.borrow_mut().insert(name.to_string()) {
                        return;
                    }
                    sink.borrow_mut().push(ScriptLog {
                        level: LogLevel::Warn,
                        msg: format!(
                            "input: \"{name}\" is reserved by the editor for {why}, so this \
                             script will never see it pressed — bind something else. (Every \
                             other key reaches a focused Game view, Tab included.)"
                        ),
                        source: caller(lua),
                    });
                }
            };
            let held = input.clone();
            let wr = warn_reserved.clone();
            let _ = t.set(
                "key",
                lua.create_function(move |lua, name: String| {
                    let name = name.to_lowercase();
                    wr(lua, &name);
                    Ok(held.borrow().keys_down.contains(&name))
                })
                .ok(),
            );
            let pressed = input.clone();
            let wr = warn_reserved.clone();
            let _ = t.set(
                "pressed",
                lua.create_function(move |lua, name: String| {
                    let name = name.to_lowercase();
                    wr(lua, &name);
                    Ok(pressed.borrow().keys_pressed.contains(&name))
                })
                .ok(),
            );
            let released = input.clone();
            let wr = warn_reserved;
            let _ = t.set(
                "released",
                lua.create_function(move |lua, name: String| {
                    let name = name.to_lowercase();
                    wr(lua, &name);
                    Ok(released.borrow().keys_released.contains(&name))
                })
                .ok(),
            );
            let ty = input.clone();
            let _ = t.set(
                "typed",
                lua.create_function(move |_, ()| Ok(ty.borrow().typed.clone())).ok(),
            );
            let m = input.clone();
            let _ = t.set(
                "mouse",
                lua.create_function(move |_, ()| {
                    let p = m.borrow().mouse;
                    Ok((p.0, p.1))
                })
                .ok(),
            );
            let md = input.clone();
            let _ = t.set(
                "mouse_delta",
                lua.create_function(move |_, ()| {
                    let d = md.borrow().mouse_delta;
                    Ok((d.0, d.1))
                })
                .ok(),
            );
            let sc = input.clone();
            let _ = t.set(
                "scroll",
                lua.create_function(move |_, ()| Ok(sc.borrow().scroll)).ok(),
            );
            // The active camera's view angles, captured WITH the input snapshot.
            // THE way to do camera-relative movement in multiplayer: the aim
            // rides the input command, so the server + prediction replay see
            // exactly the angle the player did (a camera node can't replicate
            // that). nil when the scene has no active camera.
            let ay = input.clone();
            let _ = t.set(
                "aimYaw",
                lua.create_function(move |_, ()| Ok(ay.borrow().aim.map(|a| a[0]))).ok(),
            );
            let ap = input.clone();
            let _ = t.set(
                "aimPitch",
                lua.create_function(move |_, ()| Ok(ap.borrow().aim.map(|a| a[1]))).ok(),
            );
            let bd = input.clone();
            let _ = t.set(
                "button",
                lua.create_function(move |_, i: usize| {
                    Ok(bd.borrow().buttons_down.get(i).copied().unwrap_or(false))
                })
                .ok(),
            );
            let bp = input.clone();
            let _ = t.set(
                "clicked",
                lua.create_function(move |_, i: usize| {
                    Ok(bp.borrow().buttons_pressed.get(i).copied().unwrap_or(false))
                })
                .ok(),
            );
            // A convenience -1..1 axis from a negative/positive key pair.
            let ax = input.clone();
            let _ = t.set(
                "axis",
                lua.create_function(move |_, (neg, pos): (String, String)| {
                    let d = ax.borrow();
                    let mut v = 0.0f32;
                    if d.keys_down.contains(&neg.to_lowercase()) {
                        v -= 1.0;
                    }
                    if d.keys_down.contains(&pos.to_lowercase()) {
                        v += 1.0;
                    }
                    Ok(v)
                })
                .ok(),
            );
            // Mouse capture: lock the cursor to the window and hide it (for FPS / free-look
            // mouselook without holding a button), or release it back to the desktop.
            let ml_lock = mouse_lock.clone();
            let _ = t.set(
                "lockMouse",
                lua.create_function(move |_, ()| {
                    *ml_lock.borrow_mut() = Some(true);
                    Ok(())
                })
                .ok(),
            );
            let ml_unlock = mouse_lock.clone();
            let _ = t.set(
                "unlockMouse",
                lua.create_function(move |_, ()| {
                    *ml_unlock.borrow_mut() = Some(false);
                    Ok(())
                })
                .ok(),
            );
            // Explicit form: `input.setMouseLocked(true/false)`.
            let ml_set = mouse_lock.clone();
            let _ = t.set(
                "setMouseLocked",
                lua.create_function(move |_, locked: bool| {
                    *ml_set.borrow_mut() = Some(locked);
                    Ok(())
                })
                .ok(),
            );
            // The ACTION layer sits on the same table, so a project can migrate
            // one call at a time: `input.key("w")` and `input.action("Jump")`
            // coexist for as long as a game wants them to.
            crate::input_api::install(&lua, &t, &input_sys, &input_domain);
            let _ = lua.globals().set("input", t);
        }
        // The `net.*` bridge state — created early so the raycast closure can
        // read the current-instance marker (self-hit exclusion) and `net.rewind`
        // can re-pose the hulls (the API itself installs further down).
        let net = crate::net_api::SharedNet::new(logs.clone());

        // `raycast(ox,oy,oz, dx,dy,dz, max)` against the world's colliders (terrain +
        // mesh + static primitives) AND every dynamic body's hull (players, crates):
        // returns a hit table {x,y,z, nx,ny,nz, distance, node} or nil — `node` is the
        // hit body's node handle (nil for static geometry), so combat code can do
        // `hit.node:getscript("combat")`. The caster's OWN body is excluded (a ray from
        // your center must not hit you). Use it for ground checks, line-of-sight,
        // shooting. Scripts speak WORLD coordinates; the sim runs origin-relative
        // (ADR-0015), so convert in f64 on the way in and out.
        let colliders: Rc<RefCell<Vec<floptle_physics::AnchoredCollider>>> =
            Rc::new(RefCell::new(Vec::new()));
        let hulls: Rc<RefCell<Vec<floptle_physics::BodyHull>>> =
            Rc::new(RefCell::new(Vec::new()));
        let sim_origin: Rc<RefCell<glam::DVec3>> = Rc::new(RefCell::new(glam::DVec3::ZERO));
        // The project's layer table (names → bits + collision matrix), lent by
        // the driver at Play start — shared with the raycast closure (named
        // layer filters) and the node handles (`node.layer` validation).
        let layer_table: Rc<RefCell<floptle_core::Layers>> =
            Rc::new(RefCell::new(floptle_core::Layers::default()));
        {
            let cols = colliders.clone();
            let hus = hulls.clone();
            let so = sim_origin.clone();
            let cur = net.current.clone();
            let lt = layer_table.clone();
            if let Ok(f) = lua.create_function(move |lua, args: mlua::MultiValue| {
                // Two spellings, one ray: the vector form
                // `raycast(origin, dir, max [, ignore])` — origin may be a NODE
                // handle — and the original six-number form. The docs have
                // taught the vector one since 0.17; it only became true here.
                let a: Vec<Value> = args.into_iter().collect();
                let num = |v: Option<&Value>| -> Option<f64> {
                    match v {
                        Some(Value::Number(n)) => Some(*n),
                        Some(Value::Integer(i)) => Some(*i as f64),
                        _ => None,
                    }
                };
                let (ox, oy, oz, dx, dy, dz, max, ignore) = if a.len() >= 3
                    && matches!(a[0], Value::Table(_) | Value::UserData(_))
                {
                    let (Some(o), Some(d)) = (
                        crate::math_api::vec3_of(&a[0]),
                        crate::math_api::vec3_of(&a[1]),
                    ) else {
                        return Err(mlua::Error::RuntimeError(
                            "raycast(origin, dir, max [, ignore]) — origin and dir are vec3s \
                             (or a node, or anything with x/y/z)"
                                .into(),
                        ));
                    };
                    let Some(max) = num(a.get(2)) else {
                        return Err(mlua::Error::RuntimeError(
                            "raycast(origin, dir, max) — max is a distance in metres".into(),
                        ));
                    };
                    (o.x, o.y, o.z, d.x, d.y, d.z, max, a.get(3).cloned())
                } else {
                    let n: Vec<f64> = a.iter().take(7).map(|v| num(Some(v)).unwrap_or(f64::NAN)).collect();
                    if n.len() < 7 || n.iter().any(|v| v.is_nan()) {
                        return Err(mlua::Error::RuntimeError(
                            "raycast(origin, dir, max [, ignore]) or \
                             raycast(ox,oy,oz, dx,dy,dz, max [, ignore])"
                                .into(),
                        ));
                    }
                    (n[0], n[1], n[2], n[3], n[4], n[5], n[6], a.get(7).cloned())
                };
                let origin = *so.borrow();
                let o = (glam::DVec3::new(ox, oy, oz) - origin).as_vec3();
                let dir = glam::Vec3::new(dx as f32, dy as f32, dz as f32);
                // Bodies the ray passes through: the caster's own, plus an
                // optional explicit ignore (a node handle or entity id) — e.g.
                // an orbit camera skipping the character it follows. The 8th
                // arg is either that ignore directly, or an OPTIONS table:
                // `{ ignore = node, layers = "Ground" | {"Ground", "Props"} }`
                // — `layers` filters BOTH static geometry and body hulls by
                // the project's named layers (a misspelled name is an error,
                // not a silent everything-misses).
                let mut exclude: Vec<u32> = Vec::with_capacity(2);
                let mut mask = !0u32;
                if let Some((eid, _)) = cur.borrow().as_ref() {
                    exclude.push(*eid);
                }
                match &ignore {
                    Some(Value::Table(t)) => {
                        if let Ok(eid) = t.raw_get::<u32>("__id") {
                            exclude.push(eid);
                        } else {
                            // No __id → an options table. Checked against the
                            // SAME list `shape_api`'s queries use, because this
                            // is a second copy of that parsing and the two lists
                            // drifting is how `layers` ends up honoured by one
                            // and ignored by the other (`floptle/0082`).
                            crate::opts::check_keys(
                                t,
                                crate::shape_api::QUERY_KEYS,
                                "raycast",
                            )?;
                            if let Ok(ig) = t.get::<Table>("ignore")
                                && let Ok(eid) = ig.raw_get::<u32>("__id")
                            {
                                exclude.push(eid);
                            }
                            let names: Vec<String> = match t.get::<Value>("layers") {
                                Ok(Value::String(s)) => vec![s.to_string_lossy().to_string()],
                                Ok(Value::Table(list)) => {
                                    list.sequence_values::<String>().flatten().collect()
                                }
                                _ => Vec::new(),
                            };
                            if !names.is_empty() {
                                let lt = lt.borrow();
                                mask = 0;
                                for n in &names {
                                    match lt.index_of(n) {
                                        Some(i) => mask |= 1u32 << i,
                                        None => {
                                            return Err(mlua::Error::RuntimeError(format!(
                                                "raycast: no layer named '{n}' (project layers: {})",
                                                lt.names.join(", ")
                                            )))
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Some(Value::Integer(id)) => exclude.push(*id as u32),
                    Some(Value::Number(id)) => exclude.push(*id as u32),
                    _ => {}
                }
                let solid =
                    floptle_physics::raycast_colliders(&cols.borrow(), o, dir, max as f32, mask);
                let body = floptle_physics::raycast_hulls(
                    &hus.borrow(),
                    o,
                    dir,
                    max as f32,
                    &exclude,
                    mask,
                );
                // Nearest surface wins between static geometry and body hulls.
                let (h, eid) = match (solid, body) {
                    (Some(s), Some((be, b))) if b.distance < s.distance => (b, Some(be)),
                    (Some(s), _) => (s, None),
                    (None, Some((be, b))) => (b, Some(be)),
                    (None, None) => return Ok(Value::Nil),
                };
                let t = lua.create_table()?;
                t.set("x", origin.x + h.point[0] as f64)?;
                t.set("y", origin.y + h.point[1] as f64)?;
                t.set("z", origin.z + h.point[2] as f64)?;
                t.set("nx", h.normal[0] as f64)?;
                t.set("ny", h.normal[1] as f64)?;
                t.set("nz", h.normal[2] as f64)?;
                t.set("distance", h.distance as f64)?;
                if let Some(be) = eid {
                    t.set("node", new_node_handle(lua, be)?)?;
                }
                Ok(Value::Table(t))
            }) {
                let _ = lua.globals().set("raycast", f);
            }
        }

        // Shape queries — the volume half of the same question `raycast` asks,
        // sharing its collider/hull loans (so they are rewound inside
        // `net.rewind` for free) and its options table (roadmap B2).
        crate::shape_api::install_shape_api(
            &lua,
            crate::shape_api::QueryShared {
                colliders: colliders.clone(),
                hulls: hulls.clone(),
                sim_origin: sim_origin.clone(),
                current: net.current.clone(),
                layers: layer_table.clone(),
            },
        );

        // `water.*` — the volume half of `floptle/0038`. The engine floats
        // things; a game still decides what being wet MEANS, and every one of
        // those decisions is the same question with a different answer.
        let water_volumes: Rc<RefCell<Vec<crate::water_api::WaterInfo>>> =
            Rc::new(RefCell::new(Vec::new()));
        let water_freeze: Rc<RefCell<Vec<(u32, bool)>>> = Rc::new(RefCell::new(Vec::new()));
        crate::water_api::install_water_api(
            &lua,
            crate::water_api::WaterShared {
                volumes: water_volumes.clone(),
                freeze: water_freeze.clone(),
            },
        );

        // `scatter.*` — thousands of props from a seed (`floptle/0036`). The
        // game keeps deciding what grows where; the engine draws them.
        let scatter_sources: crate::scatter_api::Sources = Rc::new(RefCell::new(Vec::new()));
        let scatter_next_id: Rc<std::cell::Cell<u32>> = Rc::new(std::cell::Cell::new(0));
        crate::scatter_api::install_scatter_api(
            &lua,
            scatter_sources.clone(),
            scatter_next_id.clone(),
            logs.clone(),
        );

        // `gizmo.*` — immediate-mode debug drawing: world-space lines, rays, spheres
        // and points that show for ONE frame in the Scene view (never the Game view;
        // the viewport's gizmo toggle hides them). Colors are optional 0–1 floats.
        // Per-frame command count is capped so a runaway loop can't flood the renderer.
        let gizmos: Rc<RefCell<Vec<GizmoCmd>>> = Rc::new(RefCell::new(Vec::new()));
        const GIZMO_CAP: usize = 4096;
        if let Ok(t) = lua.create_table() {
            let q = gizmos.clone();
            let _ = t.set(
                "line",
                lua.create_function(move |_, (x1, y1, z1, x2, y2, z2, r, g, b): GizmoLineArgs| {
                    let mut q = q.borrow_mut();
                    if q.len() < GIZMO_CAP {
                        q.push(GizmoCmd::Line {
                            a: [x1 as f32, y1 as f32, z1 as f32],
                            b: [x2 as f32, y2 as f32, z2 as f32],
                            color: gizmo_color(r, g, b),
                        });
                    }
                    Ok(())
                })
                .ok(),
            );
            let q = gizmos.clone();
            let _ = t.set(
                "ray",
                lua.create_function(move |_, (ox, oy, oz, dx, dy, dz, len, r, g, b): GizmoRayArgs| {
                    let mut q = q.borrow_mut();
                    if q.len() < GIZMO_CAP {
                        let d = glam::DVec3::new(dx, dy, dz);
                        // With a length the direction is normalized (matches raycast);
                        // without one the vector IS the ray.
                        let end = match len {
                            Some(l) if d.length_squared() > 1e-12 => {
                                glam::DVec3::new(ox, oy, oz) + d.normalize() * l
                            }
                            _ => glam::DVec3::new(ox + dx, oy + dy, oz + dz),
                        };
                        q.push(GizmoCmd::Line {
                            a: [ox as f32, oy as f32, oz as f32],
                            b: [end.x as f32, end.y as f32, end.z as f32],
                            color: gizmo_color(r, g, b),
                        });
                    }
                    Ok(())
                })
                .ok(),
            );
            let q = gizmos.clone();
            let _ = t.set(
                "sphere",
                lua.create_function(move |_, (x, y, z, radius, r, g, b): GizmoBallArgs| {
                    let mut q = q.borrow_mut();
                    if q.len() < GIZMO_CAP {
                        q.push(GizmoCmd::Sphere {
                            center: [x as f32, y as f32, z as f32],
                            radius: radius.unwrap_or(0.5).max(0.001) as f32,
                            color: gizmo_color(r, g, b),
                        });
                    }
                    Ok(())
                })
                .ok(),
            );
            let q = gizmos.clone();
            let _ = t.set(
                "point",
                lua.create_function(move |_, (x, y, z, size, r, g, b): GizmoBallArgs| {
                    let mut q = q.borrow_mut();
                    if q.len() < GIZMO_CAP {
                        q.push(GizmoCmd::Point {
                            pos: [x as f32, y as f32, z as f32],
                            size: size.unwrap_or(0.25).max(0.001) as f32,
                            color: gizmo_color(r, g, b),
                        });
                    }
                    Ok(())
                })
                .ok(),
            );
            let _ = lua.globals().set("gizmo", t);
        }

        // `assets.getFile(path)` / `assets.getContents(dir)`: resolve files in the project's
        // `Assets/` folder by a path the dev writes relative to it (e.g. "models/armor.glb").
        // getFile returns the full asset path (or nil if missing); getContents returns an
        // array of every file's path under a directory (recursive), for building tables of
        // assets. The returned strings are exactly what `node.model` / `node.material` accept.
        let project_root: Rc<RefCell<PathBuf>> = Rc::new(RefCell::new(PathBuf::from("assets")));
        if let Ok(t) = lua.create_table() {
            let pr = project_root.clone();
            let _ = t.set(
                "getFile",
                lua.create_function(move |lua, path: String| {
                    let full = pr.borrow().join(&path);
                    Ok(if full.is_file() {
                        Value::String(lua.create_string(full.to_string_lossy().as_bytes())?)
                    } else {
                        Value::Nil
                    })
                })
                .ok(),
            );
            let pr2 = project_root.clone();
            let _ = t.set(
                "getContents",
                lua.create_function(move |lua, dir: String| {
                    let base = pr2.borrow().join(&dir);
                    let mut files: Vec<String> = Vec::new();
                    let mut stack = vec![base];
                    while let Some(d) = stack.pop() {
                        if let Ok(rd) = std::fs::read_dir(&d) {
                            for entry in rd.flatten() {
                                let p = entry.path();
                                if p.is_dir() {
                                    stack.push(p);
                                } else if p.is_file() {
                                    files.push(p.to_string_lossy().to_string());
                                }
                            }
                        }
                    }
                    files.sort();
                    let arr = lua.create_table()?;
                    for (i, f) in files.iter().enumerate() {
                        arr.set(i + 1, lua.create_string(f.as_bytes())?)?;
                    }
                    Ok(arr)
                })
                .ok(),
            );
            let _ = lua.globals().set("assets", t);
        }

        // `scene.*` — scene management: `scene.load(name)` queues a transition
        // the engine performs between frames (in multiplayer only the SERVER
        // may switch — clients follow automatically); `scene.current()` is the
        // running scene's name; `scene.list()` enumerates the project's scenes.
        let scene_request: crate::SceneQueue = Rc::new(RefCell::new(Vec::new()));
        let scene_loaded: Rc<RefCell<Vec<(u32, mlua::RegistryKey)>>> =
            Rc::new(RefCell::new(Vec::new()));
        let scene_name: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        if let Ok(t) = lua.create_table() {
            let q = scene_request.clone();
            let _ = t.set(
                "load",
                lua.create_function(move |_, (name, opts): (String, Option<Table>)| {
                    // `{ additive = true }` layers the scene on top of the
                    // running one instead of replacing it.
                    //
                    // This used to ignore anything else in the table, on the
                    // reasoning that the option set could then grow without
                    // breaking a script that passed one. That reasoning was
                    // wrong in the one direction that matters: a typo'd
                    // `addative = true` reads as `additive = false`, which
                    // DESTROYS the running scene instead of layering onto it.
                    // Every node it held is gone, the request queue is cleared,
                    // and nothing anywhere mentions a key (`floptle/0082`).
                    //
                    // `{ environment = true }` additionally hands the world's
                    // environment to the layer: its sun, fog, skybox and post
                    // chain replace the base scene's for as long as it is
                    // loaded. Meaningless without `additive` (a full swap
                    // already brings its own), so it is read alongside it.
                    let (additive, environment) = match &opts {
                        Some(o) => {
                            crate::opts::check_keys(o, SCENE_LOAD_KEYS, "scene.load")?;
                            (
                                crate::opts::opt_bool(o, "scene.load", "additive")?
                                    .unwrap_or(false),
                                crate::opts::opt_bool(o, "scene.load", "environment")?
                                    .unwrap_or(false),
                            )
                        }
                        None => (false, false),
                    };
                    let req = if additive {
                        crate::SceneRequest::Additive { name, environment }
                    } else {
                        crate::SceneRequest::Load { name }
                    };
                    let mut q = q.borrow_mut();
                    // A full swap ends the frame's queue: everything already
                    // asked for named the world that is about to stop existing.
                    if req.is_swap() {
                        q.clear();
                    }
                    q.push(req);
                    Ok(())
                })
                .ok(),
            );
            let q = scene_request.clone();
            let _ = t.set(
                "unload",
                lua.create_function(move |_, name: String| {
                    q.borrow_mut().push(crate::SceneRequest::Unload { name });
                    Ok(())
                })
                .ok(),
            );
            let subs = scene_loaded.clone();
            let cur = net.current.clone();
            let _ = t.set(
                "onLoaded",
                lua.create_function(move |lua, f: mlua::Function| {
                    let owner = cur.borrow().as_ref().map(|(e, _)| *e).unwrap_or(0);
                    match lua.create_registry_value(f) {
                        Ok(k) => subs.borrow_mut().push((owner, k)),
                        Err(e) => return Err(e),
                    }
                    Ok(())
                })
                .ok(),
            );
            let sn = scene_name.clone();
            let _ = t.set(
                "current",
                lua.create_function(move |lua, ()| {
                    lua.create_string(sn.borrow().as_bytes())
                })
                .ok(),
            );
            let pr = project_root.clone();
            let _ = t.set(
                "list",
                lua.create_function(move |lua, ()| {
                    // Scene names relative to `scenes/`, extension dropped,
                    // subfolders kept ("arenas/desert") — exactly what
                    // `scene.load` accepts.
                    let base = pr.borrow().join("scenes");
                    let mut names: Vec<String> = Vec::new();
                    let mut stack = vec![base.clone()];
                    while let Some(d) = stack.pop() {
                        if let Ok(rd) = std::fs::read_dir(&d) {
                            for entry in rd.flatten() {
                                let p = entry.path();
                                if p.is_dir() {
                                    stack.push(p);
                                } else if p.extension().is_some_and(|x| x == "ron")
                                    && let Ok(rel) = p.strip_prefix(&base)
                                {
                                    let mut s = rel.to_string_lossy().replace('\\', "/");
                                    s.truncate(s.len().saturating_sub(4));
                                    names.push(s);
                                }
                            }
                        }
                    }
                    names.sort();
                    let arr = lua.create_table()?;
                    for (i, n) in names.iter().enumerate() {
                        arr.set(i + 1, lua.create_string(n.as_bytes())?)?;
                    }
                    Ok(arr)
                })
                .ok(),
            );
            let _ = lua.globals().set("scene", t);
        }

        // `ui.*` — the game-UI runtime surface. Focus is engine state rather
        // than a component (a hover that survived into a saved scene would be a
        // bug), so it travels through its own channels instead of the mirror.
        let ui_focus: Rc<RefCell<Option<u32>>> = Rc::new(RefCell::new(None));
        let ui_focus_request: Rc<RefCell<Option<Option<u32>>>> = Rc::new(RefCell::new(None));
        // (drag source, drop target under it) — live for the whole drag AND
        // for the frame the `dropped` hooks run on, which is the frame that
        // actually needs to read it.
        let ui_drag: crate::UiDragCell = Rc::new(RefCell::new(None));
        let ui_bindings: Rc<RefCell<Vec<crate::UiBinding>>> = Rc::new(RefCell::new(Vec::new()));
        let ui_makes: crate::UiMakes = Rc::new(RefCell::new(Vec::new()));
        let ui_handlers: crate::UiHandlers = Rc::new(RefCell::new(HashMap::new()));
        let ui_listeners: crate::UiListeners = Rc::new(RefCell::new(Vec::new()));
        let ui_listener_checks: Rc<RefCell<Vec<(u32, String)>>> = Rc::new(RefCell::new(Vec::new()));
        let ui_frame_events: crate::UiFrameEvents = Rc::new(RefCell::new(Vec::new()));
        let ui_hover: Rc<RefCell<Option<u32>>> = Rc::new(RefCell::new(None));
        let ui_active: Rc<RefCell<Option<u32>>> = Rc::new(RefCell::new(None));
        if let Ok(t) = lua.create_table() {
            let req = ui_focus_request.clone();
            let cur = ui_focus.clone();
            let _ = t.set(
                "focus",
                lua.create_function(move |_, node: mlua::Value| {
                    // `ui.focus(node)` moves the ring; `ui.focus(nil)` drops it
                    // (a screen that wants nothing focused until the player
                    // touches something).
                    let want = match &node {
                        mlua::Value::Nil => None,
                        v => Some(crate::env::node_id_of(v).ok_or_else(|| {
                            mlua::Error::runtime("ui.focus expects a node or nil")
                        })?),
                    };
                    // Read-your-writes within the frame, same as `node.style`.
                    *cur.borrow_mut() = want;
                    *req.borrow_mut() = Some(want);
                    Ok(())
                })
                .ok(),
            );
            let cur = ui_focus.clone();
            let _ = t.set(
                "focused",
                // `ui.focused()` = which element; `ui.focused(el)` = is it that
                // one. Same shape as `ui.hovered` / `ui.held`, so the three
                // states a screen asks about answer the same way.
                lua.create_function(move |lua, node: Option<mlua::Value>| {
                    let f = *cur.borrow();
                    match node.as_ref().and_then(crate::env::node_id_of) {
                        Some(e) => Ok(mlua::Value::Boolean(f == Some(e))),
                        None => match f {
                            Some(id) => {
                                crate::env::new_node_handle(lua, id).map(mlua::Value::Table)
                            }
                            None => Ok(mlua::Value::Nil),
                        },
                    }
                })
                .ok(),
            );
            // The drag in flight. There is no separate payload channel on
            // purpose: the source is a NODE, and a node already carries params,
            // a name, tags and its own scripts — everything an inventory row
            // needs to say what it is. A second data path would only be a
            // second thing to keep in sync.
            let d = ui_drag.clone();
            let _ = t.set(
                "dragging",
                lua.create_function(move |lua, ()| match d.borrow().map(|(s, _)| s) {
                    Some(id) => crate::env::new_node_handle(lua, id).map(mlua::Value::Table),
                    None => Ok(mlua::Value::Nil),
                })
                .ok(),
            );
            let d = ui_drag.clone();
            let _ = t.set(
                "dropTarget",
                lua.create_function(move |lua, ()| match d.borrow().and_then(|(_, t)| t) {
                    Some(id) => crate::env::new_node_handle(lua, id).map(mlua::Value::Table),
                    None => Ok(mlua::Value::Nil),
                })
                .ok(),
            );
            // `ui.bind(node, prop, fn)` — say the relationship once instead of
            // writing an `update` that keeps it true. The engine calls `fn`
            // once a frame and writes what comes back; a binding on a node
            // that goes away goes away with it.
            let binds = ui_bindings.clone();
            let _ = t.set(
                "bind",
                lua.create_function(
                    move |lua, (node, prop, f): (mlua::Value, String, mlua::Function)| {
                        let e = crate::env::node_id_of(&node).ok_or_else(|| {
                            mlua::Error::runtime("ui.bind expects (node, property, function)")
                        })?;
                        let key = lua.create_registry_value(f)?;
                        let mut b = binds.borrow_mut();
                        // Re-binding the same property replaces rather than
                        // stacks: two functions fighting over one label every
                        // frame is never what was meant.
                        b.retain(|x| !(x.e == e && x.prop == prop));
                        b.push(crate::UiBinding { e, prop, f: key });
                        Ok(())
                    },
                )
                .ok(),
            );
            let binds = ui_bindings.clone();
            let _ = t.set(
                "unbind",
                lua.create_function(move |_, (node, prop): (mlua::Value, Option<String>)| {
                    let e = crate::env::node_id_of(&node)
                        .ok_or_else(|| mlua::Error::runtime("ui.unbind expects a node"))?;
                    binds
                        .borrow_mut()
                        .retain(|x| x.e != e || prop.as_ref().is_some_and(|p| *p != x.prop));
                    Ok(())
                })
                .ok(),
            );
            // `ui.make(container, tree)` — a screen described as data, and
            // reconciled against the one already there. The counterpart to
            // `ui.bind`: bind keeps a value true, make keeps a TREE true.
            let makes = ui_makes.clone();
            let _ = t.set(
                "make",
                lua.create_function(move |lua, (node, tree): (mlua::Value, mlua::Value)| {
                    let container = crate::env::node_id_of(&node).ok_or_else(|| {
                        mlua::Error::runtime("ui.make expects (node, table)")
                    })?;
                    // Parsing raises rather than logs: a mistyped property is a
                    // mistake in the description, and a screen that quietly
                    // builds without it is harder to debug than one that stops
                    // with a line number.
                    let (roots, hooks) = crate::ui_make::parse_tree(lua, &tree)?;
                    makes.borrow_mut().push(crate::ui_make::MakeRequest {
                        container,
                        roots,
                        hooks,
                    });
                    Ok(())
                })
                .ok(),
            );
            // `ui.on(element, hook, fn)` — listen to an element from a script
            // that does NOT live on it.
            //
            // A `clicked` function in a script file answers for the node that
            // script is on, which means one script file per button: eight
            // three-line files whose only real content is "tell the menu".
            // A listener puts all eight in the menu's own script, where the
            // state they change already lives.
            let listeners = ui_listeners.clone();
            let checks = ui_listener_checks.clone();
            let n = net.clone();
            let _ = t.set(
                "on",
                lua.create_function(
                    move |lua, (node, hook, f): (mlua::Value, String, mlua::Function)| {
                        let e = crate::env::node_id_of(&node).ok_or_else(|| {
                            mlua::Error::runtime("ui.on expects (element, hook, function)")
                        })?;
                        // A mistyped hook is the failure mode here — the
                        // listener registers, nothing ever calls it, and there
                        // is nothing to see. Naming the hooks is cheap.
                        if !crate::ui_make::HOOKS.contains(&hook.as_str()) {
                            return Err(mlua::Error::runtime(format!(
                                "ui.on: \"{hook}\" is not a UI hook (one of: {})",
                                crate::ui_make::HOOKS.join(", ")
                            )));
                        }
                        // Registered outside a script (a bare `ui.on` in a made
                        // element's closure, say) — still legal, but it belongs
                        // to nobody, so nothing reloads or destroys it early.
                        let owner = n.current.borrow().clone().unwrap_or((0, String::new()));
                        let key = lua.create_registry_value(f)?;
                        let mut ls = listeners.borrow_mut();
                        // Same owner, same element, same hook REPLACES — like
                        // `ui.bind`. That makes `ui.on` safe to call from
                        // `update`, and makes the classic mistake (registering
                        // every frame) cost one closure instead of thousands.
                        if let Some(old) = ls
                            .iter_mut()
                            .find(|l| l.e == e && l.hook == hook && l.owner == owner)
                        {
                            let stale = std::mem::replace(&mut old.f, key);
                            let _ = lua.remove_registry_value(stale);
                            return Ok(());
                        }
                        checks.borrow_mut().push((e, hook.clone()));
                        ls.push(crate::UiListener { e, hook, owner, f: key });
                        Ok(())
                    },
                )
                .ok(),
            );
            // `ui.off(element)` / `ui.off(element, hook)` — stop listening.
            // Only the CALLER's listeners go: two managers on one element must
            // not be able to unregister each other.
            let listeners = ui_listeners.clone();
            let n = net.clone();
            let _ = t.set(
                "off",
                lua.create_function(move |_, (node, hook): (mlua::Value, Option<String>)| {
                    let e = crate::env::node_id_of(&node)
                        .ok_or_else(|| mlua::Error::runtime("ui.off expects an element"))?;
                    let owner = n.current.borrow().clone().unwrap_or((0, String::new()));
                    listeners.borrow_mut().retain(|l| {
                        !(l.e == e
                            && l.owner == owner
                            && hook.as_ref().is_none_or(|h| *h == l.hook))
                    });
                    Ok(())
                })
                .ok(),
            );
            // The other half: asking, instead of being called back. Both read
            // the SAME list of events the hooks fire from, published before the
            // scripts run — so a poll in `update` and a `clicked` hook can
            // never disagree about what happened this frame.
            let ev = ui_frame_events.clone();
            let _ = t.set(
                "event",
                lua.create_function(move |_, (node, hook): (mlua::Value, String)| {
                    let Some(e) = crate::env::node_id_of(&node) else {
                        return Ok(false);
                    };
                    Ok(ev.borrow().iter().any(|(x, h)| *x == e && *h == hook))
                })
                .ok(),
            );
            for (name, hook) in [
                ("clicked", "clicked"),
                ("pressed", "pressed"),
                ("released", "released"),
                ("changed", "changed"),
                ("submitted", "submitted"),
            ] {
                let ev = ui_frame_events.clone();
                let _ = t.set(
                    name,
                    lua.create_function(move |_, node: mlua::Value| {
                        let Some(e) = crate::env::node_id_of(&node) else {
                            return Ok(false);
                        };
                        Ok(ev.borrow().iter().any(|(x, h)| *x == e && h == hook))
                    })
                    .ok(),
                );
            }
            // `ui.events()` — everything that happened this frame, so a manager
            // can handle a whole screen without naming a single element:
            // `for _, ev in ipairs(ui.events("clicked")) do ... end`.
            let ev = ui_frame_events.clone();
            let _ = t.set(
                "events",
                lua.create_function(move |lua, hook: Option<String>| {
                    let out = lua.create_table()?;
                    let mut i = 1;
                    for (e, h) in ev.borrow().iter() {
                        if hook.as_ref().is_some_and(|w| w != h) {
                            continue;
                        }
                        let row = lua.create_table()?;
                        row.set("node", crate::env::new_node_handle(lua, *e)?)?;
                        row.set("event", h.as_str())?;
                        out.set(i, row)?;
                        i += 1;
                    }
                    Ok(out)
                })
                .ok(),
            );
            // Live states, not events: what the pointer is over, and what it is
            // holding down. With an element they answer yes/no; with nothing
            // they answer *which* — the shape `ui.focused()` already has.
            for (name, cell) in [("hovered", &ui_hover), ("held", &ui_active)] {
                let c = cell.clone();
                let _ = t.set(
                    name,
                    lua.create_function(move |lua, node: Option<mlua::Value>| {
                        let cur = *c.borrow();
                        match node.as_ref().and_then(crate::env::node_id_of) {
                            Some(e) => Ok(mlua::Value::Boolean(cur == Some(e))),
                            None => match cur {
                                Some(id) => {
                                    crate::env::new_node_handle(lua, id).map(mlua::Value::Table)
                                }
                                None => Ok(mlua::Value::Nil),
                            },
                        }
                    })
                    .ok(),
                );
            }
            let _ = lua.globals().set("ui", t);
        }

        // `spawnEffect(key, x, y, z [, vx, vy, vz])` — fire a one-shot particle effect at
        // a world point, no node required. The editor spawns a detached instance that
        // plays once and auto-despawns (the fire-and-forget path for hits, pickups,
        // poofs). The optional velocity is the emitter's world velocity: inherit-velocity
        // tracks (smoke/dust off a fast vessel) ride it so they aren't stranded in space.
        let spawn_effects: Rc<RefCell<Vec<crate::SpawnedEffect>>> =
            Rc::new(RefCell::new(Vec::new()));
        {
            let q = spawn_effects.clone();
            type Args = (String, f64, f64, f64, Option<f64>, Option<f64>, Option<f64>);
            if let Ok(f) = lua.create_function(move |_, (key, x, y, z, vx, vy, vz): Args| {
                q.borrow_mut().push((
                    key,
                    [x, y, z],
                    [vx.unwrap_or(0.0), vy.unwrap_or(0.0), vz.unwrap_or(0.0)],
                ));
                Ok(())
            }) {
                let _ = lua.globals().set("spawnEffect", f);
            }
        }

        // `draw.line(x1,y1,z1, x2,y2,z2, r,g,b [, a])` — queue one world-space
        // 3D line segment for THIS tick. Immediate mode: segments live for one
        // tick and are re-drawn every fixedUpdate while wanted (the S6 v2 map
        // screen draws its orbit conics this way). Depth-tested in the scene.
        let draw_lines: Rc<RefCell<Vec<crate::DrawLine>>> = Rc::new(RefCell::new(Vec::new()));
        let draw_tris: Rc<RefCell<Vec<crate::DrawTri>>> = Rc::new(RefCell::new(Vec::new()));
        let draw_rects: Rc<RefCell<Vec<crate::DrawRect>>> = Rc::new(RefCell::new(Vec::new()));
        let draw_texts: Rc<RefCell<Vec<crate::DrawText>>> = Rc::new(RefCell::new(Vec::new()));
        {
            let q = draw_lines.clone();
            if let (Ok(f), Ok(t)) = (
                lua.create_function(
                    move |_,
                          (x1, y1, z1, x2, y2, z2, r, g, b, a): (
                        f64,
                        f64,
                        f64,
                        f64,
                        f64,
                        f64,
                        f32,
                        f32,
                        f32,
                        Option<f32>,
                    )| {
                        q.borrow_mut().push(crate::DrawLine {
                            a: [x1, y1, z1],
                            b: [x2, y2, z2],
                            color: [r, g, b, a.unwrap_or(1.0)],
                        });
                        Ok(())
                    },
                ),
                lua.create_table(),
            ) {
                let _ = t.set("line", f);
                // `draw.ring(cx,cy,cz, nx,ny,nz, radius, r,g,b [,a])` — a circle
                // around `n` at `c`. `draw.sphere(cx,cy,cz, radius, r,g,b [,a])` —
                // three rings. `draw.box(cx,cy,cz, hx,hy,hz, yaw, r,g,b [,a])` —
                // a yaw-rotated wireframe box. All build on the same always-on
                // line pass: draw.* is the GAME's visual telegraph layer
                // (attach markers, selection outlines, range rings), rendered
                // unconditionally in the game view — unlike `gizmo.*`, the
                // DEBUG layer the editor's gizmos toggle gates.
                let ring_segs = |q: &mut Vec<crate::DrawLine>,
                                 c: glam::DVec3,
                                 n: glam::DVec3,
                                 radius: f64,
                                 color: [f32; 4]| {
                    let n = n.try_normalize().unwrap_or(glam::DVec3::Y);
                    let u = if n.x.abs() < 0.9 { glam::DVec3::X } else { glam::DVec3::Z };
                    let u = (u - n * u.dot(n)).normalize();
                    let v = n.cross(u);
                    const N: usize = 28;
                    let mut prev = c + u * radius;
                    for k in 1..=N {
                        let t = k as f64 / N as f64 * std::f64::consts::TAU;
                        let p = c + u * (radius * t.cos()) + v * (radius * t.sin());
                        q.push(crate::DrawLine { a: prev.into(), b: p.into(), color });
                        prev = p;
                    }
                };
                {
                    let q = draw_lines.clone();
                    type RingArgs = (f64, f64, f64, f64, f64, f64, f64, f32, f32, f32, Option<f32>);
                    if let Ok(f) = lua.create_function(
                        move |_, (cx, cy, cz, nx, ny, nz, radius, r, g, b, a): RingArgs| {
                            ring_segs(
                                &mut q.borrow_mut(),
                                glam::DVec3::new(cx, cy, cz),
                                glam::DVec3::new(nx, ny, nz),
                                radius.max(1e-4),
                                [r, g, b, a.unwrap_or(1.0)],
                            );
                            Ok(())
                        },
                    ) {
                        let _ = t.set("ring", f);
                    }
                }
                {
                    let q = draw_lines.clone();
                    type BallArgs = (f64, f64, f64, f64, f32, f32, f32, Option<f32>);
                    if let Ok(f) = lua.create_function(
                        move |_, (cx, cy, cz, radius, r, g, b, a): BallArgs| {
                            let c = glam::DVec3::new(cx, cy, cz);
                            let col = [r, g, b, a.unwrap_or(1.0)];
                            let mut q = q.borrow_mut();
                            for n in [glam::DVec3::X, glam::DVec3::Y, glam::DVec3::Z] {
                                ring_segs(&mut q, c, n, radius.max(1e-4), col);
                            }
                            Ok(())
                        },
                    ) {
                        let _ = t.set("sphere", f);
                    }
                }
                {
                    let q = draw_lines.clone();
                    type BoxArgs = (f64, f64, f64, f64, f64, f64, f64, f32, f32, f32, Option<f32>);
                    if let Ok(f) = lua.create_function(
                        move |_, (cx, cy, cz, hx, hy, hz, yaw, r, g, b, a): BoxArgs| {
                            let c = glam::DVec3::new(cx, cy, cz);
                            let (cy_, sy_) = (yaw.cos(), yaw.sin());
                            let rot = |p: glam::DVec3| {
                                glam::DVec3::new(p.x * cy_ + p.z * sy_, p.y, -p.x * sy_ + p.z * cy_)
                            };
                            let col = [r, g, b, a.unwrap_or(1.0)];
                            let corner = |i: usize| {
                                let sx = if i & 1 == 0 { -hx } else { hx };
                                let sy = if i & 2 == 0 { -hy } else { hy };
                                let sz = if i & 4 == 0 { -hz } else { hz };
                                c + rot(glam::DVec3::new(sx, sy, sz))
                            };
                            let mut q = q.borrow_mut();
                            for (i, j) in [
                                (0, 1), (2, 3), (4, 5), (6, 7), // x edges
                                (0, 2), (1, 3), (4, 6), (5, 7), // y edges
                                (0, 4), (1, 5), (2, 6), (3, 7), // z edges
                            ] {
                                q.push(crate::DrawLine { a: corner(i).into(), b: corner(j).into(), color: col });
                            }
                            Ok(())
                        },
                    ) {
                        let _ = t.set("box", f);
                    }
                }
                // ── FILLED triangle layer: solid gizmos & world markers ──
                // `draw.tri(x1..z3, r,g,b[,a])` — one raw triangle.
                {
                    let q = draw_tris.clone();
                    type TriArgs =
                        (f64, f64, f64, f64, f64, f64, f64, f64, f64, f32, f32, f32, Option<f32>);
                    if let Ok(f) = lua.create_function(
                        move |_,
                              (x1, y1, z1, x2, y2, z2, x3, y3, z3, r, g, b, a): TriArgs| {
                            q.borrow_mut().push(crate::DrawTri {
                                a: [x1, y1, z1],
                                b: [x2, y2, z2],
                                c: [x3, y3, z3],
                                color: [r, g, b, a.unwrap_or(1.0)],
                            });
                            Ok(())
                        },
                    ) {
                        let _ = t.set("tri", f);
                    }
                }
                // A basis ⊥ to a direction, for fan-tessellating cones/discs.
                let basis = |n: glam::DVec3| {
                    let n = n.try_normalize().unwrap_or(glam::DVec3::Y);
                    let u = if n.x.abs() < 0.9 { glam::DVec3::X } else { glam::DVec3::Z };
                    let u = (u - n * u.dot(n)).normalize();
                    (u, n.cross(u), n)
                };
                // `draw.cone(bx,by,bz, dx,dy,dz, radius, height, r,g,b[,a])` — a
                // solid cone: base disc at (bx,by,bz), apex `height` along the
                // unit dir. Gizmo arrowheads, thruster nozzles, markers.
                {
                    let q = draw_tris.clone();
                    type ConeArgs =
                        (f64, f64, f64, f64, f64, f64, f64, f64, f32, f32, f32, Option<f32>);
                    if let Ok(f) = lua.create_function(
                        move |_,
                              (bx, by, bz, dx, dy, dz, radius, height, r, g, b, a): ConeArgs| {
                            let base = glam::DVec3::new(bx, by, bz);
                            let (u, v, n) = basis(glam::DVec3::new(dx, dy, dz));
                            let apex = base + n * height;
                            let col = [r, g, b, a.unwrap_or(1.0)];
                            const N: usize = 20;
                            let mut q = q.borrow_mut();
                            let rim = |k: usize| {
                                let t = k as f64 / N as f64 * std::f64::consts::TAU;
                                base + u * (radius * t.cos()) + v * (radius * t.sin())
                            };
                            for k in 0..N {
                                let p0 = rim(k);
                                let p1 = rim(k + 1);
                                // side
                                q.push(crate::DrawTri {
                                    a: p0.into(),
                                    b: p1.into(),
                                    c: apex.into(),
                                    color: col,
                                });
                                // base cap
                                q.push(crate::DrawTri {
                                    a: p1.into(),
                                    b: p0.into(),
                                    c: base.into(),
                                    color: col,
                                });
                            }
                            Ok(())
                        },
                    ) {
                        let _ = t.set("cone", f);
                    }
                }
                // `draw.disc(cx,cy,cz, nx,ny,nz, r0, r1, r,g,b[,a])` — a filled
                // annulus (r0=inner, r1=outer) around normal n: solid rotation
                // gizmo bands, ring markers. r0=0 gives a full disc.
                {
                    let q = draw_tris.clone();
                    type DiscArgs =
                        (f64, f64, f64, f64, f64, f64, f64, f64, f32, f32, f32, Option<f32>);
                    if let Ok(f) = lua.create_function(
                        move |_,
                              (cx, cy, cz, nx, ny, nz, r0, r1, r, g, b, a): DiscArgs| {
                            let c = glam::DVec3::new(cx, cy, cz);
                            let (u, v, _) = basis(glam::DVec3::new(nx, ny, nz));
                            let col = [r, g, b, a.unwrap_or(1.0)];
                            const N: usize = 36;
                            let mut q = q.borrow_mut();
                            let at = |rad: f64, k: usize| {
                                let t = k as f64 / N as f64 * std::f64::consts::TAU;
                                c + u * (rad * t.cos()) + v * (rad * t.sin())
                            };
                            for k in 0..N {
                                let o0 = at(r1, k);
                                let o1 = at(r1, k + 1);
                                let i0 = at(r0, k);
                                let i1 = at(r0, k + 1);
                                q.push(crate::DrawTri {
                                    a: i0.into(),
                                    b: o0.into(),
                                    c: o1.into(),
                                    color: col,
                                });
                                q.push(crate::DrawTri {
                                    a: i0.into(),
                                    b: o1.into(),
                                    c: i1.into(),
                                    color: col,
                                });
                            }
                            Ok(())
                        },
                    ) {
                        let _ = t.set("disc", f);
                    }
                }
                // ---- screen space -------------------------------------
                // `draw.rect(x, y, w, h, r,g,b[,a][,radius])` — a filled
                // rectangle in PIXELS, and `draw.rectOutline(..., [thickness])`
                // its hollow twin. The pixels are `input.mouse()`'s, so an RTS
                // marquee is the two corners you dragged between — the 3D line
                // version of the same box has to be projected onto a ground
                // plane, which fights the camera angle and misses anything the
                // plane doesn't pass through.
                for (name, outline_default) in [("rect", 0.0f32), ("rectOutline", 2.0f32)] {
                    let q = draw_rects.clone();
                    type RectArgs =
                        (f32, f32, f32, f32, f32, f32, f32, Option<f32>, Option<f32>);
                    if let Ok(f) = lua.create_function(
                        move |_, (x, y, w, h, r, g, b, a, extra): RectArgs| {
                            // `extra` is the corner radius on a fill, the border
                            // thickness on an outline — the one number each wants.
                            let (outline, radius) = if outline_default > 0.0 {
                                (extra.unwrap_or(outline_default).max(0.0), 0.0)
                            } else {
                                (0.0, extra.unwrap_or(0.0).max(0.0))
                            };
                            q.borrow_mut().push(crate::DrawRect {
                                rect: [x, y, w, h],
                                color: [r, g, b, a.unwrap_or(1.0)],
                                outline,
                                radius,
                            });
                            Ok(())
                        },
                    ) {
                        let _ = t.set(name, f);
                    }
                }
                // `draw.circle(x, y, radius, r,g,b[,a])` and its outline twin —
                // a rect with a corner radius of half its side IS a circle to
                // the UI quad shader, so a debug ring, a minimap blip or a
                // reticle costs nothing new. `x, y` is the CENTRE, which is what
                // anyone drawing a circle has in hand.
                for (name, outline_default) in [("circle", 0.0f32), ("circleOutline", 2.0f32)] {
                    let q = draw_rects.clone();
                    type CircleArgs = (f32, f32, f32, f32, f32, f32, Option<f32>, Option<f32>);
                    if let Ok(f) = lua.create_function(
                        move |_, (x, y, rad, r, g, b, a, extra): CircleArgs| {
                            let rad = rad.max(0.0);
                            let outline = if outline_default > 0.0 {
                                extra.unwrap_or(outline_default).max(0.0)
                            } else {
                                0.0
                            };
                            q.borrow_mut().push(crate::DrawRect {
                                rect: [x - rad, y - rad, rad * 2.0, rad * 2.0],
                                color: [r, g, b, a.unwrap_or(1.0)],
                                outline,
                                radius: rad,
                            });
                            Ok(())
                        },
                    ) {
                        let _ = t.set(name, f);
                    }
                }
                // `draw.text(x, y, s, size, r,g,b[,a][,align])` — a string on
                // the screen without building a UI tree: a damage number, a
                // frame-time readout, the count under a selection box. The
                // renderer measures and lays out the glyphs (the same font
                // stack `ui.make` uses), so a script never has to know how wide
                // an 'm' is. `align` is "left" (default) | "center" | "right",
                // and x is that edge.
                {
                    let q = draw_texts.clone();
                    type TextArgs = (
                        f32,
                        f32,
                        String,
                        Option<f32>,
                        Option<f32>,
                        Option<f32>,
                        Option<f32>,
                        Option<f32>,
                        Option<String>,
                        Option<String>,
                    );
                    if let Ok(f) = lua.create_function(
                        move |_, (x, y, s, size, r, g, b, a, align, font): TextArgs| {
                            q.borrow_mut().push(crate::DrawText {
                                pos: [x, y],
                                text: s,
                                size: size.unwrap_or(16.0).max(1.0),
                                color: [
                                    r.unwrap_or(1.0),
                                    g.unwrap_or(1.0),
                                    b.unwrap_or(1.0),
                                    a.unwrap_or(1.0),
                                ],
                                align: match align.as_deref() {
                                    Some("center") | Some("centre") => 1,
                                    Some("right") => 2,
                                    _ => 0,
                                },
                                // Absent = the project's UI font, which is the
                                // answer a game wants often enough that naming
                                // it here should be the exception (`floptle/0124`).
                                font: font.unwrap_or_default(),
                            });
                            Ok(())
                        },
                    ) {
                        let _ = t.set("text", f);
                    }
                }
                let _ = lua.globals().set("draw", t);
            }
        }

        // `spawn(prefab [, pos [, fn]])` — queue a prefab instance. The driver
        // spawns the subtree after this pass (physics/animators/scripts wire up
        // automatically); the optional callback receives the new root's handle
        // right after it exists — the "configure what I just spawned" hook:
        //   spawn("bullet", node.pos + dir, function(b) b.vx = dir.x * 40 end)
        let spawn_requests: Rc<RefCell<Vec<crate::SpawnRequest>>> =
            Rc::new(RefCell::new(Vec::new()));
        {
            let q = spawn_requests.clone();
            if let Ok(f) = lua.create_function(
                move |lua, (name, a, b, c): (String, Value, Value, Value)| {
                    let (mut pos, mut cb) = (None, None);
                    for v in [a, b] {
                        match v {
                            Value::Nil => {}
                            Value::Function(f) => cb = Some(lua.create_registry_value(f)?),
                            other => match crate::math_api::vec3_of(&other) {
                                Some(p) => pos = Some([p.x, p.y, p.z]),
                                None => {
                                    return Err(mlua::Error::runtime(
                                        "spawn(prefab [, pos [, fn]]): pos must be a vec3/node and fn a function",
                                    ))
                                }
                            },
                        }
                    }
                    // Optional 4th arg: a PARENT node — the spawned subtree
                    // lands under it (still at the world `pos`).
                    let parent = match &c {
                        Value::Table(t) => t.raw_get::<u32>("__id").ok(),
                        _ => None,
                    };
                    q.borrow_mut().push(crate::SpawnRequest { prefab: name, pos, cb, parent });
                    Ok(())
                },
            ) {
                let _ = lua.globals().set("spawn", f);
            }
        }
        // `createNode(name [, parentNode] [, fn])` — queue a PLAIN node (Empty
        // matter, identity transform). The driver creates it after this pass;
        // the callback receives its handle — combine with `setTerrain`/
        // `setCelestial`/`setPrimitive`/`setMaterial` to build content from
        // script (the editor-action construction kit):
        //   createNode("Oria", function(n) n:setTerrain(2); n.x = 500 end)
        let create_requests: Rc<RefCell<Vec<crate::CreateRequest>>> =
            Rc::new(RefCell::new(Vec::new()));
        {
            let q = create_requests.clone();
            if let Ok(f) = lua.create_function(
                move |lua, (name, a, b): (String, Value, Value)| {
                    let (mut parent, mut cb) = (None, None);
                    for v in [a, b] {
                        match v {
                            Value::Nil => {}
                            Value::Function(f) => cb = Some(lua.create_registry_value(f)?),
                            Value::Table(t) => match t.raw_get::<Option<u32>>("__id")? {
                                Some(id) => parent = Some(id),
                                None => {
                                    return Err(mlua::Error::runtime(
                                        "createNode(name [, parent] [, fn]): parent must be a node handle",
                                    ))
                                }
                            },
                            _ => {
                                return Err(mlua::Error::runtime(
                                    "createNode(name [, parent] [, fn]): bad argument",
                                ))
                            }
                        }
                    }
                    q.borrow_mut().push(crate::CreateRequest { name, parent, cb });
                    Ok(())
                },
            ) {
                let _ = lua.globals().set("createNode", f);
            }
        }
        // `destroy(node)` — queue a node (and its whole subtree) for removal.
        // Also available as `node:destroy()` (installed with the handle API).
        let destroy_queue: Rc<RefCell<Vec<u32>>> = Rc::new(RefCell::new(Vec::new()));
        {
            let q = destroy_queue.clone();
            if let Ok(f) = lua.create_function(move |_, v: Value| {
                let eid = match &v {
                    Value::Table(t) => t.raw_get::<u32>("__id").ok(),
                    _ => None,
                };
                match eid {
                    Some(id) => {
                        q.borrow_mut().push(id);
                        Ok(())
                    }
                    None => Err(mlua::Error::runtime("destroy(node): pass a node or node handle")),
                }
            }) {
                let _ = lua.globals().set("destroy", f);
            }
        }

        // The cross-node / cross-script reference layer: a scene-graph mirror plus Lua
        // `node`/`script` handles and the `find`/`findScript` globals (see
        // `install_handle_api`). Shared (interior-mutable) with the handle closures.
        let shared = Shared {
            destroy_queue: destroy_queue.clone(),
            scene: Rc::new(RefCell::new(SceneMirror::default())),
            bodies: Rc::new(RefCell::new(HashMap::new())),
            ui_rects: Rc::new(RefCell::new(HashMap::new())),
            body_changes: Rc::new(RefCell::new(HashMap::new())),
            body_height_changes: Rc::new(RefCell::new(HashMap::new())),
            body_pos_changes: Rc::new(RefCell::new(HashMap::new())),
            sprite_draws: Rc::new(RefCell::new(HashMap::new())),
            shader_param_sets: Rc::new(RefCell::new(Vec::new())),
            shader_texture_sets: Rc::new(RefCell::new(Vec::new())),
            screen_shader_toggles: Rc::new(RefCell::new(Vec::new())),
            envs: Rc::new(RefCell::new(HashMap::new())),
            model_changes: Rc::new(RefCell::new(HashMap::new())),
            material_changes: Rc::new(RefCell::new(HashMap::new())),
            visible_changes: Rc::new(RefCell::new(HashMap::new())),
            enabled_changes: Rc::new(RefCell::new(HashMap::new())),
            persistent_changes: Rc::new(RefCell::new(HashMap::new())),
            layer_changes: Rc::new(RefCell::new(HashMap::new())),
            tag_changes: Rc::new(RefCell::new(HashMap::new())),
            layer_table: layer_table.clone(),
            ui_text_changes: Rc::new(RefCell::new(HashMap::new())),
            ui_style_changes: Rc::new(RefCell::new(HashMap::new())),
            ui_focus: ui_focus.clone(),
            component_changes: Rc::new(RefCell::new(HashMap::new())),
            component_colors: Rc::new(RefCell::new(HashMap::new())),
            component_strs: Rc::new(RefCell::new(HashMap::new())),
            rich_sets: Rc::new(RefCell::new(Vec::new())),
            anim_info: Rc::new(RefCell::new(HashMap::new())),
            anim_commands: Rc::new(RefCell::new(Vec::new())),
            vfx_info: Rc::new(RefCell::new(HashMap::new())),
            vfx_commands: Rc::new(RefCell::new(Vec::new())),
            broken: Rc::new(RefCell::new(std::collections::HashSet::new())),
            broken_read_warned: Rc::new(RefCell::new(std::collections::HashSet::new())),
            find_scope_warned: Rc::new(RefCell::new(std::collections::HashSet::new())),
            logs: logs.clone(),
        };
        if let Err(e) = install_handle_api(&lua, &shared) {
            eprintln!("[lua] failed to install the node/script reference API: {e}");
        }
        // Vector math: `vec3`/`vec2` value types + the `distance` global.
        if let Err(e) = crate::math_api::install(&lua) {
            eprintln!("[lua] failed to install the vector math API: {e}");
        }
        // `perf.*` — a game reading its own frame cost (`floptle/0077`). Off by
        // default and free while off, so this costs nothing but the table.
        let profile: crate::SharedProfile = Rc::new(RefCell::new(Default::default()));
        if let Err(e) = crate::perf_api::install(&lua, &profile) {
            eprintln!("[lua] failed to install the perf API: {e}");
        }
        // `access.*` + `caption(...)` — the accessibility surface a game offers
        // its players (`floptle/0079`).
        let access: crate::access_api::SharedAccess =
            Rc::new(RefCell::new(floptle_core::access::Accessibility::default()));
        let caption_queue: crate::access_api::CaptionQueue = Rc::new(RefCell::new(Vec::new()));
        if let Err(e) = crate::access_api::install(&lua, &access, &caption_queue) {
            eprintln!("[lua] failed to install the access API: {e}");
        }
        // The `audio` API (one-shots, sound handles, mixer tracks) + `node:sound()`.
        // Must come after the handle API: it extends the node methods table.
        let audio_bridges = crate::audio_api::AudioBridges {
            commands: Rc::new(RefCell::new(Vec::new())),
            info: Rc::new(RefCell::new(crate::AudioInfo::default())),
            next_handle: Rc::new(RefCell::new(0)),
        };
        if let Err(e) = crate::audio_api::install_audio_api(&lua, &audio_bridges) {
            eprintln!("[lua] failed to install the audio API: {e}");
        }
        // The `net.*` API (docs/netcode-design.md §8): command queue out,
        // session state in, `net.on` handler registry, `net.rewind` (§7).
        let synced_stores: Rc<RefCell<HashMap<(u32, String), Table>>> =
            Rc::new(RefCell::new(HashMap::new()));
        // `http.*` / `json.*` / `openUrl` (proposal §4): requests run on worker
        // threads, replies are delivered in the FRAME pass on this thread.
        let http: Rc<RefCell<crate::http_api::HttpState>> =
            Rc::new(RefCell::new(crate::http_api::HttpState::new()));
        let http_in_fixed: Rc<std::cell::Cell<bool>> = Rc::new(std::cell::Cell::new(false));
        crate::http_api::install_http_api(
            &lua,
            http.clone(),
            logs.clone(),
            http_in_fixed.clone(),
        );
        // `account.*` (task 0055 / 0054): the same worker-thread + frame-pass
        // shape, against fopull.com only, with the token kept in Rust.
        let account: Rc<RefCell<crate::account_api::AccountState>> =
            Rc::new(RefCell::new(crate::account_api::AccountState::new()));
        crate::account_api::install_account_api(
            &lua,
            account.clone(),
            logs.clone(),
            http_in_fixed.clone(),
        );

        // The rollback `replaying` flag (§4) — shared so `net.replaying()` can
        // answer it, which is the escape hatch for any cosmetic the engine's
        // queue gating can't see (a script writing a material, say).
        let replaying: Rc<std::cell::Cell<bool>> = Rc::new(std::cell::Cell::new(false));
        if let Err(e) = crate::net_api::install_net_api(
            &lua,
            &net,
            &hulls,
            &sim_origin,
            &synced_stores,
            &replaying,
        ) {
            eprintln!("[lua] failed to install the net API: {e}");
        }
        // The `terrain.*` API (Terrain 2.0 P6): writes queue TerrainOps the editor
        // drains after the script pass; reads run against the lent colliders.
        let terrain_ops: Rc<RefCell<Vec<crate::terrain_api::TerrainOp>>> =
            Rc::new(RefCell::new(Vec::new()));
        let terrain_generates: Rc<RefCell<Vec<(u32, floptle_field::procgen::PlanetFill)>>> =
            Rc::new(RefCell::new(Vec::new()));
        // Save-slot terrain persistence (G2): the game sets terrain.saveDir(path)
        // and the residency streamer prefers/writes player-edited fields there.
        let terrain_save_dir: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let terrain_warm: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let terrain_flush: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));
        let terrain_busy: Rc<std::cell::Cell<bool>> = Rc::new(std::cell::Cell::new(false));
        let terrain_yields: Rc<RefCell<Vec<crate::terrain_api::TerrainYield>>> =
            Rc::new(RefCell::new(Vec::new()));
        let terrain_op_id: Rc<std::cell::Cell<u64>> = Rc::new(std::cell::Cell::new(0));
        crate::terrain_api::install_terrain_api(
            &lua,
            terrain_ops.clone(),
            terrain_generates.clone(),
            colliders.clone(),
            logs.clone(),
            crate::terrain_api::TerrainStreamShared {
                save_dir: terrain_save_dir.clone(),
                warm: terrain_warm.clone(),
                flush: terrain_flush.clone(),
                busy: terrain_busy.clone(),
                root: project_root.clone(),
            },
            crate::terrain_api::TerrainReceipts {
                yields: terrain_yields.clone(),
                next_op_id: terrain_op_id.clone(),
            },
        );
        // The `save.*` persistent store (roadmap A2).
        let save_state: Rc<RefCell<crate::save_api::SaveState>> =
            Rc::new(RefCell::new(crate::save_api::SaveState::default()));
        crate::save_api::install_save_api(
            &lua,
            save_state.clone(),
            project_root.clone(),
            logs.clone(),
        );
        // The `after`/`every`/`tween` scheduler (roadmap A4).
        let sched: Rc<RefCell<crate::sched_api::SchedState>> =
            Rc::new(RefCell::new(crate::sched_api::SchedState::default()));
        crate::sched_api::install_sched_api(&lua, sched.clone());
        // The `space.*` orbital readouts (solar demo S2).
        let space_info: Rc<RefCell<crate::space_api::SpaceInfo>> =
            Rc::new(RefCell::new(crate::space_api::SpaceInfo::default()));
        let warp_request: Rc<RefCell<Option<f64>>> = Rc::new(RefCell::new(None));
        crate::space_api::install_space_api(&lua, space_info.clone(), warp_request.clone());
        // The `nav.*` pathfinding surface, reading whatever navmesh the open
        // scene baked. Empty until one is loaded, which is the ordinary state
        // of a project that has not made one.
        let nav_mesh: crate::nav_api::NavShared = Rc::new(RefCell::new(None));
        let nav_agents: crate::nav_api::AgentsShared = Rc::new(RefCell::new(Default::default()));
        // `nav.rebake` cannot act here: re-measuring a box needs the world's
        // triangles, which only the editor has. It queues, like `spawn`.
        let nav_rebakes: Rc<RefCell<Vec<crate::NavRebakeRequest>>> = Rc::new(RefCell::new(Vec::new()));
        crate::nav_api::install_nav_api(
            &lua,
            nav_mesh.clone(),
            nav_agents.clone(),
            shared.scene.clone(),
            nav_rebakes.clone(),
        );
        // The `physics.*` sim controls: pause/resume the whole physics step
        // while scripts keep running (loading screens, cutscenes, pause menus).
        let physics_pause_request: Rc<RefCell<Option<bool>>> = Rc::new(RefCell::new(None));
        let physics_paused: Rc<std::cell::Cell<bool>> = Rc::new(std::cell::Cell::new(false));
        let frame_step_request: Rc<std::cell::Cell<u32>> = Rc::new(std::cell::Cell::new(0));
        {
            let t = lua.create_table().expect("physics table");
            let req = physics_pause_request.clone();
            let f = lua
                .create_function(move |_, on: bool| {
                    *req.borrow_mut() = Some(on);
                    Ok(())
                })
                .expect("physics.pause");
            t.set("pause", f).expect("physics.pause");
            let mirror = physics_paused.clone();
            let f = lua
                .create_function(move |_, ()| Ok(mirror.get()))
                .expect("physics.isPaused");
            t.set("isPaused", f).expect("physics.isPaused");
            // `physics.step([n])` — frame-step the whole gameplay tick, the same thing
            // the editor's ⏭ button does, so a game can build its own training-mode
            // stepper. Freezing first is implied: advancing one frame while running
            // isn't a thing. Call it from `update` (the frame pass still runs while the
            // tick is frozen); a `fixedUpdate` caller would never get a second turn.
            let steps = frame_step_request.clone();
            let f = lua
                .create_function(move |_, n: Option<u32>| {
                    let n = n.unwrap_or(1).min(600);
                    steps.set(steps.get().saturating_add(n));
                    Ok(())
                })
                .expect("physics.step");
            t.set("step", f).expect("physics.step");
            lua.globals().set("physics", t).expect("physics global");
        }
        // The `assembly.*` API: compound-vessel forces/splits out, per-frame
        // assembly mirror in (SC1 ship physics surface).
        let assembly_info: Rc<RefCell<HashMap<u32, crate::assembly_api::AssemblyInfo>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let assembly_impacts: Rc<
            RefCell<HashMap<u32, Vec<crate::assembly_api::AssemblyImpact>>>,
        > = Rc::new(RefCell::new(HashMap::new()));
        let assembly_cmds: Rc<RefCell<Vec<crate::assembly_api::AssemblyCmd>>> =
            Rc::new(RefCell::new(Vec::new()));
        if let Err(e) = crate::assembly_api::install_assembly_api(
            &lua,
            assembly_info.clone(),
            assembly_impacts.clone(),
            assembly_cmds.clone(),
        ) {
            eprintln!("[lua] failed to install the assembly API: {e}");
        }
        // The `camera.*` world→screen API (map click-on-line picking).
        let view_info: Rc<RefCell<crate::view_api::ViewInfo>> =
            Rc::new(RefCell::new(crate::view_api::ViewInfo::default()));
        crate::view_api::install_camera_api(&lua, view_info.clone());

        Self {
            lua,
            extra_script_dirs: Vec::new(),
            sources: HashMap::new(),
            instances: HashMap::new(),
            errors: Vec::new(),
            logs,
            input,
            input_sys,
            input_domain,
            bodies: shared.bodies.clone(),
            ui_rects: shared.ui_rects.clone(),
            body_changes: shared.body_changes.clone(),
            body_height_changes: shared.body_height_changes.clone(),
            body_pos_changes: shared.body_pos_changes.clone(),
            sprite_draws: shared.sprite_draws.clone(),
            sprites_written: None,
            shader_param_sets: shared.shader_param_sets.clone(),
            shader_texture_sets: shared.shader_texture_sets.clone(),
            screen_shader_toggles: shared.screen_shader_toggles.clone(),
            colliders,
            hulls,
            sim_origin,
            terrain_ops,
            terrain_yields,
            terrain_generates,
            terrain_save_dir,
            terrain_warm,
            terrain_flush,
            terrain_busy,
            create_requests,
            rich_sets: shared.rich_sets.clone(),
            scene: shared.scene.clone(),
            envs: shared.envs.clone(),
            broken: shared.broken.clone(),
            broken_read_warned: shared.broken_read_warned.clone(),
            model_changes: shared.model_changes.clone(),
            material_changes: shared.material_changes.clone(),
            visible_changes: shared.visible_changes.clone(),
            enabled_changes: shared.enabled_changes.clone(),
            persistent_changes: shared.persistent_changes.clone(),
            layer_changes: shared.layer_changes.clone(),
            tag_changes: shared.tag_changes.clone(),
            layer_table,
            ui_text_changes: shared.ui_text_changes.clone(),
            ui_style_changes: shared.ui_style_changes.clone(),
            component_changes: shared.component_changes.clone(),
            component_colors: shared.component_colors.clone(),
            component_strs: shared.component_strs.clone(),
            materials: Rc::new(RefCell::new(HashMap::new())),
            project_root,
            save_state,
            sched,
            space_info,
            nav_mesh,
            nav_agents,
            view_info,
            warp_request,
            physics_pause_request,
            frame_step_request,
            physics_paused,
            mouse_lock,
            reserved_keys,
            profile,
            access,
            caption_queue,
            param_writes: RefCell::new(Vec::new()),
            scene_request,
            scene_loaded,
            water_volumes,
            water_freeze,
            scatter_sources,
            scene_name,
            ui_focus,
            ui_focus_request,
            ui_drag,
            ui_bindings,
            ui_makes,
            ui_handlers,
            ui_listeners,
            ui_listener_checks,
            ui_frame_events,
            ui_hover,
            ui_active,
            anim_info: shared.anim_info.clone(),
            anim_commands: shared.anim_commands.clone(),
            vfx_info: shared.vfx_info.clone(),
            vfx_commands: shared.vfx_commands.clone(),
            audio_commands: audio_bridges.commands.clone(),
            audio_info: audio_bridges.info.clone(),
            gizmos,
            spawn_effects,
            spawn_requests,
            nav_rebakes,
            assembly_info,
            assembly_impacts,
            assembly_cmds,
            draw_lines,
            draw_tris,
            draw_rects,
            draw_texts,
            destroy_queue,
            net,
            synced_stores,
            synced_warned: std::collections::HashSet::new(),
            param_warned: std::collections::HashSet::new(),
            handle_key_warned: std::collections::HashSet::new(),
            load_failure_reported: std::collections::HashSet::new(),
            upvalue_warned: std::collections::HashSet::new(),
            script_skip: std::collections::HashSet::new(),
            frame_skip: std::collections::HashSet::new(),
            driver_skip: std::collections::HashSet::new(),
            replaying,
            replay_marks: None,
            http,
            account,
            http_in_fixed,
        }
    }

    /// Every name the engine adds to a script's environment, as dotted paths
    /// (`water.depthAt`, `math.lerp`, `vec3`, …), sorted.
    ///
    /// Derived by **diffing a live host's globals against a bare `Lua`**, so it
    /// cannot drift from what scripts can actually call. Anything a hand-kept
    /// list would miss — a function added without a doc line, a table installed
    /// by a subsystem nobody remembered — shows up here the moment it exists.
    ///
    /// That is the point: `docs/lua-api.md` is checked against this, so a new
    /// API is undocumented for exactly as long as it takes `cargo test` to run.
    /// It reports only what is reachable by *name*; methods that live on a
    /// handle's metatable (`node:animator()`, the component handles) are
    /// userdata and are covered by the annotation list in the editor instead.
    pub fn api_surface() -> Vec<String> {
        let bare = Lua::new();
        let base = flatten(&bare.globals(), "", &mut std::collections::HashSet::new(), 0);
        let host = Self::new();
        let all = flatten(&host.lua.globals(), "", &mut std::collections::HashSet::new(), 0);

        let base: std::collections::HashSet<String> = base.into_iter().collect();
        let mut out: Vec<String> = all.into_iter().filter(|k| !base.contains(k)).collect();
        out.sort();
        out.dedup();
        out
    }

    /// Every method a node handle answers to — `setSprite`, `sorting`, `shake`…
    ///
    /// [`api_surface`](Self::api_surface) walks the GLOBALS, so no `node:` method
    /// has ever been in it: the whole handle surface was covered by no test at
    /// all, and six 2D bindings were in the reference only because somebody
    /// typed them there. Read off the metatable the handles actually use, so a
    /// method that ships and is not documented is a build failure like any other.
    pub fn node_methods() -> Vec<String> {
        let host = Self::new();
        let mut out: Vec<String> = Vec::new();
        if let Ok(mt) = host.lua.named_registry_value::<mlua::Table>("floptle_node_methods") {
            for pair in mt.pairs::<mlua::Value, mlua::Value>().flatten() {
                if let (mlua::Value::String(k), mlua::Value::Function(_)) = pair {
                    out.push(k.to_string_lossy().to_string());
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// Run one line of Lua against a real host, with `node` bound to a node
    /// handle — the harness `opts::TABLES`' guard test uses to call every option
    /// table for real (`floptle/0082`).
    ///
    /// A real host, not a bare `Lua`: the whole point of that test is that the
    /// check runs in the code a game reaches, and a hand-built table proves
    /// nothing about the call that ships.
    #[cfg(test)]
    pub(crate) fn eval_for_test(src: &str) -> Result<(), String> {
        let host = Self::new();
        // Play mode, so the calls that are edit-mode-gated (http, account) reach
        // their option parsing instead of refusing for a different reason.
        host.set_playing(true);
        let handle = new_node_handle(&host.lua, 1).map_err(|e| e.to_string())?;
        host.lua.globals().set("node", handle).map_err(|e| e.to_string())?;
        host.lua.load(src).exec().map_err(|e| e.to_string())
    }

    /// The player's accessibility settings as they stand — a game's options menu
    /// writes them from Lua, and the driver honours them (`floptle/0079`).
    pub fn access(&self) -> floptle_core::access::Accessibility {
        self.access.borrow().clamped()
    }

    /// Push settings IN, so the editor's ⚙ Settings and a game's own menu drive
    /// one set of values rather than two that disagree.
    pub fn set_access(&self, a: floptle_core::access::Accessibility) {
        *self.access.borrow_mut() = a.clamped();
    }

    /// Take the captions `caption(...)` asked for this frame.
    ///
    /// Drained, like every immediate-mode queue here: a caption the driver has
    /// already shown must not be shown again next frame.
    pub fn take_captions(&self) -> Vec<crate::access_api::Caption> {
        std::mem::take(&mut *self.caption_queue.borrow_mut())
    }

    /// The package script folders a script name may also resolve in, after the
    /// project's own. Set when the project's packages load; cleared with them.
    pub fn set_extra_script_dirs(&mut self, dirs: Vec<std::path::PathBuf>) {
        self.extra_script_dirs = dirs;
    }

    /// Feed the running scene's name (before `run`) — what `scene.current()` reads.
    pub fn set_scene_name(&self, name: &str) {
        let mut cur = self.scene_name.borrow_mut();
        if *cur != name {
            *cur = name.to_string();
        }
    }

    /// Drain the `scene.load` / `scene.unload` requests queued this frame, in
    /// the order they were made. The driver performs them between frames.
    pub fn take_scene_requests(&mut self) -> Vec<crate::SceneRequest> {
        std::mem::take(&mut *self.scene_request.borrow_mut())
    }

    /// Fire every live `scene.onLoaded(fn)` subscription with the name of the
    /// scene that just finished loading and whether it arrived additively.
    ///
    /// Runs AFTER the world is whole — a loading screen's whole job is to go
    /// away once the thing it was covering exists, so being told early would be
    /// worse than not being told.
    pub fn fire_scene_loaded(&mut self, world: &mut World, name: &str, additive: bool) {
        let subs: Vec<(u32, mlua::RegistryKey)> =
            std::mem::take(&mut *self.scene_loaded.borrow_mut());
        if subs.is_empty() {
            return;
        }
        self.sync_scene(world);
        let mut kept = Vec::with_capacity(subs.len());
        for (owner, key) in subs {
            // A subscription whose owning script no longer exists is dead
            // weight — the swap it was waiting for is the thing that took it.
            if !self.envs.borrow().keys().any(|(e, _)| *e == owner) {
                let _ = self.lua.remove_registry_value(key);
                continue;
            }
            let Ok(f) = self.lua.registry_value::<mlua::Function>(&key) else { continue };
            if let Err(err) = f.call::<()>((name, additive)) {
                self.record_error("scene", format!("scene.onLoaded: {err}"));
            }
            kept.push((owner, key));
        }
        self.scene_loaded.borrow_mut().extend(kept);
        self.flush_writes(world);
    }

    /// The scatter sources scripts have declared — what the driver resolves
    /// into instances and draws each frame.
    pub fn scatter_sources(&self) -> std::cell::Ref<'_, Vec<floptle_core::scatter::ScatterSource>> {
        self.scatter_sources.borrow()
    }

    /// Tell a source where its anchor node has got to (`floptle/0073`).
    ///
    /// Called once a frame by the driver, before the sources are drawn or
    /// queried. This is the ONLY thing that changes when a planet orbits — every
    /// id, every local position, every settled height and every removal is
    /// expressed in the frame this describes, so none of them move.
    pub fn set_scatter_frame(&self, id: u32, frame: floptle_core::scatter::Frame) {
        if let Some(s) = self.scatter_sources.borrow_mut().iter_mut().find(|s| s.id == id) {
            s.frame = frame;
        }
    }

    /// Every source that rides a node, as `(id, node name)`.
    pub fn anchored_scatter(&self) -> Vec<(u32, String)> {
        self.scatter_sources
            .borrow()
            .iter()
            .filter_map(|s| s.anchor.clone().map(|a| (s.id, a)))
            .collect()
    }

    /// Drop every scatter source — a SCENE SWITCH. A source names a region of
    /// the world that is about to stop existing.
    pub fn clear_scatter(&mut self) {
        self.scatter_sources.borrow_mut().clear();
    }

    /// Publish the scene's bodies of water, before scripts run — what
    /// `water.depthAt` and friends answer from.
    pub fn set_water_volumes(&mut self, v: Vec<crate::water_api::WaterInfo>) {
        *self.water_volumes.borrow_mut() = v;
    }

    /// Drain `water.setFrozen(node, on)` calls made this frame.
    pub fn take_water_freezes(&mut self) -> Vec<(u32, bool)> {
        std::mem::take(&mut *self.water_freeze.borrow_mut())
    }

    /// Publish the engine's current UI focus, before scripts run.
    pub fn set_ui_focus(&mut self, focused: Option<u32>) {
        *self.ui_focus.borrow_mut() = focused;
    }

    /// Drain a `ui.focus(...)` call made this frame (last one wins). `Some(None)`
    /// is a script explicitly asking for NOTHING to be focused, which is
    /// different from not asking.
    pub fn take_ui_focus_request(&mut self) -> Option<Option<u32>> {
        self.ui_focus_request.borrow_mut().take()
    }

    /// Publish the drag in flight, before scripts run.
    pub fn set_ui_drag(&mut self, drag: Option<(u32, Option<u32>)>) {
        *self.ui_drag.borrow_mut() = drag;
    }

    /// Publish this frame's UI events (`(element, hook)`) and the elements the
    /// pointer is over / holding down, BEFORE the scripts run — what
    /// `ui.clicked(el)`, `ui.events()`, `ui.hovered()` and `ui.held()` read.
    ///
    /// Early on purpose: these are the same events dispatched as hooks after
    /// the run, so a script that polls in `update` and a script that answers a
    /// `clicked` hook are looking at one frame's truth, not two.
    pub fn set_ui_frame_state(
        &mut self,
        events: &[(u32, &'static str)],
        hover: Option<u32>,
        held: Option<u32>,
    ) {
        let mut ev = self.ui_frame_events.borrow_mut();
        ev.clear();
        ev.extend(events.iter().map(|(e, h)| (*e, (*h).to_string())));
        *self.ui_hover.borrow_mut() = hover;
        *self.ui_active.borrow_mut() = held;
    }

    /// Evaluate every live `ui.bind` and queue what it returned.
    ///
    /// A binding whose node no longer exists is dropped silently — a screen
    /// closing should not be an error. A binding whose function THROWS is
    /// dropped **loudly, once**: left in place it would report the same
    /// failure sixty times a second and bury everything else in the Console.
    fn run_ui_bindings(&mut self) {
        if self.ui_bindings.borrow().is_empty() {
            return;
        }
        let mut dead: Vec<usize> = Vec::new();
        let mut failures: Vec<String> = Vec::new();
        {
            let binds = self.ui_bindings.borrow();
            let scene = self.scene.borrow();
            for (i, b) in binds.iter().enumerate() {
                if !scene.ents.contains_key(&b.e) {
                    dead.push(i);
                    continue;
                }
                let Ok(f) = self.lua.registry_value::<mlua::Function>(&b.f) else {
                    dead.push(i);
                    continue;
                };
                match f.call::<mlua::Value>(()) {
                    Ok(v) => self.apply_binding(&scene, b.e, &b.prop, v),
                    Err(err) => {
                        failures.push(format!("ui.bind({}): {err}", b.prop));
                        dead.push(i);
                    }
                }
            }
        }
        for i in dead.into_iter().rev() {
            let b = self.ui_bindings.borrow_mut().remove(i);
            let _ = self.lua.remove_registry_value(b.f);
        }
        for msg in failures {
            self.record_error("ui.bind", msg);
        }
    }

    /// Route one binding result onto the right write channel.
    ///
    /// The property name decides nothing on its own — `value` belongs to a
    /// slider and `opacity` to an element — so the component is picked by
    /// asking the mirror which one actually has that field. A binding written
    /// against the wrong component then does nothing instead of quietly
    /// writing to a field of the same name somewhere else.
    fn apply_binding(&self, scene: &crate::SceneMirror, e: u32, prop: &str, v: mlua::Value) {
        match (&v, prop) {
            // `text` and `style` are node properties, not component fields.
            (_, "text") => {
                let s = match &v {
                    mlua::Value::String(s) => s.to_string_lossy(),
                    mlua::Value::Number(n) => crate::api::format_lua_number(*n),
                    mlua::Value::Integer(n) => n.to_string(),
                    mlua::Value::Boolean(b) => b.to_string(),
                    _ => return,
                };
                self.ui_text_changes.borrow_mut().insert(e, s);
            }
            (mlua::Value::String(s), "style") => {
                self.ui_style_changes.borrow_mut().insert(e, s.to_string_lossy());
            }
            (mlua::Value::Table(t), _) => {
                if let Ok(c) = crate::api::read_color(t) {
                    let comp = Self::owning_component(scene, e, prop, true);
                    self.component_colors.borrow_mut().insert((e, comp, prop.to_string()), c);
                }
            }
            _ => {
                let n = match v {
                    mlua::Value::Number(n) => n,
                    mlua::Value::Integer(n) => n as f64,
                    mlua::Value::Boolean(b) => f64::from(u8::from(b)),
                    // nil means "nothing to say this frame", not "write zero".
                    _ => return,
                };
                let comp = Self::owning_component(scene, e, prop, false);
                self.component_changes.borrow_mut().insert((e, comp, prop.to_string()), n);
            }
        }
    }

    /// Which component on `e` owns `prop`, per the mirror. Defaults to
    /// `UiElement`, which is what a binding is nearly always about.
    fn owning_component(scene: &crate::SceneMirror, e: u32, prop: &str, color: bool) -> String {
        let found = if color {
            scene.component_colors.get(&e).and_then(|m| {
                m.iter().find(|(_, fields)| fields.contains_key(prop)).map(|(k, _)| k.clone())
            })
        } else {
            scene.components.get(&e).and_then(|m| {
                m.iter().find(|(_, fields)| fields.contains_key(prop)).map(|(k, _)| k.clone())
            })
        };
        found.unwrap_or_else(|| "UiElement".to_string())
    }

    /// Drop every per-(node, script) environment plus its net handlers and
    /// synced stores — a SCENE SWITCH: the next `run` rebuilds fresh instances
    /// against the new world, and every `start` re-fires. Compiled sources stay
    /// cached (rebuilding is per-instance, not per-file).
    pub fn reset_instances(&mut self) {
        self.reset_instances_keeping(&std::collections::HashSet::new());
    }

    /// [`Self::reset_instances`], except that instances (and the UI
    /// subscriptions they own) belonging to the entities in `keep` are left
    /// running untouched.
    ///
    /// This is what `node.persistent` costs the script host. A persistent node
    /// keeps its ENTITY across the swap — the driver despawns the old scene in
    /// place rather than building a new world, so the surviving index cannot be
    /// handed out again while it is alive — and because instances are keyed by
    /// entity index, keeping the instance keeps the running script: its state,
    /// its coroutines, its `synced` values. `start` does not re-fire, which is
    /// the difference between "survives" and "is rebuilt".
    ///
    /// A subscription is kept only when EVERY entity it names survives. A
    /// binding whose element belonged to the old scene would otherwise drive
    /// whatever node inherits that index next.
    pub fn reset_instances_keeping(&mut self, keep: &std::collections::HashSet<u32>) {
        // Pending timers belong to the old session — a scene switch drops them.
        // (Including a persistent node's: a timer is a promise about a world.)
        self.sched.borrow_mut().clear();
        // So do agents: one belongs to a node in a world that is going away, and
        // a crowd that survived a Stop would walk the next Play's units from
        // wherever the last one left them.
        {
            let mut agents = self.nav_agents.borrow_mut();
            agents.crowd.clear();
            agents.bound.clear();
        }
        let all: Vec<_> = self.instances.drain().collect();
        for (k, inst) in all {
            if keep.contains(&k.0) {
                self.instances.insert(k, inst);
                continue;
            }
            let _ = self.lua.remove_registry_value(inst.env);
            self.drop_net_instance(&k);
        }
        self.envs.borrow_mut().retain(|(e, _), _| keep.contains(e));
        // Bindings point at the OLD scene's entity indices, which the new
        // scene will reuse. Left in place they would drive the wrong nodes.
        {
            let mut binds = self.ui_bindings.borrow_mut();
            let mut i = 0;
            while i < binds.len() {
                if keep.contains(&binds[i].e) {
                    i += 1;
                } else {
                    let b = binds.remove(i);
                    let _ = self.lua.remove_registry_value(b.f);
                }
            }
        }
        // Same reasoning for made screens: their described trees and their
        // behaviour closures both name the OLD scene's entity indices, which
        // the new scene reuses from zero.
        {
            let mut makes = self.ui_makes.borrow_mut();
            let mut i = 0;
            while i < makes.len() {
                if keep.contains(&makes[i].container) {
                    i += 1;
                } else {
                    let req = makes.remove(i);
                    for (_, _, f) in req.hooks {
                        let _ = self.lua.remove_registry_value(f);
                    }
                }
            }
        }
        {
            let mut handlers = self.ui_handlers.borrow_mut();
            let dead: Vec<_> =
                handlers.keys().filter(|(e, _)| !keep.contains(e)).cloned().collect();
            for k in dead {
                if let Some(f) = handlers.remove(&k) {
                    let _ = self.lua.remove_registry_value(f);
                }
            }
        }
        // …and `ui.on` listeners, which name old entity indices on both sides:
        // the element they watch AND the script that owns them. BOTH have to
        // survive for the listener to mean anything.
        {
            let mut ls = self.ui_listeners.borrow_mut();
            let mut i = 0;
            while i < ls.len() {
                if keep.contains(&ls[i].e) && keep.contains(&ls[i].owner.0) {
                    i += 1;
                } else {
                    let l = ls.remove(i);
                    let _ = self.lua.remove_registry_value(l.f);
                }
            }
        }
        self.ui_listener_checks.borrow_mut().clear();
        // `scene.onLoaded` subscriptions follow their owner, so a persistent
        // loading screen is still listening on the other side of the swap —
        // which is the only place a loading screen is any use.
        {
            let mut subs = self.scene_loaded.borrow_mut();
            let mut i = 0;
            while i < subs.len() {
                if keep.contains(&subs[i].0) {
                    i += 1;
                } else {
                    let (_, f) = subs.remove(i);
                    let _ = self.lua.remove_registry_value(f);
                }
            }
        }
        // Queued spawn/create/destroy requests must not leak across a scene
        // switch (their entities/prefabs belong to the old scene's session).
        // All three are drained in the same place — `apply_script_spawns` — so
        // all three have to be dropped here: a request queued on a frame that
        // ended in Stop would otherwise be applied on the NEXT session, where
        // `parent` is an index the new scene has given to a different node and
        // `cb` closes over an environment that has already been dropped.
        for req in self.spawn_requests.borrow_mut().drain(..) {
            if let Some(cb) = req.cb {
                let _ = self.lua.remove_registry_value(cb);
            }
        }
        for req in self.create_requests.borrow_mut().drain(..) {
            if let Some(cb) = req.cb {
                let _ = self.lua.remove_registry_value(cb);
            }
        }
        self.destroy_queue.borrow_mut().clear();
        self.assembly_info.borrow_mut().clear();
        self.assembly_impacts.borrow_mut().clear();
        for cmd in self.assembly_cmds.borrow_mut().drain(..) {
            if let crate::assembly_api::AssemblyCmd::Split { cb: Some(cb), .. } = cmd {
                let _ = self.lua.remove_registry_value(cb);
            }
        }
    }

    /// Feed each animated entity's controller state for this frame (before `run`),
    /// so scripts can read `anim:state()/:time()/:clips()`.
    pub fn set_anim_info(&self, map: HashMap<u32, AnimInfo>) {
        *self.anim_info.borrow_mut() = map;
    }

    /// Feed this frame's assembly mirror (`assembly.info` reads it).
    pub fn set_assembly_info(&self, map: HashMap<u32, crate::assembly_api::AssemblyInfo>) {
        *self.assembly_info.borrow_mut() = map;
    }

    /// Feed the last tick's per-part contact loads (`assembly.impacts` reads it).
    pub fn set_assembly_impacts(
        &self,
        map: HashMap<u32, Vec<crate::assembly_api::AssemblyImpact>>,
    ) {
        *self.assembly_impacts.borrow_mut() = map;
    }

    /// Drain the `assembly.*` commands scripts queued (held forces, impulses,
    /// splits) — the driver applies them to the Sim, performing splits itself.
    pub fn take_assembly_cmds(&self) -> Vec<crate::assembly_api::AssemblyCmd> {
        std::mem::take(&mut *self.assembly_cmds.borrow_mut())
    }

    /// Hand the runtime the scene's baked navmesh, or `None` for a scene with
    /// none. Called when a scene loads and after a bake, not per frame: a
    /// navmesh changes when somebody bakes it and at no other time.
    pub fn set_nav_mesh(&self, mesh: Option<floptle_nav::NavMesh>) {
        *self.nav_mesh.borrow_mut() = mesh;
        // Every route in flight was worked out against the mesh that just went
        // away, and every filter was resolved against its area names. Both are
        // re-done on the next step rather than left to rot.
        let mut agents = self.nav_agents.borrow_mut();
        agents.crowd.navmesh_changed();
        let guard = self.nav_mesh.borrow();
        agents.resolve_filters(guard.as_ref());
    }

    /// How many times runtime obstacles have been cut or removed on the live
    /// navmesh, and the mesh itself if anything wants to draw it.
    ///
    /// The editor's navmesh overlay is built from the bake, and while a game is
    /// running the mesh it is actually walking on is the bake with holes in it.
    /// Comparing this number is how the overlay notices; `nav_mesh_snapshot`
    /// is what it then draws.
    pub fn nav_obstacle_rev(&self) -> u64 {
        self.nav_mesh.borrow().as_ref().map_or(0, |m| m.obstacle_rev())
    }

    /// A copy of the navmesh the game is walking on right now, holes included.
    pub fn nav_mesh_snapshot(&self) -> Option<floptle_nav::NavMesh> {
        self.nav_mesh.borrow().clone()
    }

    /// Walk every `nav.agent` one frame.
    ///
    /// Runs after the scripts, so an order given this frame is acted on this
    /// frame, and before the writes are flushed, so an agent's movement rides
    /// the same pass as a hand-written one.
    ///
    /// Positions are read from the scene **every frame** rather than owned
    /// outright: whatever else moved a node — a script, the physics sim, a
    /// parent, a cutscene — is the truth, and an agent that insisted otherwise
    /// would fight it and win, which is the ugliest way for this to fail.
    fn step_nav_agents(&mut self, dt: f32) {
        if self.nav_agents.borrow().crowd.is_empty() {
            return;
        }
        let mesh = self.nav_mesh.borrow();
        let mut agents = self.nav_agents.borrow_mut();
        let agents = &mut *agents;

        // ---- into the crowd ------------------------------------------------
        // Teleports write the NODE (below) rather than reading it — collected
        // here because the scene is only borrowed immutably in this pass.
        let mut teleports: Vec<(u32, [f64; 3])> = Vec::new();
        let gone: Vec<floptle_nav::AgentId> = {
            let scene = self.scene.borrow();
            let mut gone = Vec::new();
            for (id, b) in agents.bound.iter_mut() {
                if !scene.transforms.contains_key(&b.entity) {
                    // Its node has been destroyed. The agent goes with it.
                    gone.push(*id);
                    continue;
                }
                if let Some(tp) = b.teleport.take() {
                    // The scene still holds the OLD position this frame — skip
                    // the read-back, or the teleport is undone before it lands.
                    b.pos = tp;
                    if b.drive != crate::nav_api::Drive::None {
                        teleports.push((b.entity, tp));
                    }
                    continue;
                }
                // The node is the truth: whatever else moved it (physics, a
                // parent, a cutscene) wins over the agent.
                let world = crate::api::world_transform_of(&scene, b.entity).translation;
                b.pos = [world.x, world.y, world.z];
                let Some(agent) = agents.crowd.agent_mut(*id) else { continue };
                agent.pos = match mesh.as_ref() {
                    Some(m) => m.to_local(b.pos),
                    None => [world.x as f32, world.y as f32, world.z as f32],
                };
                match b.target {
                    Some(t) => {
                        let local = match mesh.as_ref() {
                            Some(m) => m.to_local(t),
                            None => [t[0] as f32, t[1] as f32, t[2] as f32],
                        };
                        // Sync, not re-order: an unchanged target must not wake
                        // a Blocked agent (or re-queue a search) every frame.
                        agent.sync_target(local);
                    }
                    None => agent.stop(),
                }
            }
            gone
        };
        for id in gone {
            agents.crowd.remove(id);
            agents.bound.remove(&id);
        }
        self.write_agent_moves(teleports);

        agents.crowd.step(mesh.as_ref(), dt);

        // ---- and back out --------------------------------------------------
        let Some(mesh) = mesh.as_ref() else { return };
        let mut moves: Vec<(u32, [f64; 3])> = Vec::new();
        let mut velocities: Vec<(u32, [f32; 3])> = Vec::new();
        for (id, b) in agents.bound.iter_mut() {
            let Some(agent) = agents.crowd.agent(*id) else { continue };
            let world = mesh.to_world(agent.pos);
            b.pos = world;
            let physics = match b.drive {
                crate::nav_api::Drive::None => continue,
                crate::nav_api::Drive::Transform => false,
                crate::nav_api::Drive::Velocity => true,
                // A node with a body is driven through the body, because moving
                // its transform underneath the sim is a fight nobody wins.
                crate::nav_api::Drive::Auto => self.bodies.borrow().contains_key(&b.entity),
            };
            if physics {
                // Horizontal only: gravity, slopes and jumps stay the sim's.
                let keep = self.bodies.borrow().get(&b.entity).map(|s| s.vel[1]).unwrap_or(0.0);
                velocities.push((b.entity, [agent.vel[0], keep, agent.vel[2]]));
            } else {
                moves.push((b.entity, world));
            }
        }
        self.write_agent_moves(moves);
        for (e, v) in velocities {
            self.body_changes.borrow_mut().insert(e, v);
        }
    }

    /// Put nodes where their agents say they are: world position in, node-local
    /// translation out (through the parent chain). Shared by the per-frame
    /// drive write-back and `agent:teleport`.
    fn write_agent_moves(&self, moves: Vec<(u32, [f64; 3])>) {
        if moves.is_empty() {
            return;
        }
        let mut scene = self.scene.borrow_mut();
        for (e, world) in moves {
            let parent = crate::api::parent_world_of(&scene, e);
            let Some(tr) = scene.transforms.get(&e).copied() else { continue };
            let mut want = tr;
            want.translation = glam::DVec3::new(world[0], world[1], world[2]);
            let local = parent.inv_mul(&want).translation;
            if let Some(slot) = scene.transforms.get_mut(&e) {
                slot.translation = local;
            }
            scene.dirty.insert(e);
            // A node with a body that was asked to be moved by its transform
            // anyway: the physics writeback would stomp it, so this has to go
            // through the same teleport channel a cross-node `node.pos` write
            // does.
            if self.bodies.borrow().contains_key(&e) {
                self.body_pos_changes.borrow_mut().insert(e, [local.x, local.y, local.z]);
            }
        }
    }

    /// Feed this tick's celestial snapshot (`space.*` reads it — solar demo S2).
    pub fn set_space(&self, info: crate::space_api::SpaceInfo) {
        *self.space_info.borrow_mut() = info;
    }

    /// Feed this frame's active game camera + viewport (`camera.worldToScreen`
    /// reads it). Fed every frame regardless of focus, so the map can pick.
    pub fn set_view(&self, info: crate::view_api::ViewInfo) {
        *self.view_info.borrow_mut() = info;
    }

    /// Drain a pending `space.warp(m)` request (the editor applies + clamps it).
    pub fn take_warp_request(&self) -> Option<f64> {
        self.warp_request.borrow_mut().take()
    }

    /// Drain a pending `physics.pause(on)` request (the editor gates its step).
    pub fn take_physics_pause_request(&self) -> Option<bool> {
        self.physics_pause_request.borrow_mut().take()
    }

    /// Gameplay ticks a script asked to frame-step through `physics.step([n])`.
    pub fn take_frame_steps(&self) -> u32 {
        self.frame_step_request.replace(0)
    }

    /// Mirror the background terrain worker's state into `terrain.busy()`.
    pub fn set_terrain_busy(&self, on: bool) {
        self.terrain_busy.set(on);
    }

    /// Mirror the editor's physics-paused state into `physics.isPaused()`.
    pub fn set_physics_paused(&self, on: bool) {
        self.physics_paused.set(on);
    }

    /// Drain the animator commands scripts queued this frame — the editor applies
    /// them to the controller runtimes before advancing them.
    pub fn take_anim_commands(&self) -> Vec<(u32, AnimCmd)> {
        std::mem::take(&mut *self.anim_commands.borrow_mut())
    }

    /// Feed each particle node's live state for this frame (before `run`), so scripts
    /// can read `node:particles():isPlaying()` / `:alive()`.
    pub fn set_vfx_info(&self, map: HashMap<u32, VfxInfo>) {
        *self.vfx_info.borrow_mut() = map;
    }

    /// Drain the particle commands scripts queued this frame — the editor applies
    /// them to the live VFX instances before advancing them.
    pub fn take_vfx_commands(&self) -> Vec<(u32, VfxCmd)> {
        std::mem::take(&mut *self.vfx_commands.borrow_mut())
    }

    /// Feed the audio playback mirror for this frame (before `run`), so scripts
    /// can read `sound:isPlaying()` / `node:sound():position()` / ….
    pub fn set_audio_info(&self, info: crate::AudioInfo) {
        *self.audio_info.borrow_mut() = info;
    }

    /// Drain the audio commands scripts queued this frame — the editor applies
    /// them to the audio engine the same frame.
    pub fn take_audio_commands(&self) -> Vec<crate::AudioCmd> {
        std::mem::take(&mut *self.audio_commands.borrow_mut())
    }

    /// Drain the debug-draw commands scripts queued this frame (`gizmo.*`) — the
    /// editor projects and paints them over the viewport for one frame.
    pub fn take_gizmos(&self) -> Vec<GizmoCmd> {
        std::mem::take(&mut *self.gizmos.borrow_mut())
    }

    /// Drain the one-shot effects scripts requested this frame (`spawnEffect(...)`):
    /// (asset key, world position). The editor spawns a detached instance for each.
    pub fn take_spawn_effects(&self) -> Vec<crate::SpawnedEffect> {
        std::mem::take(&mut *self.spawn_effects.borrow_mut())
    }

    /// Drain the prefab instances scripts requested via `spawn(...)`. The driver
    /// spawns each subtree, then calls [`Self::call_spawn_callback`] per request.
    /// Drain queued `nav.rebake(...)` requests. See [`crate::NavRebakeRequest`].
    pub fn take_nav_rebakes(&self) -> Vec<crate::NavRebakeRequest> {
        std::mem::take(&mut *self.nav_rebakes.borrow_mut())
    }

    pub fn take_spawn_requests(&self) -> Vec<crate::SpawnRequest> {
        std::mem::take(&mut *self.spawn_requests.borrow_mut())
    }

    /// Drain queued `createNode(...)` requests (see the spawn drain).
    pub fn take_create_requests(&self) -> Vec<crate::CreateRequest> {
        std::mem::take(&mut *self.create_requests.borrow_mut())
    }

    /// Drain queued `terrain.generatePlanet` requests — heavyweight; run them
    /// on a background thread and adopt the fields when they arrive.
    pub fn take_terrain_generates(&self) -> Vec<(u32, floptle_field::procgen::PlanetFill)> {
        std::mem::take(&mut *self.terrain_generates.borrow_mut())
    }

    /// The game's save-slot terrain directory (`terrain.saveDir(path)`), or None
    /// when unset (editor-authoring mode — fields live in the project). Read by
    /// the residency streamer every load/evict.
    pub fn terrain_save_dir(&self) -> Option<String> {
        self.terrain_save_dir.borrow().clone()
    }

    /// Reset the save-slot terrain dir (Play stop — a slot never outlives its run).
    pub fn clear_terrain_save_dir(&self) {
        *self.terrain_save_dir.borrow_mut() = None;
    }

    /// Drain this frame's `terrain.warm(name)` requests — body names whose
    /// terrain must be resident regardless of gameplay-anchor distance
    /// (immediate mode: callers re-warm every frame, e.g. the map's focus).
    pub fn take_terrain_warm(&self) -> Vec<String> {
        std::mem::take(&mut *self.terrain_warm.borrow_mut())
    }

    /// Drain the one-shot `terrain.flush()` request (write dirty resident
    /// fields to the save slot now — checkpoints, exit-to-menu).
    pub fn take_terrain_flush(&self) -> bool {
        std::mem::take(&mut *self.terrain_flush.borrow_mut())
    }

    /// Invoke `cb` with a fresh handle for `eid` — the shared callback shape
    /// used by both `spawn(...)` and `createNode(...)` drains.
    pub fn call_create_callback(
        &mut self,
        world: &mut World,
        cb: mlua::RegistryKey,
        e: floptle_core::Entity,
    ) {
        self.call_spawn_callback(world, cb, e.index(), &[e]);
    }

    /// Drain this tick's `draw.line(...)` segments (immediate mode — the editor
    /// replaces its line list with each tick's drain, so an idle script clears).
    pub fn take_draw_lines(&self) -> Vec<crate::DrawLine> {
        std::mem::take(&mut *self.draw_lines.borrow_mut())
    }

    /// Drain this tick's filled triangles (`draw.tri/cone/disc`).
    pub fn take_draw_tris(&self) -> Vec<crate::DrawTri> {
        std::mem::take(&mut *self.draw_tris.borrow_mut())
    }

    /// Drain this tick's screen-space rectangles (`draw.rect/rectOutline`).
    pub fn take_draw_rects(&self) -> Vec<crate::DrawRect> {
        std::mem::take(&mut *self.draw_rects.borrow_mut())
    }

    /// Play started / stopped. `http.*` and `openUrl` refuse outside Play, and
    /// Stop cancels every request in flight — a callback from the last session
    /// closes over nodes that no longer exist, so delivering it into a fresh
    /// Play is how one run inherits the previous one's network.
    pub fn set_playing(&self, playing: bool) {
        self.http.borrow_mut().set_playing(playing);
        // The account's SESSION survives Stop — it is the player's, stored in
        // the OS keyring, and signing in again every time you press Play would
        // be absurd. Only the callbacks and an unfinished sign-in are dropped.
        self.account.borrow_mut().set_playing(playing);
    }

    /// A scene load mid-session: same rule as Stop for anything on the wire.
    pub fn cancel_web_requests(&self) {
        self.http.borrow_mut().cancel_all();
        self.account.borrow_mut().cancel_all();
    }

    /// How many web requests are still waiting on a reply (`http.inFlight()`).
    pub fn web_requests_in_flight(&self) -> usize {
        self.http.borrow().in_flight()
    }

    /// Drain this tick's screen-space strings (`draw.text`).
    pub fn take_draw_texts(&self) -> Vec<crate::DrawText> {
        std::mem::take(&mut *self.draw_texts.borrow_mut())
    }

    /// Feed this frame's solved UI element rects in WINDOW physical pixels
    /// (entity index → [x, y, w, h]); `node:uiRect()` reads it. Window, not
    /// viewport-local, so it lines up with `input.mouse()`.
    pub fn set_ui_rects(&self, map: HashMap<u32, [f32; 4]>) {
        *self.ui_rects.borrow_mut() = map;
    }

    /// Drain the nodes scripts asked to remove via `destroy(...)` (entity indices).
    pub fn take_destroy_requests(&self) -> Vec<u32> {
        std::mem::take(&mut *self.destroy_queue.borrow_mut())
    }

    /// Apply this pass's `ui.make(...)` calls: reconcile each described tree
    /// against the world, install the behaviour closures on whichever entities
    /// the described elements turned out to be, and report the elements that
    /// are no longer described so the caller's destroy path can take them.
    ///
    /// Returns the entity indices to destroy. They are NOT despawned here: a
    /// made container can carry a repeater, whose rows are ordinary prefabs
    /// with scripts and physics, and the driver's destroy is the one path that
    /// knows how to unwind those.
    pub fn apply_ui_makes(&mut self, world: &mut World) -> Vec<u32> {
        let reqs = std::mem::take(&mut *self.ui_makes.borrow_mut());
        if reqs.is_empty() {
            return Vec::new();
        }
        let mut destroy = Vec::new();
        for req in reqs {
            let out = crate::ui_make::reconcile(world, req.container, &req.roots);
            destroy.extend(out.destroy);
            let mut handlers = self.ui_handlers.borrow_mut();
            // What this description asked for, so anything it did NOT ask for
            // can be taken off afterwards.
            let mut asked: std::collections::HashSet<(u32, &'static str)> =
                std::collections::HashSet::new();
            for (path, hook, f) in req.hooks {
                let Some((_, e)) = out.bound.iter().find(|(p, _)| *p == path) else {
                    // The described element it belonged to didn't survive
                    // parsing into a node — drop the closure rather than leak
                    // its registry slot.
                    let _ = self.lua.remove_registry_value(f);
                    continue;
                };
                asked.insert((*e, hook));
                // Re-describing a screen replaces its handlers: the closure is
                // freshly made every call and captures this call's values, so
                // keeping the old one would run against stale state.
                if let Some(old) = handlers.insert((*e, hook.to_string()), f) {
                    let _ = self.lua.remove_registry_value(old);
                }
            }
            // …and REMOVES the ones it stopped asking for. Reconcile reuses
            // entities, so an element that was a buy button and is now a
            // sold-out label is the same entity with no `clicked` in its new
            // description — and it kept answering the old closure. Clicking one
            // thing did another thing's job, which reads from the outside as
            // the menu selecting the wrong row, intermittently, depending on
            // what the screen happened to show last.
            //
            // Scoped to the elements THIS call described (`out.bound`), so a
            // second `ui.make` on another container never disarms this one's.
            let stale: Vec<(u32, String)> = handlers
                .keys()
                .filter(|(e, hook)| {
                    out.bound.iter().any(|(_, b)| b == e)
                        && !asked.iter().any(|(ae, ah)| ae == e && ah == hook)
                })
                .cloned()
                .collect();
            for k in stale {
                if let Some(f) = handlers.remove(&k) {
                    let _ = self.lua.remove_registry_value(f);
                }
            }
        }
        destroy
    }

    /// Throw away queued `ui.make(...)` calls without applying them, freeing
    /// their closures. Returns how many were dropped.
    ///
    /// Edit mode: a made tree is runtime content — conjured from data the game
    /// is holding right now — and materialising it into the open scene would
    /// put engine-built nodes in a file about to be saved. Same rule as a
    /// repeater's rows, for the same reason.
    pub fn discard_ui_makes(&mut self) -> usize {
        let reqs = std::mem::take(&mut *self.ui_makes.borrow_mut());
        let n = reqs.len();
        for req in reqs {
            for (_, _, f) in req.hooks {
                let _ = self.lua.remove_registry_value(f);
            }
        }
        n
    }

    /// Forget the behaviour closures on entities that no longer exist.
    ///
    /// Called with the set that just went away rather than sweeping every
    /// frame: entity indices are REUSED, so a handler left behind by a
    /// destroyed element would fire on whatever node inherits its slot.
    pub fn drop_ui_handlers(&mut self, gone: &[u32]) {
        if gone.is_empty() {
            return;
        }
        let dead: Vec<(u32, String)> = self
            .ui_handlers
            .borrow()
            .keys()
            .filter(|(e, _)| gone.contains(e))
            .cloned()
            .collect();
        for k in dead {
            if let Some(f) = self.ui_handlers.borrow_mut().remove(&k) {
                let _ = self.lua.remove_registry_value(f);
            }
        }
        // A `ui.on` listener dies with EITHER end: the element it watches (same
        // index-reuse reasoning as above) or the script that registered it — a
        // menu manager that has been destroyed should stop answering buttons,
        // and its closure still holds its whole environment alive.
        let orphans: Vec<crate::UiListener> = {
            let mut ls = self.ui_listeners.borrow_mut();
            let (dead, live) = std::mem::take(&mut *ls)
                .into_iter()
                .partition(|l| gone.contains(&l.e) || gone.contains(&l.owner.0));
            *ls = live;
            dead
        };
        for l in orphans {
            let _ = self.lua.remove_registry_value(l.f);
        }
    }

    /// Report `ui.on(...)` registrations aimed at an element that can't fire
    /// the hook — the one mistake this API makes easy, and the one that leaves
    /// nothing at all to look at.
    ///
    /// Deferred to here rather than checked inside `ui.on` because the element
    /// may not exist yet when the call runs (a `ui.make` screen is described in
    /// the same pass it is built). An element we can't find is left alone; only
    /// one we can see, and can see doesn't listen, is worth a word.
    fn check_ui_listeners(&mut self, world: &World) {
        let pending: Vec<(u32, String)> = std::mem::take(&mut *self.ui_listener_checks.borrow_mut());
        for (eid, hook) in pending {
            let Some(&ent) = self.scene.borrow().ents.get(&eid) else { continue };
            let Some(spec) = world.get::<floptle_ui::ElementSpec>(ent) else { continue };
            if crate::ui_make::hook_reaches(spec, &hook) {
                continue;
            }
            let name = world
                .get::<floptle_core::Name>(ent)
                .map(|n| n.0.clone())
                .unwrap_or_else(|| "element".into());
            let needs = crate::ui_make::hook_needs(&hook);
            self.logs.borrow_mut().push(crate::ScriptLog {
                level: crate::LogLevel::Warn,
                msg: format!(
                    "ui.on(\"{name}\", \"{hook}\", …): \"{name}\" doesn't take that \
                     interaction, so the listener will never fire — turn on {needs}."
                ),
                source: None,
            });
        }
    }

    /// Forget the `ui.on` listeners a `(node, script)` instance registered —
    /// it is being rebuilt (hot reload) or has gone away.
    ///
    /// Registering replaces (same owner, element and hook), so a reload that
    /// re-registers is already clean. This is for the reload that does NOT:
    /// delete the `ui.on` line, save, and the old closure would otherwise keep
    /// answering that button until the scene changed.
    fn drop_ui_listeners_of(&mut self, key: &(u32, String)) {
        let orphans: Vec<crate::UiListener> = {
            let mut ls = self.ui_listeners.borrow_mut();
            if !ls.iter().any(|l| l.owner == *key) {
                return;
            }
            let (dead, live) =
                std::mem::take(&mut *ls).into_iter().partition(|l| l.owner == *key);
            *ls = live;
            dead
        };
        for l in orphans {
            let _ = self.lua.remove_registry_value(l.f);
        }
    }

    /// Invoke a `spawn(...)` request's callback with the freshly spawned root's
    /// node handle. The new nodes are mirrored first (they didn't exist at the
    /// last sync) and the callback's writes are flushed straight back — so
    /// `spawn("bullet", p, function(b) b.vx = 40 end)` lands the same frame.
    ///
    /// **`new` is the entities this spawn created, and only those.** This used to
    /// re-mirror the whole scene per spawn, which is fine for a bullet and
    /// quadratic for a script that builds a level: `floptle/0138`, where a
    /// streamer spawning ~800 nodes a chunk into a 7,000-node scene rebuilt a
    /// twenty-collection table 800 times to add 800 rows to it.
    /// The FULL write flush runs (not just transforms): a `createNode` callback
    /// configures its node with the construction API (`setTerrain`/`setCelestial`/
    /// `setPrimitive`/…), and those are RichSet-queued — in the play loop the next
    /// pass's flush would catch them a tick late, but an EDITOR ACTION drain has
    /// no next pass, and the components simply never landed (the "generated field
    /// … but no node carries it" bug: generator bodies stayed Matter::Empty).
    pub fn call_spawn_callback(
        &mut self,
        world: &mut World,
        cb: mlua::RegistryKey,
        eid: u32,
        new: &[floptle_core::Entity],
    ) {
        self.sync_new_entities(world, new);
        let Ok(f) = self.lua.registry_value::<mlua::Function>(&cb) else { return };
        let Ok(node) = new_node_handle(&self.lua, eid) else { return };
        if let Err(err) = f.call::<()>(node) {
            self.record_error("spawn", format!("spawn callback: {err}"));
        }
        let _ = self.lua.remove_registry_value(cb);
        self.flush_writes(world);
    }

    /// Run a callback after re-mirroring the WHOLE scene.
    ///
    /// The expensive one, kept for the case that needs it: an assembly split
    /// does not add nodes so much as re-parent existing ones, and the
    /// incremental path only ever inserts — it cannot correct a `parent` that
    /// changed. A split is one event when a vessel comes apart, not one per node
    /// of a level being built, so the cost is in the right place.
    pub fn call_resync_callback(&mut self, world: &mut World, cb: mlua::RegistryKey, eid: u32) {
        self.sync_scene(world);
        let Ok(f) = self.lua.registry_value::<mlua::Function>(&cb) else { return };
        let Ok(node) = new_node_handle(&self.lua, eid) else { return };
        if let Err(err) = f.call::<()>(node) {
            self.record_error("spawn", format!("spawn callback: {err}"));
        }
        let _ = self.lua.remove_registry_value(cb);
        self.flush_writes(world);
    }

    /// Discard an unused callback registry key (a split that failed).
    pub fn drop_registry_value(&self, cb: mlua::RegistryKey) {
        let _ = self.lua.remove_registry_value(cb);
    }

    /// Call `func(node)` on every script instance attached to entity `eid` whose
    /// environment defines it — the dispatch path for animation clip events.
    /// Missing functions are fine (an event can target one script of several).
    /// Runs after `run()`, so any transform writes the handler makes are
    /// flushed back to the ECS here (the next `run` would otherwise wipe them
    /// when it re-syncs the mirror).
    pub fn call_function(&mut self, world: &mut World, eid: u32, func: &str) {
        let targets: Vec<(String, Table)> = self
            .envs
            .borrow()
            .iter()
            .filter(|((id, _), _)| *id == eid)
            .filter_map(|((_, kind), key)| Some((kind.clone(), self.env_of(key)?)))
            .collect();
        let mut called = false;
        for (kind, env) in targets {
            // raw_get: the env's metatable falls through to the Lua globals,
            // and an event must never mis-dispatch to a stdlib/global name.
            let Ok(Some(f)) = env.raw_get::<Option<mlua::Function>>(func) else { continue };
            let node = match new_node_handle(&self.lua, eid) {
                Ok(n) => n,
                Err(_) => continue,
            };
            called = true;
            if let Err(err) = f.call::<()>(node) {
                self.record_error(&kind, format!("{kind}: anim event {func}: {err}"));
            }
        }
        if called {
            self.flush_scene(world);
        }
    }

    /// EDITOR ACTIONS (`--@editorButton`): run ONE named function of ONE
    /// script on ONE node against the (edit-mode) world. Syncs the scene
    /// mirror, builds the script's env if needed — WITHOUT firing `start()`
    /// or any update pass — calls `func(node)`, and flushes every node/
    /// component write back. The editor then drains the spawn / create /
    /// terrain queues itself (that's where `createNode` and
    /// `terrain.generatePlanet` land). Returns whether the function existed.
    pub fn call_action(
        &mut self,
        world: &mut World,
        scripts_dir: &Path,
        eid: u32,
        kind: &str,
        func: &str,
    ) -> bool {
        self.sync_scene(world);
        let Some(e) = self.scene.borrow().ents.get(&eid).copied() else {
            self.record_error(kind, format!("{kind}: editor action target node #{eid} not found"));
            return false;
        };
        if !self.ensure_instance(e, kind, scripts_dir) {
            return false;
        }
        let key = (eid, kind.to_string());
        let Some(inst) = self.instances.get(&key) else { return false };
        let Ok(env) = self.lua.registry_value::<Table>(&inst.env) else { return false };
        let Ok(Some(f)) = env.raw_get::<Option<mlua::Function>>(func) else {
            self.record_error(kind, format!("{kind}: editor action '{func}' is not defined"));
            return false;
        };
        // Seed `params` from the node's stored tunables (what the Inspector
        // shows), exactly like a lifecycle tick would — an action without it
        // would read stale defaults. Reference params resolve by name.
        {
            let (params, refs, strs) = world
                .get::<Scripts>(e)
                .and_then(|s| s.0.iter().find(|i| i.kind == kind))
                .map(|i| (i.params.clone(), i.refs.clone(), i.strs.clone()))
                .unwrap_or_default();
            let resolved = self.resolve_refs(&env, &refs);
            if let Ok(t) = crate::env::params_table(&self.lua, &env, &params, &resolved, &strs) {
                let _ = env.set("params", t);
            }
        }
        let Ok(node) = new_node_handle(&self.lua, eid) else { return false };
        // An editor action is this script running, same as a lifecycle call —
        // so anything that registers by owner (`ui.on`, `net.on`) knows whose
        // it is. Without this, a `--@editorButton` that wires up a screen
        // registers listeners belonging to nobody, which nothing can unregister.
        *self.net.current.borrow_mut() = Some((eid, kind.to_string()));
        let r = f.call::<()>(node);
        *self.net.current.borrow_mut() = None;
        if let Err(err) = r {
            self.record_error(kind, format!("{kind}: {func}: {err}"));
        }
        self.flush_writes(world);
        true
    }

    /// Dispatch one collision/trigger event to every script on `eid` that
    /// defines `func` (`onCollisionEnter/Stay/Exit`, `onTriggerEnter/Stay/
    /// Exit`): called as `func(node, other, hit)` where `other` is the other
    /// node's handle and `hit = { x, y, z, nx, ny, nz }` (world point +
    /// contact normal). Same dispatch rules as anim events (raw env lookup —
    /// never a global), with a write flush when anything ran.
    #[allow(clippy::too_many_arguments)]
    pub fn call_touch(
        &mut self,
        world: &mut World,
        eid: u32,
        func: &str,
        other: u32,
        point: [f64; 3],
        normal: [f32; 3],
    ) {
        let targets: Vec<(String, Table)> = self
            .envs
            .borrow()
            .iter()
            .filter(|((id, _), _)| *id == eid)
            .filter_map(|((_, kind), key)| Some((kind.clone(), self.env_of(key)?)))
            .collect();
        if targets.is_empty() {
            return;
        }
        let mut called = false;
        for (kind, env) in targets {
            let Ok(Some(f)) = env.raw_get::<Option<mlua::Function>>(func) else { continue };
            let (Ok(node), Ok(other)) =
                (new_node_handle(&self.lua, eid), new_node_handle(&self.lua, other))
            else {
                continue;
            };
            let hit = match self.lua.create_table() {
                Ok(t) => t,
                Err(_) => continue,
            };
            let _ = hit.set("x", point[0]);
            let _ = hit.set("y", point[1]);
            let _ = hit.set("z", point[2]);
            let _ = hit.set("nx", normal[0] as f64);
            let _ = hit.set("ny", normal[1] as f64);
            let _ = hit.set("nz", normal[2] as f64);
            called = true;
            *self.net.current.borrow_mut() = Some((eid, kind.clone()));
            let result = f.call::<()>((node, other, hit));
            *self.net.current.borrow_mut() = None;
            if let Err(err) = result {
                self.record_error(&kind, format!("{kind}: {func}: {err}"));
            }
        }
        if called {
            self.flush_scene(world);
        }
    }

    // -------------------------------------------------------------------
    // net.* bridge (docs/netcode-design.md §8)
    // -------------------------------------------------------------------

    /// Drain the session commands scripts queued (`net.host{}`, `net.rpc`, …).
    pub fn take_net_commands(&self) -> Vec<crate::NetCmd> {
        std::mem::take(&mut *self.net.cmds.borrow_mut())
    }

    /// Mirror the live session state in for `net.role()`/`peers()`/`ping()`.
    pub fn set_net_state(&self, state: crate::NetState) {
        *self.net.state.borrow_mut() = state;
    }

    /// Mirror networked nodes' owners in (entity index → `Replicated::owner`)
    /// for `net.isMine(node)`. Feed each tick during a session; empty offline.
    pub fn set_net_owners(&self, owners: HashMap<u32, Option<u64>>) {
        *self.net.owners.borrow_mut() = owners;
    }

    /// Dispatch a received RPC to every script defining `onRpc.<name>` —
    /// `function onRpc.explode(args, sender) ... end`. Mirrors the animation
    /// clip-event dispatch; transform writes flush after the handlers.
    pub fn dispatch_rpc(
        &mut self,
        world: &mut World,
        name: &str,
        args: &floptle_net::NetValue,
        sender: u64,
    ) {
        let targets: Vec<((u32, String), Table)> =
            self.envs.borrow().iter().filter_map(|(k, key)| Some((k.clone(), self.env_of(key)?))).collect();
        let mut called = false;
        for ((eid, kind), env) in targets {
            // raw_get: never fall through the env metatable to globals.
            let Ok(Some(handlers)) = env.raw_get::<Option<Table>>("onRpc") else { continue };
            let Ok(Some(f)) = handlers.raw_get::<Option<mlua::Function>>(name) else { continue };
            let arg = match crate::net_api::netvalue_to_lua(&self.lua, args) {
                Ok(a) => a,
                Err(_) => continue,
            };
            *self.net.current.borrow_mut() = Some((eid, kind.clone()));
            let r = f.call::<()>((arg, sender));
            *self.net.current.borrow_mut() = None;
            called = true;
            if let Err(err) = r {
                self.record_error(&kind, format!("{kind}: onRpc.{name}: {err}"));
            }
        }
        if called {
            self.flush_scene(world);
        }
    }

    /// Fire a `net.on(event, fn)` handler set — `playerJoined`/`playerLeft`
    /// carry the peer id, `disconnected` a reason string, `connected` nothing.
    /// Fire `net.on("desync")` with a table the game can actually act on:
    /// `{ tick = n, node = "Player2" }`.
    ///
    /// It used to fire with no payload at all — not the tick, not the node, not
    /// the script, not the key — so a game could not tell a player whether
    /// their connection or their build was at fault, and "desynced",
    /// "disconnected" and "opponent quit" all reached them as the same thing:
    /// the game closed the match. Which is what they reported. floptle/0045.
    pub fn fire_desync(&mut self, world: &mut World, tick: u64, node: Option<&str>) {
        let payload = self.lua.create_table().ok().inspect(|t| {
            let _ = t.set("tick", tick);
            if let Some(n) = node {
                let _ = t.set("node", n.to_string());
            }
        });
        let handlers: Vec<(u32, String, mlua::Function)> = {
            let hs = self.net.handlers.borrow();
            hs.iter()
                .filter(|h| h.event == "desync")
                .filter_map(|h| {
                    self.lua
                        .registry_value::<mlua::Function>(&h.key)
                        .ok()
                        .map(|f| (h.eid, h.kind.clone(), f))
                })
                .collect()
        };
        let mut called = false;
        for (eid, kind, f) in handlers {
            *self.net.current.borrow_mut() = Some((eid, kind.clone()));
            let r = match &payload {
                Some(t) => f.call::<()>(t.clone()),
                None => f.call::<()>(()),
            };
            *self.net.current.borrow_mut() = None;
            called = true;
            if let Err(err) = r {
                self.record_error(&kind, format!("{kind}: net.on(\"desync\"): {err}"));
            }
        }
        if called {
            self.flush_scene(world);
        }
    }

    pub fn fire_net_event(
        &mut self,
        world: &mut World,
        event: &str,
        peer: Option<u64>,
        reason: Option<&str>,
    ) {
        let handlers: Vec<(u32, String, mlua::Function)> = {
            let hs = self.net.handlers.borrow();
            hs.iter()
                .filter(|h| h.event == event)
                .filter_map(|h| {
                    self.lua
                        .registry_value::<mlua::Function>(&h.key)
                        .ok()
                        .map(|f| (h.eid, h.kind.clone(), f))
                })
                .collect()
        };
        let mut called = false;
        for (eid, kind, f) in handlers {
            *self.net.current.borrow_mut() = Some((eid, kind.clone()));
            let r = match (peer, reason) {
                (Some(p), _) => f.call::<()>(p),
                (None, Some(s)) => f.call::<()>(s.to_string()),
                (None, None) => f.call::<()>(()),
            };
            *self.net.current.borrow_mut() = None;
            called = true;
            if let Err(err) = r {
                self.record_error(&kind, format!("{kind}: net.on(\"{event}\"): {err}"));
            }
        }
        if called {
            self.flush_scene(world);
        }
    }

    /// Server: collect every instance's current `synced` values for the
    /// session to diff + send: (entity index, script kind, name→value).
    /// Guardrail violations drop the var with a once-per-var Console warning.
    #[allow(clippy::type_complexity)]
    pub fn collect_synced(&mut self) -> Vec<(u32, String, Vec<(String, floptle_net::NetValue)>)> {
        let mut out = Vec::new();
        let stores = self.synced_stores.borrow();
        for ((eid, kind), store) in stores.iter() {
            let mut vars = Vec::new();
            for pair in store.clone().pairs::<mlua::Value, mlua::Value>() {
                let Ok((k, v)) = pair else { continue };
                let name = match &k {
                    mlua::Value::String(s) => s.to_string_lossy().to_string(),
                    other => format!("{other:?}"),
                };
                match crate::net_api::lua_to_netvalue(&v, 0)
                    .and_then(|nv| nv.validate().map_err(|e| e.to_string()).map(|_| nv))
                {
                    Ok(nv) => vars.push((name, nv)),
                    Err(e) => {
                        let key = (*eid, kind.clone(), name.clone());
                        if self.synced_warned.insert(key) {
                            self.logs.borrow_mut().push(crate::ScriptLog {
                                level: crate::LogLevel::Warn,
                                msg: format!("{kind}: synced.{name}: {e} — not replicated"),
                                source: None,
                            });
                        }
                    }
                }
            }
            if !vars.is_empty() {
                vars.sort_by(|a, b| a.0.cmp(&b.0)); // deterministic order
                out.push((*eid, kind.clone(), vars));
            }
        }
        out
    }

    /// Client: write received `synced` updates into the instance's store
    /// (bypassing the client-write warning — this IS the server's word).
    pub fn apply_synced(&self, eid: u32, kind: &str, vars: &[(String, floptle_net::NetValue)]) {
        let stores = self.synced_stores.borrow();
        let Some(store) = stores.get(&(eid, kind.to_string())) else { return };
        for (k, v) in vars {
            if let Ok(val) = crate::net_api::netvalue_to_lua(&self.lua, v) {
                let _ = store.raw_set(k.as_str(), val);
            }
        }
    }

    /// Reset the net bridge at a play-session boundary (Stop): queued commands
    /// and session state go; `net.on` handlers/synced stores belong to script
    /// instances and clean up with them.
    pub fn clear_net_state(&mut self) {
        self.net.cmds.borrow_mut().clear();
        *self.net.state.borrow_mut() = crate::NetState::default();
        *self.net.rewind.borrow_mut() = None;
        self.synced_warned.clear();
        self.param_warned.clear();
        self.handle_key_warned.clear();
    }

    /// Build the `synced` proxy for an instance whose script declares
    /// `replicated = { ... }` (called on every env (re)build).
    fn setup_synced(&mut self, env: &Table, key: &(u32, String)) {
        let Ok(Some(declared)) = env.raw_get::<Option<Table>>("replicated") else { return };
        match crate::net_api::build_synced_proxy(&self.lua, &self.net, &declared, &key.1) {
            Ok((proxy, store)) => {
                let _ = env.set("synced", proxy);
                self.synced_stores.borrow_mut().insert(key.clone(), store);
            }
            Err(e) => self.record_error(&key.1, format!("{}: replicated/synced: {e}", key.1)),
        }
    }

    /// Drop an instance's net registrations (env rebuild or instance death).
    fn drop_net_instance(&mut self, key: &(u32, String)) {
        self.synced_stores.borrow_mut().remove(key);
        let mut hs = self.net.handlers.borrow_mut();
        hs.retain(|h| !(h.eid == key.0 && h.kind == key.1));
    }

    /// Reset the animator bridge at a play-session boundary: drop the state
    /// mirror and any commands queued after the final drain (e.g. by a clip
    /// event handler on the session's last frame, or top-level script code
    /// evaluated outside play) so nothing leaks into the next session.
    pub fn clear_anim_state(&self) {
        self.anim_info.borrow_mut().clear();
        self.anim_commands.borrow_mut().clear();
        self.vfx_info.borrow_mut().clear();
        self.vfx_commands.borrow_mut().clear();
        self.spawn_effects.borrow_mut().clear();
    }

    /// Drain a pending `input.lockMouse()` / `input.unlockMouse()` request from this frame:
    /// `Some(true)` = lock (grab + hide cursor), `Some(false)` = unlock, `None` = unchanged.
    pub fn take_mouse_lock(&self) -> Option<bool> {
        self.mouse_lock.borrow_mut().take()
    }

    /// Lend the sim's colliders to the script host for one frame so `raycast(...)` can see
    /// them (the editor takes them back with [`take_colliders`](Self::take_colliders)
    /// before stepping physics). `origin` is the sim's world origin (ADR-0015) so ray
    /// origins/hits convert between the world coordinates scripts speak and the sim frame.
    /// Call before [`run`](Self::run).
    pub fn set_colliders(&self, cols: Vec<floptle_physics::AnchoredCollider>, origin: glam::DVec3) {
        *self.colliders.borrow_mut() = cols;
        *self.sim_origin.borrow_mut() = origin;
    }

    /// Reclaim the colliders lent via [`set_colliders`](Self::set_colliders). Call after
    /// [`run`](Self::run), before stepping the sim.
    pub fn take_colliders(&self) -> Vec<floptle_physics::AnchoredCollider> {
        std::mem::take(&mut self.colliders.borrow_mut())
    }

    /// Flush the `save.*` store to disk if dirty (editor calls this on Stop and
    /// every few seconds during Play, so a crash loses little). Errors surface in
    /// the Console via the script log channel.
    pub fn flush_save(&self) {
        let mut s = self.save_state.borrow_mut();
        if let Err(e) = crate::save_api::flush(&mut s, &self.project_root.borrow()) {
            self.logs.borrow_mut().push(crate::ScriptLog {
                level: crate::LogLevel::Error,
                msg: e,
                source: None,
            });
        }
    }

    /// Drain the terrain edits scripts queued this pass (`terrain.sculpt/dig/paint`).
    /// The editor applies each to the authority field, the sim's collider copy, the
    /// remesh queue and the shadow proxy — the same pipeline as an editor brush dab.
    /// Post the measured result of an applied op back to the scripts, to be read
    /// by `terrain.yields()` on the next pass (floptle/0037).
    pub fn push_terrain_yield(&self, y: crate::TerrainYield) {
        let mut q = self.terrain_yields.borrow_mut();
        // A game that never calls `terrain.yields()` must not grow a list
        // forever; the cap is far above any real frame's worth of edits.
        if q.len() < 4096 {
            q.push(y);
        }
    }

    pub fn take_terrain_ops(&self) -> Vec<crate::TerrainOp> {
        std::mem::take(&mut self.terrain_ops.borrow_mut())
    }

    /// Feed this frame's dynamic-body hulls ([`Sim::body_hulls`] copies) so
    /// `raycast(...)` can hit players/crates and name the node it hit. Copies,
    /// not a loan — nothing to take back. Call next to [`Self::set_colliders`].
    pub fn set_hulls(&self, hulls: Vec<floptle_physics::BodyHull>) {
        *self.hulls.borrow_mut() = hulls;
    }

    /// Stage (or clear) the lag-compensation context for the RPC about to be
    /// dispatched (`docs/netcode-design.md` §7): the rewound world as the
    /// sender perceived it, precomputed by the driver from its history ring.
    /// `net.rewind(peer, fn)` applies it for the duration of `fn`. Clear after
    /// the dispatch — a stale scope must never leak into the next handler.
    pub fn set_rewind(&self, scope: Option<crate::RewindScope>) {
        *self.net.rewind.borrow_mut() = scope;
    }

    /// Set the player input for the frame's scripts (call before [`run`](Self::run)).
    pub fn set_input(&self, snapshot: InputSnapshot) {
        *self.input.borrow_mut() = snapshot;
    }

    /// The shared action layer. The driver resolves devices into it (see
    /// [`floptle_input::InputSystem::resolve_frame`] /
    /// [`resolve_tick`](floptle_input::InputSystem::resolve_tick)) and the Lua
    /// `input.action(...)` family reads out of it.
    /// This frame's cost breakdown, shared with the Lua `perf` table
    /// (`floptle/0077`).
    ///
    /// The driver records subsystem times into it and folds each frame with
    /// `end_frame`; the host itself records per-script times inside `run_pass`.
    /// One structure on purpose — the number a game asserts on and the number the
    /// editor shows must be the same number, or one of them is a lie.
    pub fn profile(&self) -> &crate::SharedProfile {
        &self.profile
    }

    pub fn input_system(&self) -> &crate::input_api::SharedInput {
        &self.input_sys
    }

    /// Install the project's action map (call at project load and whenever
    /// `input.ron` changes on disk). Resets per-player state, since action
    /// indices may have moved.
    pub fn set_input_map(&self, map: floptle_input::InputMap) {
        self.input_sys.borrow_mut().set_map(map);
    }

    /// Declare the keys the HOST answers itself, as `(script name, why)`.
    ///
    /// A script polling one of these gets a Console warning naming the key and
    /// what takes it, once, the first time — instead of `false` forever
    /// (`floptle/0084`). The editor passes its three transport controls; a
    /// headless harness passes nothing, which is why the default is empty.
    ///
    /// The point is the *reachability* of the information, not the reservation:
    /// a key that will never arrive reads identically to a key the player did not
    /// press, so there is nothing to detect from inside the game and nothing to
    /// fall back to. A game shipped an inventory bound to Tab, passed its
    /// headless tests, and learnt about it from a player.
    pub fn set_reserved_keys(&self, keys: &[(&str, &str)]) {
        *self.reserved_keys.borrow_mut() =
            keys.iter().map(|(k, w)| (k.to_lowercase(), (*w).to_owned())).collect();
    }

    /// Lend the project's resolved layer table (call at Play start, alongside
    /// the sim build). Validates `node.layer` writes and resolves the named
    /// `layers` filter in `raycast(...)` to a bitmask.
    pub fn set_layers(&self, layers: floptle_core::Layers) {
        *self.layer_table.borrow_mut() = layers;
    }

    /// Lend the project's loaded tilesets, keyed by project-relative path.
    ///
    /// The host does no file I/O, so the parse belongs to whoever owns the project
    /// — the editor at Play start, the runtime at load. A path the map references
    /// but this map does not contain makes `tm:solid` answer `false`; the caller is
    /// the one positioned to say why, and does.
    pub fn set_tilesets(
        &self,
        sets: std::collections::HashMap<String, floptle_tiles::TileSet>,
    ) {
        self.scene.borrow_mut().tilesets = sets;
    }

    /// Lend the material slots of every model the editor has imported, keyed by
    /// asset path — what `node:materials()` answers from.
    ///
    /// Lent rather than read: a `.glb`'s parts are the importer's knowledge and
    /// this host does no file I/O, the same deal `set_tilesets` makes. Cheap to
    /// re-publish (one `Vec` per model), so the editor does it whenever the mesh
    /// registry changes rather than trying to keep a diff.
    pub fn set_model_slots(&self, slots: std::collections::HashMap<String, Vec<crate::ModelSlot>>) {
        self.scene.borrow_mut().model_slots = slots;
    }

    /// A live `(entity, script)` environment table, if built — for tests and
    /// tooling that read a script's state from Rust.
    pub fn instance_env(&self, eid: u32, kind: &str) -> Option<Table> {
        self.env_of(self.envs.borrow().get(&(eid, kind.to_string()))?)
    }

    /// Resolve a stored environment key to its Lua table.
    ///
    /// The one place the registry indirection is paid, and it is paid at USE
    /// rather than held: see the note on `Shared::envs` for why holding it
    /// capped how many scripted nodes a scene could have (`floptle/0069`).
    fn env_of(&self, key: &RegistryKey) -> Option<Table> {
        self.lua.registry_value::<Table>(key).ok()
    }

    /// Every script kind on `eid`, in the order the instances run. The rollback
    /// driver walks this so a captured state and a restore visit the same
    /// scripts in the same order.
    fn script_kinds_on(&self, eid: u32) -> Vec<(String, Table)> {
        let mut v: Vec<(String, Table)> = self
            .envs
            .borrow()
            .iter()
            .filter(|((id, _), _)| *id == eid)
            .filter_map(|((_, kind), key)| Some((kind.clone(), self.env_of(key)?)))
            .collect();
        // `envs` is a HashMap; a rollback must be reproducible, and "which
        // script's restore ran first" is observable when two scripts on a node
        // talk to each other. Sort so the order is the same on every machine and
        // every replay.
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// Capture `eid`'s rollback state: each script's `snapshot()` return value,
    /// deep-copied out of Lua (see [`crate::rollback_api`]). Scripts that define
    /// no `snapshot` contribute nothing and are simply not rolled back.
    ///
    /// Runs every tick under rollback, so it must stay cheap; the conversion is
    /// linear in the size of what the script actually returns.
    pub fn snapshot_scripts(&mut self, eid: u32) -> crate::rollback_api::ScriptState {
        let mut out = crate::rollback_api::ScriptState::default();
        for (kind, env) in self.script_kinds_on(eid) {
            let Ok(Some(f)) = env.raw_get::<Option<mlua::Function>>("snapshot") else { continue };
            // A SECOND return value is the cosmetic half: restored on rollback,
            // never hashed. Multi-return rather than a `{state=, cosmetic=}`
            // table so it cannot be confused with a state table that happens to
            // use those keys — and so every existing one-value `snapshot()`
            // keeps its exact meaning, with the second value simply nil.
            // floptle/0045.
            let (v, cosmetic) = match f.call::<(mlua::Value, mlua::Value)>(()) {
                Ok(pair) => pair,
                Err(e) => {
                    self.fail(&kind, format!("{kind}: snapshot() failed — {e}"));
                    continue;
                }
            };
            if !matches!(cosmetic, mlua::Value::Nil) {
                match crate::net_api::lua_to_netvalue_max(
                    &cosmetic,
                    0,
                    crate::rollback_api::MAX_STATE_DEPTH,
                ) {
                    Ok(nv) => out.cosmetic.push((kind.clone(), nv)),
                    Err(e) => self.fail(
                        &kind,
                        format!("{kind}: snapshot()'s cosmetic half can't be rolled back — {e}"),
                    ),
                }
            }
            match crate::net_api::lua_to_netvalue_max(
                &v,
                0,
                crate::rollback_api::MAX_STATE_DEPTH,
            ) {
                Ok(nv) => out.entries.push((kind, nv)),
                Err(e) => self.fail(
                    &kind,
                    format!(
                        "{kind}: snapshot() returned something that can't be rolled back — \
                         {e}. Rollback state holds numbers, strings, booleans and nested \
                         tables; a node handle or a function can't be restored."
                    ),
                ),
            }
        }
        out
    }

    /// Hand `eid`'s scripts a previously captured state through their
    /// `restore(s)` hooks. Each script sees a FRESH table built from the
    /// capture, so re-simulating after the restore cannot corrupt the snapshot
    /// it came from — which would otherwise make the second replay of a tick
    /// disagree with the first.
    pub fn restore_scripts(&mut self, eid: u32, state: &crate::rollback_api::ScriptState) {
        for (kind, env) in self.script_kinds_on(eid) {
            let Some((_, nv)) = state.entries.iter().find(|(k, _)| *k == kind) else { continue };
            let Ok(Some(f)) = env.raw_get::<Option<mlua::Function>>("restore") else { continue };
            let v = match crate::net_api::netvalue_to_lua(&self.lua, nv) {
                Ok(v) => v,
                Err(e) => {
                    self.fail(&kind, format!("{kind}: restore() value rebuild failed — {e}"));
                    continue;
                }
            };
            // The cosmetic half rides as a second argument, mirroring
            // `snapshot()`'s second return. A `restore(s)` that ignores it is
            // unaffected.
            let c = state
                .cosmetic
                .iter()
                .find(|(k, _)| *k == kind)
                .and_then(|(_, nv)| crate::net_api::netvalue_to_lua(&self.lua, nv).ok())
                .unwrap_or(mlua::Value::Nil);
            if let Err(e) = f.call::<()>((v, c)) {
                self.fail(&kind, format!("{kind}: restore() failed — {e}"));
            }
        }
    }

    /// Enter re-simulation (`docs/rollback-netcode-design.md` §4).
    ///
    /// A re-simulated tick runs the same Lua the live tick ran, so without a
    /// gate every replay re-fires the cosmetics: `spawnEffect` doubles the hit
    /// sparks, `audio.play` stutters the same impact, `spawn()` duplicates a
    /// projectile prefab, `print` floods the Console, `net.rpc` sends again.
    /// Simulation-relevant writes — body velocity/position, script state,
    /// component writes — all still land; that is the point of the replay.
    ///
    /// Errors are deliberately NOT suppressed. A replay that throws is a
    /// correctness problem, and hiding it would leave a desync with no
    /// symptom. The scheduler needs no gating here: it already refuses to
    /// advance in the targeted passes a replay uses (see [`crate::sched_api`]).
    pub fn begin_replay(&mut self) {
        if self.replay_marks.is_some() {
            return; // already replaying; a nested begin must not move the marks
        }
        self.replaying.set(true);
        self.replay_marks = Some(crate::ReplayMarks {
            spawn_effects: self.spawn_effects.borrow().len(),
            audio_commands: self.audio_commands.borrow().len(),
            spawn_requests: self.spawn_requests.borrow().len(),
            destroy_queue: self.destroy_queue.borrow().len(),
            net_cmds: self.net.cmds.borrow().len(),
            logs: self.logs.borrow().len(),
        });
    }

    /// Leave re-simulation, discarding the one-shot side effects it queued.
    ///
    /// The honest consequence, which belongs in the docs and not in a comment
    /// alone: a correction can *eat* a cosmetic (the spark that only exists on
    /// the corrected timeline never fires) or *orphan* one (the spark fired on
    /// the mispredicted timeline for a hit that turned out not to happen).
    /// Every rollback game lives with this; at depth ≤ 8 ticks it reads as
    /// network crackle. Gameplay-relevant spawns belong in rollback state.
    pub fn end_replay(&mut self) {
        let Some(m) = self.replay_marks.take() else { return };
        self.replaying.set(false);
        self.spawn_effects.borrow_mut().truncate(m.spawn_effects);
        self.audio_commands.borrow_mut().truncate(m.audio_commands);
        self.destroy_queue.borrow_mut().truncate(m.destroy_queue);
        self.net.cmds.borrow_mut().truncate(m.net_cmds);
        // Spawn requests carry a Lua registry key for their callback — dropping
        // the request without releasing it leaks a registry slot per replayed
        // `spawn()`, which a match-long stream of corrections would grow without
        // bound.
        for req in self.spawn_requests.borrow_mut().drain(m.spawn_requests..) {
            if let Some(cb) = req.cb {
                let _ = self.lua.remove_registry_value(cb);
            }
        }
        // Console output goes, errors stay.
        let mut logs = self.logs.borrow_mut();
        let mut i = m.logs;
        while i < logs.len() {
            if logs[i].level == crate::LogLevel::Error {
                i += 1;
            } else {
                logs.remove(i);
            }
        }
    }

    /// Is the driver re-simulating right now? (`net.replaying()` in Lua.)
    pub fn is_replaying(&self) -> bool {
        self.replaying.get()
    }

    /// Feed this tick's rollback state — diagnostics for `net.rollbackDepth()`
    /// and friends, and the seed behind `net.random()`.
    ///
    /// The draw counter resets here, which is the whole trick: a re-simulated
    /// tick starts its random sequence over, so it draws exactly the numbers
    /// the live tick drew. Call it once per tick, live and replayed alike,
    /// before any hook runs.
    pub fn set_rollback_info(&self, info: crate::RollbackInfo) {
        self.net.rollback.set(info);
        self.net.random_draws.set(0);
    }

    /// Has `eid`'s script environment been BUILT yet — i.e. has pass 1 run for
    /// it and published a table to look things up in?
    ///
    /// The distinction matters because every other query here answers "no" for
    /// a node whose envs do not exist yet, which reads identically to "no, and
    /// I checked". A rollback driver engaging on the same frame as a scene
    /// switch asked [`Self::has_rollback_hooks`] before the new scene's scripts
    /// had been loaded, got `false`, and told the user their fighter would not
    /// be rolled back — about a script that defines both hooks (floptle/0039).
    /// Callers that audit a node must gate on this and try again later.
    pub fn has_env(&self, eid: u32) -> bool {
        self.envs.borrow().keys().any(|(id, _)| *id == eid)
    }

    /// Does any script on `eid` participate in rollback? A `Rollback` node whose
    /// scripts define neither hook is almost always a mistake, and the driver
    /// warns about it once rather than desyncing quietly.
    ///
    /// Answers `false` for a node whose environments have not been built yet —
    /// check [`Self::has_env`] first if the answer is going to be shown to
    /// anyone.
    pub fn has_rollback_hooks(&self, eid: u32) -> bool {
        self.script_kinds_on(eid).iter().any(|(_, env)| {
            matches!(env.raw_get::<Option<mlua::Function>>("snapshot"), Ok(Some(_)))
                || matches!(env.raw_get::<Option<mlua::Function>>("restore"), Ok(Some(_)))
        })
    }

    /// Which of `eid`'s scripts declare `synced` vars, if any.
    ///
    /// On a rollback node that is two ownership models fighting over one value:
    /// `snapshot()`/`restore()` say the local simulation owns it and a
    /// correction may rewrite it, while `synced` says the host owns it and
    /// ships it. The driver reports the overlap rather than letting the loser
    /// be decided by arrival timing.
    pub fn synced_kinds_on(&self, eid: u32) -> Vec<String> {
        let stores = self.synced_stores.borrow();
        let mut out: Vec<String> = stores
            .iter()
            .filter(|((e, _), store)| {
                *e == eid && (*store).clone().pairs::<mlua::Value, mlua::Value>().next().is_some()
            })
            .map(|((_, kind), _)| kind.clone())
            .collect();
        out.sort();
        out
    }

    /// Feed the physics body state (entity index → vel + grounded) for the frame, so
    /// scripts can read `node.vx/vy/vz/grounded`. Call before [`run`](Self::run).
    pub fn set_bodies(&self, map: HashMap<u32, BodyState>) {
        *self.bodies.borrow_mut() = map;
    }

    /// Drain the velocities scripts wrote this frame (entity index → new velocity), to
    /// apply back to the physics sim. Call after [`run`](Self::run).
    pub fn take_body_changes(&self) -> HashMap<u32, [f32; 3]> {
        std::mem::take(&mut *self.body_changes.borrow_mut())
    }

    /// Drain the capsule heights scripts wrote this frame (entity index → height), for
    /// the editor to apply to the sim (crouch). Call after [`run`](Self::run).
    pub fn take_body_height_changes(&self) -> HashMap<u32, f32> {
        std::mem::take(&mut *self.body_height_changes.borrow_mut())
    }

    /// Drain cross-node position writes on BODY entities — the driver teleports
    /// each body there (the transform alone would be stomped by the physics
    /// writeback next frame).
    pub fn take_body_pos_changes(&self) -> HashMap<u32, [f64; 3]> {
        std::mem::take(&mut *self.body_pos_changes.borrow_mut())
    }


    /// Lend the material presets (name → Material) so a script can apply one with
    /// `node.material = "<name>"`. Call before [`run`](Self::run).
    pub fn set_materials(&self, map: HashMap<String, Material>) {
        *self.materials.borrow_mut() = map;
    }

    /// Point `assets.getFile` / `assets.getContents` at the project's asset root (the
    /// `Assets/` folder). Paths the dev writes are resolved relative to this.
    pub fn set_project_root(&self, root: PathBuf) {
        *self.project_root.borrow_mut() = root;
    }

    /// Drain the mesh model swaps scripts wrote this frame (entity index → new asset
    /// path), so the editor can re-import the GPU mesh. The `Matter::Mesh` component is
    /// already updated by [`run`](Self::run); this only signals which paths to load.
    pub fn take_model_changes(&self) -> HashMap<u32, String> {
        std::mem::take(&mut *self.model_changes.borrow_mut())
    }

    /// Errors raised by the most recent [`run`](Self::run) (one per failing script).
    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    /// Take the script log lines captured since the last call (Console feed).
    pub fn drain_logs(&self) -> Vec<ScriptLog> {
        std::mem::take(&mut self.logs.borrow_mut())
    }

    /// Record a script error: into `errors` (the Scripting tab) and the Console feed
    /// (tagged with the script's name + parsed line for jump-to-source).
    fn record_error(&mut self, name: &str, msg: String) {
        self.logs.borrow_mut().push(ScriptLog {
            level: LogLevel::Error,
            msg: msg.clone(),
            source: Some((name.to_string(), error_line(&msg))),
        });
        self.errors.push(msg);
    }

    /// Syntax-check Lua source without running it. Returns `(line, message)` for the
    /// first error (the line parsed from the `[string ...]:N:` prefix), or `None` if
    /// it parses cleanly — the in-engine IDE uses this for red squiggles.
    pub fn check_syntax(&self, src: &str) -> Option<(usize, String)> {
        let src = preprocess(src);
        match self.lua.load(&src).set_name("@chunk").into_function() {
            Ok(_) => None,
            Err(e) => {
                let full = e.to_string();
                // mlua formats syntax errors as `...:LINE: message`.
                let line = full
                    .split(':')
                    .find_map(|s| s.trim().parse::<usize>().ok())
                    .unwrap_or(1);
                let msg = full.lines().next().unwrap_or(&full).to_string();
                Some((line, msg))
            }
        }
    }

    /// Run every enabled script attached to a node in `world`. `scripts_dir` is the
    /// project's `scripts/` folder (script names resolve to `<dir>/<name>.lua`);
    /// `dt` is the frame delta and `time` is seconds since play started.
    pub fn run(&mut self, world: &mut World, scripts_dir: &Path, dt: f32, time: f32) {
        self.errors.clear();
        // Gizmos are immediate mode — a fresh frame starts empty even if the last
        // frame's batch was never drained. Sprite-batch draws are the same
        // contract and so are cleared in the same place: every pass of the
        // frame may draw, and the frame boundary — here — is the only thing
        // that empties them (`floptle/0070`).
        self.gizmos.borrow_mut().clear();
        self.sprite_draws.borrow_mut().clear();
        self.sprites_written = None;
        for inst in self.instances.values_mut() {
            inst.seen = false;
        }
        // Mirror the scene graph (names / parents / transforms / scripts) so node handles
        // can traverse and reference any node this frame.
        self.sync_scene(world);

        // Snapshot (entity, scripts) so we can mutate Transforms while iterating.
        // A switched-off node's scripts do not run — not its `update`, not its
        // `start`, and (because the node is out of the sim too) not its collision
        // hooks. `Disabled` is inherited, so turning off a folder turns off every
        // script under it.
        let work: Vec<(Entity, Scripts)> = world
            .query::<Scripts>()
            .filter(|(e, _)| !floptle_core::is_disabled(world, *e))
            .map(|(e, s)| (e, s.clone()))
            .collect();
        // Pass 1: build/refresh every environment so cross-references (findScript, etc.)
        // resolve regardless of which script ticks first.
        for (e, scripts) in &work {
            for inst in &scripts.0 {
                if inst.enabled {
                    self.ensure_instance(*e, &inst.kind, scripts_dir);
                    self.seed_params(*e, &inst.kind, &inst.params, &inst.refs, &inst.strs);
                }
            }
        }
        // Web replies land HERE — the frame pass, never the tick pass. A reply
        // arrives when it arrives, so a rollback replay must never see one.
        // Before `update`, so a callback's writes are visible to the same frame.
        crate::http_api::set_now(&self.http, time as f64);
        crate::http_api::drain(&self.lua, &self.http, &self.logs);
        crate::account_api::drain(&self.lua, &self.account, &self.logs);
        // Pass 2: run each script's start/update.
        self.run_pass(world, &work, dt, time, Pass::Frame, floptle_input::Domain::Frame);
        // Every `nav.agent` walks HERE: after the orders this frame's scripts
        // gave, before those writes reach the ECS.
        self.step_nav_agents(dt);
        // Bindings run AFTER every `update`, so a label always shows this
        // frame's value rather than last frame's, and before the flush, so a
        // binding's write rides the same pass as a hand-written one.
        self.run_ui_bindings();
        self.flush_writes(world);
        self.check_ui_listeners(world);

        // Drop environments whose (node, script) no longer exists.
        let stale: Vec<(u32, String)> =
            self.instances.iter().filter(|(_, i)| !i.seen).map(|(k, _)| k.clone()).collect();
        for k in stale {
            if let Some(inst) = self.instances.remove(&k) {
                let _ = self.lua.remove_registry_value(inst.env);
            }
            self.envs.borrow_mut().remove(&k);
            self.drop_net_instance(&k);
            self.drop_ui_listeners_of(&k);
        }
    }

    /// Run every script's `fixedUpdate(node, dt)` for ONE gameplay tick (the netcode-era
    /// fixed step, `docs/netcode-design.md` §3). Called zero or more times per frame by
    /// the play loop, between [`Self::run`] (which handles `start`/`update`, instance
    /// lifecycle, and hot reload) and the physics tick — so `dt` here is the CONSTANT
    /// tick delta and gameplay code sees the same cadence the sim steps at.
    ///
    /// Instances are the ones `run` built this frame; a not-yet-`start`ed script is
    /// skipped (its `start` fires in the next frame pass first). Errors accumulate onto
    /// the frame's list rather than clearing it.
    pub fn run_fixed(&mut self, world: &mut World, dt: f32, time: f32) {
        // Re-mirror the scene: earlier ticks this frame moved transforms/physics, and
        // handles must read post-step state, not the frame-start snapshot.
        self.sync_scene(world);
        // Advance `after`/`every`/`tween` timers — HERE and only here (the global
        // tick). `run_fixed_for` replays must not touch the scheduler, or a net
        // correction double-fires every pending timer. Before the script pass, so
        // a timer's effects are visible to this tick's `fixedUpdate`s.
        crate::sched_api::tick(&self.sched, &self.logs, dt as f64);
        // A switched-off node's scripts do not run — not its `update`, not its
        // `start`, and (because the node is out of the sim too) not its collision
        // hooks. `Disabled` is inherited, so turning off a folder turns off every
        // script under it.
        let work: Vec<(Entity, Scripts)> = world
            .query::<Scripts>()
            .filter(|(e, _)| !floptle_core::is_disabled(world, *e))
            .map(|(e, s)| (e, s.clone()))
            .collect();
        self.run_pass(world, &work, dt, time, Pass::Fixed, floptle_input::Domain::Tick);
        self.flush_writes(world);
    }

    /// Run every script's `lateUpdate(node, dt)` — the CAMERA pass. The driver
    /// calls it once per frame AFTER scripts, animation, physics, and the
    /// interpolated transform writeback, so a follower (orbit camera, name
    /// tag, listener) samples this frame's FINAL poses. Positioning a camera
    /// in `update` reads the PREVIOUS frame's pose — a follow error of
    /// `velocity × dt` that turns frame-time noise into visible jitter.
    pub fn run_late(&mut self, world: &mut World, dt: f32, time: f32) {
        // Re-mirror: physics writeback just moved transforms.
        self.sync_scene(world);
        // A switched-off node's scripts do not run — not its `update`, not its
        // `start`, and (because the node is out of the sim too) not its collision
        // hooks. `Disabled` is inherited, so turning off a folder turns off every
        // script under it.
        let work: Vec<(Entity, Scripts)> = world
            .query::<Scripts>()
            .filter(|(e, _)| !floptle_core::is_disabled(world, *e))
            .map(|(e, s)| (e, s.clone()))
            .collect();
        self.run_pass(world, &work, dt, time, Pass::Late, floptle_input::Domain::Frame);
        self.flush_writes(world);
    }

    /// Run ONE entity's `fixedUpdate` for one tick — the prediction-replay
    /// driver (`docs/netcode-design.md` §6): after a correction, the owner's
    /// controller re-runs its buffered inputs off the server state, touching
    /// only the predicted node's scripts.
    pub fn run_fixed_for(&mut self, world: &mut World, eid: u32, dt: f32, time: f32) {
        self.run_one(world, eid, dt, time, true);
    }

    /// Run ONE entity's FRAME pass (`update`) at the gameplay-tick cadence —
    /// how a predicted node's `update`-style controller stays deterministic in
    /// a net session: the server integrates it per tick, so the owning client
    /// must too, or every snapshot reads as a misprediction and the two sides
    /// fight. Pair with the frame filter (skip it in the per-frame pass).
    pub fn run_frame_for(&mut self, world: &mut World, eid: u32, dt: f32, time: f32) {
        self.run_one(world, eid, dt, time, false);
    }

    fn run_one(&mut self, world: &mut World, eid: u32, dt: f32, time: f32, fixed: bool) {
        self.sync_scene(world);
        let work: Vec<(Entity, Scripts)> = world
            .query::<Scripts>()
            .filter(|(e, _)| e.index() == eid)
            .map(|(e, s)| (e, s.clone()))
            .collect();
        // The targeted passes bypass the skip sets — they ARE the substitute
        // execution for a filtered entity.
        let (skip, fskip, dskip) = (
            std::mem::take(&mut self.script_skip),
            std::mem::take(&mut self.frame_skip),
            std::mem::take(&mut self.driver_skip),
        );
        // BOTH targeted passes run on the gameplay tick, including the `update`
        // one — that is the whole point of `run_frame_for`. So a predicted
        // node's `update` reads TICK input, or client and server would resolve
        // different edges and every snapshot would read as a misprediction.
        self.run_pass(
            world,
            &work,
            dt,
            time,
            if fixed { Pass::Fixed } else { Pass::Frame },
            floptle_input::Domain::Tick,
        );
        self.script_skip = skip;
        self.frame_skip = fskip;
        self.driver_skip = dskip;
        self.flush_writes(world);
    }

    /// Skip these entities' scripts in every pass — a networked CLIENT doesn't
    /// run server-authoritative nodes' scripts (their state arrives in
    /// snapshots). Pass an empty set to clear (Stop / role change).
    /// Is `eid` excluded from the global script passes? True means SOMETHING
    /// else has taken responsibility for running it (the rollback driver, the
    /// host's replayed-input pass) — and if nothing has, its scripts never run
    /// at all, which is a failure with no symptom other than silence.
    pub fn is_filtered(&self, eid: u32) -> bool {
        self.script_skip.contains(&eid) || self.driver_skip.contains(&eid)
    }

    /// Is `eid` in the SNAPSHOT-driven filter — the one that gates every pass,
    /// `lateUpdate` included? A driver-owned node must never be in here: no
    /// driver replays the late pass, so it would simply stop. floptle/0042.
    pub fn is_snapshot_filtered(&self, eid: u32) -> bool {
        self.script_skip.contains(&eid)
    }

    pub fn set_script_filter(&mut self, skip: std::collections::HashSet<u32>) {
        self.script_skip = skip;
    }

    /// Skip these entities in the PER-FRAME pass only (`update`) — the driver
    /// re-runs them on the gameplay tick via [`Self::run_frame_for`] instead
    /// (a predicted node in a net session). `fixedUpdate` is unaffected.
    pub fn set_frame_filter(&mut self, skip: std::collections::HashSet<u32>) {
        self.frame_skip = skip;
    }

    /// Add to the filters instead of replacing them, for a caller that owns
    /// only part of the set (the rollback driver's nodes, which leave both
    /// passes on top of whatever the session already skips).
    /// A driver (the rollback driver, the replayed-input pass) is taking over
    /// this node's **ticks**. Its `fixedUpdate` and `update` run from there
    /// instead of the global passes — but its `lateUpdate` does NOT, and must
    /// keep running here.
    ///
    /// That distinction is the whole reason this is a separate set from
    /// [`Self::set_script_filter`]. A snapshot-driven node is not simulated
    /// locally at all, so skipping every pass is right. A driver-owned node IS
    /// locally simulated — only the *scheduling* of its ticks moved. Its
    /// per-frame cosmetic pass has no substitute anywhere, and putting it in
    /// `script_skip` silently stopped it: `lateUpdate` is where the docs send
    /// you to write a node's presentation transform (it runs after the
    /// interpolated writeback, which would otherwise overwrite it), so a game
    /// that follows that advice broke the moment the node became a Rollback
    /// node — offline it was perfect, and it produced no error and no log line.
    /// floptle/0042.
    pub fn extend_filters(&mut self, skip: impl IntoIterator<Item = u32> + Clone) {
        self.driver_skip.extend(skip);
    }

    /// Whole-set form of [`Self::extend_filters`] — the session recomputing who
    /// the driver owns. Pass an empty set to clear (Stop / session end).
    pub fn set_driver_filter(&mut self, skip: std::collections::HashSet<u32>) {
        self.driver_skip = skip;
    }

    /// Undo an [`Self::extend_filters`] — the same caller taking its own half
    /// back out. Needed because the whole-set setters do not always run after a
    /// caller stops owning a set (an entity index left behind here is later
    /// REUSED by the allocator, and would then silently skip an unrelated
    /// node's scripts).
    pub fn shrink_filters(&mut self, drop: impl IntoIterator<Item = u32> + Clone) {
        for eid in drop.clone() {
            self.driver_skip.remove(&eid);
        }
        // Historic entries: drivers used to write into both of these sets, and
        // a stale index here would silently skip an UNRELATED node once the
        // allocator reused it.
        for eid in drop.clone() {
            self.script_skip.remove(&eid);
        }
        for eid in drop {
            self.frame_skip.remove(&eid);
        }
    }

    /// One lifecycle pass over `work`: per-frame (`start`/`update`), per-tick
    /// (`fixedUpdate`), or post-physics (`lateUpdate`), with the same self-move
    /// write-back rules.
    fn run_pass(
        &mut self,
        world: &mut World,
        work: &[(Entity, Scripts)],
        dt: f32,
        time: f32,
        pass: Pass,
        domain: floptle_input::Domain,
    ) {
        // Point the action API at this pass's input domain before any script
        // runs. It is passed in rather than derived from `pass` because the
        // prediction paths run an `update` pass on the TICK clock.
        self.input_domain.set(domain);
        // `http.*` warns when it is called from the tick pass — a reply arrives
        // when it arrives, which no replay can reproduce.
        self.http_in_fixed.set(pass == Pass::Fixed);
        for (e, scripts) in work {
            if self.script_skip.contains(&e.index()) {
                continue; // networked: this node's state arrives in snapshots
            }
            // A driver owns this node's TICKS, not its frames. `lateUpdate` is
            // per-frame cosmetic work that no driver replays — and must not be
            // replayed, since a rollback frame runs many ticks and would fire it
            // once per tick. So it alone survives this filter. floptle/0042.
            if pass != Pass::Late && self.driver_skip.contains(&e.index()) {
                continue; // driver-owned: its fixedUpdate/update run from there
            }
            if pass == Pass::Frame && self.frame_skip.contains(&e.index()) {
                continue; // predicted: its `update` re-runs on the tick clock
            }
            let Some(mut tr) = world.get::<Transform>(*e).copied() else { continue };
            let tr0 = tr; // pass-start, to detect a self-move via the `node` argument
            let mut ran = false;
            for inst in &scripts.0 {
                if inst.enabled {
                    // Per-SCRIPT attribution (`floptle/0077`). One `Instant` pair
                    // per instance per pass would be thousands of syscalls in a
                    // crowded scene, so it is skipped entirely unless somebody is
                    // collecting — the profiler must not be a frame cost itself.
                    // Named by KIND, because "which of my scripts is doing this"
                    // is the whole question and a game author says `planet_walker`,
                    // not entity 4173.
                    let span = self.profile.borrow().enabled().then(floptle_core::profile::Span::new);
                    self.tick_instance(
                        *e, &inst.kind, &inst.params, &inst.refs, &inst.strs, &mut tr, dt, time,
                        pass,
                    );
                    if let Some(span) = span {
                        self.profile.borrow_mut().record_script(&inst.kind, span.ms());
                    }
                    ran = true;
                }
            }
            // Only write back when the script moved its OWN node via the `node` argument.
            // If it didn't, leave the transform alone so a write from ANOTHER script's
            // handle (which lands in the mirror) isn't clobbered by a no-op rewrite. A
            // later script reading this node via a handle then sees the move this frame.
            if ran && tr != tr0 {
                if let Some(slot) = world.get_mut::<Transform>(*e) {
                    *slot = tr;
                }
                let mut s = self.scene.borrow_mut();
                s.transforms.insert(e.index(), tr);
                s.dirty.remove(&e.index());
            }
        }
    }

    /// Flush a pass's queued writes to the ECS: cross-node handle transforms, model /
    /// material / visibility swaps, and `node:getcomponent(...)` field writes. Runs
    /// after every pass (frame or fixed) so a tick's writes land before physics steps.
    fn flush_writes(&mut self, world: &mut World) {
        // Flush transforms that a handle wrote on OTHER nodes back to the ECS.
        self.flush_scene(world);
        // Construction-API writes (setCelestial/setMaterial/setTerrain/
        // setPrimitive) — before the numeric component mirror, so a component
        // created here can be tweaked by getcomponent writes the same pass.
        {
            let sets = std::mem::take(&mut *self.rich_sets.borrow_mut());
            if !sets.is_empty() {
                let (ents, tilesets) = {
                    let s = self.scene.borrow();
                    (s.ents.clone(), s.tilesets.clone())
                };
                crate::api::apply_rich_sets(world, &ents, sets, &tilesets);
            }
        }
        // 2D sprite batches (`floptle/0058`). IMMEDIATE MODE, scoped to the
        // FRAME: whatever the scripts drew since the frame began is the node's
        // whole set of sprites, and a batch nobody drew to all frame draws
        // nothing. That is what makes `b:draw` behave like `draw.*` — no
        // retained list, so no pool to grow and no `clear()` anyone can forget
        // on the frame a wave dies.
        //
        // The frame, not the pass, is the unit — and that distinction was worth
        // a silent, total blackout (`floptle/0070`). Emptying per pass meant
        // the fixed and late passes wiped whatever `update` drew, so a game
        // that put its renderer where every tutorial puts per-frame work saw
        // nothing at all: not a flicker, not a subset, every batch in the game.
        // `draw.rect` and `draw.line` are drained once a frame by the driver
        // and always behaved this way; these are described in the same sentence
        // in the docs and now have the same lifetime to go with it.
        //
        // `sprites_written` is how a pass that drew nothing new costs nothing:
        // the accumulator only grows within a frame, so an unchanged total
        // means unchanged contents. It is reset (not zeroed) at the frame
        // boundary, because a frame that happens to draw the same NUMBER of
        // sprites as the last one is still drawing them somewhere else.
        {
            let drawn = self.sprite_draws.borrow();
            let n: usize = drawn.values().map(Vec::len).sum();
            if self.sprites_written != Some(n) {
                let batches: Vec<(Entity, u32)> = world
                    .query::<Matter>()
                    .filter(|(_, m)| matches!(m, Matter::SpriteBatch { .. }))
                    .map(|(e, _)| (e, e.index()))
                    .collect();
                for (ent, id) in batches {
                    let list = drawn.get(&id).cloned().unwrap_or_default();
                    match world.get_mut::<floptle_core::Sprites>(ent) {
                        Some(slot) => slot.0 = list,
                        None => world.insert(ent, floptle_core::Sprites(list)),
                    }
                }
                self.sprites_written = Some(n);
            }
        }
        // Persist `params.X = ...` writes into the node's stored ScriptInst —
        // the next pass seeds from them (the write STICKS) and the Inspector
        // shows them live. Stop reverts them with the rest of the play state.
        {
            let scene = self.scene.borrow();
            for (eid, kind, key, v) in self.param_writes.borrow_mut().drain(..) {
                if let Some(&ent) = scene.ents.get(&eid)
                    && let Some(scripts) = world.get_mut::<Scripts>(ent)
                    && let Some(inst) = scripts.0.iter_mut().find(|i| i.kind == kind)
                {
                    match v {
                        crate::ParamWrite::Num(v) => {
                            match inst.params.iter_mut().find(|(k, _)| *k == key) {
                                Some(slot) => slot.1 = v,
                                None => inst.params.push((key, v)),
                            }
                        }
                        crate::ParamWrite::Str(v) => {
                            match inst.strs.iter_mut().find(|(k, _)| *k == key) {
                                Some(slot) => slot.1 = v,
                                None => inst.strs.push((key, v)),
                            }
                        }
                    }
                }
            }
        }
        // Apply script-driven component swaps: mesh model + material. (Model paths stay in
        // `model_changes` for the editor to drain and re-import the GPU mesh; materials are
        // resolved here against the lent preset map and applied directly.)
        {
            let scene = self.scene.borrow();
            for (eid, path) in self.model_changes.borrow().iter() {
                if let Some(&ent) = scene.ents.get(eid)
                    && let Some(Matter::Mesh { asset_path }) = world.get_mut::<Matter>(ent) {
                        *asset_path = path.clone();
                    }
            }
            let mats = self.materials.borrow();
            for (eid, refstr) in self.material_changes.borrow().iter() {
                if let Some(&ent) = scene.ents.get(eid)
                    && let Some(m) = mats.get(&material_key(refstr)) {
                        world.insert(ent, m.clone());
                    }
            }
            for (eid, shown) in self.visible_changes.borrow().iter() {
                if let Some(&ent) = scene.ents.get(eid) {
                    world.insert(ent, Visible(*shown));
                }
            }
            // `node.enabled = …`. Absence IS enabled, so turning one on REMOVES the
            // marker rather than storing a `true` — same rule as `layer`, and it keeps
            // scene files free of a field that means nothing.
            for (eid, on) in self.enabled_changes.borrow().iter() {
                if let Some(&ent) = scene.ents.get(eid) {
                    if *on {
                        world.remove::<floptle_core::Disabled>(ent);
                    } else {
                        world.insert(ent, floptle_core::Disabled);
                    }
                }
            }
            // `node.persistent = …`. Same shape as `enabled`, opposite polarity:
            // presence IS the flag, so `false` removes the marker.
            for (eid, on) in self.persistent_changes.borrow().iter() {
                if let Some(&ent) = scene.ents.get(eid) {
                    if *on {
                        world.insert(ent, floptle_core::Persistent);
                    } else {
                        world.remove::<floptle_core::Persistent>(ent);
                    }
                }
            }
            // `node.layer = ...` (pre-validated): "Default" removes the
            // component (absence IS Default — keeps scene files clean).
            for (eid, layer) in self.layer_changes.borrow().iter() {
                if let Some(&ent) = scene.ents.get(eid) {
                    if layer == floptle_core::layers::DEFAULT_LAYER {
                        world.remove::<floptle_core::Layer>(ent);
                    } else {
                        world.insert(ent, floptle_core::Layer(layer.clone()));
                    }
                }
            }
            // Tag edits: the handle computed the node's full new list.
            for (eid, tags) in self.tag_changes.borrow().iter() {
                if let Some(&ent) = scene.ents.get(eid) {
                    if tags.is_empty() {
                        world.remove::<floptle_core::Tags>(ent);
                    } else {
                        world.insert(ent, floptle_core::Tags(tags.clone()));
                    }
                }
            }
            // `node.text = ...`: write the UI element's label (creating the text
            // spec if the element doesn't have one yet).
            for (eid, txt) in self.ui_text_changes.borrow().iter() {
                if let Some(&ent) = scene.ents.get(eid)
                    && let Some(spec) = world.get_mut::<floptle_ui::ElementSpec>(ent)
                {
                    spec.text.get_or_insert_with(Default::default).text = txt.clone();
                }
            }
            // `node.style = ...`: swap which named style paints the element —
            // a row that becomes an error row, a button that turns primary.
            for (eid, name) in self.ui_style_changes.borrow().iter() {
                if let Some(&ent) = scene.ents.get(eid)
                    && let Some(spec) = world.get_mut::<floptle_ui::ElementSpec>(ent)
                {
                    spec.style = name.clone();
                }
            }
            // Apply node:getcomponent(...) field writes back to the ECS.
            for ((eid, comp, field), val) in self.component_changes.borrow().iter() {
                if let Some(&ent) = scene.ents.get(eid) {
                    apply_component_field(world, ent, comp, field, *val);
                }
            }
            for ((eid, comp, field), c) in self.component_colors.borrow().iter() {
                if let Some(&ent) = scene.ents.get(eid) {
                    crate::apply_component_color(world, ent, comp, field, *c);
                }
            }
            // String-valued component writes — `el.texture = "portraits/sae.png"`
            // and friends. The apply side has existed since the animation
            // system's property tracks used it; until 0052 nothing in Lua could
            // reach it, so a portrait assignment did nothing at all, silently.
            for ((eid, comp, field), s) in self.component_strs.borrow().iter() {
                if let Some(&ent) = scene.ents.get(eid) {
                    crate::apply_component_field_str(world, ent, comp, field, s);
                }
            }
            // `node:setShaderParam(name, ...)`: fold into the node's UI element
            // (when it has a `stage ui` shader), the sky, or its Material's
            // params. The per-frame shader drivers see the change and upload a
            // uniform write — never a recompile.
            for (eid, name, v) in self.shader_param_sets.borrow_mut().drain(..) {
                let Some(&ent) = scene.ents.get(&eid) else { continue };
                let on_ui = world
                    .get::<floptle_ui::ElementSpec>(ent)
                    .is_some_and(|s| !s.shader.is_empty());
                let on_sky =
                    matches!(world.get::<Matter>(ent), Some(Matter::Skybox { .. }));
                let on_post =
                    matches!(world.get::<Matter>(ent), Some(Matter::PostProcess { .. }));
                if on_post {
                    // A PostProcess node holds a LIST of screen shaders, so the
                    // name may carry which one: `"inkOutline.thickness"`. Left
                    // bare it writes the knob on every pass that has one, which
                    // is what a scene with a single screen shader wants and what
                    // it will almost always be.
                    //
                    // Matched on the file STEM, because the file name is what
                    // the Inspector shows and what a script author is looking
                    // at — not the project-relative path they never typed.
                    if let Some(Matter::PostProcess { screen_shaders, .. }) =
                        world.get_mut::<Matter>(ent)
                    {
                        let (want, knob) = match name.split_once('.') {
                            Some((pass, knob)) => (Some(pass.to_ascii_lowercase()), knob),
                            None => (None, name.as_str()),
                        };
                        for pass in screen_shaders.iter_mut() {
                            let stem = std::path::Path::new(&pass.shader)
                                .file_stem()
                                .map(|s| s.to_string_lossy().to_ascii_lowercase())
                                .unwrap_or_default();
                            if want.as_ref().is_none_or(|w| *w == stem) {
                                pass.params.insert(knob.to_string(), v);
                            }
                        }
                    }
                } else if on_ui {
                    if let Some(spec) = world.get_mut::<floptle_ui::ElementSpec>(ent) {
                        spec.shader_params.insert(name, v);
                    }
                } else if on_sky {
                    // Before the Material arm, not after: the sky pipeline reads
                    // `Matter::Skybox.shader_params`, so that is where a write
                    // has to land even on the unlikely sky node that also
                    // carries a Material (`floptle/0118`).
                    if let Some(Matter::Skybox { shader_params, .. }) = world.get_mut::<Matter>(ent)
                    {
                        shader_params.insert(name, v);
                    }
                } else if let Some(mat) = world.get_mut::<floptle_core::Material>(ent) {
                    mat.shader_params.insert(name, v);
                }
            }
            // `node:setShaderTexture(slot, ref)`: point one of the shader's
            // declared texture slots somewhere else, this frame.
            //
            // A shader could always DECLARE up to eight textures; what it could
            // not do was change one at runtime, so every multi-texture effect
            // was frozen at whatever the Inspector was set to when the scene was
            // saved. A damage state, a swapped decal, a screen showing what
            // another camera sees — all of them are this one call.
            //
            // Empty CLEARS the slot rather than binding an empty path: a slot
            // pointed at "" would otherwise fail to resolve every frame and the
            // shader would read whatever the fallback is, silently.
            for (eid, slot, path) in self.shader_texture_sets.borrow_mut().drain(..) {
                let Some(&ent) = scene.ents.get(&eid) else { continue };
                if let Some(mat) = world.get_mut::<floptle_core::Material>(ent) {
                    if path.is_empty() {
                        mat.shader_textures.remove(&slot);
                    } else {
                        mat.shader_textures.insert(slot, path);
                    }
                }
            }
            // `node:setScreenShader(name, on)`: switch one of the PostProcess
            // node's screen shaders on or off this frame. The pass and its knobs
            // stay in the scene either way — an outline that turns on for a boss
            // and off again is one call, not an edit to the scene's list.
            for (eid, name, on) in self.screen_shader_toggles.borrow_mut().drain(..) {
                let Some(&ent) = scene.ents.get(&eid) else { continue };
                if let Some(Matter::PostProcess { screen_shaders, .. }) =
                    world.get_mut::<Matter>(ent)
                {
                    let want = name.to_ascii_lowercase();
                    for pass in screen_shaders.iter_mut() {
                        let stem = std::path::Path::new(&pass.shader)
                            .file_stem()
                            .map(|s| s.to_string_lossy().to_ascii_lowercase())
                            .unwrap_or_default();
                        if want.is_empty() || want == stem {
                            pass.enabled = on;
                        }
                    }
                }
            }
        }
        self.material_changes.borrow_mut().clear();
        self.visible_changes.borrow_mut().clear();
        self.enabled_changes.borrow_mut().clear();
        self.persistent_changes.borrow_mut().clear();
        self.layer_changes.borrow_mut().clear();
        self.tag_changes.borrow_mut().clear();
        self.ui_text_changes.borrow_mut().clear();
        self.ui_style_changes.borrow_mut().clear();
        self.component_changes.borrow_mut().clear();
        self.component_colors.borrow_mut().clear();
        self.component_strs.borrow_mut().clear();
    }

    /// Fire UI-interaction hooks on a node's scripts: for each `(entity, hook)`
    /// event, every script instance on that entity that defines `hook` as a
    /// function is called with a node handle (`function clicked(node) ... end`).
    /// Hooks: `hoverStart`, `hoverEnd`, `pressed`, `released`, `clicked`.
    /// Call AFTER [`run`](Self::run) each frame — the events were detected
    /// against this frame's layout, and the writes flush here.
    pub fn run_ui_hooks(&mut self, world: &mut World, events: &[(u32, &str)]) {
        if events.is_empty() {
            return;
        }
        let mut failures: Vec<(String, String)> = Vec::new();
        for (eid, hook) in events {
            let envs: Vec<(String, Table)> = self
                .envs
                .borrow()
                .iter()
                .filter(|((e, _), _)| e == eid)
                .filter_map(|((_, kind), key)| Some((kind.clone(), self.env_of(key)?)))
                .collect();
            for (kind, env) in envs {
                let f = match env.get::<Value>(*hook) {
                    Ok(Value::Function(f)) => f,
                    _ => continue,
                };
                let node = match crate::env::new_node_handle(&self.lua, *eid) {
                    Ok(n) => n,
                    Err(_) => continue,
                };
                *self.net.current.borrow_mut() = Some((*eid, kind.clone()));
                if let Err(err) = f.call::<()>(node) {
                    failures.push((kind.clone(), format!("{kind}: {hook}: {err}")));
                }
                *self.net.current.borrow_mut() = None;
            }
            // …and the closure a described element carried, if any. Same
            // event, same node handle, same place — a made button and an
            // authored one behave identically from here on.
            let handler = self
                .ui_handlers
                .borrow()
                .get(&(*eid, (*hook).to_string()))
                .and_then(|k| self.lua.registry_value::<mlua::Function>(k).ok());
            if let Some(f) = handler
                && let Ok(node) = crate::env::new_node_handle(&self.lua, *eid)
                && let Err(err) = f.call::<()>(node)
            {
                failures.push(("ui.make".into(), format!("ui.make: {hook} handler: {err}")));
            }
            // …and every `ui.on(element, hook, fn)` listener, in registration
            // order. Last, because the element's own script is the specific
            // answer and a manager listening from across the scene is the
            // general one — and because a listener that despawns the screen
            // must not do it out from under the element's own handler.
            let listening: Vec<(u32, String, mlua::Function)> = self
                .ui_listeners
                .borrow()
                .iter()
                .filter(|l| l.e == *eid && l.hook == *hook)
                .filter_map(|l| {
                    let f = self.lua.registry_value::<mlua::Function>(&l.f).ok()?;
                    Some((l.owner.0, l.owner.1.clone(), f))
                })
                .collect();
            for (owner_e, owner_kind, f) in listening {
                let Ok(node) = crate::env::new_node_handle(&self.lua, *eid) else { continue };
                // The listening SCRIPT is what's running, not the element's —
                // so `synced`, `net.*` and error reporting name the manager.
                *self.net.current.borrow_mut() = Some((owner_e, owner_kind.clone()));
                let r = f.call::<()>((node, *hook));
                *self.net.current.borrow_mut() = None;
                if let Err(err) = r {
                    let who = if owner_kind.is_empty() { "ui.on" } else { &owner_kind };
                    failures.push((who.to_string(), format!("{who}: {hook} listener: {err}")));
                }
            }
        }
        for (kind, msg) in failures {
            self.record_error(&kind, msg);
        }
        self.flush_writes(world);
    }

    /// Rebuild the scene-graph mirror the Lua handles read/write, from the live ECS.
    fn sync_scene(&self, world: &World) {
        let mut s = self.scene.borrow_mut();
        s.order.clear();
        s.names.clear();
        s.by_name.clear();
        s.parent.clear();
        s.children.clear();
        s.scripts.clear();
        s.transforms.clear();
        s.ents.clear();
        s.dirty.clear();
        s.models.clear();
        s.visible.clear();
        s.disabled.clear();
        s.persistent.clear();
        s.layers.clear();
        s.tags.clear();
        s.components.clear();
        s.ui_texts.clear();
        s.ui_styles.clear();
        s.ui_textures.clear();
        s.component_strings.clear();
        // NOT cleared: the grids are reused when a map has not changed
        // (`floptle/0117`). Entities that are gone, or that stopped being a
        // tilemap, are dropped by the retain after the loop.
        let mut live_tilemaps: std::collections::HashSet<u32> =
            std::collections::HashSet::new();
        s.sprite_batches.clear();
        s.sprites.clear();
        s.sorting.clear();
        s.by_kind.clear();
        s.by_tag.clear();
        for (e, tr) in world.query::<Transform>() {
            Self::mirror_entity(&mut s, world, e, tr, Some(&mut live_tilemaps));
        }
        // A node that was despawned, or whose Matter became something else, must
        // not leave a grid behind for a later node to be handed by id reuse.
        s.tilemaps.retain(|id, _| live_tilemaps.contains(id));
    }

    /// Put ONE entity into the mirror.
    ///
    /// Split out of [`Self::sync_scene`] so that a freshly spawned node can be
    /// added to the mirror without rebuilding it. Every collection it writes to
    /// is an insert or a push, never a replace, so mirroring an entity that is
    /// already in there would duplicate it — the incremental caller is for NEW
    /// entities and nothing else.
    ///
    /// `live` is `Some` only on a full rebuild, where it collects which tilemaps
    /// still exist so the stale ones can be dropped afterwards. There is nothing
    /// to drop when the pass only adds.
    fn mirror_entity(
        s: &mut crate::SceneMirror,
        world: &World,
        e: floptle_core::Entity,
        tr: &Transform,
        mut live: Option<&mut std::collections::HashSet<u32>>,
    ) {
            let id = e.index();
            s.order.push(id);
            s.ents.insert(id, e);
            s.transforms.insert(id, *tr);
            match world.get::<Matter>(e) {
                Some(Matter::Mesh { asset_path }) => {
                    s.models.insert(id, asset_path.clone());
                }
                Some(Matter::Tilemap { cols, rows, tile, data, tileset }) => {
                    if let Some(live) = &mut live {
                        live.insert(id);
                    }
                    // `floptle/0117`: the grid used to be `data.clone()`d here,
                    // unconditionally, twice a frame — a heap allocation and a
                    // memcpy of the whole map whether or not any script ever
                    // looked at it. A 200×200 map is 160 KB a frame of pure
                    // churn, and a level made of several is worse.
                    //
                    // Reuse the buffer when the map has not changed, which is
                    // nearly always: a comparison reads the same bytes a copy
                    // would but allocates nothing, frees nothing, and stops at
                    // the first square that differs.
                    //
                    // Deliberately NOT "clone only for maps a script asked
                    // about": the mirror is built BEFORE the scripts run, so the
                    // first `node:tilemap()` of a session would read a grid that
                    // was not there yet. A quiet wrong answer costs more than
                    // this comparison does.
                    match s.tilemaps.get_mut(&id) {
                        Some(m) if m.data == *data => {
                            m.cols = *cols;
                            m.rows = *rows;
                            m.tile = *tile;
                            if m.tileset != *tileset {
                                m.tileset.clone_from(tileset);
                            }
                        }
                        Some(m) => {
                            m.cols = *cols;
                            m.rows = *rows;
                            m.tile = *tile;
                            // `clone_from` reuses the existing allocation when
                            // it is big enough — a repaint of the same-sized map
                            // is then a memcpy with no allocator traffic at all.
                            m.data.clone_from(data);
                            if m.tileset != *tileset {
                                m.tileset.clone_from(tileset);
                            }
                        }
                        None => {
                            s.tilemaps.insert(
                                id,
                                crate::TilemapMirror {
                                    cols: *cols,
                                    rows: *rows,
                                    tile: *tile,
                                    data: data.clone(),
                                    tileset: tileset.clone(),
                                },
                            );
                        }
                    }
                }
                Some(Matter::Sprite { ppu, size, cell, flip_x, flip_y, pivot }) => {
                    s.sprites.insert(
                        id,
                        crate::SpriteMirror {
                            ppu: *ppu,
                            size: *size,
                            cell: *cell,
                            flip_x: *flip_x,
                            flip_y: *flip_y,
                            pivot: *pivot,
                        },
                    );
                }
                Some(Matter::SpriteBatch { .. }) => {
                    s.sprite_batches.insert(id);
                }
                _ => {}
            }
            if let Some(so) = world.get::<floptle_core::Sorting>(e) {
                let mode = match so.mode {
                    floptle_core::SortMode::Y => "y",
                    floptle_core::SortMode::Order => "order",
                };
                s.sorting.insert(id, (so.layer.clone(), so.order, mode));
            }
            if let Some(spec) = world.get::<floptle_ui::ElementSpec>(e) {
                if let Some(t) = spec.text.as_ref() {
                    s.ui_texts.insert(id, t.text.clone());
                }
                if !spec.style.is_empty() {
                    s.ui_styles.insert(id, spec.style.clone());
                }
                if let Some(img) = spec.image.as_ref()
                    && !img.texture.is_empty()
                {
                    s.ui_textures.insert(id, img.texture.clone());
                }
            }
            // Mirror the numeric fields scripts can reach via node:getcomponent(...).
            let comps = mirror_components(world, e);
            if !comps.is_empty() {
                s.components.insert(id, comps);
            }
            if let Some(i) = world.get::<floptle_core::RepeatIndex>(e) {
                s.repeat_index.insert(id, i.0);
            }
            let cols = crate::mirror_component_colors(world, e);
            if !cols.is_empty() {
                s.component_colors.insert(id, cols);
            }
            // The string half — what a material is WEARING, what an element
            // says. Mirrored for the same reason the numbers are: a field a
            // script can write and cannot read is half a field.
            let strs = crate::mirror_component_strings(world, e);
            if !strs.is_empty() {
                s.component_strings.insert(id, strs);
            }
            if let Some(v) = world.get::<Visible>(e) {
                s.visible.insert(id, v.0);
            }
            if world.get::<floptle_core::Disabled>(e).is_some() {
                s.disabled.insert(id);
            }
            if world.get::<floptle_core::Persistent>(e).is_some() {
                s.persistent.insert(id);
            }
            if let Some(l) = world.get::<floptle_core::Layer>(e) {
                s.layers.insert(id, l.0.clone());
            }
            if let Some(t) = world.get::<floptle_core::Tags>(e) {
                for tag in &t.0 {
                    s.by_tag.entry(tag.clone()).or_default().push(id);
                }
                s.tags.insert(id, t.0.clone());
            }
            if let Some(n) = world.get::<floptle_core::Name>(e) {
                s.names.insert(id, n.0.clone());
                s.by_name.entry(n.0.clone()).or_insert(id);
            }
            if let Some(p) = world.get::<floptle_core::Parent>(e) {
                let pid = p.0.index();
                s.parent.insert(id, pid);
                s.children.entry(pid).or_default().push(id);
            }
            if let Some(sc) = world.get::<Scripts>(e) {
                for inst in &sc.0 {
                    s.by_kind.entry(inst.kind.clone()).or_default().push(id);
                }
                s.scripts.insert(id, sc.0.iter().map(|i| i.kind.clone()).collect());
            }
    }

    /// Add newly created entities to the mirror, leaving the rest of it alone.
    ///
    /// **This is what a `spawn(...)` callback needs and a full re-sync was doing
    /// instead.** The callback has to be able to hold a handle on a node that did
    /// not exist at the last sync, which is an argument for inserting one entity,
    /// not for rebuilding a table of twenty collections from every entity in the
    /// scene. Doing the latter once per spawned node makes building a level
    /// quadratic in how much level is already loaded — invisible when a script
    /// spawns a bullet at a time, and the whole frame budget when it streams a
    /// building.
    pub fn sync_new_entities(&self, world: &World, ents: &[floptle_core::Entity]) {
        let mut s = self.scene.borrow_mut();
        for &e in ents {
            let Some(tr) = world.get::<Transform>(e) else { continue };
            Self::mirror_entity(&mut s, world, e, tr, None);
        }
    }

    /// Write transforms that a node handle modified on OTHER nodes back to the ECS.
    fn flush_scene(&self, world: &mut World) {
        let s = self.scene.borrow();
        for &id in &s.dirty {
            if let (Some(&ent), Some(tr)) = (s.ents.get(&id), s.transforms.get(&id))
                && let Some(slot) = world.get_mut::<Transform>(ent) {
                    *slot = *tr;
                }
        }
    }

    /// The tunables a script declares via its top-level `defaults` table, used to
    /// seed a freshly attached instance's params: `(numeric params, reference
    /// params with their kinds)`. A default of `noderef()` / `scriptref(kind)` /
    /// `componentref(name)` marks a reference param (the Inspector shows a
    /// filtered node picker for it). Empty if none declared or unloadable.
    pub fn script_defaults(&self, path: &Path) -> crate::ScriptDefaults {
        let Ok(src) = std::fs::read_to_string(path) else { return Default::default() };
        let name = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let Ok(env) = build_env(&self.lua, &src, &name) else { return Default::default() };
        let Ok(defaults) = env.get::<Table>("defaults") else { return Default::default() };
        let mut nums = Vec::new();
        let mut refs = Vec::new();
        let mut strs = Vec::new();
        for (k, v) in defaults.pairs::<String, mlua::Value>().flatten() {
            match v {
                mlua::Value::Number(n) => nums.push((k, n as f32)),
                mlua::Value::Integer(n) => nums.push((k, n as f32)),
                // A `flag = false` default is a BOOLEAN tunable: stored as 0/1
                // (the param vec is floats), drawn as a checkbox, and handed
                // back to Lua as a real boolean by `params_table` — so the
                // script sees the type it declared.
                mlua::Value::Boolean(b) => nums.push((k, f32::from(b))),
                mlua::Value::String(s) => {
                    let s = s.to_string_lossy();
                    match crate::env::parse_ref_sentinel(&s) {
                        Some(kind) => refs.push((k, kind)),
                        // A plain string default = a STRING param (a portal's
                        // destination scene, an item id) — Inspector-editable.
                        None => strs.push((k, s.to_string())),
                    }
                }
                _ => {}
            }
        }
        nums.sort_by(|a, b| a.0.cmp(&b.0));
        refs.sort_by(|a, b| a.0.cmp(&b.0));
        strs.sort_by(|a, b| a.0.cmp(&b.0));
        (nums, refs, strs)
    }

    /// Make sure the `(entity, script)` environment is built (hot-reloading on change),
    /// published to the shared env map, and carries its persistent `node` handle — so
    /// cross-references (`findScript`, `node:getscript`, …) resolve no matter the run
    /// order. Returns false if the script is missing or broken this frame. Done for EVERY
    /// script before ANY `update`, so a manager is reachable even by a script that ticks
    /// first.
    fn ensure_instance(&mut self, e: Entity, name: &str, scripts_dir: &Path) -> bool {
        let path = resolve_script_path(scripts_dir, &self.extra_script_dirs, name);
        let Some(generation) = self.ensure_source(name, &path) else {
            self.record_error(name, format!("{name}: script not found ({})", path.display()));
            return false;
        };
        let key = (e.index(), name.to_string());
        let needs_build = self.instances.get(&key).is_none_or(|i| i.generation != generation);
        if needs_build {
            // Don't recompile a known-broken generation every frame; re-emit it
            // to the Scripting tab, which is a live list of what is wrong right
            // now. NOT to the Console — `fail` already said it once, and this
            // path runs every frame for as long as the file stays broken.
            if let Some(err) = self.sources.get(name).and_then(|s| s.error.clone()) {
                self.errors.push(err);
                return false;
            }
            let src = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(err) => {
                    self.fail_load(name, format!("{name}.lua could not be read: {err}"), generation);
                    return false;
                }
            };
            // Mark the current instance while top-level code runs, so a
            // `net.on(...)` at file scope knows its owner.
            *self.net.current.borrow_mut() = Some((e.index(), name.to_string()));
            let built = build_env(&self.lua, &src, name);
            *self.net.current.borrow_mut() = None;
            match built {
                Ok(env) => {
                    // It loaded, so it is no longer broken — and a script that
                    // was fixed must stop being reported as broken by the
                    // handles reading it.
                    if self.broken.borrow_mut().remove(name) {
                        self.broken_read_warned.borrow_mut().retain(|(k, _)| k != name);
                    }
                    self.warn_upvalue_pressure(name, generation, &src);
                    if let Some(old) = self.instances.remove(&key) {
                        let _ = self.lua.remove_registry_value(old.env);
                    }
                    // A rebuild (hot reload) drops the old generation's net
                    // handlers + synced store + UI listeners — the fresh run
                    // re-registers whatever it still asks for.
                    self.drop_net_instance(&key);
                    self.drop_ui_listeners_of(&key);
                    self.setup_synced(&env, &key);
                    match self.lua.create_registry_value(env) {
                        Ok(reg) => {
                            self.instances.insert(
                                key.clone(),
                                Instance {
                                    env: reg,
                                    generation,
                                    started: false,
                                    seen: true,
                                    node: None,
                                },
                            );
                        }
                        Err(err) => {
                            self.fail_load(name, format!("{name}.lua did not load: {err}"), generation);
                            return false;
                        }
                    }
                }
                // LuaJIT's own words are terse and name the wrong line — the
                // upvalue ceiling in particular. Say what happened in the
                // engine's voice, naming the script and the limit
                // (`floptle/0086`).
                Err(err) => {
                    self.fail_load(name, crate::load_error::explain(name, &err.to_string()), generation);
                    return false;
                }
            }
        }
        let Some(inst) = self.instances.get_mut(&key) else { return false };
        inst.seen = true;
        let Ok(env) = self.lua.registry_value::<Table>(&inst.env) else { return false };
        // A persistent `node` handle for this script's own entity, so methods called from
        // OTHER scripts (which don't get the per-call `node` argument) can still reach it.
        if let Ok(h) = new_node_handle(&self.lua, e.index()) {
            let _ = env.set("node", h);
        }
        // Publish the live environment for other scripts' handles.
        // Published as a registry key: a live `Table` per instance is what the
        // auxiliary ref stack runs out of (`floptle/0069`).
        let Ok(key) = self.lua.create_registry_value(&env) else { return false };
        self.envs.borrow_mut().insert((e.index(), name.to_string()), key);
        true
    }

    /// A script instance's REFERENCE params, resolved against the scene by name.
    ///
    /// Its own function because two callers need the same answer and used to
    /// have one and a half: the editor-action path resolved them, and the
    /// pass-1 seed passed an empty list — so a library script's wired node
    /// reference read nil, and for a HOOKLESS script (which never ticks) it
    /// stayed nil for the whole session.
    fn resolve_refs(
        &self,
        env: &Table,
        refs: &[(String, String)],
    ) -> Vec<(String, crate::env::ResolvedRef)> {
        use crate::env::{parse_ref_sentinel, ResolvedRef};
        let s = self.scene.borrow();
        let defaults = env.get::<Table>("defaults").ok();
        refs.iter()
            .map(|(k, target)| {
                let id =
                    (!target.is_empty()).then(|| s.by_name.get(target).copied()).flatten();
                let rk = defaults
                    .as_ref()
                    .and_then(|d| d.get::<String>(k.as_str()).ok())
                    .and_then(|v| parse_ref_sentinel(&v));
                let r = match (rk, id) {
                    (Some(crate::RefKind::Node), Some(id)) => ResolvedRef::Node(id),
                    _ => ResolvedRef::None,
                };
                (k.clone(), r)
            })
            .collect()
    }

    /// Give a freshly built environment its `params` before ANY script runs.
    ///
    /// `tick` seeds `params` too, but only for the instance it is ticking — so
    /// until an instance has had its first tick, its `params` is **nil**, not
    /// the defaults-seeded table `params_table` promises. That is invisible for
    /// a script that only reads its own tunables, and fatal for the pair that
    /// pass 1 exists to support: a script whose `update` runs early calls a
    /// LIBRARY script through `findScript`, the callee reads `params.foo`, and
    /// it raises on `params` being nil. Whether it raises depends on the two
    /// nodes' order in the scene file, which is why it reads as random.
    ///
    /// A hookless library script never ticks at all, so for it this is the only
    /// seed it ever gets.
    ///
    /// Only when unset: once an instance has ticked, `tick`'s table is the live
    /// one — it carries the resolved reference params, which need the world and
    /// so cannot be built here. Two-way param writes live in that table too, and
    /// re-seeding would discard them.
    fn seed_params(
        &self,
        e: Entity,
        name: &str,
        params: &[(String, f32)],
        refs: &[(String, String)],
        strs: &[(String, String)],
    ) {
        let Some(inst) = self.instances.get(&(e.index(), name.to_string())) else { return };
        let Ok(env) = self.lua.registry_value::<Table>(&inst.env) else { return };
        // `raw_get`, so an env whose metatable reaches the globals cannot answer
        // this with somebody else's `params`.
        if env.raw_get::<Value>("params").is_ok_and(|v| !v.is_nil()) {
            return;
        }
        // REFERENCE params too, resolved the same way every other caller
        // resolves them. Seeding without them looked harmless — the tick would
        // fill them in — but a hookless library script never ticks, so its
        // wired-up node reference would have been nil for the entire session.
        let resolved = self.resolve_refs(&env, refs);
        if let Ok(t) = crate::env::params_table(&self.lua, &env, params, &resolved, strs) {
            let _ = env.set("params", t);
        }
    }

    /// Run one already-ensured `(entity, script)` instance's lifecycle for
    /// `pass` — per-frame (`start`/`update`), per-gameplay-tick
    /// (`fixedUpdate`), or post-physics (`lateUpdate`).
    /// Report a script exporting a name a `findScript` handle answers itself —
    /// once per `(script, key)` per session (`floptle/0085`).
    ///
    /// **Why at load.** The handle is a proxy, so the export is reachable from
    /// inside the script and from nowhere else: every cross-script caller gets
    /// the handle's own value instead. What arrives is the wrong TYPE, not a nil
    /// — so nothing raises when the handle resolves, nothing raises when the
    /// field is read, and the eventual `attempt to call field 'x' (a string
    /// value)` points at the caller rather than at the script that took the
    /// name. One real case broke four screens in a shipped game, each only at
    /// the moment it had something to show.
    ///
    /// The collision is decidable here, where both the script and the reserved
    /// list are in hand, and undecidable by anyone reading a call site.
    fn warn_shadowed_handle_keys(&mut self, kind: &str, env: &Table) {
        for (key, purpose) in crate::api::HANDLE_KEYS {
            // `node` is set INTO every env by the host itself (a persistent
            // handle for the script's own entity), so an env having it proves
            // nothing about what the script wrote.
            if *key == "node" {
                continue;
            }
            if matches!(env.get::<Value>(*key), Ok(Value::Nil) | Err(_)) {
                continue;
            }
            if !self.handle_key_warned.insert((kind.to_string(), (*key).to_string())) {
                continue;
            }
            self.logs.borrow_mut().push(crate::ScriptLog {
                level: crate::LogLevel::Warn,
                msg: format!(
                    "{kind}.lua exports `{key}`, which a findScript handle answers itself \
                     ({purpose}) — so `{kind}` can use it and no other script can: a \
                     cross-script `h.{key}` reads the handle's value, not yours. Rename the \
                     export (`{key}Of`, `label`, …). Note `name` is NOT reserved: a script's \
                     own `name` wins, and `kind` still reports which script a handle is."
                ),
                // Line 0: the collision is with the handle, not with a line —
                // the editor's `reservedKey` lint is what points at the export.
                source: Some((kind.to_string(), 0)),
            });
        }
    }

    /// Report a scene param the script no longer declares — once per
    /// `(script, param)` per session (`floptle/0068`).
    ///
    /// **Why this is worth a line in the Console.** A scene's `params:` list
    /// overrides a script's `defaults`. An entry naming something the script
    /// does not declare is not an error today — it is simply a value nobody
    /// reads. Both that and its sibling (a name the script DOES still declare,
    /// pinned to whatever it was when the scene was saved) look identical from
    /// the outside: you change a number in the script, press Play, and nothing
    /// happens. One real case cost two rounds of "the laser still feels wrong"
    /// spent editing numbers the game was never reading.
    ///
    /// This catches the cheap half — the name that means nothing any more. The
    /// pinned half is the Inspector's to show, because there the value is
    /// legitimate and only its AGE is wrong.
    fn warn_unread_params(
        &mut self,
        eid: u32,
        kind: &str,
        params: &[(String, f32)],
        strs: &[(String, String)],
        env: &Table,
    ) {
        if params.is_empty() && strs.is_empty() {
            return;
        }
        let declared = env.get::<Table>("defaults").ok();
        let known = |k: &str| {
            declared.as_ref().is_some_and(|d| {
                !matches!(d.get::<Value>(k), Ok(Value::Nil) | Err(_))
            })
        };
        let unread: Vec<String> = params
            .iter()
            .map(|(k, _)| k)
            .chain(strs.iter().map(|(k, _)| k))
            .filter(|k| !known(k))
            .filter(|k| self.param_warned.insert((kind.to_string(), (*k).to_string())))
            .cloned()
            .collect();
        if unread.is_empty() {
            return;
        }
        let node = self
            .scene
            .borrow()
            .names
            .get(&eid)
            .cloned()
            .unwrap_or_else(|| format!("node {eid}"));
        let scene = self.scene_name.borrow().clone();
        let where_ = if scene.is_empty() {
            format!("{node} · {kind}")
        } else {
            format!("{scene} · {node} · {kind}")
        };
        let list = unread.join(", ");
        let (is, does) = if unread.len() == 1 { ("is", "does") } else { ("are", "do") };
        self.logs.borrow_mut().push(crate::ScriptLog {
            level: crate::LogLevel::Warn,
            msg: format!(
                "{where_}: the scene sets {list}, which the script no longer declares — \
                 {is} stored and never read. Delete the param, or add it to `defaults` if \
                 it {does} belong."
            ),
            // Line 0: the param is in the SCENE file, not on a line of the
            // script — pointing at line 1 would send you to the wrong file.
            source: Some((kind.to_string(), 0)),
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn tick_instance(
        &mut self,
        e: Entity,
        name: &str,
        params: &[(String, f32)],
        refs: &[(String, String)],
        strs: &[(String, String)],
        tr: &mut Transform,
        dt: f32,
        time: f32,
        pass: Pass,
    ) {
        let key = (e.index(), name.to_string());
        let (first, env, mut node_slot) = {
            let Some(inst) = self.instances.get_mut(&key) else { return };
            // `fixedUpdate`/`lateUpdate` never run before `start` — a brand-new
            // instance waits for the next frame pass to start it first.
            if pass != Pass::Frame && !inst.started {
                return;
            }
            let first = !inst.started;
            if pass == Pass::Frame {
                inst.started = true;
            }
            let Ok(env) = self.lua.registry_value::<Table>(&inst.env) else { return };
            // Taken out and put back, because `tick` borrows `self` while it runs.
            (first, env, inst.node.take())
        };
        let eid = e.index();
        if first {
            self.warn_unread_params(eid, name, params, strs, &env);
            self.warn_shadowed_handle_keys(name, &env);
        }
        let body = self.bodies.borrow().get(&eid).copied();
        // Resolve reference params by NAME through the O(1) index — per tick, so
        // a target spawned or renamed mid-play rebinds automatically. The KIND
        // (node / script / component) comes from the declared `defaults` sentinel,
        // and script/component targets validate against the live scene so an
        // invalid wire reads nil rather than a dead handle.
        let resolved: Vec<(String, crate::env::ResolvedRef)> = {
            use crate::env::{parse_ref_sentinel, ResolvedRef};
            let s = self.scene.borrow();
            let envs = self.envs.borrow();
            let defaults = env.get::<Table>("defaults").ok();
            refs.iter()
                .map(|(k, target)| {
                    let id = if target.is_empty() {
                        None
                    } else {
                        s.by_name.get(target).copied()
                    };
                    let kind = defaults
                        .as_ref()
                        .and_then(|d| d.get::<String>(k.as_str()).ok())
                        .and_then(|v| parse_ref_sentinel(&v));
                    let r = match (kind, id) {
                        (Some(crate::RefKind::Node), Some(id)) => ResolvedRef::Node(id),
                        (Some(crate::RefKind::Script(sk)), Some(id))
                            if envs.contains_key(&(id, sk.clone())) =>
                        {
                            ResolvedRef::Script(id, sk)
                        }
                        (Some(crate::RefKind::Component(c)), Some(id))
                            if s.components.get(&id).is_some_and(|m| m.contains_key(&c)) =>
                        {
                            ResolvedRef::Component(id, c)
                        }
                        _ => ResolvedRef::None,
                    };
                    (k.clone(), r)
                })
                .collect()
        };
        // Mark the current instance while its hooks run (`net.on` ownership).
        *self.net.current.borrow_mut() = Some((eid, name.to_string()));
        let result = self.tick(
            &env,
            params,
            &resolved,
            strs,
            tr,
            dt,
            time,
            first,
            eid,
            body,
            pass,
            &mut node_slot,
        );
        *self.net.current.borrow_mut() = None;
        if let Some(inst) = self.instances.get_mut(&key) {
            inst.node = node_slot;
        }
        match result {
            Ok(()) => self.collect_param_writes(&env, name, eid, params, strs),
            Err(err) => self.fail(name, format!("{name}: {err}")),
        }
    }

    /// Persist `params.X = value` writes the hook just made: tunables are
    /// TWO-WAY — a script's write sticks across frames (the next seed reads it
    /// back) and lands in the node's stored params, so the Inspector shows it
    /// live during Play (and Stop reverts it with everything else). Numbers
    /// AND strings; only DECLARED tunables persist — a key present in
    /// `defaults` or the stored params; ad-hoc keys stay frame-local, and
    /// reference params (node/script/component handles) never round-trip.
    fn collect_param_writes(
        &self,
        env: &Table,
        name: &str,
        eid: u32,
        seeded: &[(String, f32)],
        seeded_strs: &[(String, String)],
    ) {
        let Ok(pt) = env.get::<Table>("params") else { return };
        let defaults = env.get::<Table>("defaults").ok();
        for (k, v) in pt.pairs::<String, Value>().flatten() {
            match v {
                Value::Number(_) | Value::Integer(_) => {
                    let new = match v {
                        Value::Number(n) => n as f32,
                        Value::Integer(i) => i as f32,
                        _ => unreachable!(),
                    };
                    // The value this key was SEEDED with: the stored override,
                    // else the declared default. (f32 → f64 → f32 is exact, so
                    // an untouched param compares bit-equal and costs nothing.)
                    let seed = seeded
                        .iter()
                        .find(|(pk, _)| *pk == k)
                        .map(|(_, pv)| *pv)
                        .or_else(|| {
                            defaults
                                .as_ref()
                                .and_then(|d| d.get::<f64>(k.as_str()).ok())
                                .map(|d| d as f32)
                        });
                    let Some(seed) = seed else { continue }; // undeclared: frame-local
                    if new != seed {
                        self.param_writes.borrow_mut().push((
                            eid,
                            name.to_string(),
                            k,
                            crate::ParamWrite::Num(new),
                        ));
                    }
                }
                Value::String(s) => {
                    let new = s.to_string_lossy().to_string();
                    // Declared = stored string override, or a NON-SENTINEL
                    // string default (ref sentinels never round-trip).
                    let seed = seeded_strs
                        .iter()
                        .find(|(pk, _)| *pk == k)
                        .map(|(_, pv)| pv.clone())
                        .or_else(|| {
                            defaults
                                .as_ref()
                                .and_then(|d| d.get::<String>(k.as_str()).ok())
                                .filter(|d| crate::env::parse_ref_sentinel(d).is_none())
                        });
                    let Some(seed) = seed else { continue }; // undeclared: frame-local
                    if new != seed {
                        self.param_writes.borrow_mut().push((
                            eid,
                            name.to_string(),
                            k,
                            crate::ParamWrite::Str(new),
                        ));
                    }
                }
                // `params.flag = true` from Lua — the boolean twin of the
                // numeric arm above (stored as 0/1, seeded from the declared
                // boolean default).
                Value::Boolean(b) => {
                    let new = f32::from(b);
                    let seed = seeded
                        .iter()
                        .find(|(pk, _)| *pk == k)
                        .map(|(_, pv)| *pv)
                        .or_else(|| {
                            defaults
                                .as_ref()
                                .and_then(|d| d.get::<bool>(k.as_str()).ok())
                                .map(f32::from)
                        });
                    let Some(seed) = seed else { continue };
                    if new != seed {
                        self.param_writes.borrow_mut().push((
                            eid,
                            name.to_string(),
                            k,
                            crate::ParamWrite::Num(new),
                        ));
                    }
                }
                _ => continue,
            }
        }
    }

    /// One lifecycle tick against an already-built environment.
    #[allow(clippy::too_many_arguments)]
    fn tick(
        &self,
        env: &Table,
        params: &[(String, f32)],
        refs: &[(String, crate::env::ResolvedRef)],
        strs: &[(String, String)],
        tr: &mut Transform,
        dt: f32,
        time: f32,
        first: bool,
        eid: u32,
        body: Option<BodyState>,
        pass: Pass,
        slot: &mut Option<(RegistryKey, crate::env::NodeStamp)>,
    ) -> mlua::Result<()> {
        env.set("params", params_table(&self.lua, env, params, refs, strs)?)?;
        env.set("time", time as f64)?;
        env.set("dt", dt as f64)?;

        // ONE node table per instance, re-stamped each hook — see `node_table`. A handle
        // kept from `start()` is therefore the same table, and stays live.
        // Held as a REGISTRY key, not a live `Table`, and resolved here — a
        // `Table` alive in Rust occupies a slot on mlua's auxiliary ref stack,
        // which is bounded at ~8,000, and one per script instance is a hard
        // ceiling on how big a scene may be (`floptle/0069`). The registry has
        // no such bound. Resolving costs one raw index, the same thing the
        // instance's env two lines up already does every tick.
        let cached =
            slot.as_ref().and_then(|(k, _)| self.lua.registry_value::<Table>(k).ok()).filter(
                // Entity indices are reused after a despawn; a table tagged with a
                // different one is not this node's, so start over.
                |t| t.raw_get::<u32>("__id").ok() == Some(eid),
            );
        let node = match cached {
            Some(t) => t,
            None => {
                let t = node_table(&self.lua, eid, tr, body)?;
                *slot =
                    Some((self.lua.create_registry_value(&t)?, crate::env::node_stamp(&t, tr)));
                t
            }
        };
        if let Some((_, stamp)) = slot.as_ref() {
            // Writes made through a stashed handle from OUTSIDE this script's hooks (a
            // cross-script `other:knockBack()`, a timer callback) land after the last
            // read-back has drained. Apply them now, before the re-stamp overwrites them.
            let drained = crate::env::drain_node_writes(&node, stamp, tr)?;
            if drained.moved && body.is_some() {
                self.body_pos_changes.borrow_mut().insert(
                    eid,
                    [tr.translation.x, tr.translation.y, tr.translation.z],
                );
            }
            if let Some(v) = drained.vel {
                self.body_changes.borrow_mut().insert(eid, v);
            }
            if let Some(h) = drained.height {
                self.body_height_changes.borrow_mut().insert(eid, h);
            }
            if let Some(p) = drained.tick_pos {
                self.body_pos_changes.borrow_mut().insert(eid, p);
            }
        }
        crate::env::stamp_node_table(&node, tr, body)?;
        if let Some((_, stamp)) = slot.as_mut() {
            *stamp = crate::env::node_stamp(&node, tr);
        }

        let pre = node_pre(tr);
        let ran = (|| -> mlua::Result<bool> {
            match pass {
                Pass::Fixed => {
                    // The per-gameplay-tick hook (constant dt — gameplay/netcode cadence).
                    let Some(f) = lifecycle_fn(env, &["fixedUpdate", "onFixedUpdate"])? else {
                        return Ok(false); // no hook: skip the body read-back (nothing ran)
                    };
                    f.call::<()>((node.clone(), dt as f64))?;
                }
                Pass::Late => {
                    // The post-physics camera pass — followers sample FINAL poses.
                    let Some(f) = lifecycle_fn(env, &["lateUpdate", "onLateUpdate"])? else {
                        return Ok(false);
                    };
                    f.call::<()>((node.clone(), dt as f64))?;
                }
                Pass::Frame => {
                    // Prefer the short hook names (`start`/`update`); `on_start`/`on_update`
                    // still work for older scripts.
                    if first
                        && let Some(f) = lifecycle_fn(env, &["start", "on_start"])? {
                            f.call::<()>(node.clone())?;
                        }
                    if let Some(f) = lifecycle_fn(env, &["update", "on_update"])? {
                        f.call::<()>((node.clone(), dt as f64))?;
                    }
                }
            }
            Ok(true)
        })();
        // Record what the table holds now, whatever happened — an errored hook's partial
        // writes are discarded (the read-back below is skipped), so they must not read as
        // pending writes on the next hook either.
        let finish = |slot: &mut Option<(RegistryKey, crate::env::NodeStamp)>, tr: &Transform| {
            if let Some((_, stamp)) = slot.as_mut() {
                *stamp = crate::env::node_stamp(&node, tr);
            }
        };
        match ran {
            Err(e) => {
                finish(slot, tr);
                return Err(e);
            }
            Ok(false) => {
                finish(slot, tr);
                return Ok(());
            }
            Ok(true) => {}
        }
        // Read back the velocity + height for a physics body — but only when
        // THIS script actually changed them from the seeded values. The node
        // table was seeded from the body's pre-hook state (f32→f64→f32 is
        // exact, so untouched fields compare bit-equal); writing back
        // unconditionally would let a second script on the same node clobber
        // an earlier script's writes with the stale seed (e.g. a weapon
        // script silently canceling the movement controller every frame).
        if let Some(b) = body {
            let vx: f64 = node.get("vx").unwrap_or(0.0);
            let vy: f64 = node.get("vy").unwrap_or(0.0);
            let vz: f64 = node.get("vz").unwrap_or(0.0);
            let vel = [vx as f32, vy as f32, vz as f32];
            if vel != b.vel {
                self.body_changes.borrow_mut().insert(eid, vel);
            }
            let h: f64 = node.get("height").unwrap_or(b.height as f64);
            if h as f32 != b.height {
                self.body_height_changes.borrow_mut().insert(eid, h as f32);
            }
            // OWN-node position writes on a body entity teleport the body,
            // exactly like cross-node handle writes do — without this, the
            // physics writeback reverts the transform next frame and
            // `node.x = spawn_x` (respawns!) silently does nothing.
            let (x, y, z) = (
                node.get::<f64>("x").unwrap_or(pre.x),
                node.get::<f64>("y").unwrap_or(pre.y),
                node.get::<f64>("z").unwrap_or(pre.z),
            );
            if x != pre.x || y != pre.y || z != pre.z {
                self.body_pos_changes.borrow_mut().insert(eid, [x, y, z]);
            }
            // A write to the TICK channel is a body teleport that never touches
            // the render transform — which is the whole point of having it
            // (`docs/rollback-netcode-design.md` §3). It is read back AFTER the
            // transform write above, so a script that sets both gets the one it
            // meant: the deterministic one.
            let tick = [
                node.get::<f64>("tickX").unwrap_or(b.pos[0]),
                node.get::<f64>("tickY").unwrap_or(b.pos[1]),
                node.get::<f64>("tickZ").unwrap_or(b.pos[2]),
            ];
            if tick != b.pos {
                self.body_pos_changes.borrow_mut().insert(eid, tick);
            }
        }
        let out = apply_node(&node, tr, &pre);
        finish(slot, tr);
        out
    }

    /// Stat the source; bump its generation (and clear the cached error) when the
    /// file's mtime changes. Returns the current generation, or `None` if missing.
    fn ensure_source(&mut self, name: &str, path: &Path) -> Option<u64> {
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        mtime?;
        let entry = self.sources.entry(name.to_string()).or_insert(Source {
            generation: 0,
            mtime: None,
            error: None,
        });
        if entry.mtime != mtime {
            entry.mtime = mtime;
            entry.generation += 1;
            entry.error = None;
        }
        Some(entry.generation)
    }

    /// A script failed to LOAD. Three things have to happen, and they are not
    /// the same thing (`floptle/0086`):
    ///
    /// * the message is cached on the source, so the next frame re-emits it
    ///   instead of recompiling a file that cannot compile;
    /// * the kind joins `broken`, so a handle reading it can say "this script
    ///   did not load" rather than handing back the `nil` that a missing export
    ///   also gives;
    /// * and the Console gets it ONCE per version of the file. Failing at load
    ///   means failing every frame, and sixty identical lines a second is how a
    ///   Console stops being read at all.
    fn fail_load(&mut self, name: &str, msg: String, generation: u64) {
        if let Some(src) = self.sources.get_mut(name) {
            src.error = Some(msg.clone());
        }
        self.broken.borrow_mut().insert(name.to_string());
        self.errors.push(msg.clone());
        if self.load_failure_reported.insert((name.to_string(), generation)) {
            self.logs.borrow_mut().push(ScriptLog {
                level: LogLevel::Error,
                msg: msg.clone(),
                source: Some((name.to_string(), error_line(&msg))),
            });
        }
    }

    /// A script failed while RUNNING — a hook raised. The script itself loaded,
    /// so this is not [`fail_load`](Self::fail_load): nothing here touches
    /// `broken`, and the Console wants every occurrence.
    fn fail(&mut self, name: &str, msg: String) {
        if let Some(src) = self.sources.get_mut(name) {
            src.error = Some(msg.clone());
        }
        self.record_error(name, msg);
    }

    /// Warn a script that is one edit from unloadable.
    ///
    /// The ceiling is invisible from inside the editor — there is no way to know
    /// you are at 58 rather than 30 — and crossing it costs the whole script, at
    /// load, with a message that names neither the limit nor the fix. So a
    /// script that loads cleanly but sits within
    /// [`UPVALUE_WARN`](crate::load_error::UPVALUE_WARN) of the wall says so,
    /// once per version of the file.
    ///
    /// This runs where the editor's Lua lint does NOT: on the scripts a scene
    /// actually runs, in a build as well as in the IDE, whether or not anyone
    /// has the file open.
    fn warn_upvalue_pressure(&mut self, name: &str, generation: u64, src: &str) {
        use crate::load_error::{file_scope_locals, UPVALUE_LIMIT, UPVALUE_WARN};
        let n = file_scope_locals(src);
        if n < UPVALUE_WARN {
            return;
        }
        if !self.upvalue_warned.insert((name.to_string(), generation)) {
            return;
        }
        let left = UPVALUE_LIMIT.saturating_sub(n);
        self.logs.borrow_mut().push(ScriptLog {
            level: LogLevel::Warn,
            msg: format!(
                "{name}.lua declares {n} file-scope locals and LuaJIT allows {UPVALUE_LIMIT} \
                 upvalues per function — {left} to go before the script stops loading at all. \
                 Every file-scope `local` is an upvalue of every function below it, so hold \
                 related state in ONE table (`local s = {{ … }}`) or split the long function."
            ),
            source: Some((name.to_string(), 1)),
        });
    }
}

/// Where a script name resolves to.
///
/// The project's own `scripts/` folder first, then any **package** script
/// folders in load order. The project wins on purpose: a project that has a
/// `player.lua` and installs a package that also ships one keeps its own, and a
/// package cannot change what a game's scripts mean by being installed.
///
/// A name that resolves nowhere comes back as the project-relative path it
/// would have had, so the error names the file somebody meant to write.
fn resolve_script_path(scripts_dir: &Path, extra: &[PathBuf], name: &str) -> PathBuf {
    for dir in std::iter::once(scripts_dir).chain(extra.iter().map(|p| p.as_path())) {
        let direct = dir.join(format!("{name}.lua"));
        if direct.exists() {
            return direct;
        }
        let nested = dir.join(name).with_extension("lua");
        if nested.exists() {
            return nested;
        }
    }
    scripts_dir.join(format!("{name}.lua"))
}

#[cfg(test)]
mod host_tests {
    use super::*;

    #[test]
    fn resolve_script_path_supports_nested_script_folders() {
        let dir = std::env::temp_dir().join(format!("floptle-host-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("scripts/fighterScripts")).unwrap();
        std::fs::write(dir.join("scripts/fighterScripts/attack.lua"), "return {}\n").unwrap();

        let path = resolve_script_path(&dir.join("scripts"), &[], "fighterScripts/attack");
        assert_eq!(path, dir.join("scripts/fighterScripts/attack.lua"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod pretty_tests {
    use super::*;

    #[test]
    fn prints_tables_arrays_and_strings_deeply() {
        let lua = Lua::new();
        let v: Value = lua
            .load("return {b = 2, a = 1, list = {1, 2, 3}, s = \"hi\", nested = {x = {y = true}}}")
            .eval()
            .unwrap();
        let s = pretty_value(&v, 0, &mut Vec::new());
        assert!(s.contains("a = 1"), "{s}");
        assert!(s.contains("list = {1, 2, 3}"), "short arrays inline: {s}");
        assert!(s.contains("s = \"hi\""), "nested strings quoted: {s}");
        assert!(s.contains("y = true"), "recurses: {s}");
        // Keys are sorted for stable output.
        assert!(s.find("a = 1").unwrap() < s.find("b = 2").unwrap(), "{s}");
    }

    #[test]
    fn cycles_and_depth_never_hang() {
        let lua = Lua::new();
        let cyc: Value = lua.load("local t = {}; t.me = t; return t").eval().unwrap();
        let s = pretty_value(&cyc, 0, &mut Vec::new());
        assert!(s.contains("<cycle>"), "{s}");
        let deep: Value = lua
            .load("local t = {}; local c = t; for _ = 1, 10 do c.next = {}; c = c.next end; return t")
            .eval()
            .unwrap();
        let s = pretty_value(&deep, 0, &mut Vec::new());
        assert!(s.contains("{…}"), "depth caps: {s}");
    }

    #[test]
    fn engine_handles_print_by_identity() {
        let lua = Lua::new();
        let t = lua.create_table().unwrap();
        t.raw_set("__id", 7u32).unwrap();
        t.raw_set("__comp", "RigidBody").unwrap();
        let s = pretty_value(&Value::Table(t), 0, &mut Vec::new());
        assert_eq!(s, "component \"RigidBody\" (node #7)");
        let t = lua.create_table().unwrap();
        t.raw_set("__id", 3u32).unwrap();
        let s = pretty_value(&Value::Table(t), 0, &mut Vec::new());
        assert!(s.starts_with("node #3"), "{s}");
    }
}
