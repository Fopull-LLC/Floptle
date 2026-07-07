//! The [`ScriptHost`] engine loop: source hot-reload generations, per-(node,
//! script) sandbox instances, the per-frame update (mirror the scene, call
//! `start`/`update`, apply node writes), and log/error capture.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use floptle_core::transform::Transform;
use floptle_core::{Entity, Material, Matter, Scripts, Visible, World};
use mlua::{Lua, Table, Value, Variadic};

use crate::api::{apply_component_field, mirror_components};
use crate::env::{
    apply_node, build_env, lifecycle_fn, material_key, new_node_handle, node_pre, node_table,
    params_table,
};
use crate::preprocess::preprocess;

/// Lua argument tuples for the `gizmo.*` draw calls: positions, then the
/// optional size/length and 0–1 RGB tail.
type GizmoLineArgs = (f64, f64, f64, f64, f64, f64, Option<f64>, Option<f64>, Option<f64>);
type GizmoRayArgs = (f64, f64, f64, f64, f64, f64, Option<f64>, Option<f64>, Option<f64>, Option<f64>);
type GizmoBallArgs = (f64, f64, f64, Option<f64>, Option<f64>, Option<f64>, Option<f64>);
use crate::{
    error_line, gizmo_color, install_handle_api, AnimCmd, AnimInfo, BodyState, GizmoCmd,
    InputSnapshot, Instance, LogLevel, SceneMirror, ScriptHost, ScriptLog, Shared, Source, VfxCmd,
    VfxInfo,
};

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
                let parts: Vec<String> = args
                    .iter()
                    .map(|v| match v {
                        Value::String(s) => s.to_string_lossy().to_string(),
                        Value::Integer(n) => n.to_string(),
                        Value::Number(n) => n.to_string(),
                        Value::Boolean(b) => b.to_string(),
                        Value::Nil => "nil".to_string(),
                        other => format!("{other:?}"),
                    })
                    .collect();
                let msg = parts.join("\t");
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
        // Mouse-lock request channel (drained by the editor each frame). See the field docs.
        let mouse_lock: Rc<RefCell<Option<bool>>> = Rc::new(RefCell::new(None));
        if let Ok(t) = lua.create_table() {
            let held = input.clone();
            let _ = t.set(
                "key",
                lua.create_function(move |_, name: String| {
                    Ok(held.borrow().keys_down.contains(&name.to_lowercase()))
                })
                .ok(),
            );
            let pressed = input.clone();
            let _ = t.set(
                "pressed",
                lua.create_function(move |_, name: String| {
                    Ok(pressed.borrow().keys_pressed.contains(&name.to_lowercase()))
                })
                .ok(),
            );
            let released = input.clone();
            let _ = t.set(
                "released",
                lua.create_function(move |_, name: String| {
                    Ok(released.borrow().keys_released.contains(&name.to_lowercase()))
                })
                .ok(),
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
        {
            let cols = colliders.clone();
            let hus = hulls.clone();
            let so = sim_origin.clone();
            let cur = net.current.clone();
            type Args = (f64, f64, f64, f64, f64, f64, f64, Option<Value>);
            if let Ok(f) = lua.create_function(move |lua,
                (ox, oy, oz, dx, dy, dz, max, ignore): Args| {
                let origin = *so.borrow();
                let o = (glam::DVec3::new(ox, oy, oz) - origin).as_vec3();
                let dir = glam::Vec3::new(dx as f32, dy as f32, dz as f32);
                let solid = floptle_physics::raycast_colliders(&cols.borrow(), o, dir, max as f32);
                // Bodies the ray passes through: the caster's own, plus an
                // optional explicit ignore (a node handle or entity id) — e.g.
                // an orbit camera skipping the character it follows.
                let mut exclude: Vec<u32> = Vec::with_capacity(2);
                if let Some((eid, _)) = cur.borrow().as_ref() {
                    exclude.push(*eid);
                }
                match &ignore {
                    Some(Value::Table(t)) => {
                        if let Ok(eid) = t.raw_get::<u32>("__id") {
                            exclude.push(eid);
                        }
                    }
                    Some(Value::Integer(id)) => exclude.push(*id as u32),
                    Some(Value::Number(id)) => exclude.push(*id as u32),
                    _ => {}
                }
                let body =
                    floptle_physics::raycast_hulls(&hus.borrow(), o, dir, max as f32, &exclude);
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

        // `spawnEffect(key, x, y, z)` — fire a one-shot particle effect at a world
        // point, no node required. The editor spawns a detached instance that plays
        // once and auto-despawns (the fire-and-forget path for hits, pickups, poofs).
        let spawn_effects: Rc<RefCell<Vec<crate::SpawnedEffect>>> =
            Rc::new(RefCell::new(Vec::new()));
        {
            let q = spawn_effects.clone();
            if let Ok(f) = lua.create_function(move |_, (key, x, y, z): (String, f64, f64, f64)| {
                q.borrow_mut().push((key, [x, y, z]));
                Ok(())
            }) {
                let _ = lua.globals().set("spawnEffect", f);
            }
        }

        // The cross-node / cross-script reference layer: a scene-graph mirror plus Lua
        // `node`/`script` handles and the `find`/`findScript` globals (see
        // `install_handle_api`). Shared (interior-mutable) with the handle closures.
        let shared = Shared {
            scene: Rc::new(RefCell::new(SceneMirror::default())),
            bodies: Rc::new(RefCell::new(HashMap::new())),
            body_changes: Rc::new(RefCell::new(HashMap::new())),
            body_height_changes: Rc::new(RefCell::new(HashMap::new())),
            envs: Rc::new(RefCell::new(HashMap::new())),
            model_changes: Rc::new(RefCell::new(HashMap::new())),
            material_changes: Rc::new(RefCell::new(HashMap::new())),
            visible_changes: Rc::new(RefCell::new(HashMap::new())),
            component_changes: Rc::new(RefCell::new(HashMap::new())),
            anim_info: Rc::new(RefCell::new(HashMap::new())),
            anim_commands: Rc::new(RefCell::new(Vec::new())),
            vfx_info: Rc::new(RefCell::new(HashMap::new())),
            vfx_commands: Rc::new(RefCell::new(Vec::new())),
        };
        if let Err(e) = install_handle_api(&lua, &shared) {
            eprintln!("[lua] failed to install the node/script reference API: {e}");
        }
        // The `net.*` API (docs/netcode-design.md §8): command queue out,
        // session state in, `net.on` handler registry, `net.rewind` (§7).
        let synced_stores: Rc<RefCell<HashMap<(u32, String), Table>>> =
            Rc::new(RefCell::new(HashMap::new()));
        if let Err(e) =
            crate::net_api::install_net_api(&lua, &net, &hulls, &sim_origin, &synced_stores)
        {
            eprintln!("[lua] failed to install the net API: {e}");
        }

        Self {
            lua,
            sources: HashMap::new(),
            instances: HashMap::new(),
            errors: Vec::new(),
            logs,
            input,
            bodies: shared.bodies.clone(),
            body_changes: shared.body_changes.clone(),
            body_height_changes: shared.body_height_changes.clone(),
            colliders,
            hulls,
            sim_origin,
            scene: shared.scene.clone(),
            envs: shared.envs.clone(),
            model_changes: shared.model_changes.clone(),
            material_changes: shared.material_changes.clone(),
            visible_changes: shared.visible_changes.clone(),
            component_changes: shared.component_changes.clone(),
            materials: Rc::new(RefCell::new(HashMap::new())),
            project_root,
            mouse_lock,
            anim_info: shared.anim_info.clone(),
            anim_commands: shared.anim_commands.clone(),
            vfx_info: shared.vfx_info.clone(),
            vfx_commands: shared.vfx_commands.clone(),
            gizmos,
            spawn_effects,
            net,
            synced_stores,
            synced_warned: std::collections::HashSet::new(),
            script_skip: std::collections::HashSet::new(),
            frame_skip: std::collections::HashSet::new(),
        }
    }

    /// Feed each animated entity's controller state for this frame (before `run`),
    /// so scripts can read `anim:state()/:time()/:clips()`.
    pub fn set_anim_info(&self, map: HashMap<u32, AnimInfo>) {
        *self.anim_info.borrow_mut() = map;
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
            .map(|((_, kind), env)| (kind.clone(), env.clone()))
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
            self.envs.borrow().iter().map(|(k, env)| (k.clone(), env.clone())).collect();
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
        // frame's batch was never drained.
        self.gizmos.borrow_mut().clear();
        for inst in self.instances.values_mut() {
            inst.seen = false;
        }
        // Mirror the scene graph (names / parents / transforms / scripts) so node handles
        // can traverse and reference any node this frame.
        self.sync_scene(world);

        // Snapshot (entity, scripts) so we can mutate Transforms while iterating.
        let work: Vec<(Entity, Scripts)> =
            world.query::<Scripts>().map(|(e, s)| (e, s.clone())).collect();
        // Pass 1: build/refresh every environment so cross-references (findScript, etc.)
        // resolve regardless of which script ticks first.
        for (e, scripts) in &work {
            for inst in &scripts.0 {
                if inst.enabled {
                    self.ensure_instance(*e, &inst.kind, scripts_dir);
                }
            }
        }
        // Pass 2: run each script's start/update.
        self.run_pass(world, &work, dt, time, false);
        self.flush_writes(world);

        // Drop environments whose (node, script) no longer exists.
        let stale: Vec<(u32, String)> =
            self.instances.iter().filter(|(_, i)| !i.seen).map(|(k, _)| k.clone()).collect();
        for k in stale {
            if let Some(inst) = self.instances.remove(&k) {
                let _ = self.lua.remove_registry_value(inst.env);
            }
            self.envs.borrow_mut().remove(&k);
            self.drop_net_instance(&k);
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
        let work: Vec<(Entity, Scripts)> =
            world.query::<Scripts>().map(|(e, s)| (e, s.clone())).collect();
        self.run_pass(world, &work, dt, time, true);
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
        let (skip, fskip) = (
            std::mem::take(&mut self.script_skip),
            std::mem::take(&mut self.frame_skip),
        );
        self.run_pass(world, &work, dt, time, fixed);
        self.script_skip = skip;
        self.frame_skip = fskip;
        self.flush_writes(world);
    }

    /// Skip these entities' scripts in every pass — a networked CLIENT doesn't
    /// run server-authoritative nodes' scripts (their state arrives in
    /// snapshots). Pass an empty set to clear (Stop / role change).
    pub fn set_script_filter(&mut self, skip: std::collections::HashSet<u32>) {
        self.script_skip = skip;
    }

    /// Skip these entities in the PER-FRAME pass only (`update`) — the driver
    /// re-runs them on the gameplay tick via [`Self::run_frame_for`] instead
    /// (a predicted node in a net session). `fixedUpdate` is unaffected.
    pub fn set_frame_filter(&mut self, skip: std::collections::HashSet<u32>) {
        self.frame_skip = skip;
    }

    /// One lifecycle pass over `work`: the per-frame pass (`start`/`update`) or the
    /// per-tick pass (`fixedUpdate`), with the same self-move write-back rules.
    fn run_pass(&mut self, world: &mut World, work: &[(Entity, Scripts)], dt: f32, time: f32, fixed: bool) {
        for (e, scripts) in work {
            if self.script_skip.contains(&e.index()) {
                continue; // networked: this node's state arrives in snapshots
            }
            if !fixed && self.frame_skip.contains(&e.index()) {
                continue; // predicted: its `update` re-runs on the tick clock
            }
            let Some(mut tr) = world.get::<Transform>(*e).copied() else { continue };
            let tr0 = tr; // pass-start, to detect a self-move via the `node` argument
            let mut ran = false;
            for inst in &scripts.0 {
                if inst.enabled {
                    self.tick_instance(*e, &inst.kind, &inst.params, &mut tr, dt, time, fixed);
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
            // Apply node:getcomponent(...) field writes back to the ECS.
            for ((eid, comp, field), val) in self.component_changes.borrow().iter() {
                if let Some(&ent) = scene.ents.get(eid) {
                    apply_component_field(world, ent, comp, field, *val);
                }
            }
        }
        self.material_changes.borrow_mut().clear();
        self.visible_changes.borrow_mut().clear();
        self.component_changes.borrow_mut().clear();
    }

    /// Rebuild the scene-graph mirror the Lua handles read/write, from the live ECS.
    fn sync_scene(&self, world: &World) {
        let mut s = self.scene.borrow_mut();
        s.order.clear();
        s.names.clear();
        s.parent.clear();
        s.children.clear();
        s.scripts.clear();
        s.transforms.clear();
        s.ents.clear();
        s.dirty.clear();
        s.models.clear();
        s.visible.clear();
        s.components.clear();
        for (e, tr) in world.query::<Transform>() {
            let id = e.index();
            s.order.push(id);
            s.ents.insert(id, e);
            s.transforms.insert(id, *tr);
            if let Some(Matter::Mesh { asset_path }) = world.get::<Matter>(e) {
                s.models.insert(id, asset_path.clone());
            }
            // Mirror the numeric fields scripts can reach via node:getcomponent(...).
            let comps = mirror_components(world, e);
            if !comps.is_empty() {
                s.components.insert(id, comps);
            }
            if let Some(v) = world.get::<Visible>(e) {
                s.visible.insert(id, v.0);
            }
            if let Some(n) = world.get::<floptle_core::Name>(e) {
                s.names.insert(id, n.0.clone());
            }
            if let Some(p) = world.get::<floptle_core::Parent>(e) {
                let pid = p.0.index();
                s.parent.insert(id, pid);
                s.children.entry(pid).or_default().push(id);
            }
            if let Some(sc) = world.get::<Scripts>(e) {
                s.scripts.insert(id, sc.0.iter().map(|i| i.kind.clone()).collect());
            }
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
    /// seed a freshly attached instance's params. Empty if it declares none or
    /// can't be loaded.
    pub fn script_defaults(&self, path: &Path) -> Vec<(String, f32)> {
        let Ok(src) = std::fs::read_to_string(path) else { return Vec::new() };
        let name = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let Ok(env) = build_env(&self.lua, &src, &name) else { return Vec::new() };
        let Ok(defaults) = env.get::<Table>("defaults") else { return Vec::new() };
        let mut out = Vec::new();
        for pair in defaults.pairs::<String, f32>().flatten() {
            out.push(pair);
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Make sure the `(entity, script)` environment is built (hot-reloading on change),
    /// published to the shared env map, and carries its persistent `node` handle — so
    /// cross-references (`findScript`, `node:getscript`, …) resolve no matter the run
    /// order. Returns false if the script is missing or broken this frame. Done for EVERY
    /// script before ANY `update`, so a manager is reachable even by a script that ticks
    /// first.
    fn ensure_instance(&mut self, e: Entity, name: &str, scripts_dir: &Path) -> bool {
        let path = scripts_dir.join(format!("{name}.lua"));
        let Some(generation) = self.ensure_source(name, &path) else {
            self.record_error(name, format!("{name}: script not found ({})", path.display()));
            return false;
        };
        let key = (e.index(), name.to_string());
        let needs_build = self.instances.get(&key).is_none_or(|i| i.generation != generation);
        if needs_build {
            // Don't recompile a known-broken generation every frame; re-emit it.
            if let Some(err) = self.sources.get(name).and_then(|s| s.error.clone()) {
                self.record_error(name, err);
                return false;
            }
            let src = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(err) => {
                    self.fail(name, format!("{name}: {err}"));
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
                    if let Some(old) = self.instances.remove(&key) {
                        let _ = self.lua.remove_registry_value(old.env);
                    }
                    // A rebuild (hot reload) drops the old generation's net
                    // handlers + synced store — the fresh run re-registers.
                    self.drop_net_instance(&key);
                    self.setup_synced(&env, &key);
                    match self.lua.create_registry_value(env) {
                        Ok(reg) => {
                            self.instances.insert(
                                key.clone(),
                                Instance { env: reg, generation, started: false, seen: true },
                            );
                        }
                        Err(err) => {
                            self.fail(name, format!("{name}: {err}"));
                            return false;
                        }
                    }
                }
                Err(err) => {
                    self.fail(name, format!("{name}: {err}"));
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
        self.envs.borrow_mut().insert((e.index(), name.to_string()), env);
        true
    }

    /// Run one already-ensured `(entity, script)` instance's lifecycle for this pass —
    /// `fixed = false` is the per-frame pass (`start`/`update`), `fixed = true` the
    /// per-gameplay-tick pass (`fixedUpdate`).
    #[allow(clippy::too_many_arguments)]
    fn tick_instance(
        &mut self,
        e: Entity,
        name: &str,
        params: &[(String, f32)],
        tr: &mut Transform,
        dt: f32,
        time: f32,
        fixed: bool,
    ) {
        let key = (e.index(), name.to_string());
        let (first, env) = {
            let Some(inst) = self.instances.get_mut(&key) else { return };
            // `fixedUpdate` never runs before `start` — a brand-new instance waits for
            // the next frame pass to start it first.
            if fixed && !inst.started {
                return;
            }
            let first = !inst.started && !fixed;
            if !fixed {
                inst.started = true;
            }
            let Ok(env) = self.lua.registry_value::<Table>(&inst.env) else { return };
            (first, env)
        };
        let eid = e.index();
        let body = self.bodies.borrow().get(&eid).copied();
        // Mark the current instance while its hooks run (`net.on` ownership).
        *self.net.current.borrow_mut() = Some((eid, name.to_string()));
        let result = self.tick(&env, params, tr, dt, time, first, eid, body, fixed);
        *self.net.current.borrow_mut() = None;
        if let Err(err) = result {
            self.fail(name, format!("{name}: {err}"));
        }
    }

    /// One lifecycle tick against an already-built environment.
    #[allow(clippy::too_many_arguments)]
    fn tick(
        &self,
        env: &Table,
        params: &[(String, f32)],
        tr: &mut Transform,
        dt: f32,
        time: f32,
        first: bool,
        eid: u32,
        body: Option<BodyState>,
        fixed: bool,
    ) -> mlua::Result<()> {
        env.set("params", params_table(&self.lua, env, params)?)?;
        env.set("time", time as f64)?;
        env.set("dt", dt as f64)?;

        let node = node_table(&self.lua, eid, tr, body)?;
        let pre = node_pre(tr);
        if fixed {
            // The per-gameplay-tick hook (constant dt — gameplay/netcode cadence).
            if let Some(f) = lifecycle_fn(env, &["fixedUpdate", "onFixedUpdate"])? {
                f.call::<()>((node.clone(), dt as f64))?;
            } else {
                return Ok(()); // no hook: skip the body read-back (nothing ran)
            }
        } else {
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
        }
        apply_node(&node, tr, &pre)
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

    fn fail(&mut self, name: &str, msg: String) {
        if let Some(src) = self.sources.get_mut(name) {
            src.error = Some(msg.clone());
        }
        self.record_error(name, msg);
    }
}
